//! T18：zcode-auth 迁 Rust（与 python zcode-auth.py 逐条对齐）。
//!
//! 读 ZCode 的 provider 级 OAuth 身份，不暴露明文 token。格式跟随本机安装的官方凭据密码。
//! 输入/加密快照经 stdin/stdout 管道；日志只收错误。
//!
//! 安全规则（与 python 版一致）：
//! 1. 密码学/JSON 错误永不包含明文输入。
//! 2. proxy 导出只认选定卡片的身份，绝不用另一个登录。
//! 3. merge 逐字节保留其它 provider 的密文、bot 凭据与项目 API 配置。
//!
//! 加密用 T14 已建的 agentcrypt::aes_gcm_decrypt（golden 钉住与 python cryptography 逐字节一致）。
//! 本模块只读凭据 + 派生身份，不写任何生产凭据存储；merge 返回新 record 由调用方决定是否落盘。

use serde_json::{json, Value};
use std::path::Path;

/// base64url 字母表（RFC 4648 §5，-_ 替代 +/）。
const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url 解码（忽略 padding），与 python urlsafe_b64decode(p + '='*(-len%4)) 对齐。
/// 自实现避免新增 base64 直接依赖（项目既有 secrets::b64_decode 是标准字母表，不通用）。
fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut vals: Vec<u8> = Vec::with_capacity(s.len());
    for c in s.chars() {
        if c == '=' || c.is_whitespace() { continue; }
        let i = B64URL.iter().position(|b| *b as char == c)
            .ok_or_else(|| "Invalid encrypted ZCode credential".to_string())?;
        vals.push(i as u8);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        let mut n: u32 = 0;
        for (i, v) in chunk.iter().enumerate() { n |= (*v as u32) << (18 - 6 * i); }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 { out.push((n >> 8) as u8); }
        if chunk.len() > 3 { out.push(n as u8); }
    }
    Ok(out)
}

/// 与 python key() 对齐：ZCODE_CREDENTIAL_SECRET 或 fallback 串 sha256。
fn key() -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let secret = std::env::var("ZCODE_CREDENTIAL_SECRET").unwrap_or_else(|_| {
        let home = crate::registry::home();
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".to_string());
        format!("zcode-credential-fallback:{}:{}:{}", std::env::consts::OS, home.display(), user)
    });
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    h.finalize().into()
}

/// 与 python decode 对齐：enc:v1: 前缀的 AES-GCM 解密；非该前缀原样返回。
fn decode(value: &Value) -> Result<String, String> {
    let s = match value {
        Value::String(s) => s,
        _ => return Err("ZCode credential values must be strings".into()),
    };
    if !s.starts_with("enc:v1:") {
        return Ok(s.clone());
    }
    let parts: Vec<&str> = s[7..].split('.').collect();
    if parts.len() != 3 {
        return Err("Invalid encrypted ZCode credential".into());
    }
    let iv = b64url_decode(parts[0])?;
    let tag = b64url_decode(parts[1])?;
    let data = b64url_decode(parts[2])?;
    if iv.len() != 12 || tag.len() != 16 {
        return Err("Invalid encrypted ZCode credential".into());
    }
    let mut ct = data;
    ct.extend_from_slice(&tag);
    let pt = crate::agentcrypt::aes_gcm_decrypt(&key(), &iv, &ct, &[])?;
    String::from_utf8(pt).map_err(|_| "ZCode credential reader failed".to_string())
}

/// 与 python identity 对齐：派生 provider 级身份（不暴露明文 token）。
pub fn identity(record: &Value) -> Result<Value, String> {
    let empty = json!({"has_auth": false, "provider": "", "account": "", "user_id": ""});
    let provider = decode(record.get("oauth:active_provider").unwrap_or(&json!("")))?;
    if provider != "bigmodel" && provider != "zai" {
        return Ok(empty);
    }
    let access = decode(record.get(&format!("oauth:{}:access_token", provider)).unwrap_or(&json!("")))?;
    let raw = decode(record.get(&format!("oauth:{}:user_info", provider)).unwrap_or(&json!("")))?;
    if access.is_empty() || raw.is_empty() {
        return Ok(json!({"has_auth": false, "provider": provider, "account": "", "user_id": ""}));
    }
    let info: Value = serde_json::from_str(&raw).map_err(|_| "ZCode credential reader failed".to_string())?;
    if !info.is_object() {
        return Err("ZCode account identity is invalid".into());
    }
    let identifier = {
        let id = info.get("id").filter(|v| crate::usage_local::is_truthy_pub(v))
            .or_else(|| info.get("user_id").filter(|v| crate::usage_local::is_truthy_pub(v)));
        id.map(|v| crate::usage_local::py_str_pub(v)).unwrap_or_default().trim().to_string()
    };
    if identifier.is_empty() {
        return Err("ZCode account ID is missing".into());
    }
    let jwt = decode(record.get("zcodejwttoken").unwrap_or(&json!("")))?;
    if !jwt.is_empty() {
        let parts: Vec<&str> = jwt.split('.').collect();
        if parts.len() != 3 {
            return Err("ZCode login token is invalid".into());
        }
        let claims_raw = b64url_decode(parts[1])?;
        let claims: Value = serde_json::from_slice(&claims_raw)
            .map_err(|_| "ZCode credential reader failed".to_string())?;
        let subject = {
            let s = claims.get("user_id").filter(|v| crate::usage_local::is_truthy_pub(v))
                .or_else(|| claims.get("sub").filter(|v| crate::usage_local::is_truthy_pub(v)));
            s.map(|v| crate::usage_local::py_str_pub(v)).unwrap_or_default().trim().to_string()
        };
        if !subject.is_empty() && subject != identifier {
            return Err("ZCode OAuth and billing identities differ".into());
        }
    }
    let account = {
        let a = ["displayName", "name", "username", "email"].iter()
            .find_map(|k| info.get(*k).filter(|v| crate::usage_local::is_truthy_pub(v)))
            .map(|v| crate::usage_local::py_str_pub(v))
            .unwrap_or_else(|| identifier.clone());
        a.trim().to_string()
    };
    let email = info.get("email").filter(|v| crate::usage_local::is_truthy_pub(v))
        .map(|v| crate::usage_local::py_str_pub(v)).unwrap_or_default();
    Ok(json!({
        "has_auth": true, "provider": provider, "account": account,
        "user_id": format!("zcode:{}:{}", provider, identifier),
        "provider_user_id": identifier, "email": email,
    }))
}

/// 与 python snapshot 对齐：只保留选定 provider 的 OAuth 字段 + 公共字段。
pub fn snapshot(record: &Value) -> Result<Value, String> {
    let who = identity(record)?;
    if who["has_auth"] != true {
        return Err("ZCode is not logged in".into());
    }
    let provider = who["provider"].as_str().unwrap_or("");
    let mut allowed: Vec<String> = vec![
        "oauth:active_provider".into(), "oauth:login_attribution".into(), "zcodejwttoken".into(),
    ];
    for item in ["access_token", "refresh_token", "user_info"] {
        allowed.push(format!("oauth:{}:{}", provider, item));
    }
    let mut rec = serde_json::Map::new();
    if let Some(o) = record.as_object() {
        for (k, v) in o {
            if allowed.contains(k) { rec.insert(k.clone(), v.clone()); }
        }
    }
    Ok(json!({"identity": who, "record": Value::Object(rec)}))
}

/// 与 python proxy_credential 对齐：只导出选定卡片的 JWT，校验身份+过期。
pub fn proxy_credential(record: &Value, expected: &str) -> Result<Value, String> {
    let who = identity(record)?;
    if expected.is_empty() || who["has_auth"] != true || who["user_id"].as_str().unwrap_or("") != expected {
        return Err("ZCode proxy account identity does not match the selected card".into());
    }
    let jwt = decode(record.get("zcodejwttoken").unwrap_or(&json!("")))?.trim().to_string();
    if jwt.is_empty() {
        return Err("ZCode Start Plan token is missing; open ZCode and save this login again".into());
    }
    let parts: Vec<&str> = jwt.split('.').collect();
    let claims_raw = b64url_decode(parts.get(1).unwrap_or(&""))?;
    let claims: Value = serde_json::from_slice(&claims_raw)
        .map_err(|_| "ZCode credential reader failed".to_string())?;
    if let Some(exp) = claims.get("exp").filter(|v| !v.is_null()) {
        let exp_f = crate::usage_local::number_pub(exp);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_secs_f64();
        if exp_f <= now {
            return Err("ZCode Start Plan token expired; sign in again and save the login".into());
        }
    }
    Ok(json!({"provider": who["provider"], "user_id": expected, "jwt": jwt}))
}

/// 与 python merge 对齐：逐字节保留其它 provider 的密文/bot/项目 API 配置。
pub fn merge(current: &Value, target: Option<&Value>, selected_provider: Option<&str>) -> Result<Value, String> {
    let snap = match target {
        Some(t) => Some(snapshot(t)?),
        None => None,
    };
    let provider = match &snap {
        Some(s) => s["identity"]["provider"].as_str().unwrap_or("").to_string(),
        None => selected_provider.unwrap_or("").to_string(),
    };
    if provider != "bigmodel" && provider != "zai" {
        return Err("The selected ZCode provider is required".into());
    }
    if snap.is_none() {
        let active = decode(current.get("oauth:active_provider").unwrap_or(&json!("")))?;
        if active != provider {
            return Err("The active ZCode provider changed; logout was cancelled".into());
        }
    }
    let prefix = format!("oauth:{}:", provider);
    let mut result = serde_json::Map::new();
    if let Some(o) = current.as_object() {
        for (k, v) in o {
            if !k.starts_with(&prefix) && k != "oauth:login_attribution" && k != "zcodejwttoken" {
                result.insert(k.clone(), v.clone());
            }
        }
    }
    if let Some(s) = snap {
        if let Some(rec) = s["record"].as_object() {
            for (k, v) in rec { result.insert(k.clone(), v.clone()); }
        }
    }
    Ok(Value::Object(result))
}

/// 与 python run 对齐：按 mode 分派。
pub fn run(request: &Value) -> Result<Value, String> {
    let record = match request.get("record") {
        Some(r) if !r.is_null() => r.clone(),
        _ => {
            let root = request.get("root").and_then(|v| v.as_str()).unwrap_or("");
            let path = Path::new(root).join("v2").join("credentials.json");
            if path.exists() {
                let text = std::fs::read_to_string(&path).map_err(|_| "ZCode credential reader failed".to_string())?;
                serde_json::from_str(&text).map_err(|_| "ZCode credential reader failed".to_string())?
            } else {
                json!({})
            }
        }
    };
    if !record.is_object() {
        return Err("ZCode credential store must be an object".into());
    }
    let mode = request.get("mode").and_then(|v| v.as_str()).unwrap_or("identity");
    match mode {
        "identity" => identity(&record),
        "snapshot" => snapshot(&record),
        "proxy" => proxy_credential(&record, request.get("expected_user_id").and_then(|v| v.as_str()).unwrap_or("")),
        "merge" => {
            let target = request.get("target").filter(|v| !v.is_null());
            let provider = request.get("provider").and_then(|v| v.as_str());
            Ok(json!({"record": merge(&record, target, provider)?}))
        }
        _ => Err("Unknown ZCode credential operation".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成明文 record（与 zcode.rs tests::record 同口径，未加密）。
    fn record(provider: &str, id: &str) -> Value {
        let mut value = json!({"oauth:active_provider": provider});
        value[format!("oauth:{}:access_token", provider)] = json!("synthetic-token");
        value[format!("oauth:{}:user_info", provider)] =
            json!(json!({"user_id": id, "name": "Example"}).to_string());
        value
    }

    /// identity：明文 record 派生身份；provider 不在 bigmodel/zai → has_auth=false。
    #[test]
    fn identity_plain_record() {
        let who = identity(&record("bigmodel", "u1")).unwrap();
        assert_eq!(who["has_auth"], true);
        assert_eq!(who["provider"], "bigmodel");
        assert_eq!(who["user_id"], "zcode:bigmodel:u1");
        assert_eq!(who["account"], "Example");
        let none = identity(&json!({"oauth:active_provider": "other"})).unwrap();
        assert_eq!(none["has_auth"], false);
    }

    /// snapshot：只保留选定 provider 的 OAuth 字段，bot/项目/其它 provider 密文不进快照。
    #[test]
    fn snapshot_only_selected_provider() {
        let mut current = record("zai", "one");
        current["oauth:bigmodel:access_token"] = json!("other");
        current["bot:private:credential"] = json!("keep");
        let snap = snapshot(&current).unwrap();
        assert!(snap["record"].get("bot:private:credential").is_none());
        assert!(snap["record"].get("oauth:bigmodel:access_token").is_none());
        assert!(snap["record"].get("oauth:zai:access_token").is_some());
    }

    /// merge：逐字节保留其它 provider 密文/bot/项目 API 配置。
    #[test]
    fn merge_preserves_other_credentials() {
        let mut current = record("zai", "one");
        current["bot:private:credential"] = json!("unchanged-bot");
        current["project-key-location:v1:example"] = json!("unchanged-api");
        let target = record("bigmodel", "two");
        let merged = merge(&current, Some(&target), None).unwrap();
        assert_eq!(merged["bot:private:credential"], "unchanged-bot");
        assert_eq!(merged["project-key-location:v1:example"], "unchanged-api");
        assert_eq!(merged["oauth:zai:access_token"], current["oauth:zai:access_token"]);
        let who = identity(&merged).unwrap();
        assert_eq!(who["user_id"], "zcode:bigmodel:two");
    }

    /// merge logout：保留 active_provider 指向登出 provider，不静默激活另一个。
    #[test]
    fn merge_logout_keeps_active_provider() {
        let current = record("zai", "one");
        let merged = merge(&current, None, Some("zai")).unwrap();
        let who = identity(&merged).unwrap();
        assert_eq!(who["has_auth"], false);
        assert_eq!(who["provider"], "zai");
    }

    /// merge：active_provider 变了 → 拒绝登出。
    #[test]
    fn merge_rejects_changed_provider() {
        let current = record("bigmodel", "two");
        assert!(merge(&current, None, Some("zai")).is_err());
    }

    /// 加密 round-trip：用 key() 加密合成向量 → decode 解密回原文（验证 AES-GCM 接线）。
    #[test]
    fn encrypted_round_trip() {
        use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
        let k = key();
        let cipher = Aes256Gcm::new((&k).into());
        let nonce_bytes = [7u8; 12];
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = b"bigmodel";
        let ct = cipher.encrypt(nonce, plaintext.as_ref()).unwrap();
        // ct 末尾 16 字节是 tag
        let (data, tag) = ct.split_at(ct.len() - 16);
        let enc = format!("enc:v1:{}.{}.{}",
            b64url_encode(&nonce_bytes), b64url_encode(tag), b64url_encode(data));
        let decoded = decode(&json!(enc)).unwrap();
        assert_eq!(decoded, "bigmodel");
    }

    /// base64url 编码（测试用，与 python urlsafe_b64encode 对齐，去 padding）。
    fn b64url_encode(src: &[u8]) -> String {
        let mut s = String::new();
        for chunk in src.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            s.push(B64URL[(n >> 18) as usize & 63] as char);
            s.push(B64URL[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 { s.push(B64URL[(n >> 6) as usize & 63] as char); }
            if chunk.len() > 2 { s.push(B64URL[n as usize & 63] as char); }
        }
        s
    }

    /// proxy：身份不符 → 拒绝；缺 JWT → 报错。
    #[test]
    fn proxy_rejects_mismatch() {
        let rec = record("bigmodel", "u1");
        assert!(proxy_credential(&rec, "zcode:bigmodel:other").is_err());
        assert!(proxy_credential(&rec, "").is_err());
        // 无 zcodejwttoken → 缺 token 报错
        assert!(proxy_credential(&rec, "zcode:bigmodel:u1").is_err());
    }

    /// identity：access_token 为空但 user_info 非空 → has_auth=false（python 真值口径）。
    #[test]
    fn identity_empty_access_token() {
        let mut rec = record("bigmodel", "u1");
        rec["oauth:bigmodel:access_token"] = json!("");
        let who = identity(&rec).unwrap();
        assert_eq!(who["has_auth"], false, "access_token 空应判未登录");
        assert_eq!(who["provider"], "bigmodel");
    }

    /// T18 golden：与 python zcode-auth.py run 输出逐字段比对（含 enc:v1: 加密字段）。
    /// 由 tests/zcode-auth-rust-golden.test.py 触发；未设环境变量时跳过。
    #[test]
    fn golden_matches_python() {
        let Ok(req_path) = std::env::var("T18_ZCODE_REQ") else {
            eprintln!("跳过 golden：未设 T18_ZCODE_REQ");
            return;
        };
        let request: Value = serde_json::from_str(&std::fs::read_to_string(&req_path).unwrap()).unwrap();
        let py: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("T18_ZCODE_PY").unwrap()).unwrap()).unwrap();
        let rs = run(&request).expect("Rust run 应成功");
        assert_eq!(rs, py, "zcode-auth 输出不一致：rust={} python={}", rs, py);
    }
}