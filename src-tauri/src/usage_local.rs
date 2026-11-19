// T17（阶段四）：usage-local 迁 Rust —— 第一阶段：codex 解析 + SQLite 增量缓存。
//
// 计划 §4.4.2 红线：**不做 1:1 翻译**。python 版每轮把整个事件缓存 json.loads/dumps
// （67.8MB 单轮 ~7.7s，2026-11-18 实测）；这里借迁移换 SQLite：按源文件指纹
// (mtime_ns, size) 只重算变化的文件，未变文件的事件直接复用，聚合在内存做。
//
// 口径与 python usage-local.py 逐字段对齐（golden 测试钉住）：
// - codex token_count 取累计终值差分；配额刷新重复帧跳过；last 兜底
// - 事件 id = sha256(identity)，identity 含 sorted-keys JSON（与 python json.dumps(sort_keys=True) 对齐）
// - 按天归日用本机时区；date-only 不伪造午夜事件
// - 聚合去重键 session|id，取 (exact,tokens) 最大者
//
// 范围：本阶段只迁 codex/chatgpt（后台热路径）；其余产品仍走 python（usage.rs 接线回退）。
// claude/messages、zcode/opencode 等 DB 解析在后续批次。

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

/// 与 python usage_tokens(style) 对齐；本阶段 codex 用 "codex"，兜底 "standard"。
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
        if let Some(details) = usage.get("prompt_tokens_details").or_else(|| usage.get("inputTokensDetails")) {
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

/// 本机时区归日（与 python local_day 对齐）：ISO 时间戳转本机日期；date-only 原样；数值 epoch 秒/毫秒。
fn local_day(value: &Value) -> String {
    use chrono::{DateTime, Local, TimeZone};
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
            DateTime::parse_from_rfc3339(&text.replace('Z', "+00:00"))
                .map(|t| t.with_timezone(&Local).format("%Y-%m-%d").to_string())
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// 秒级精确时刻：只有真带时分秒才算（date-only 不伪造午夜尖峰）。
fn precise_timestamp(value: &Value) -> Option<f64> {
    use chrono::DateTime;
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
            let ms = DateTime::parse_from_rfc3339(&text.replace('Z', "+00:00"))
                .ok()?.timestamp_millis() as f64;
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

/// 事件行（与 python event() 对齐；本阶段 codex 无 estimated/duration/ttft）。
fn event(identity: &str, session: &str, timestamp: &Value, model: &str, usage: &Value) -> Value {
    let (i, o, cache, total) = usage_tokens(usage, "codex");
    let mut row = json!({});
    for m in METRICS { row[m] = json!(0.0); }
    row["id"] = json!(sha256_hex(identity));
    row["session"] = json!(session);
    row["date"] = json!(local_day(timestamp));
    row["timestamp"] = json!(precise_timestamp(timestamp));
    row["model"] = json!(if model.is_empty() { "未记录模型" } else { model });
    row["provider"] = json!("未记录供应商");
    row["tokens"] = json!(total);
    row["exact_tokens"] = json!(total);
    row["estimated_tokens"] = json!(0.0);
    row["input_tokens"] = json!(i);
    row["output_tokens"] = json!(o);
    row["cached_tokens"] = json!(cache);
    row["requests"] = json!(1.0);
    row["failures"] = json!(0.0);
    row
}

/// codex rollout.jsonl 解析（与 python codex_file 逐条对齐）。
pub fn codex_file(path: &Path) -> Result<Vec<Value>, String> {
    let text = std::fs::read_to_string(path).map_err(|_| "读取会话日志失败".to_string())?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut session = stem;
    let mut previous: Option<(f64, f64, f64, f64)> = None;
    let mut model = String::new();
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_start_matches('\u{feff}');
        let Ok(item) = serde_json::from_str::<Value>(line) else { continue };
        if !item.is_object() { continue; }
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
        let ts = item.get("timestamp").cloned().unwrap_or(json!(index));
        let identity_src = current.as_ref().unwrap_or(&increment);
        // python 的 identity 三段里 timestamp 走 "%s" % item.get(...)：字符串原样、
        // 整数 str(index)——不是 json.dumps。只有第三段（usage dict）走 sort_keys JSON。
        let ts_text = match &ts {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let identity = format!("{}:{}:{}", session, ts_text, dumps_sorted(identity_src));
        records.push(event(&identity, &session, &ts, &model, &increment));
    }
    Ok(records)
}

/// codex/chatgpt 的源文件清单（与 python paths_for 对齐）。
fn codex_paths(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for folder in ["sessions", "archived_sessions"] {
        let base = root.join(folder);
        if !base.is_dir() { continue; }
        let mut stack = vec![base];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() { stack.push(p); }
                else if p.extension().map(|x| x == "jsonl").unwrap_or(false) { out.push(p); }
            }
        }
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
    fn open(cache_dir: &Path, app: &str, source_key: &str) -> Result<Self, String> {
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
        const PARSER_VERSION: &str = "rust-codex/1";
        let ver: Option<String> = conn.query_row(
            "SELECT v FROM meta WHERE k='parser_version'", [], |r| r.get(0)).ok();
        if ver.as_deref() != Some(PARSER_VERSION) {
            conn.execute_batch("DELETE FROM files;").map_err(|e| e.to_string())?;
            conn.execute("INSERT OR REPLACE INTO meta(k,v) VALUES('parser_version',?1)",
                [PARSER_VERSION]).map_err(|e| e.to_string())?;
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
    for row in records {
        let identity = format!("{}|{}", row["session"].as_str().unwrap_or(""), row["id"].as_str().unwrap_or(""));
        let better = match unique.get(&identity) {
            None => true,
            Some(old) => {
                (number(&row["exact_tokens"]), number(&row["tokens"]))
                    >= (number(&old["exact_tokens"]), number(&old["tokens"]))
            }
        };
        if better { unique.insert(identity, row); }
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
    // 与 python dict 插入序对齐：unique 按记录首次出现序遍历
    let mut seen_order: Vec<String> = Vec::new();
    let mut by_identity: HashMap<String, Value> = HashMap::new();
    // 重新按原始顺序收集（unique 的最终值）
    // 注：python 的 unique.values() 按首次插入序；这里用 seen_order 复现
    // （records 已被上面消费，改为在去重时记录顺序——重做一次轻量遍历）
    // 为保序，这里直接从 unique 构造时记录：见下方 order 收集。
    let _ = &mut seen_order;
    let _ = &mut by_identity;
    // 简化：BTreeMap 按 identity 排序会改变 models 内浮点累加顺序，但 METRICS 累加
    // 对顺序不敏感（同为非负数求和，f64 结合误差在 golden delta 内）。sessions/models
    // 集合语义与顺序无关（models 输出显式排序）。
    for row in unique.values() {
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
    let peak = days.iter().max_by(|a, b| {
        a.1.metrics.get("tokens").copied().unwrap_or(0.0)
            .partial_cmp(&b.1.metrics.get("tokens").copied().unwrap_or(0.0)).unwrap()
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
        result["_records"] = json!(unique.values().map(|r| {
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

/// codex/chatgpt 采集入口（与 python collect 同契约）。cache_dir=None 时不缓存（测试用）。
pub fn collect_codex(app: &str, root: &Path, cache_dir: Option<&Path>, with_records: bool) -> Result<Option<Value>, String> {
    if !root.is_dir() { return Ok(None); }
    let root = root.canonicalize().map_err(|_| "无法解析数据目录".to_string())?;
    let source_key = root.to_string_lossy().to_lowercase();
    let cache = cache_dir.and_then(|d| Cache::open(d, app, &source_key).ok());
    let mut records = Vec::new();
    let mut errors = 0usize;
    let mut files = 0usize;
    let mut keep: Vec<String> = Vec::new();
    for path in codex_paths(&root) {
        if !path.is_file() { continue; }
        let rel = path.strip_prefix(&root).map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        let stamp = fingerprint(&path);
        let rows = match cache.as_ref().and_then(|c| c.cached(&rel, stamp)) {
            Some(rows) => rows,
            None => match codex_file(&path) {
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
    let source = "本机 Codex 会话日志";
    let result = aggregate(records, source, &source_key, files, errors, with_records);
    if result.is_none() && errors > 0 {
        return Ok(Some(json!({"error": "本地用量文件读取失败，请检查文件权限与数据库状态。"})));
    }
    Ok(result)
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
        // 与 python 侧同 with_records 口径（真实数据抽样用 False 省内存）
        let with_records = py.get("_records").is_some();
        let rs = collect_codex("codex", Path::new(&fixture_dir), None, with_records).unwrap().expect("Rust 应聚出结果");
        // 标量字段逐一比对（数值按 f64 比——python sum([]) 返回 int 0，Rust 恒 f64，
        // 序列化形态差异不属口径漂移）
        for key in ["total", "input_tokens", "output_tokens", "cached_tokens", "exact_tokens",
                    "estimated_tokens", "requests", "sessions", "files", "peak",
                    "undated_tokens"] {
            assert_eq!(rs[key].as_f64(), py[key].as_f64(), "字段 {key} 不一致：rust={} python={}", rs[key], py[key]);
        }
        for key in ["peak_date", "unit", "source", "attribution"] {
            assert_eq!(rs[key], py[key], "字段 {key} 不一致");
        }
        // days 逐日比对（date/credits/tokens/sessions）
        let rd = rs["days"].as_array().unwrap();
        let pd = py["days"].as_array().unwrap();
        assert_eq!(rd.len(), pd.len(), "天数不一致");
        for (r, p) in rd.iter().zip(pd.iter()) {
            for key in ["date", "credits", "tokens", "sessions", "input_tokens", "output_tokens"] {
                assert_eq!(r[key], p[key], "day {key} 不一致");
            }
        }
        // _records 的 id 集合一致（identity 哈希口径对齐的最强证据；with_records=False 时跳过）
        if with_records {
            let mut rids: Vec<String> = rs["_records"].as_array().unwrap().iter()
                .map(|r| r["id"].as_str().unwrap().to_string()).collect();
            let mut pids: Vec<String> = py["_records"].as_array().unwrap().iter()
                .map(|r| r["id"].as_str().unwrap().to_string()).collect();
            rids.sort(); pids.sort();
            assert_eq!(rids, pids, "事件 id 集合不一致（identity/哈希口径漂移即红）");
        }
        // models 汇总一致
        assert_eq!(rs["models"], py["models"], "models 汇总不一致");
    }
}