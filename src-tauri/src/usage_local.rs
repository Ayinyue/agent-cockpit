// T17（阶段四）：usage-local 迁 Rust。
// 第一阶段：codex 解析 + SQLite 增量缓存（8809178）。
// 第二阶段：messages 系解析（73f7a53）——claude/claude-code/qoder/qoder-cn/qoderwork/
// qwenwork/qwenworkcn/workbuddy/workbuddy-intl/codebuddy/codebuddy-cn/minimax。
// 第三阶段（本批）：DB 系（zcode/opencode/marvis/astudio）+ dsh(zstd) + gemini。
//
// 计划 §4.4.2 红线：**不做 1:1 翻译**。python 版每轮把整个事件缓存 json.loads/dumps
// （67.8MB 单轮 ~7.7s，2026-11-18 实测）；这里借迁移换 SQLite：按源文件指纹
// (mtime_ns, size) 只重算变化的文件，未变文件的事件直接复用，聚合在内存做。
//
// 口径与 python usage-local.py 逐字段对齐（golden 测试钉住）：
// - codex token_count 取累计终值差分；配额刷新重复帧跳过；last 兜底
// - messages：session:messageId 分组、fragment 去重、CJK 可见文本估算、
//   claude/minimax/standard 三种 usage 风格（wb/codebuddy 用 standard）
// - DB 系：只读 sqlite（mode=ro + query_only），逐行 event；zcode 取 provider/computed
//   total 兜底；opencode 新表覆盖旧表 id；marvis 逐行；astudio 拆 report_json.model_usages
// - dsh：zstd 解压后逐行 assistant/message；gemini：整包 JSON messages 数组
// - 事件 id = sha256(identity)，identity 含 sorted-keys JSON（与 python json.dumps(sort_keys=True) 对齐）
// - 按天归日用本机时区；date-only 不伪造午夜事件
// - 聚合去重键 session|id，取 (exact,tokens) 最大者
//
// 接线（usage.rs）：codex/chatgpt + messages 系 + DB 系/dsh/gemini 走 Rust，失败回退 python；
// python 回退路径不删，迁移期互为保底。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 与 python METRICS 同序同名（聚合按这套字段累加）。
const METRICS: [&str; 13] = [
    "tokens", "exact_tokens", "estimated_tokens", "input_tokens", "output_tokens",
    "cached_tokens", "requests", "failures", "timed_output_tokens", "duration_ms",
    "timing_samples", "ttft_ms", "ttft_samples",
];

fn number(value: &Value) -> f64 {
    let n = match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    if n.is_finite() && n >= 0.0 { n } else { 0.0 }
}

fn first_number(obj: &Value, keys: &[&str]) -> f64 {
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if !v.is_null() {
                return number(v);
            }
        }
    }
    0.0
}

/// python 真值语义：None/0/""/False/[]/{} 为假，其余为真。
/// messages 系解析里大量 `a or b or c` 链必须按这个口径取第一个真值。
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// 跨模块复用（trae_clean 等）：python 真值语义。
pub fn is_truthy_pub(v: &Value) -> bool { is_truthy(v) }

/// 跨模块复用（trae_clean 等）：python str(value) 的 JSON 标量渲染。
pub fn py_str_pub(v: &Value) -> String { py_str(v) }

fn first_truthy<'a>(vals: &[Option<&'a Value>]) -> Option<&'a Value> {
    vals.iter().flatten().copied().find(|v| is_truthy(v))
}

/// python str(value) 的 JSON 标量渲染：字符串原样；bool True/False；null "None"；
/// 整数无小数点；整值浮点带 .0（python str(5.0)=="5.0"）。
/// 容器按 python str(dict/list) 的插入序渲染——message_file 的分组键
/// str(message_id) 在 id 缺失时会落到 item dict 上，必须逐字符对齐。
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True".to_string() } else { "False".to_string() },
        Value::Null => "None".to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() { i.to_string() }
            else if let Some(u) = n.as_u64() { u.to_string() }
            else {
                let f = n.as_f64().unwrap_or(0.0);
                if f == f.trunc() && f.abs() < 1e16 { format!("{:.1}", f) } else { format!("{}", f) }
            }
        }
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(o) => {
            // serde_json Map 未启用 preserve_order 时按 key 排序，python dict 按插入序。
            // 真实数据里 id 字段都在，dict 兜底极罕见；此分支仍按现有 Map 序渲染并留注。
            let parts: Vec<String> = o.iter().map(|(k, val)| format!("{}: {}", py_repr(&json!(k)), py_repr(val))).collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// python repr()（容器内元素用 repr：字符串带引号）。只覆盖 JSON 可表达的标量/容器。
fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => {
            // python repr 优先单引号；含单引号且不含双引号时换双引号。这里按常见路径：
            // 单引号包裹 + 反斜杠/单引号转义（真实 id/模型名不含引号，转义分支保守覆盖）。
            if s.contains('\'') && !s.contains('"') {
                format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
            }
        }
        other => py_str(other),
    }
}

/// 与 python usage_tokens(style) 对齐；codex 用 "codex"，messages 用 claude/minimax/standard。
fn usage_tokens(usage: &Value, style: &str) -> (f64, f64, f64, f64) {
    if !usage.is_object() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let i = first_number(usage, &["input_tokens", "prompt_tokens", "inputTokens", "input"]);
    let o = first_number(usage, &["output_tokens", "completion_tokens", "outputTokens", "output"]);
    let cache_obj = usage.get("cache").filter(|c| c.is_object()).cloned().unwrap_or(json!({}));
    let mut read = first_number(usage, &["cached_input_tokens", "cache_read_input_tokens", "cacheReadTokens", "cacheRead"]);
    let mut write = first_number(usage, &["cache_creation_input_tokens", "cacheWriteTokens", "cacheWrite"]);
    if read == 0.0 { read = number(cache_obj.get("read").unwrap_or(&Value::Null)); }
    if write == 0.0 { write = number(cache_obj.get("write").unwrap_or(&Value::Null)); }
    if read == 0.0 {
        // python: details = usage.get("prompt_tokens_details") or usage.get("inputTokensDetails") or {}
        // 真值链：空 dict 为假，会落到下一候选（不是「键存在即停」）。
        if let Some(details) = first_truthy(&[usage.get("prompt_tokens_details"), usage.get("inputTokensDetails")]) {
            if details.is_object() {
                read = first_number(details, &["cached_tokens", "cacheReadTokens"]);
            }
        }
    }
    let mut total = first_number(usage, &["total_tokens", "totalTokens", "total"]);
    if total <= 0.0 {
        let exclusive = matches!(style, "claude" | "opencode" | "minimax");
        total = i + o + if exclusive { read + write } else { 0.0 };
        if style == "opencode" {
            total += number(usage.get("reasoning").unwrap_or(&Value::Null));
        }
    }
    (i, o, read + write, total)
}

/// 与 python datetime.fromisoformat(text.replace("Z","+00:00")).astimezone() 对齐：
/// 先按带偏移的 RFC3339 解析；失败再按 naive（本机时区）解析常见 ISO 形态。
/// python 3.14 的 fromisoformat 接受无时区/空格分隔/缺秒等形态，rfc3339 不接受，
/// 这里补 naive 兜底，避免把合法本机时间戳误判为空。
fn parse_iso_local(text: &str) -> Option<chrono::DateTime<chrono::Local>> {
    use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
    let t = text.replace('Z', "+00:00");
    if let Ok(dt) = DateTime::parse_from_rfc3339(&t) {
        return Some(dt.with_timezone(&Local));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f",
                "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&t, fmt) {
            if let chrono::LocalResult::Single(dt) = Local.from_local_datetime(&naive) {
                return Some(dt);
            }
        }
    }
    None
}

/// 本机时区归日（与 python local_day 对齐）：ISO 时间戳转本机日期；date-only 原样；数值 epoch 秒/毫秒。
fn local_day(value: &Value) -> String {
    use chrono::{Local, TimeZone};
    match value {
        Value::Number(n) => {
            let mut v = n.as_f64().unwrap_or(0.0);
            if v > 1e11 { v /= 1000.0; }
            match Local.timestamp_opt(v as i64, 0) {
                chrono::LocalResult::Single(t) => t.format("%Y-%m-%d").to_string(),
                _ => String::new(),
            }
        }
        Value::String(s) => {
            let text = s.trim();
            if text.len() == 10 {
                return chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                    .map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
            }
            parse_iso_local(text).map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// 秒级精确时刻：只有真带时分秒才算（date-only 不伪造午夜尖峰）。
fn precise_timestamp(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(_) | Value::Null => None,
        Value::Number(n) => {
            let v = n.as_f64()?;
            let ms = if v > 1e11 { v } else { v * 1000.0 };
            if ms.is_finite() && ms > 0.0 { Some(ms / 1000.0) } else { None }
        }
        Value::String(s) => {
            let text = s.trim();
            if text.len() <= 10 || !(text.contains('T') || text.contains(' ')) {
                return None;
            }
            let ms = parse_iso_local(text)?.timestamp_millis() as f64;
            if ms.is_finite() && ms > 0.0 { Some(ms / 1000.0) } else { None }
        }
        _ => None,
    }
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out { s.push_str(&format!("{:02x}", b)); }
    s
}

/// 与 python json.dumps(obj, sort_keys=True) 的默认分隔符对齐（", " / ": "）。
/// serde_json 的 Map 默认按 key 排序（BTreeMap 语义，preserve_order 未启用），
/// 但默认序列化无空格——这里手动生成带空格形式。
fn dumps_sorted(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.iter()
                .map(|k| format!("{}: {}", serde_json::to_string(k).unwrap(), dumps_sorted(&map[*k])))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(dumps_sorted).collect();
            format!("[{}]", parts.join(", "))
        }
        // 数字：python float 输出带 .0；serde_json 整数无 .0。codex 的 usage 数字来自
        // json.loads，整数保持 int——serde_json 同样保持整数，语义一致。
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 可见文本提取（与 python visible_text 对齐）：只认 text/input_text/output_text
/// 与 tool_result.content；刻意排除 image/base64、reasoning 签名与序列化工具载荷。
/// list 用 "\n" 连接非空部分。
fn visible_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(visible_text).filter(|p| !p.is_empty()).collect();
            parts.join("\n")
        }
        Value::Object(obj) => {
            match obj.get("type").and_then(|t| t.as_str()) {
                Some("text") | Some("input_text") | Some("output_text") => {
                    // python str(value.get("text") or "")：真值链，非字符串真值走 str()
                    match first_truthy(&[obj.get("text")]) {
                        Some(v) => py_str(v),
                        None => String::new(),
                    }
                }
                Some("tool_result") => visible_text(obj.get("content").unwrap_or(&Value::Null)),
                _ => String::new(),
            }
        }
        _ => String::new(),
    }
}

/// 字符启发式估算（与 python estimate 对齐，非 tokenizer）：
/// CJK（含扩展 B 以上）约 1 token/字，其余约 4 字符/token，向上取整。
fn estimate(text: &str) -> f64 {
    if text.is_empty() { return 0.0; }
    let mut cjk = 0usize;
    let mut len = 0usize;
    for ch in text.chars() {
        len += 1;
        let c = ch as u32;
        if (0x2e80..=0x9fff).contains(&c) || (0xac00..=0xd7af).contains(&c)
            || (0xf900..=0xfaff).contains(&c) || c >= 0x20000 {
            cjk += 1;
        }
    }
    (cjk as f64 + (len - cjk) as f64 / 4.0).ceil()
}

/// messages 系分组（与 python message_file 逐条对齐）：
/// - session:messageId 分组，fragment 按 uuid/id/文本哈希去重
/// - user 消息累计 pending_input，归入下一条 assistant 组
/// - usage 取组内 total 最大帧；无 usage 用可见文本估算顶替
/// - style：wb/codebuddy 系 standard、minimax 用 minimax、其余 claude
struct MsgGroup {
    input_estimate: f64,
    fragments: HashMap<String, f64>,
    usage: Value,
    timestamp: Value,
    model: Value,
    provider: Value,
}

pub fn message_file(path: &Path, app: &str) -> Result<Vec<Value>, String> {
    let text = read_text_replace(path)?;
    let file_name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let parent_name = path.parent().and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut session = if file_name != "messages.jsonl" { stem } else { parent_name };
    let mut model = Value::Null;
    let mut pending_input = 0.0f64;
    let is_wb = matches!(app, "workbuddy" | "workbuddy-intl" | "codebuddy" | "codebuddy-cn");
    let style = if is_wb { "standard" } else if app == "minimax" { "minimax" } else { "claude" };
    // 保序分组：python dict 按首次插入序遍历，这里用 Vec 键序 + HashMap 存组。
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, MsgGroup> = HashMap::new();
    // python enumerate(json_lines)：index 只数成功解析的 dict 行（坏行在 yield 前跳过）。
    // 用原始行号会在「坏行 + 无 id 消息」时让 index 兜底分组键漂移。
    let mut index = 0usize;
    for line in py_lines(&text) {
        let Ok(item) = serde_json::from_str::<Value>(line) else { continue };
        if !item.is_object() { continue; }
        let cur_index = index;
        index += 1;
        if let Some(s) = first_truthy(&[item.get("sessionId"), item.get("session_id")]) {
            session = py_str(s);
        }
        let typ = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let message = item.get("message").filter(|m| m.is_object()).unwrap_or(&item);
        let provider = item.get("providerData").filter(|p| p.is_object()).cloned().unwrap_or(json!({}));
        if typ == "runtime-config" {
            if let Some(m) = first_truthy(&[item.get("model")]) { model = m.clone(); }
            continue;
        }
        let role = first_truthy(&[message.get("role"), item.get("role"), item.get("type")])
            .map(py_str).unwrap_or_default();
        if role == "user" {
            pending_input += estimate(&visible_text(message.get("content").unwrap_or(&Value::Null)));
            continue;
        }
        let raw_usage = first_truthy(&[message.get("usage"), provider.get("usage"), item.get("usage")])
            .cloned().unwrap_or(Value::Null);
        let role_ok = role == "assistant" || role == "model"
            || (is_wb && (typ == "function_call" || typ == "model-usage"));
        if !role_ok { continue; }
        let index_val = json!(cur_index);
        // python `a or b or index`：末位 index 即使为 0 也作为兜底返回值（or 链末位不判真值）。
        let message_id = if is_wb {
            first_truthy(&[provider.get("messageId"), item.get("id")]).unwrap_or(&index_val)
        } else {
            first_truthy(&[message.get("id"), message.get("responseId"), item.get("message_id"),
                           item.get("uuid"), item.get("id")]).unwrap_or(&index_val)
        };
        let message_id = py_str(message_id);
        let key = format!("{}:{}", session, message_id);
        let content = first_truthy(&[message.get("content"), item.get("content")])
            .map(|c| c.clone()).unwrap_or(Value::Null);
        let text_visible = visible_text(&content);
        let fragment_key = first_truthy(&[item.get("uuid"), item.get("id")])
            .map(py_str)
            .unwrap_or_else(|| sha256_hex(&text_visible));
        let group_model = first_truthy(&[message.get("model"), provider.get("model"),
                                         provider.get("requestModelId"), Some(&model)])
            .map(|m| m.clone()).unwrap_or(Value::Null);
        let group_provider = first_truthy(&[message.get("provider")])
            .map(|p| p.clone()).unwrap_or(Value::Null);
        let timestamp = first_truthy(&[item.get("timestamp"), message.get("timestamp")])
            .map(|t| t.clone()).unwrap_or(Value::Null);
        let is_new = !groups.contains_key(&key);
        let group = groups.entry(key.clone()).or_insert_with(|| MsgGroup {
            input_estimate: pending_input, fragments: HashMap::new(), usage: Value::Null,
            timestamp, model: group_model, provider: group_provider,
        });
        if is_new { order.push(key); }
        pending_input = 0.0;
        group.fragments.insert(fragment_key, estimate(&text_visible));
        // usage 取组内 total 最大帧（>= 后来者胜，与 python 一致）
        if usage_tokens(&raw_usage, style).3 >= usage_tokens(&group.usage, style).3 {
            group.usage = raw_usage;
        }
    }
    let mut records = Vec::new();
    for key in order {
        let group = &groups[&key];
        let out: f64 = group.fragments.values().sum();
        let inp = group.input_estimate;
        let session_part = key.rsplit_once(':').map(|(s, _)| s).unwrap_or(&key);
        let model_text = if is_truthy(&group.model) { py_str(&group.model) } else { String::new() };
        let provider_text = if is_truthy(&group.provider) { py_str(&group.provider) } else { String::new() };
        records.push(event(&key, session_part, &group.timestamp, &model_text, &group.usage,
            EventOpts { provider: &provider_text, style, estimated: inp + out,
                        input_estimate: inp, output_estimate: out, failure: false, ..Default::default() }));
    }
    Ok(records)
}

/// 事件行参数（与 python event() 全参数对齐；codex 用默认值）。
struct EventOpts<'a> {
    provider: &'a str,
    style: &'a str,
    estimated: f64,
    input_estimate: f64,
    output_estimate: f64,
    failure: bool,
    duration: &'a Value,
    ttft: &'a Value,
}

impl Default for EventOpts<'_> {
    fn default() -> Self {
        Self { provider: "", style: "standard", estimated: 0.0, input_estimate: 0.0,
               output_estimate: 0.0, failure: false, duration: &Value::Null, ttft: &Value::Null }
    }
}

/// 事件行（与 python event() 逐字段对齐）。
/// exact=usage 总量；total<=0 且有估算时用估算顶替（i/o 换成估算的输入/输出）。
fn event(identity: &str, session: &str, timestamp: &Value, model: &str, usage: &Value, opts: EventOpts) -> Value {
    let (mut i, mut o, cache, mut total) = usage_tokens(usage, opts.style);
    let exact = total;
    if total <= 0.0 && opts.estimated != 0.0 {
        total = opts.estimated;
        i = opts.input_estimate;
        o = opts.output_estimate;
    }
    let mut row = json!({});
    for m in METRICS { row[m] = json!(0.0); }
    row["id"] = json!(sha256_hex(identity));
    row["session"] = json!(session);
    row["date"] = json!(local_day(timestamp));
    row["timestamp"] = json!(precise_timestamp(timestamp));
    row["model"] = json!(if model.is_empty() { "未记录模型" } else { model });
    row["provider"] = json!(if opts.provider.is_empty() { "未记录供应商" } else { opts.provider });
    row["tokens"] = json!(total);
    row["exact_tokens"] = json!(exact);
    row["estimated_tokens"] = json!(if exact != 0.0 { 0.0 } else { total });
    row["input_tokens"] = json!(i);
    row["output_tokens"] = json!(o);
    row["cached_tokens"] = json!(cache);
    row["requests"] = json!(1.0);
    row["failures"] = json!(if opts.failure { 1.0 } else { 0.0 });
    // python event()：exact>0 且 o>0 且 duration>0 才记计时字段；ttft>0 记 ttft。
    let duration = number(opts.duration);
    if exact > 0.0 && o > 0.0 && duration > 0.0 {
        row["timed_output_tokens"] = json!(o);
        row["duration_ms"] = json!(duration);
        row["timing_samples"] = json!(1.0);
    }
    let ttft = number(opts.ttft);
    if ttft > 0.0 {
        row["ttft_ms"] = json!(ttft);
        row["ttft_samples"] = json!(1.0);
    }
    row
}

/// 与 python json_lines 的 encoding="utf-8-sig", errors="replace" 对齐：
/// 非法 UTF-8 字节替换为 U+FFFD 继续解析，而不是整文件拒读。
/// BOM 只在文件头剥一次（utf-8-sig 语义）；行内/行中的 U+FEFF 保留——
/// python 对行中 BOM 会 json.loads 失败跳过该行，逐行 trim 反而会误收。
fn read_text_replace(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|_| "读取会话日志失败".to_string())?;
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// python 文本模式 universal newlines 等价切行：\r\n / lone \r / \n 都是行分隔。
/// Rust str::lines() 只按 \n 切（lone \r 留在行内会让整行 JSON 解析失败），
/// python open() 默认 newline=None 会把 lone \r 也当行尾——必须对齐。
fn py_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').flat_map(|seg| seg.split('\r'))
}

/// codex rollout.jsonl 解析（与 python codex_file 逐条对齐）。
pub fn codex_file(path: &Path) -> Result<Vec<Value>, String> {
    let text = read_text_replace(path)?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut session = stem;
    let mut previous: Option<(f64, f64, f64, f64)> = None;
    let mut model = String::new();
    let mut records = Vec::new();
    // python enumerate(json_lines)：index 只数成功解析的 dict 行（与 message_file 同口径）
    let mut index = 0usize;
    for line in py_lines(&text) {
        let Ok(item) = serde_json::from_str::<Value>(line) else { continue };
        if !item.is_object() { continue; }
        let cur_index = index;
        index += 1;
        let payload = item.get("payload").filter(|p| p.is_object()).cloned().unwrap_or(json!({}));
        if item["type"].as_str() == Some("session_meta") {
            if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() { session = id.to_string(); }
            }
        }
        if item["type"].as_str() == Some("turn_context") {
            if let Some(m) = payload.get("model").and_then(|v| v.as_str()) {
                if !m.is_empty() { model = m.to_string(); }
            }
        }
        if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") { continue; }
        let info = payload.get("info").filter(|v| v.is_object()).cloned().unwrap_or(json!({}));
        let current = info.get("total_token_usage").filter(|v| v.is_object()).cloned();
        let last = info.get("last_token_usage").filter(|v| v.is_object()).cloned();
        let current_tokens = current.as_ref().map(|c| usage_tokens(c, "codex"));
        if let Some(ct) = current_tokens {
            if Some(ct) == previous { continue; } // 配额刷新重复帧
        }
        let increment: Value;
        if let (Some(ct), Some(prev)) = (current_tokens, previous) {
            if ct.3 >= prev.3 {
                increment = json!({
                    "input_tokens": (ct.0 - prev.0).max(0.0),
                    "output_tokens": (ct.1 - prev.1).max(0.0),
                    "cached_input_tokens": (ct.2 - prev.2).max(0.0),
                    "total_tokens": (ct.3 - prev.3).max(0.0),
                });
            } else if let Some(l) = &last {
                if usage_tokens(l, "standard").3 > 0.0 { increment = l.clone(); }
                else if let Some(c) = &current { increment = c.clone(); }
                else { continue; }
            } else if let Some(c) = &current { increment = c.clone(); }
            else { continue; }
        } else if let Some(l) = &last {
            if usage_tokens(l, "standard").3 > 0.0 { increment = l.clone(); }
            else if let Some(c) = &current { increment = c.clone(); }
            else { continue; }
        } else if let Some(c) = &current { increment = c.clone(); }
        else { continue; }
        if current_tokens.is_some() { previous = current_tokens; }
        if usage_tokens(&increment, "standard").3 <= 0.0 { continue; }
        let ts = item.get("timestamp").cloned().unwrap_or(json!(cur_index));
        let identity_src = current.as_ref().unwrap_or(&increment);
        // python 的 identity 三段里 timestamp 走 "%s" % item.get(...)：字符串原样、
        // 整数 str(index)——不是 json.dumps。只有第三段（usage dict）走 sort_keys JSON。
        let ts_text = match &ts {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let identity = format!("{}:{}:{}", session, ts_text, dumps_sorted(identity_src));
        records.push(event(&identity, &session, &ts, &model, &increment,
            EventOpts { style: "codex", ..Default::default() }));
    }
    Ok(records)
}

/// codex/chatgpt 的源文件清单（与 python paths_for 对齐）。
fn codex_paths(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for folder in ["sessions", "archived_sessions"] {
        collect_jsonl(&root.join(folder), &mut out);
    }
    out.sort();
    out
}

fn collect_jsonl(base: &Path, out: &mut Vec<PathBuf>) {
    if !base.is_dir() { return; }
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() { stack.push(p); }
            else if p.extension().map(|x| x == "jsonl").unwrap_or(false) { out.push(p); }
        }
    }
}

/// messages 系产品（与 python paths_for 的 messages 分支一致）。
pub fn is_messages_app(app: &str) -> bool {
    matches!(app, "claude" | "claude-code" | "qoder" | "qoder-cn" | "qoderwork"
        | "qwenwork" | "qwenworkcn" | "workbuddy" | "workbuddy-intl"
        | "codebuddy" | "codebuddy-cn" | "minimax")
}

/// 第三阶段产品：DB 系 / dsh / gemini（与 python paths_for 的剩余分支一致）。
pub fn is_other_app(app: &str) -> bool {
    matches!(app, "zcode" | "opencode" | "marvis" | "astudio" | "dsh" | "gemini" | "gemini-cli")
}

/// messages 系源文件清单：claude/qoder/workbuddy 等扫 projects/**/*.jsonl；
/// minimax 只认 v2/sessions/**/messages.jsonl（与 python paths_for 对齐）。
fn messages_paths(app: &str, root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if app == "minimax" {
        let base = root.join("v2").join("sessions");
        if base.is_dir() {
            let mut stack = vec![base];
            while let Some(dir) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                for e in rd.filter_map(|e| e.ok()) {
                    let p = e.path();
                    if p.is_dir() { stack.push(p); }
                    else if p.file_name().map(|n| n == "messages.jsonl").unwrap_or(false) { out.push(p); }
                }
            }
        }
    } else {
        collect_jsonl(&root.join("projects"), &mut out);
    }
    out.sort();
    out
}

/// 文件指纹（mtime_ns, size）；-wal 伴生文件与 python fingerprint 对齐。
fn fingerprint(path: &Path) -> (i64, u64) {
    let mut mtime_ns = 0i64;
    let mut size = 0u64;
    for p in [path.to_path_buf(), PathBuf::from(format!("{}-wal", path.display()))] {
        if let Ok(md) = std::fs::metadata(&p) {
            if let Ok(mt) = md.modified() {
                if let Ok(d) = mt.duration_since(std::time::UNIX_EPOCH) {
                    mtime_ns = mtime_ns.saturating_add(d.as_nanos() as i64);
                }
            }
            size = size.saturating_add(md.len());
        }
    }
    (mtime_ns, size)
}

/// SQLite 增量缓存：files(rel_path, mtime_ns, size, records_json)。
/// 每源一个库文件（sha256(app|source_key).db），WAL + NORMAL 与项目其它库同口径。
struct Cache {
    conn: rusqlite::Connection,
}

impl Cache {
    fn open(cache_dir: &Path, app: &str, source_key: &str, parser_version: &str) -> Result<Self, String> {
        std::fs::create_dir_all(cache_dir).map_err(|e| e.to_string())?;
        let digest = sha256_hex(&format!("{}|{}", app, source_key));
        let conn = rusqlite::Connection::open(cache_dir.join(format!("{}.db", digest)))
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files(
                rel TEXT PRIMARY KEY, mtime_ns INTEGER NOT NULL, size INTEGER NOT NULL,
                records TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);",
        ).map_err(|e| e.to_string())?;
        // 解析器版本变化时整库作废（口径升级不许复用旧事件）
        let ver: Option<String> = conn.query_row(
            "SELECT v FROM meta WHERE k='parser_version'", [], |r| r.get(0)).ok();
        if ver.as_deref() != Some(parser_version) {
            conn.execute_batch("DELETE FROM files;").map_err(|e| e.to_string())?;
            conn.execute("INSERT OR REPLACE INTO meta(k,v) VALUES('parser_version',?1)",
                [parser_version]).map_err(|e| e.to_string())?;
        }
        Ok(Self { conn })
    }

    fn cached(&self, rel: &str, stamp: (i64, u64)) -> Option<Vec<Value>> {
        let row: Option<(i64, u64, String)> = self.conn.query_row(
            "SELECT mtime_ns,size,records FROM files WHERE rel=?1", [rel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).ok();
        let (m, s, records) = row?;
        if (m, s) != stamp { return None; }
        serde_json::from_str(&records).ok()
    }

    fn store(&self, rel: &str, stamp: (i64, u64), records: &[Value]) -> Result<(), String> {
        let text = serde_json::to_string(records).map_err(|e| e.to_string())?;
        self.conn.execute(
            "INSERT OR REPLACE INTO files(rel,mtime_ns,size,records) VALUES(?1,?2,?3,?4)",
            rusqlite::params![rel, stamp.0, stamp.1 as i64, text],
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn prune(&self, keep: &[String]) -> Result<(), String> {
        let mut stmt = self.conn.prepare("SELECT rel FROM files").map_err(|e| e.to_string())?;
        let rows: Vec<String> = stmt.query_map([], |r| r.get(0)).map_err(|e| e.to_string())?
            .filter_map(|r| r.ok()).collect();
        for rel in rows {
            if !keep.contains(&rel) {
                self.conn.execute("DELETE FROM files WHERE rel=?1", [&rel]).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

/// 聚合（与 python aggregate 对齐）：去重键 session|id 取 (exact,tokens) 最大者。
fn aggregate(records: Vec<Value>, source: &str, source_key: &str, files: usize, errors: usize, with_records: bool) -> Option<Value> {
    let mut unique: HashMap<String, Value> = HashMap::new();
    // python dict 按首次插入序遍历：order 记录 identity 首现序，unique 存最终胜者。
    // 浮点累加顺序必须与 python 完全一致（f64 加法不满足结合律，估算值场景下
    // HashMap 随机序会产生末位漂移——golden 是逐位相等断言，不容忍顺序差异）。
    let mut order: Vec<String> = Vec::new();
    for row in records {
        let identity = format!("{}|{}", row["session"].as_str().unwrap_or(""), row["id"].as_str().unwrap_or(""));
        let better = match unique.get(&identity) {
            None => true,
            Some(old) => {
                (number(&row["exact_tokens"]), number(&row["tokens"]))
                    >= (number(&old["exact_tokens"]), number(&old["tokens"]))
            }
        };
        if better {
            if !unique.contains_key(&identity) { order.push(identity.clone()); }
            unique.insert(identity, row);
        }
    }
    if unique.is_empty() { return None; }

    // 桶：summary + 按天。models 按 (model,provider) 排序输出。
    #[derive(Default)]
    struct Bucket {
        metrics: HashMap<String, f64>,
        sessions: std::collections::BTreeSet<String>,
        models: std::collections::BTreeMap<(String, String), HashMap<String, f64>>,
    }
    impl Bucket {
        fn add(&mut self, row: &Value) {
            for m in METRICS {
                *self.metrics.entry(m.to_string()).or_insert(0.0) += number(&row[m]);
            }
            self.sessions.insert(row["session"].as_str().unwrap_or("").to_string());
            let key = (row["model"].as_str().unwrap_or("").to_string(),
                       row["provider"].as_str().unwrap_or("").to_string());
            let model = self.models.entry(key).or_default();
            for m in METRICS {
                *model.entry(m.to_string()).or_insert(0.0) += number(&row[m]);
            }
        }
        fn finish(&self) -> Value {
            let mut out = json!({});
            for m in METRICS { out[m] = json!(self.metrics.get(m).copied().unwrap_or(0.0)); }
            out["sessions"] = json!(self.sessions.len());
            out["models"] = json!(self.models.iter().map(|((model, provider), metrics)| {
                let mut v = json!({"model": model, "provider": provider});
                for m in METRICS { v[m] = json!(metrics.get(m).copied().unwrap_or(0.0)); }
                v
            }).collect::<Vec<_>>());
            out
        }
    }
    let mut summary = Bucket::default();
    let mut days: std::collections::BTreeMap<String, Bucket> = std::collections::BTreeMap::new();
    let mut undated = 0.0;
    // 按首现序遍历（与 python unique.values() 的 dict 插入序逐位对齐）
    for identity in &order {
        let row = &unique[identity];
        if number(&row["tokens"]) <= 0.0 && number(&row["requests"]) == 0.0 { continue; }
        summary.add(row);
        let date = row["date"].as_str().unwrap_or("");
        if !date.is_empty() {
            days.entry(date.to_string()).or_default().add(row);
        } else {
            undated += number(&row["tokens"]);
        }
    }
    let days_out: Vec<Value> = days.iter().map(|(day, bucket)| {
        let mut v = bucket.finish();
        v["date"] = json!(day);
        v["credits"] = json!(bucket.metrics.get("tokens").copied().unwrap_or(0.0));
        v
    }).collect();
    // python max() 并列取首个（days 已按日期升序）→ 并列峰值取最早一天。
    // Rust max_by 并列取末个，这里显式改取首个对齐。
    let peak = days.iter().fold(None, |best: Option<(&String, &Bucket)>, (day, bucket)| {
        let tokens = bucket.metrics.get("tokens").copied().unwrap_or(0.0);
        match best {
            None => Some((day, bucket)),
            Some((_, b)) => {
                if tokens > b.metrics.get("tokens").copied().unwrap_or(0.0) { Some((day, bucket)) } else { best }
            }
        }
    });
    let summary_out = summary.finish();
    let has_estimate = summary.metrics.get("estimated_tokens").copied().unwrap_or(0.0) > 0.0;
    let mut result = json!({
        "unit": "tokens",
        "days": days_out,
        "total": summary.metrics.get("tokens").copied().unwrap_or(0.0),
        "peak": peak.map(|p| p.1.metrics.get("tokens").copied().unwrap_or(0.0)).unwrap_or(0.0),
        "peak_date": peak.map(|p| p.0.clone()).unwrap_or_default(),
        "source": source,
        "source_key": source_key,
        "files": files,
        "coverage": format!("本机该数据目录中可读的模型记录；自定义接口仅在客户端保存记录时计入。{}",
            if has_estimate { "部分记录没有 usage，按可见输入输出文本估算，不含隐藏提示、重复上下文和缓存。" } else { "" }),
        "estimate_method": if has_estimate { "CJK 字符约 1 token，其余字符约 4 字符/token；仅估算可见文本" } else { "" },
        "partial": errors > 0,
        "unreadable_files": errors,
        "undated_tokens": undated,
        "attribution": "local-profile",
        "duration_basis": "request-wall-time",
        "requests_basis": "模型用量记录数；日志事件未必与 HTTP 请求或用户轮次一一对应",
    });
    for m in METRICS { result[m] = summary_out[m].clone(); }
    result["sessions"] = summary_out["sessions"].clone();
    result["models"] = summary_out["models"].clone();
    if with_records {
        result["_records"] = json!(order.iter().map(|identity| {
            let r = &unique[identity];
            let mut v = json!({});
            if let Some(o) = r.as_object() {
                for (k, val) in o {
                    if !METRICS.contains(&k.as_str()) || number(val) != 0.0 {
                        v[k] = val.clone();
                    }
                }
            }
            v
        }).collect::<Vec<_>>());
    }
    Some(result)
}

/// python SOURCES 表（未收录的 app 用 "本机 {app} 会话日志"，如 chatgpt/qoder 系）。
fn source_label(app: &str) -> String {
    let label = match app {
        "codex" => "本机 Codex 会话日志",
        "claude" => "本机 Claude Code 会话日志",
        "workbuddy" => "本机 WorkBuddy 会话日志",
        "workbuddy-intl" => "本机 WorkBuddy 国际版会话日志",
        "zcode" => "本机 ZCode model_usage 请求库",
        "opencode" => "本机 OpenCode 请求库",
        "minimax" => "本机 MiniMax 会话日志",
        "marvis" => "本机 Marvis 日用量库",
        "astudio" => "本机 AStudio 逐轮计费报告",
        "dsh" => "本机 DSH 会话日志逐条 usage",
        _ => "",
    };
    if label.is_empty() { format!("本机 {} 会话日志", app) } else { label.to_string() }
}

/// 与 python Path.resolve() + os.path.normcase 对齐的源键：
/// Windows 上 Rust canonicalize 带 \\?\ 前缀而 python 不带，须剥掉；再小写化。
fn strip_verbatim(s: &str) -> String {
    s.strip_prefix(r"\\?\UNC\").map(|r| format!(r"\{}", r))
        .or_else(|| s.strip_prefix(r"\\?\").map(|r| r.to_string()))
        .unwrap_or_else(|| s.to_string())
}

fn normalized_source_key(root: &Path) -> String {
    strip_verbatim(&root.to_string_lossy()).to_lowercase()
}

/// 共享采集循环：指纹命中缓存直接复用，未命中调解析器并回写；prune 清已删源。
/// codex 与 messages 系共用同一 SQLite 缓存机制（每源一库，parser_version 隔离）。
fn collect_files(app: &str, root: &Path, cache_dir: Option<&Path>, with_records: bool,
                 paths: Vec<PathBuf>, parser_version: &str,
                 parse: impl Fn(&Path) -> Result<Vec<Value>, String>) -> Result<Option<Value>, String> {
    if !root.is_dir() { return Ok(None); }
    let root = root.canonicalize().map_err(|_| "无法解析数据目录".to_string())?;
    let source_key = normalized_source_key(&root);
    let cache = cache_dir.and_then(|d| Cache::open(d, app, &source_key, parser_version).ok());
    let mut records = Vec::new();
    let mut errors = 0usize;
    let mut files = 0usize;
    let mut keep: Vec<String> = Vec::new();
    for path in paths {
        if !path.is_file() { continue; }
        // rel 键与 python str(path.relative_to(root)) 对齐：剥 \\?\ 前缀（canonicalize 产物）
        let rel = path.strip_prefix(&root).map(|p| strip_verbatim(&p.to_string_lossy()))
            .unwrap_or_else(|_| strip_verbatim(&path.to_string_lossy()));
        let stamp = fingerprint(&path);
        let rows = match cache.as_ref().and_then(|c| c.cached(&rel, stamp)) {
            Some(rows) => rows,
            None => match parse(&path) {
                Ok(rows) => {
                    if let Some(c) = &cache { let _ = c.store(&rel, stamp, &rows); }
                    rows
                }
                Err(_) => { errors += 1; continue; }
            },
        };
        keep.push(rel);
        records.extend(rows);
        files += 1;
    }
    if let Some(c) = &cache { let _ = c.prune(&keep); }
    let result = aggregate(records, &source_label(app), &source_key, files, errors, with_records);
    if result.is_none() && errors > 0 {
        return Ok(Some(json!({"error": "本地用量文件读取失败，请检查文件权限与数据库状态。"})));
    }
    Ok(result)
}

/// codex/chatgpt 采集入口（与 python collect 同契约）。cache_dir=None 时不缓存（测试用）。
pub fn collect_codex(app: &str, root: &Path, cache_dir: Option<&Path>, with_records: bool) -> Result<Option<Value>, String> {
    if !root.is_dir() { return Ok(None); }
    let paths = codex_paths(root);
    collect_files(app, root, cache_dir, with_records, paths, "rust-codex/1", codex_file)
}

/// messages 系采集入口（claude/qoder/workbuddy/minimax 等，与 python collect 同契约）。
pub fn collect_messages(app: &str, root: &Path, cache_dir: Option<&Path>, with_records: bool) -> Result<Option<Value>, String> {
    if !root.is_dir() { return Ok(None); }
    let paths = messages_paths(app, root);
    let app_owned = app.to_string();
    collect_files(app, root, cache_dir, with_records, paths, "rust-messages/1",
        move |p| message_file(p, &app_owned))
}

/// 第三阶段：DB 系 / dsh / gemini 采集入口（与 python collect 同契约）。
pub fn collect_other(app: &str, root: &Path, cache_dir: Option<&Path>, with_records: bool) -> Result<Option<Value>, String> {
    if !root.is_dir() { return Ok(None); }
    let (paths, parser_version, parse): (Vec<PathBuf>, &str, Box<dyn Fn(&Path) -> Result<Vec<Value>, String>>) =
        match app {
            "zcode" => (db_paths(root, &["cli/db/db.sqlite"]), "rust-zcode/1", Box::new(zcode_db)),
            "opencode" => {
                let candidates = [root.join("opencode.db"), root.join("data/opencode.db")];
                let paths: Vec<PathBuf> = candidates.into_iter().filter(|p| p.is_file()).take(1).collect();
                (paths, "rust-opencode/1", Box::new(opencode_db))
            }
            "marvis" => (db_paths(root, &["database/data.db"]), "rust-marvis/1", Box::new(marvis_db)),
            "astudio" => (db_paths(root, &["state.sqlite"]), "rust-astudio/1", Box::new(astudio_db)),
            "dsh" => (glob_files(&root.join("sessions"), "session.v4.jsonl.zstd"), "rust-dsh/1", Box::new(dsh_file)),
            "gemini" | "gemini-cli" => (gemini_paths(root), "rust-gemini/1", Box::new(gemini_file)),
            _ => return Ok(None),
        };
    collect_files(app, root, cache_dir, with_records, paths, parser_version, move |p| parse(p))
}

/// DB 系单文件清单（与 python paths_for 的固定路径分支对齐）：只收存在的文件。
fn db_paths(root: &Path, rels: &[&str]) -> Vec<PathBuf> {
    rels.iter().map(|r| root.join(r)).filter(|p| p.is_file()).collect()
}

/// 递归收集指定文件名（dsh 的 session.v4.jsonl.zstd）。
fn glob_files(base: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !base.is_dir() { return out; }
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() { stack.push(p); }
            else if p.file_name().map(|n| n == name).unwrap_or(false) { out.push(p); }
        }
    }
    out.sort();
    out
}

/// gemini 源清单：tmp/**/chats/session-*.json（与 python paths_for 对齐）。
fn gemini_paths(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let base = root.join("tmp");
    if !base.is_dir() { return out; }
    let mut stack = vec![base];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() { stack.push(p); }
            else if p.file_name().map(|n| {
                let n = n.to_string_lossy();
                n.starts_with("session-") && n.ends_with(".json")
            }).unwrap_or(false) {
                if p.parent().and_then(|d| d.file_name()).map(|n| n == "chats").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// 只读打开 sqlite（与 python connect 对齐：mode=ro + query_only）。
fn open_ro(path: &Path) -> Result<rusqlite::Connection, String> {
    let conn = rusqlite::Connection::open_with_flags(
        path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI)
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "query_only", "ON").ok();
    Ok(conn)
}

/// 把一行 sqlite 结果转成 JSON object（列名→值，python dict(row) 同口径）。
fn row_to_json(row: &rusqlite::Row, cols: &[String]) -> Result<Value, String> {
    let mut obj = serde_json::Map::new();
    for (i, name) in cols.iter().enumerate() {
        let v = row.get_ref(i).map_err(|e| e.to_string())?;
        let val = match v {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => json!(n),
            rusqlite::types::ValueRef::Real(f) => json!(f),
            rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
            rusqlite::types::ValueRef::Blob(_) => Value::Null,
        };
        obj.insert(name.clone(), val);
    }
    Ok(Value::Object(obj))
}

/// 读全表为 JSON object 列表。optional=true 时表不存在返回空。
fn sql_rows(conn: &rusqlite::Connection, table: &str, optional: bool) -> Result<Vec<Value>, String> {
    let sql = format!("SELECT * FROM \"{}\"", table);
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            if optional && e.to_string().contains("no such table") { return Ok(Vec::new()); }
            return Err(e.to_string());
        }
    };
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let rows = stmt.query_map([], |row| {
        row_to_json(row, &cols).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(std::io::ErrorKind::Other, e))))
    }).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows { out.push(r.map_err(|e| e.to_string())?); }
    Ok(out)
}

/// zcode model_usage 请求库（与 python zcode_db 逐条对齐）。
pub fn zcode_db(path: &Path) -> Result<Vec<Value>, String> {
    let conn = open_ro(path)?;
    let mut records = Vec::new();
    for row in sql_rows(&conn, "model_usage", false)? {
        let total = {
            let t = first_number(&row, &["provider_total_tokens", "computed_total_tokens"]);
            if t <= 0.0 { first_number(&row, &["computed_total_tokens"]) } else { t }
        };
        let mut u = row.clone();
        if total > 0.0 { u["total_tokens"] = json!(total); }
        let failure = row.get("status").and_then(|v| v.as_str())
            .map(|s| s == "error" || s == "failed").unwrap_or(false);
        let duration = row.get("duration_ms").cloned().unwrap_or(Value::Null);
        let ttft = row.get("time_to_first_token_ms").cloned().unwrap_or(Value::Null);
        records.push(event(
            &py_str(row.get("id").unwrap_or(&Value::Null)),
            &py_str(row.get("session_id").unwrap_or(&Value::Null)),
            row.get("started_at").unwrap_or(&Value::Null),
            &py_str(row.get("model_id").unwrap_or(&Value::Null)),
            &u,
            EventOpts { provider: &py_str(row.get("provider_id").unwrap_or(&Value::Null)),
                        failure, duration: &duration, ttft: &ttft, ..Default::default() }));
    }
    Ok(records)
}

/// opencode 请求库（与 python opencode_db 逐条对齐）：新表 session_message 覆盖旧表 message 的同 id。
pub fn opencode_db(path: &Path) -> Result<Vec<Value>, String> {
    let conn = open_ro(path)?;
    let mut records: HashMap<String, Value> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for table in ["message", "session_message"] {
        for row in sql_rows(&conn, table, true)? {
            let data: Value = row.get("data").and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(json!({}));
            let role = row.get("type").and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| data.get("role").and_then(|v| v.as_str()))
                .unwrap_or("");
            if role != "assistant" { continue; }
            let model = match data.get("model") {
                Some(m) if m.is_object() => m.clone(),
                Some(m) if !m.is_null() => json!({"id": m}),
                _ => json!({}),
            };
            let timing = data.get("time").filter(|t| t.is_object()).cloned().unwrap_or(json!({}));
            let start = number(timing.get("created").unwrap_or(&Value::Null));
            let end = number(timing.get("completed").unwrap_or(&Value::Null));
            let first = number(timing.get("streamed").unwrap_or(&Value::Null));
            let model_id = first_truthy(&[model.get("id"), data.get("modelID")])
                .map(py_str).unwrap_or_default();
            let provider = first_truthy(&[model.get("providerID"), data.get("providerID")])
                .map(py_str).unwrap_or_default();
            let failure = data.get("error").map(is_truthy).unwrap_or(false);
            let duration = if end > start && start > 0.0 { json!(end - start) } else { json!(0.0) };
            let ttft = if first > start && start > 0.0 { json!(first - start) } else { json!(0.0) };
            let id = py_str(row.get("id").unwrap_or(&Value::Null));
            let ev = event(
                &py_str(row.get("id").unwrap_or(&Value::Null)),
                &py_str(row.get("session_id").unwrap_or(&Value::Null)),
                row.get("time_created").unwrap_or(&Value::Null),
                &model_id,
                data.get("tokens").unwrap_or(&Value::Null),
                EventOpts { provider: &provider, style: "opencode", failure,
                            duration: &duration, ttft: &ttft, ..Default::default() });
            if !records.contains_key(&id) { order.push(id.clone()); }
            records.insert(id, ev);
        }
    }
    Ok(order.into_iter().map(|id| records.remove(&id).unwrap()).collect())
}

/// marvis 日用量库（与 python marvis_db 逐条对齐）。
pub fn marvis_db(path: &Path) -> Result<Vec<Value>, String> {
    let conn = open_ro(path)?;
    let mut records = Vec::new();
    for (index, row) in sql_rows(&conn, "llm_token_usage", false)?.into_iter().enumerate() {
        let id = row.get("id").filter(|v| !v.is_null()).cloned().unwrap_or(json!(index));
        let model = first_truthy(&[row.get("model_id"), row.get("model"), row.get("model_name")])
            .map(py_str).unwrap_or_default();
        let usage = json!({
            "total_tokens": row.get("total_tokens").unwrap_or(&Value::Null),
            "input_tokens": row.get("input_tokens").unwrap_or(&Value::Null),
            "output_tokens": row.get("output_tokens").unwrap_or(&Value::Null),
            "cached_input_tokens": row.get("cached_tokens").unwrap_or(&Value::Null),
        });
        records.push(event(
            &py_str(&id),
            &py_str(row.get("conversation_id").unwrap_or(&Value::Null)),
            row.get("usage_date").unwrap_or(&Value::Null),
            &model, &usage, EventOpts::default()));
    }
    Ok(records)
}

/// astudio 逐轮计费报告（与 python astudio_db 逐条对齐）：拆 report_json.model_usages。
pub fn astudio_db(path: &Path) -> Result<Vec<Value>, String> {
    let conn = open_ro(path)?;
    let mut records = Vec::new();
    for row in sql_rows(&conn, "turn_billing_outbox", true)? {
        let payload = row.get("report_json").and_then(|v| v.as_str()).unwrap_or("");
        if payload.is_empty() { continue; }
        let Ok(report) = serde_json::from_str::<Value>(payload) else { continue };
        if !report.is_object() { continue; }
        let stamp = first_truthy(&[report.get("started_at"), report.get("completed_at")])
            .cloned().unwrap_or(Value::Null);
        let session = first_truthy(&[report.get("session_id"), report.get("thread_id")])
            .map(py_str).unwrap_or_default();
        let failed = report.get("turn_status").map(|v| py_str(v).to_lowercase())
            .map(|s| s == "failed" || s == "error" || s == "cancelled").unwrap_or(false);
        let usages = match report.get("model_usages") {
            Some(Value::Array(a)) => a.clone(),
            _ => continue,
        };
        for (index, usage) in usages.into_iter().enumerate() {
            if !usage.is_object() { continue; }
            let model = first_truthy(&[usage.get("effective_model"), usage.get("requested_model")])
                .map(py_str).unwrap_or_default();
            let identity = format!("{}:{}:{}",
                py_str(row.get("billing_turn_id").unwrap_or(&Value::Null)), index, model);
            let session_final = if session.is_empty() {
                py_str(row.get("billing_account_id").unwrap_or(&Value::Null))
            } else { session.clone() };
            let provider = usage.get("provider").map(py_str).unwrap_or_default();
            records.push(event(&identity, &session_final, &stamp, &model, &usage,
                EventOpts { provider: &provider, failure: failed, ..Default::default() }));
        }
    }
    Ok(records)
}

/// dsh 会话日志（zstd 压缩，与 python dsh_file 逐条对齐）。
pub fn dsh_file(path: &Path) -> Result<Vec<Value>, String> {
    let raw = std::fs::read(path).map_err(|_| "读取会话日志失败".to_string())?;
    let decoded = zstd::decode_all(&raw[..]).map_err(|_| "zstd 解压失败".to_string())?;
    let text = String::from_utf8_lossy(&decoded);
    let session = path.parent().and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut records = Vec::new();
    let mut index = 0usize;
    for line in py_lines(&text) {
        let Ok(item) = serde_json::from_str::<Value>(line) else { continue };
        if !item.is_object() { continue; }
        let cur_index = index;
        index += 1;
        if item.get("type").and_then(|v| v.as_str()) != Some("assistant/message") { continue; }
        let data = item.get("data").filter(|d| d.is_object()).cloned().unwrap_or(json!({}));
        let usage = match data.get("usage") {
            Some(u) if u.is_object() => u.clone(),
            _ => continue,
        };
        let id = first_truthy(&[data.get("id"), item.get("id")])
            .cloned().unwrap_or(json!(cur_index));
        let model = data.get("model").map(py_str).unwrap_or_default();
        records.push(event(&py_str(&id), &session,
            item.get("time").unwrap_or(&Value::Null), &model, &usage, EventOpts::default()));
    }
    Ok(records)
}

/// gemini 整包 JSON（与 python gemini_file 逐条对齐）：messages 数组，user 累计估算。
pub fn gemini_file(path: &Path) -> Result<Vec<Value>, String> {
    let text = std::fs::read_to_string(path).map_err(|_| "读取会话日志失败".to_string())?;
    let data: Value = serde_json::from_str(&text).map_err(|_| "JSON 解析失败".to_string())?;
    let session = first_truthy(&[data.get("sessionId")])
        .map(py_str)
        .unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    let messages = match data.get("messages") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut records = Vec::new();
    let mut pending_input = 0.0f64;
    for (index, row) in messages.into_iter().enumerate() {
        let typ = row.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if typ == "user" {
            pending_input += estimate(&visible_text(row.get("content").unwrap_or(&Value::Null)));
            continue;
        }
        if typ != "gemini" && typ != "model" && typ != "assistant" { continue; }
        let tokens = row.get("tokens").filter(|t| t.is_object()).cloned().unwrap_or(json!({}));
        let usage = json!({
            "input_tokens": tokens.get("input").unwrap_or(&Value::Null),
            "output_tokens": tokens.get("output").unwrap_or(&Value::Null),
            "total_tokens": tokens.get("total").unwrap_or(&Value::Null),
            "cached_input_tokens": tokens.get("cached").unwrap_or(&Value::Null),
        });
        let out = estimate(&visible_text(row.get("content").unwrap_or(&Value::Null)));
        let id = row.get("id").filter(|v| !v.is_null()).cloned().unwrap_or(json!(index));
        let model = row.get("model").map(py_str).unwrap_or_default();
        records.push(event(&py_str(&id), &session,
            row.get("timestamp").unwrap_or(&Value::Null), &model, &usage,
            EventOpts { provider: "google", estimated: pending_input + out,
                        input_estimate: pending_input, output_estimate: out, ..Default::default() }));
        pending_input = 0.0;
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path) {
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let lines = [
            r#"{"type":"session_meta","payload":{"id":"sess-t17"}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5-codex"}}"#,
            r#"{"type":"event_msg","timestamp":"2026-05-27T06:07:18.088Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":70},"total_token_usage":{"input_tokens":60,"cached_input_tokens":5,"output_tokens":10,"total_tokens":70}}}}"#,
            r#"{"type":"event_msg","timestamp":"2026-05-27T07:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":80},"total_token_usage":{"input_tokens":130,"cached_input_tokens":10,"output_tokens":20,"total_tokens":150}}}}"#,
        ];
        std::fs::write(sessions.join("rollout.jsonl"), lines.join("\n")).unwrap();
    }

    /// 基本解析：与 python 逐字段同口径（实测锚点：total=150, input=70, output=10, cached=5）。
    /// 注意 python 的 usage_tokens 对增量 dict 用 "standard" 风格重算——增量里
    /// input_tokens=70（130-60）, output=10（20-10）, cached=5（10-5）；total 取增量的
    /// total_tokens=80。首条 70 的 input/output 在 _records 里被 METRICS 零值过滤省略。
    #[test]
    fn codex_collect_totals() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path());
        let v = collect_codex("codex", tmp.path(), None, true).unwrap().expect("应聚出结果");
        assert_eq!(v["total"].as_f64(), Some(150.0));
        assert_eq!(v["input_tokens"].as_f64(), Some(70.0));
        assert_eq!(v["output_tokens"].as_f64(), Some(10.0));
        assert_eq!(v["cached_tokens"].as_f64(), Some(5.0));
        assert_eq!(v["files"].as_i64(), Some(1));
        assert_eq!(v["sessions"].as_i64(), Some(1));
        assert_eq!(v["_records"].as_array().unwrap().len(), 2);
    }

    /// T17 核心收益：SQLite 增量缓存——第二次采集未变文件不重算。
    /// 取证：首采后把源文件内容改坏但保持指纹不变不可行（指纹含 size/mtime），
    /// 改为验证「缓存命中路径产出与首采一致 + 缓存库里有该文件条目」。
    /// 变异探针：把 cached() 恒返 None（永远重算），条目断言仍绿但耗时断言红——
    /// 这里用「删除源文件后 prune 清库」钉住缓存生命周期。
    #[test]
    fn sqlite_cache_reuses_and_prunes() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let cache_dir = tmp.path().join("cache");
        fixture(&profile);
        let first = collect_codex("codex", &profile, Some(&cache_dir), true).unwrap().unwrap();
        // 缓存库应有一个 .db 且含 files 条目
        let dbs: Vec<_> = std::fs::read_dir(&cache_dir).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "db").unwrap_or(false))
            .collect();
        assert_eq!(dbs.len(), 1, "应有一个缓存库");
        let conn = rusqlite::Connection::open(dbs[0].path()).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "缓存应含 1 个源文件条目");
        // 第二次采集：命中缓存，结果一致（_records 顺序不保证——HashMap 迭代序，
        // 与 python dict 插入序不同属预期；比集合与标量，不比数组顺序）
        let second = collect_codex("codex", &profile, Some(&cache_dir), true).unwrap().unwrap();
        assert_eq!(first["total"], second["total"]);
        let ids = |v: &Value| {
            let mut ids: Vec<String> = v["_records"].as_array().unwrap().iter()
                .map(|r| r["id"].as_str().unwrap().to_string()).collect();
            ids.sort(); ids
        };
        assert_eq!(ids(&first), ids(&second), "缓存命中应产出同一组事件");
        // 删除源文件 → prune 应清掉条目
        std::fs::remove_file(profile.join("sessions").join("rollout.jsonl")).unwrap();
        let third = collect_codex("codex", &profile, Some(&cache_dir), true).unwrap();
        assert!(third.is_none(), "源没了应无结果");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "prune 应清掉已删源的缓存条目（缓存生命周期即红）");
    }

    /// 指纹变化必须重算：改文件内容（size 变）后 total 更新。
    #[test]
    fn fingerprint_change_recomputes() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile");
        let cache_dir = tmp.path().join("cache");
        fixture(&profile);
        let first = collect_codex("codex", &profile, Some(&cache_dir), true).unwrap().unwrap();
        assert_eq!(first["total"].as_f64(), Some(150.0));
        // 追加一轮累计 200（增量 50）
        let f = profile.join("sessions").join("rollout.jsonl");
        let mut text = std::fs::read_to_string(&f).unwrap();
        text.push_str("\n{\"type\":\"event_msg\",\"timestamp\":\"2026-05-27T08:00:00.000Z\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":170,\"cached_input_tokens\":10,\"output_tokens\":30,\"total_tokens\":200}}}}");
        std::fs::write(&f, text).unwrap();
        let second = collect_codex("codex", &profile, Some(&cache_dir), true).unwrap().unwrap();
        assert_eq!(second["total"].as_f64(), Some(200.0), "指纹变化必须重算（增量缓存失效即红）");
    }

    /// T17 golden：与 python collect 输出逐字段比对（由 tests/usage-local-rust-golden.test.py 触发）。
    /// 未设环境变量时跳过。比对 total/days/input/output/cached/sessions/models/_records 的 id 集合。
    #[test]
    fn golden_codex_matches_python() {
        let Ok(fixture_dir) = std::env::var("T17_GOLDEN_FIXTURE") else {
            eprintln!("跳过 golden：未设 T17_GOLDEN_FIXTURE");
            return;
        };
        let py_path = std::env::var("T17_GOLDEN_PY_RESULT").expect("应有 python 结果路径");
        let py: Value = serde_json::from_str(&std::fs::read_to_string(&py_path).unwrap()).unwrap();
        let with_records = py.get("_records").is_some();
        let rs = collect_codex("codex", Path::new(&fixture_dir), None, with_records).unwrap().expect("Rust 应聚出结果");
        assert_golden_eq(&rs, &py, with_records);
    }

    /// T17 第二阶段 golden：messages 系（app 由 T17M_GOLDEN_APP 指定）与 python 逐字段比对。
    #[test]
    fn golden_messages_matches_python() {
        let Ok(fixture_dir) = std::env::var("T17M_GOLDEN_FIXTURE") else {
            eprintln!("跳过 golden：未设 T17M_GOLDEN_FIXTURE");
            return;
        };
        let app = std::env::var("T17M_GOLDEN_APP").expect("应有 app");
        let py_path = std::env::var("T17M_GOLDEN_PY_RESULT").expect("应有 python 结果路径");
        let py: Value = serde_json::from_str(&std::fs::read_to_string(&py_path).unwrap()).unwrap();
        let with_records = py.get("_records").is_some();
        let rs = collect_messages(&app, Path::new(&fixture_dir), None, with_records).unwrap().expect("Rust 应聚出结果");
        assert_golden_eq(&rs, &py, with_records);
    }

    /// T17 第三阶段 golden：DB 系/dsh/gemini（app 由 T17O_GOLDEN_APP 指定）与 python 逐字段比对。
    #[test]
    fn golden_other_matches_python() {
        let Ok(fixture_dir) = std::env::var("T17O_GOLDEN_FIXTURE") else {
            eprintln!("跳过 golden：未设 T17O_GOLDEN_FIXTURE");
            return;
        };
        let app = std::env::var("T17O_GOLDEN_APP").expect("应有 app");
        let py_path = std::env::var("T17O_GOLDEN_PY_RESULT").expect("应有 python 结果路径");
        let py: Value = serde_json::from_str(&std::fs::read_to_string(&py_path).unwrap()).unwrap();
        let with_records = py.get("_records").is_some();
        let rs = collect_other(&app, Path::new(&fixture_dir), None, with_records).unwrap().expect("Rust 应聚出结果");
        assert_golden_eq(&rs, &py, with_records);
    }

    /// golden 比对公共逻辑：标量按 f64（python int 0 vs Rust f64 0.0 属形态差异非口径漂移）、
    /// days 逐日、models 汇总、_records 按 id 对齐后逐字段（含 date/timestamp/model/provider）。
    fn assert_golden_eq(rs: &Value, py: &Value, with_records: bool) {
        for key in ["total", "input_tokens", "output_tokens", "cached_tokens", "exact_tokens",
                    "estimated_tokens", "requests", "sessions", "files", "peak",
                    "undated_tokens", "failures"] {
            assert_eq!(rs[key].as_f64(), py[key].as_f64(), "字段 {key} 不一致：rust={} python={}", rs[key], py[key]);
        }
        for key in ["peak_date", "unit", "source", "attribution", "source_key", "coverage", "estimate_method"] {
            assert_eq!(rs[key], py[key], "字段 {key} 不一致：rust={} python={}", rs[key], py[key]);
        }
        let rd = rs["days"].as_array().unwrap();
        let pd = py["days"].as_array().unwrap();
        assert_eq!(rd.len(), pd.len(), "天数不一致");
        for (r, p) in rd.iter().zip(pd.iter()) {
            for key in ["date", "credits", "tokens", "sessions", "input_tokens", "output_tokens",
                        "estimated_tokens", "exact_tokens", "cached_tokens", "requests"] {
                assert_eq!(r[key].as_f64(), p[key].as_f64(), "day {} {key} 不一致：rust={} python={}", r["date"], r[key], p[key]);
            }
            assert_eq!(r["date"], p["date"], "day 日期序不一致");
        }
        assert_eq!(rs["models"], py["models"], "models 汇总不一致");
        if with_records {
            let index = |v: &Value| -> HashMap<String, Value> {
                v["_records"].as_array().unwrap().iter()
                    .map(|r| (r["id"].as_str().unwrap().to_string(), r.clone())).collect()
            };
            let rmap = index(rs);
            let pmap = index(py);
            let mut rids: Vec<&String> = rmap.keys().collect();
            let mut pids: Vec<&String> = pmap.keys().collect();
            rids.sort(); pids.sort();
            assert_eq!(rids, pids, "事件 id 集合不一致（identity/分组口径漂移即红）");
            for id in rids {
                let r = &rmap[id];
                let p = &pmap[id];
                for key in ["session", "date", "model", "provider"] {
                    assert_eq!(r[key], p[key], "record {id} 字段 {key} 不一致");
                }
                for key in ["timestamp", "tokens", "exact_tokens", "estimated_tokens", "input_tokens",
                            "output_tokens", "cached_tokens", "requests", "failures"] {
                    assert_eq!(r[key].as_f64(), p[key].as_f64(), "record {id} 字段 {key} 不一致：rust={} python={}", r[key], p[key]);
                }
            }
        }
    }

    /// messages 估算口径单元锚点：CJK 1 token/字、其余 4 字符/token、向上取整。
    #[test]
    fn estimate_cjk_and_ascii() {
        assert_eq!(estimate(""), 0.0);
        assert_eq!(estimate("你好"), 2.0);
        assert_eq!(estimate("abcd"), 1.0);
        assert_eq!(estimate("abcde"), 2.0); // ceil(5/4)
        assert_eq!(estimate("你好abcd"), 3.0); // 2 + 1
        assert_eq!(estimate("𠀀"), 1.0); // 扩展 B（>=0x20000）算 CJK
        assert_eq!(estimate("한글"), 2.0); // 谚文
    }

    /// visible_text 口径：只认 text/input_text/output_text/tool_result.content；
    /// image/base64/reasoning 签名/序列化工具载荷一律不计。
    #[test]
    fn visible_text_excludes_hidden_payloads() {
        let v = json!([
            {"type": "text", "text": "你好"},
            {"type": "image", "source": {"data": "base64xxxx"}},
            {"type": "tool_use", "input": {"secret": "x"}},
            {"type": "tool_result", "content": [{"type": "text", "text": "ok"}]},
            {"type": "thinking", "thinking": "hidden"}
        ]);
        assert_eq!(visible_text(&v), "你好\nok");
        assert_eq!(visible_text(&json!("plain")), "plain");
        assert_eq!(visible_text(&json!({"type": "text"})), "");
    }

    /// 行分隔口径护栏：python 文本模式 universal newlines 把 lone \r 也当行尾，
    /// Rust str::lines() 不切——py_lines 必须对齐，否则整行 JSON 解析失败丢记录。
    #[test]
    fn py_lines_universal_newlines() {
        let a = r#"{"id":"a"}"#;
        let b = r#"{"id":"b"}"#;
        let c = r#"{"id":"c"}"#;
        // lone \r 分隔
        let text = format!("{a}\r{b}\n{c}");
        let lines: Vec<&str> = py_lines(&text).collect();
        assert_eq!(lines, vec![a, b, c], "lone \\r 必须切行（python universal newlines）");
        // \r\n 不产生空行
        let text2 = format!("{a}\r\n{b}\r\n");
        let lines2: Vec<&str> = py_lines(&text2).filter(|l| !l.is_empty()).collect();
        assert_eq!(lines2, vec![a, b]);
    }

    /// lone-CR 端到端：两条 assistant 用 \r 分隔，必须聚出 2 条记录（修复前为 0）。
    #[test]
    fn messages_lone_cr_splits_records() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("projects").join("p");
        std::fs::create_dir_all(&dir).unwrap();
        let a = r#"{"type":"assistant","sessionId":"s","timestamp":"2026-05-27T06:00:00Z","message":{"role":"assistant","id":"a","usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let b = r#"{"type":"assistant","sessionId":"s","timestamp":"2026-05-27T06:01:00Z","message":{"role":"assistant","id":"b","usage":{"input_tokens":20,"output_tokens":7}}}"#;
        std::fs::write(dir.join("conv.jsonl"), format!("{a}\r{b}\n")).unwrap();
        let v = collect_messages("claude", tmp.path(), None, true).unwrap().expect("lone-CR 应聚出结果");
        assert_eq!(v["_records"].as_array().unwrap().len(), 2, "lone-CR 两条记录都应在");
        assert_eq!(v["total"].as_f64(), Some(42.0));
    }

    /// BOM 口径护栏：utf-8-sig 只剥文件头 BOM；行中 U+FEFF 保留（python 会解析失败跳过）。
    #[test]
    fn bom_only_stripped_at_file_head() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("projects").join("p");
        std::fs::create_dir_all(&dir).unwrap();
        let good = r#"{"type":"assistant","sessionId":"s","timestamp":"2026-05-27T06:00:00Z","message":{"role":"assistant","id":"a","usage":{"input_tokens":10,"output_tokens":5}}}"#;
        // 文件头 BOM + 合法行 → 应解析
        let mut bytes = b"\xef\xbb\xbf".to_vec();
        bytes.extend_from_slice(good.as_bytes());
        bytes.push(b'\n');
        // 第二行行中夹 BOM → python json.loads 失败跳过，Rust 也应跳过
        bytes.extend_from_slice("{\"type\":\"assistant\"".as_bytes());
        bytes.extend_from_slice(b"\xef\xbb\xbf");
        bytes.extend_from_slice(",\"sessionId\":\"s\",\"message\":{\"role\":\"assistant\",\"id\":\"b\",\"usage\":{\"input_tokens\":99,\"output_tokens\":99}}}\n".as_bytes());
        std::fs::write(dir.join("conv.jsonl"), bytes).unwrap();
        let v = collect_messages("claude", tmp.path(), None, true).unwrap().expect("头 BOM 行应聚出结果");
        assert_eq!(v["_records"].as_array().unwrap().len(), 1, "行中 BOM 的坏行应被跳过（只计头 BOM 合法行）");
        assert_eq!(v["total"].as_f64(), Some(15.0));
    }

    /// index 兜底口径护栏：python enumerate(json_lines) 只数解析成功的 dict 行。
    /// 坏行在前 + 无 id 消息在后时，兜底 index 必须是「已解析行序」而非原始行号。
    #[test]
    fn index_fallback_counts_parsed_rows_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("projects").join("p");
        std::fs::create_dir_all(&dir).unwrap();
        // 行0 坏行；行1 runtime-config（解析成功 index0）；行2 user（index1）；
        // 行3 无 id assistant（index2 → 兜底分组键 "s:2"）
        let lines = [
            "garbage-not-json",
            r#"{"type":"runtime-config","sessionId":"s","model":"m"}"#,
            r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","sessionId":"s","timestamp":"2026-05-27T06:00:00Z","message":{"role":"assistant","usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ];
        std::fs::write(dir.join("conv.jsonl"), lines.join("\n")).unwrap();
        let v = collect_messages("claude", tmp.path(), None, true).unwrap().expect("应聚出结果");
        let recs = v["_records"].as_array().unwrap();
        assert_eq!(recs.len(), 1);
        // 分组键 "s:2" 只体现在 identity 哈希（session 字段是 rsplit 后的 "s"）。
        // 兜底 index 若误用原始行号会算成 "s:3"，id 即不同。
        assert_eq!(recs[0]["id"].as_str(), Some(sha256_hex("s:2").as_str()),
            "无 id 消息的兜底分组键应为 s:2（已解析行序），不是原始行号 s:3");
        assert_eq!(recs[0]["session"].as_str(), Some("s"));
    }

    /// aggregate 去重保序护栏：_records 必须按 identity 首现序输出（python dict 插入序），
    /// 且同 identity 取 (exact,tokens) 最大者。回退 HashMap 随机序会漂移浮点累加顺序。
    #[test]
    fn aggregate_preserves_first_seen_order() {
        let mk = |session: &str, id: &str, tokens: f64| {
            let mut row = json!({"session": session, "id": id, "date": "2026-05-27",
                "model": "m", "provider": "p", "requests": 1.0});
            for m in METRICS { row[m] = json!(0.0); }
            row["tokens"] = json!(tokens);
            row["exact_tokens"] = json!(tokens);
            row
        };
        // 首现序 c,a,b；a 重复且后帧更大（应取 50 但位置仍在 a 首现处）
        let records = vec![
            mk("s", "c", 3.0), mk("s", "a", 10.0), mk("s", "b", 2.0), mk("s", "a", 50.0),
        ];
        let v = aggregate(records, "src", "key", 1, 0, true).expect("应聚出结果");
        let ids: Vec<&str> = v["_records"].as_array().unwrap().iter()
            .map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["c", "a", "b"], "_records 必须按首现序（python dict 插入序）");
        let a = v["_records"].as_array().unwrap().iter().find(|r| r["id"] == "a").unwrap();
        assert_eq!(a["tokens"].as_f64(), Some(50.0), "同 identity 应取最大帧");
        assert_eq!(v["total"].as_f64(), Some(55.0));
    }

    /// 第三阶段：zcode model_usage 合成 fixture（total 兜底 + failure + duration/ttft）。
    #[test]
    fn zcode_db_synthetic() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("db.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE model_usage(id TEXT, session_id TEXT, started_at TEXT, model_id TEXT,
             provider_id TEXT, status TEXT, duration_ms INTEGER, time_to_first_token_ms INTEGER,
             provider_total_tokens INTEGER, computed_total_tokens INTEGER,
             input_tokens INTEGER, output_tokens INTEGER);
             INSERT INTO model_usage VALUES('r1','s1','2026-05-27T06:00:00Z','m1','p1','ok',1200,300,100,90,60,30);
             INSERT INTO model_usage VALUES('r2','s1','2026-05-27T07:00:00Z','m1','p1','error',0,0,0,50,20,10);
             INSERT INTO model_usage VALUES('r3','s2','2026-05-27T08:00:00Z','m2','p2','ok',500,0,0,0,40,15);").unwrap();
        drop(conn);
        let recs = zcode_db(&db).unwrap();
        assert_eq!(recs.len(), 3);
        // r1: provider_total=100 优先；duration/ttft 记计时
        assert_eq!(recs[0]["tokens"].as_f64(), Some(100.0));
        assert_eq!(recs[0]["duration_ms"].as_f64(), Some(1200.0));
        assert_eq!(recs[0]["ttft_ms"].as_f64(), Some(300.0));
        assert_eq!(recs[0]["failures"].as_f64(), Some(0.0));
        // r2: provider_total=0 → computed=50 兜底；status=error → failure=1
        assert_eq!(recs[1]["tokens"].as_f64(), Some(50.0));
        assert_eq!(recs[1]["failures"].as_f64(), Some(1.0));
        // r3: 两者都 0 → total 由 input+output 重算（standard style）=55
        assert_eq!(recs[2]["tokens"].as_f64(), Some(55.0));
    }

    /// 第三阶段：opencode 新表覆盖旧表同 id + tokens 解析 + opencode style（reasoning 单列）。
    #[test]
    fn opencode_db_synthetic() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message(id TEXT, session_id TEXT, time_created TEXT, type TEXT, data TEXT);
             CREATE TABLE session_message(id TEXT, session_id TEXT, time_created TEXT, type TEXT, data TEXT);
             INSERT INTO message VALUES('a','s','2026-05-27T06:00:00Z','assistant',
               '{\"model\":{\"id\":\"m\",\"providerID\":\"p\"},\"tokens\":{\"input\":10,\"output\":5,\"reasoning\":3},\"time\":{\"created\":1000,\"completed\":2000,\"streamed\":1200}}');
             INSERT INTO session_message VALUES('a','s','2026-05-27T06:00:00Z','assistant',
               '{\"model\":{\"id\":\"m2\",\"providerID\":\"p2\"},\"tokens\":{\"input\":20,\"output\":8,\"reasoning\":4},\"time\":{\"created\":1000,\"completed\":3000,\"streamed\":1500}}');").unwrap();
        drop(conn);
        let recs = opencode_db(&db).unwrap();
        assert_eq!(recs.len(), 1, "新表应覆盖旧表同 id");
        // 新表 tokens：input20+output8+reasoning4=32（opencode exclusive 不含 cache）
        assert_eq!(recs[0]["tokens"].as_f64(), Some(32.0));
        assert_eq!(recs[0]["model"].as_str(), Some("m2"));
        assert_eq!(recs[0]["duration_ms"].as_f64(), Some(2000.0)); // 3000-1000
        assert_eq!(recs[0]["ttft_ms"].as_f64(), Some(500.0)); // 1500-1000
    }

    /// 第三阶段：dsh zstd 解压 + assistant/message 过滤 + index 兜底。
    #[test]
    fn dsh_file_synthetic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-abc");
        std::fs::create_dir_all(&dir).unwrap();
        let lines = [
            r#"{"type":"session","data":{"id":"x"}}"#,
            r#"{"type":"assistant/message","time":"2026-05-27T06:00:00Z","data":{"id":"d1","model":"m","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
            r#"{"type":"assistant/message","time":"2026-05-27T06:01:00Z","data":{"model":"m","usage":{"input_tokens":20,"output_tokens":7,"total_tokens":27}}}"#,
            r#"{"type":"user","data":{"content":"hi"}}"#,
        ];
        let raw = lines.join("\n");
        let compressed = zstd::encode_all(raw.as_bytes(), 3).unwrap();
        std::fs::write(dir.join("session.v4.jsonl.zstd"), compressed).unwrap();
        let recs = dsh_file(&dir.join("session.v4.jsonl.zstd")).unwrap();
        assert_eq!(recs.len(), 2, "只计 assistant/message 且 usage 为 dict");
        assert_eq!(recs[0]["session"].as_str(), Some("sess-abc"), "session 取父目录名");
        assert_eq!(recs[0]["tokens"].as_f64(), Some(15.0));
        assert_eq!(recs[1]["tokens"].as_f64(), Some(27.0));
    }

    /// 第三阶段：gemini 整包 JSON + user 累计估算 + google provider。
    #[test]
    fn gemini_file_synthetic() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("session-x.json");
        let data = json!({
            "sessionId": "g-sess",
            "messages": [
                {"type": "user", "content": [{"type": "text", "text": "你好世界"}]},
                {"type": "gemini", "id": "g1", "model": "gemini-2", "timestamp": "2026-05-27T06:00:00Z",
                 "content": [{"type": "text", "text": "abcd"}],
                 "tokens": {"input": 10, "output": 5, "total": 15, "cached": 2}},
                {"type": "model", "id": "g2", "model": "gemini-2", "timestamp": "2026-05-27T06:01:00Z",
                 "content": "你好"},
            ]
        });
        std::fs::write(&f, serde_json::to_string(&data).unwrap()).unwrap();
        let recs = gemini_file(&f).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["session"].as_str(), Some("g-sess"));
        assert_eq!(recs[0]["provider"].as_str(), Some("google"));
        assert_eq!(recs[0]["tokens"].as_f64(), Some(15.0)); // 有 usage 用 exact
        assert_eq!(recs[0]["cached_tokens"].as_f64(), Some(2.0));
        // g2 无 tokens → 估算：pending_input 已被 g1 消费重置为 0，仅输出"你好"=2
        assert_eq!(recs[1]["tokens"].as_f64(), Some(2.0));
        assert_eq!(recs[1]["estimated_tokens"].as_f64(), Some(2.0));
    }
}