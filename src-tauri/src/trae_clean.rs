//! T18：trae-gateway-clean 迁 Rust（与 python trae-gateway-clean.py 逐条对齐）。
//!
//! Trae 把自定义 OpenAI 兼容供应商缓存在 SQLite ItemTable 的
//! `<uid>:AI.agent.model.model_list_map` 键里。每条带 base_url，网关可从数据本身识别。
//! 只删指向本机网关的条目；用户手工添加的自定义供应商保留。
//! 仍引用被删模型的选择记录一并剪枝，否则 Trae 会记住一个不存在的 model id。
//!
//! 安全规则（与 python 版一致）：
//! 1. 只删 base_url 命中本机网关 host 的条目。
//! 2. 整标识符匹配（不是更长模型名的前缀）——has_token 的左右边界判定。
//! 3. 剪枝只丢指向被删模型的条目，其余逐字节保留。

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

const MAP_SUFFIX: &str = "AI.agent.model.model_list_map";
const SELECT_SUFFIXES: [&str; 4] = [
    "AI.agent.model.session_selected_model",
    "AI.agent.model.recent_user_selection_by_agent_label",
    "AI.agent.model.reasoning_effort_level_by_agent_model_v2",
    "AI.agent.model.max_mode_by_agent_model",
];

/// 与 python is_gateway 对齐：base_url 非空且含任一 host（小写比较）。
/// python str(entry.get("base_url") or "")：真值链，非字符串真值走 str()。
fn is_gateway(entry: &Value, hosts: &[String]) -> bool {
    if !entry.is_object() { return false; }
    let base = match entry.get("base_url") {
        Some(Value::String(s)) => s.clone(),
        Some(v) if crate::usage_local::is_truthy_pub(v) => crate::usage_local::py_str_pub(v),
        _ => String::new(),
    };
    let base = base.trim().to_lowercase();
    !base.is_empty() && hosts.iter().any(|h| base.contains(h.as_str()))
}

/// 与 python model_tokens 对齐：Trae 可能用来记住该模型选择的标识符集合。
fn model_tokens(entry: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for key in ["name", "config_name", "display_name", "custom_model_id"] {
        if let Some(Value::String(s)) = entry.get(key) {
            let s = s.trim();
            if !s.is_empty() { out.insert(s.to_string()); }
        }
    }
    out
}

/// python str.isalnum 的 Unicode 口径：字母/数字/小数/数位字符。
fn is_alnum(ch: char) -> bool {
    ch.is_alphabetic() || ch.is_numeric()
}

/// 与 python has_token 对齐：整标识符匹配，不是更长模型名的前缀。
/// 左右边界都不能是字母数字或 "./"。
fn has_token(text: &str, tokens: &BTreeSet<String>) -> bool {
    if text.is_empty() { return false; }
    for token in tokens {
        if token.is_empty() { continue; }
        let mut start = text.find(token.as_str());
        while let Some(s) = start {
            let end = s + token.len();
            let after = text[end..].chars().next();
            let before = if s > 0 { text[..s].chars().next_back() } else { None };
            let right_ok = after.map(|c| !(is_alnum(c) || c == '.' || c == '/')).unwrap_or(true);
            let left_ok = before.map(|c| !(is_alnum(c) || c == '.' || c == '/')).unwrap_or(true);
            if right_ok && left_ok { return true; }
            start = text[s + 1..].find(token.as_str()).map(|p| s + 1 + p);
        }
    }
    false
}

/// 与 python references 对齐：递归查找字符串/列表/字典值里的整标识符。
fn references(value: &Value, tokens: &BTreeSet<String>) -> bool {
    match value {
        Value::String(s) => has_token(s, tokens),
        Value::Array(a) => a.iter().any(|item| references(item, tokens)),
        Value::Object(o) => o.values().any(|item| references(item, tokens)),
        _ => false,
    }
}

/// 与 python prune_selection 对齐：只丢指向被删网关模型的条目。
/// 返回 None 表示整个值应被丢弃（python 的 None 语义）。
fn prune_selection(value: &Value, tokens: &BTreeSet<String>) -> Option<Value> {
    match value {
        Value::Object(o) => {
            if o.contains_key("modelId") && references(o.get("modelId").unwrap(), tokens) {
                return None;
            }
            let mut out = serde_json::Map::new();
            for (key, item) in o {
                // python：标量值且键命中 token → 跳过该键
                let is_scalar = matches!(item, Value::String(_) | Value::Number(_) | Value::Bool(_));
                if is_scalar && has_token(key, tokens) { continue; }
                if let Some(kept) = prune_selection(item, tokens) {
                    out.insert(key.clone(), kept);
                }
            }
            if out.is_empty() { None } else { Some(Value::Object(out)) }
        }
        Value::Array(a) => {
            Some(Value::Array(a.iter().filter(|item| !references(item, tokens)).cloned().collect()))
        }
        other => Some(other.clone()),
    }
}

/// 与 python run 对齐：清理一个 state.vscdb，返回 {removed, keys}。
pub fn clean_db(path: &Path, hosts: &[String]) -> Result<Value, String> {
    let conn = rusqlite::Connection::open(path).map_err(|e| e.to_string())?;
    conn.pragma_update(None, "busy_timeout", 5000).ok();
    let mut stmt = conn.prepare("SELECT key, value FROM ItemTable").map_err(|e| e.to_string())?;
    let rows: Vec<(String, String)> = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?.unwrap_or_default()))
    }).map_err(|e| e.to_string())?
        .filter_map(|r| r.ok()).collect();
    drop(stmt);

    let mut removed = 0usize;
    let mut changed_keys: Vec<String> = Vec::new();
    let mut updates: Vec<(String, String)> = Vec::new();
    let mut tokens: BTreeSet<String> = BTreeSet::new();

    for (key, raw) in &rows {
        if key.is_empty() || !key.ends_with(MAP_SUFFIX) { continue; }
        let Ok(doc) = serde_json::from_str::<Value>(if raw.is_empty() { "{}" } else { raw }) else { continue };
        let Value::Object(mut doc) = doc else { continue };
        let mut touched = false;
        let channels: Vec<String> = doc.keys().cloned().collect();
        for channel in channels {
            let entries = match doc.get(&channel) {
                Some(Value::Array(a)) => a.clone(),
                _ => continue,
            };
            let mut kept = Vec::new();
            for entry in entries {
                if is_gateway(&entry, hosts) {
                    tokens.extend(model_tokens(&entry));
                    removed += 1;
                    touched = true;
                } else {
                    kept.push(entry);
                }
            }
            if kept.is_empty() { doc.remove(&channel); }
            else { doc.insert(channel, Value::Array(kept)); }
        }
        if touched {
            updates.push((dumps_compact(&Value::Object(doc)), key.clone()));
            changed_keys.push(key.clone());
        }
    }

    if !tokens.is_empty() {
        for (key, raw) in &rows {
            if key.is_empty() || !SELECT_SUFFIXES.iter().any(|s| key.ends_with(s)) { continue; }
            let Ok(doc) = serde_json::from_str::<Value>(if raw.is_empty() { "{}" } else { raw }) else { continue };
            let pruned = prune_selection(&doc, &tokens).unwrap_or(json!({}));
            if pruned != doc {
                updates.push((dumps_compact(&pruned), key.clone()));
                changed_keys.push(key.clone());
            }
        }
    }

    if !updates.is_empty() {
        for (value, key) in &updates {
            conn.execute("UPDATE ItemTable SET value = ?1 WHERE key = ?2", [value, key])
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(json!({"removed": removed, "keys": changed_keys}))
}

/// 与 python json.dumps(ensure_ascii=False, separators=(",",":")) 对齐的紧凑序列化。
/// serde_json 默认即紧凑（无空格）、非 ASCII 原样输出。
fn dumps_compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<String> {
        vec!["127.0.0.1:8788".to_string(), "localhost:8788".to_string(), "[::1]:8788".to_string()]
    }

    /// is_gateway：只认 base_url 命中 host；非 dict/空 base 不认。
    #[test]
    fn is_gateway_matches_base_url_only() {
        let h = hosts();
        assert!(is_gateway(&json!({"base_url": "http://127.0.0.1:8788/v1"}), &h));
        assert!(is_gateway(&json!({"base_url": "HTTP://LOCALHOST:8788/v1"}), &h));
        assert!(!is_gateway(&json!({"base_url": "https://api.example.com/v1"}), &h));
        assert!(!is_gateway(&json!({"base_url": ""}), &h));
        assert!(!is_gateway(&json!({"base_url": "  "}), &h));
        assert!(!is_gateway(&json!("not-a-dict"), &h));
        assert!(!is_gateway(&json!({}), &h));
    }

    /// has_token：整标识符匹配，左右边界不能是字母数字或 "./"。
    /// 注意 python 真值：'trae/glm-5.3' 左邻 '/' 属边界字符 → 不匹配（与 python 逐条核对）。
    #[test]
    fn has_token_whole_identifier_boundary() {
        let mut t = BTreeSet::new();
        t.insert("glm-5.3".to_string());
        assert!(has_token("glm-5.3", &t));
        assert!(has_token("x glm-5.3 y", &t)); // 空格边界 OK
        assert!(!has_token("trae/glm-5.3", &t)); // 左邻 '/' → 不匹配（python 同口径）
        assert!(!has_token("glm-5.30", &t)); // 右邻数字 → 不匹配
        assert!(!has_token("xglm-5.3", &t)); // 左邻字母 → 不匹配
        assert!(!has_token("glm-5.3/x", &t)); // 右邻 '/' → 不匹配
        assert!(!has_token("", &t));
    }

    /// prune_selection：modelId 命中整条丢；标量值键命中丢键；列表过滤；空 dict→None。
    /// 注意 python 真值：'trae/glm-5.3' 左邻 '/' 不匹配，故用裸 'glm-5.3' 测命中。
    #[test]
    fn prune_selection_drops_only_gateway_refs() {
        let mut t = BTreeSet::new();
        t.insert("glm-5.3".to_string());
        // modelId 命中 → 整条 None
        assert!(prune_selection(&json!({"modelId": "glm-5.3", "x": 1}), &t).is_none());
        // modelId 不命中 → 保留（'trae/glm-5.3' 左邻 '/' 不匹配，python 同口径）
        let keep = json!({"modelId": "mine", "x": 1});
        assert_eq!(prune_selection(&keep, &t), Some(keep.clone()));
        assert_eq!(prune_selection(&json!({"modelId": "trae/glm-5.3"}), &t),
                   Some(json!({"modelId": "trae/glm-5.3"})));
        // 标量值 + 键命中 → 丢键
        let doc = json!({"glm-5.3": "selected", "mine": "selected"});
        let pruned = prune_selection(&doc, &t).unwrap();
        assert_eq!(pruned, json!({"mine": "selected"}));
        // 列表过滤命中项
        let arr = json!(["glm-5.3", "mine"]);
        assert_eq!(prune_selection(&arr, &t), Some(json!(["mine"])));
        // 全丢 → None
        assert!(prune_selection(&json!({"glm-5.3": 1}), &t).is_none());
    }

    /// 端到端：合成 state.vscdb，只删网关条目+剪枝选择记录，自定义供应商保留。
    #[test]
    fn clean_db_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.vscdb");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value TEXT);").unwrap();
        let map = json!({
            "openai": [
                {"name": "glm-5.3", "config_name": "trae/glm-5.3", "base_url": "http://127.0.0.1:8788/v1"},
                {"name": "my-custom", "base_url": "https://api.example.com/v1"}
            ],
            "anthropic": [
                {"name": "claude-x", "base_url": "http://localhost:8788/v1"}
            ]
        });
        let sel = json!({"recent": ["trae/glm-5.3", "my-custom"], "glm-5.3": "effort-high", "mine": "keep"});
        conn.execute("INSERT INTO ItemTable VALUES(?1,?2)",
            ["uid:AI.agent.model.model_list_map", &serde_json::to_string(&map).unwrap()]).unwrap();
        conn.execute("INSERT INTO ItemTable VALUES(?1,?2)",
            ["uid:AI.agent.model.session_selected_model", &serde_json::to_string(&sel).unwrap()]).unwrap();
        drop(conn);

        let result = clean_db(&db, &hosts()).unwrap();
        assert_eq!(result["removed"].as_u64(), Some(2), "应删 2 条网关条目");
        // 验证写回：自定义供应商保留，网关条目没了
        let conn = rusqlite::Connection::open(&db).unwrap();
        let map_raw: String = conn.query_row(
            "SELECT value FROM ItemTable WHERE key='uid:AI.agent.model.model_list_map'", [], |r| r.get(0)).unwrap();
        let map_after: Value = serde_json::from_str(&map_raw).unwrap();
        assert_eq!(map_after["openai"].as_array().unwrap().len(), 1);
        assert_eq!(map_after["openai"][0]["name"], "my-custom");
        assert!(map_after.get("anthropic").is_none(), "全网关的 channel 应被移除");
        // 选择记录剪枝：glm-5.3 引用没了，mine 保留
        let sel_raw: String = conn.query_row(
            "SELECT value FROM ItemTable WHERE key='uid:AI.agent.model.session_selected_model'", [], |r| r.get(0)).unwrap();
        let sel_after: Value = serde_json::from_str(&sel_raw).unwrap();
        assert_eq!(sel_after["recent"], json!(["my-custom"]));
        assert!(sel_after.get("glm-5.3").is_none());
        assert_eq!(sel_after["mine"], "keep");
    }

    /// 无网关条目时零改动（不写库）。
    #[test]
    fn clean_db_no_gateway_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.vscdb");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value TEXT);").unwrap();
        let map = json!({"openai": [{"name": "mine", "base_url": "https://api.example.com/v1"}]});
        conn.execute("INSERT INTO ItemTable VALUES(?1,?2)",
            ["uid:AI.agent.model.model_list_map", &serde_json::to_string(&map).unwrap()]).unwrap();
        drop(conn);
        let result = clean_db(&db, &hosts()).unwrap();
        assert_eq!(result["removed"].as_u64(), Some(0));
        assert_eq!(result["keys"].as_array().unwrap().len(), 0);
    }

    /// T18 golden：与 python trae-gateway-clean.py run 输出 + 写回后库内容逐字段比对。
    /// 由 tests/trae-clean-rust-golden.test.py 触发；未设环境变量时跳过。
    #[test]
    fn golden_matches_python() {
        let Ok(db) = std::env::var("T18_TRAE_FIXTURE") else {
            eprintln!("跳过 golden：未设 T18_TRAE_FIXTURE");
            return;
        };
        let hosts: Vec<String> = serde_json::from_str(&std::env::var("T18_TRAE_HOSTS").unwrap()).unwrap();
        let py_result: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("T18_TRAE_PY_RESULT").unwrap()).unwrap()).unwrap();
        let py_after: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("T18_TRAE_PY_AFTER").unwrap()).unwrap()).unwrap();
        let rs_result = clean_db(Path::new(&db), &hosts).unwrap();
        assert_eq!(rs_result["removed"], py_result["removed"], "removed 不一致");
        assert_eq!(rs_result["keys"], py_result["keys"], "keys 不一致");
        // 写回后库内容逐键比对
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mut stmt = conn.prepare("SELECT key, value FROM ItemTable").unwrap();
        let rs_after: Value = {
            let mut obj = serde_json::Map::new();
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }).unwrap();
            for r in rows {
                let (k, v) = r.unwrap();
                obj.insert(k, serde_json::from_str(&v).unwrap());
            }
            Value::Object(obj)
        };
        assert_eq!(rs_after, py_after, "写回后库内容不一致：rust={} python={}", rs_after, py_after);
    }
}