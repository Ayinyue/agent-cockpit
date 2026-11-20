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
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// enc:v2 密钥环条目后缀（与 python KEYRING_SUFFIX 一致）。
const KEYRING_SUFFIX: &str = ".ZCode Credential Encryption";

thread_local! {
    /// 与 python 模块级 ROOT 对齐：run() 按请求设置，decode/os_key/env_key 读取。
    /// 用 thread_local 而非全局 static：cargo 并行测试互不串扰（python 每进程一个 ROOT，
    /// Rust 每线程一个，语义等价——helper 是单线程 stdin/stdout 管道）。
    static ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn set_root(root: Option<PathBuf>) {
    ROOT.with(|r| *r.borrow_mut() = root);
}

fn current_root() -> Option<PathBuf> {
    ROOT.with(|r| r.borrow().clone())
}

/// 与 python default_root 对齐：ZCODE_HOME 或 ~/.zcode。
fn default_root() -> PathBuf {
    std::env::var_os("ZCODE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::registry::home().join(".zcode"))
}

/// 与 python fingerprint 对齐：sha256(key).hexdigest()[:24]。
fn fingerprint(key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key);
    let out = h.finalize();
    out.iter().take(12).map(|b| format!("{:02x}", b)).collect()
}

/// 与 python _key_from_text 对齐：base64url 解码，仅接受 32 字节。
fn key_from_text(secret: &str) -> Option<[u8; 32]> {
    if secret.is_empty() { return None; }
    let bytes = b64url_decode(secret).ok()?;
    if bytes.len() != 32 { return None; }
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes);
    Some(k)
}

/// Windows 凭据库读取（与 python _win_cred_read/_win_cred_targets 对齐）。
/// 仅 Windows；其它平台返回空（python sys.platform != 'win32' 同口径）。
#[cfg(windows)]
mod wincred {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    const CRED_TYPE_GENERIC: u32 = 1;

    #[repr(C)]
    struct FileTime { dw_low: u32, dw_high: u32 }

    #[repr(C)]
    struct CredentialW {
        flags: u32,
        cred_type: u32,
        target_name: *const u16,
        comment: *const u16,
        last_written: FileTime,
        credential_blob_size: u32,
        credential_blob: *const u8,
        persist: u32,
        attribute_count: u32,
        attributes: *const std::ffi::c_void,
        target_alias: *const u16,
        user_name: *const u16,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn CredReadW(target: *const u16, type_: u32, flags: u32, cred: *mut *mut CredentialW) -> i32;
        fn CredEnumerateW(filter: *const u16, flags: u32, count: *mut u32, creds: *mut *mut *mut CredentialW) -> i32;
        fn CredFree(buffer: *mut std::ffi::c_void);
        fn CredWriteW(cred: *const CredentialW, flags: u32) -> i32;
        fn CredDeleteW(target: *const u16, type_: u32, flags: u32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 读一条 Generic 凭据的密码（UTF-16LE 解码，与 python raw.decode('utf-16-le') 一致）。
    pub fn read(target: &str) -> Option<String> {
        let t = wide(target);
        let mut ptr: *mut CredentialW = ptr::null_mut();
        unsafe {
            if CredReadW(t.as_ptr(), CRED_TYPE_GENERIC, 0, &mut ptr) == 0 || ptr.is_null() {
                return None;
            }
            let c = &*ptr;
            let out = if c.credential_blob.is_null() || c.credential_blob_size == 0 {
                None
            } else {
                let bytes = std::slice::from_raw_parts(c.credential_blob, c.credential_blob_size as usize);
                // UTF-16LE：两字节一码元；奇数字节截断（与 python 容错一致）
                let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
                String::from_utf16(&units).ok()
            };
            CredFree(ptr.cast());
            out
        }
    }

    /// 枚举所有以 suffix 结尾的凭据条目名（与 python _win_cred_targets 一致）。
    pub fn targets(suffix: &str) -> Vec<String> {
        let mut count: u32 = 0;
        let mut arr: *mut *mut CredentialW = ptr::null_mut();
        unsafe {
            if CredEnumerateW(ptr::null(), 0, &mut count, &mut arr) == 0 || arr.is_null() {
                return Vec::new();
            }
            let mut out = Vec::new();
            for i in 0..count as isize {
                let c = &**arr.offset(i);
                if c.target_name.is_null() { continue; }
                let len = (0..).take_while(|&j| *c.target_name.offset(j) != 0).count();
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(c.target_name, len));
                if name.ends_with(suffix) { out.push(name); }
            }
            CredFree(arr.cast());
            out
        }
    }

    /// 测试专用：写一条 Generic 凭据（blob 为 UTF-16LE 文本，与 python 客户端同口径）。
    #[cfg(test)]
    pub fn write_test(target: &str, text: &str) -> Result<(), String> {
        let t = wide(target);
        let units: Vec<u16> = text.encode_utf16().collect();
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let cred = CredentialW {
            flags: 0, cred_type: CRED_TYPE_GENERIC, target_name: t.as_ptr(),
            comment: ptr::null(), last_written: FileTime { dw_low: 0, dw_high: 0 },
            credential_blob_size: bytes.len() as u32, credential_blob: bytes.as_ptr(),
            persist: 2, attribute_count: 0, attributes: ptr::null(),
            target_alias: ptr::null(), user_name: ptr::null(),
        };
        let ok = unsafe { CredWriteW(&cred, 0) };
        if ok == 0 { return Err(format!("CredWriteW 失败：{}", std::io::Error::last_os_error())); }
        Ok(())
    }

    /// 测试专用：删除一条凭据。
    #[cfg(test)]
    pub fn delete_test(target: &str) -> bool {
        let t = wide(target);
        unsafe { CredDeleteW(t.as_ptr(), CRED_TYPE_GENERIC, 0) != 0 }
    }
}

#[cfg(not(windows))]
mod wincred {
    pub fn read(_target: &str) -> Option<String> { None }
    pub fn targets(_suffix: &str) -> Vec<String> { Vec::new() }
}

/// 与 python os_key 对齐：解析指纹为 want 的 OS 凭据库密钥。
/// 快路径从 credentials.json 真实路径派生条目名；慢路径枚举（捕获的登录把密文带到
/// 临时/实例目录，而密钥仍在原路径下）。
fn os_key(want: &str) -> Option<[u8; 32]> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(root) = current_root() {
        let path = root.join("v2").join("credentials.json");
        // realpath 与 abspath 两种形式（python os.path.realpath / abspath）
        let real = path.canonicalize().unwrap_or_else(|_| path.clone());
        let real = crate::registry::strip_verbatim(&real);
        let abs = crate::registry::strip_verbatim(&path);
        let mut forms: Vec<String> = vec![real.to_string_lossy().to_string(), abs.to_string_lossy().to_string()];
        forms.sort();
        forms.dedup();
        for form in forms {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(form.as_bytes());
            let digest: String = h.finalize().iter().map(|b| format!("{:02x}", b)).collect();
            candidates.push(format!("{}{}", digest, KEYRING_SUFFIX));
        }
    }
    for target in &candidates {
        if let Some(text) = wincred::read(target) {
            if let Some(k) = key_from_text(&text) {
                if fingerprint(&k) == want { return Some(k); }
            }
        }
    }
    for target in wincred::targets(KEYRING_SUFFIX) {
        if let Some(text) = wincred::read(&target) {
            if let Some(k) = key_from_text(&text) {
                if fingerprint(&k) == want { return Some(k); }
            }
        }
    }
    None
}

/// 与 python env_key 对齐：env 变量 → 声明的 secret 文件 → ROOT/v2/credential-secret。
/// 每个候选文本 sha256 成密钥，指纹匹配才返回（绝不猜）。
fn env_key(want: &str) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut texts: Vec<String> = Vec::new();
    if let Ok(v) = std::env::var("ZCODE_CREDENTIAL_SECRET") { texts.push(v); }
    if let Ok(declared) = std::env::var("ZCODE_CREDENTIAL_SECRET_FILE") {
        if let Ok(t) = std::fs::read_to_string(&declared) { texts.push(t); }
    }
    if let Some(root) = current_root() {
        if let Ok(t) = std::fs::read_to_string(root.join("v2").join("credential-secret")) { texts.push(t); }
    }
    for text in texts {
        let n = text.len();
        if !(32..=4096).contains(&n) { continue; }
        let mut h = Sha256::new();
        h.update(text.as_bytes());
        let k: [u8; 32] = h.finalize().into();
        if fingerprint(&k) == want { return Some(k); }
    }
    None
}

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
/// fallback 里的平台段必须用 python sys.platform 口径（win32/macos/linux），
/// 不是 Rust std::env::consts::OS（windows/macos/linux）——否则无 env 密钥时
/// Windows 上 v1 解密会与 python 不一致（golden 总设 env 密钥，曾掩盖此漂移）。
fn py_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

fn key() -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let secret = std::env::var("ZCODE_CREDENTIAL_SECRET").unwrap_or_else(|_| {
        let home = crate::registry::home();
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".to_string());
        format!("zcode-credential-fallback:{}:{}:{}", py_platform(), home.display(), user)
    });
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    h.finalize().into()
}

/// 与 python _open_envelope 对齐：iv.tag.ct 三段 base64url → AES-GCM 解密。
fn open_envelope(body: &str, key: &[u8; 32]) -> Result<String, String> {
    let parts: Vec<&str> = body.split('.').collect();
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
    let pt = crate::agentcrypt::aes_gcm_decrypt(key, &iv, &ct, &[])?;
    String::from_utf8(pt).map_err(|_| "ZCode credential reader failed".to_string())
}

/// 与 python decode 对齐：enc:v1（legacy 密钥）/ enc:v2（os/env 双源 + 指纹校验）。
/// **未知信封绝不原样当明文返回**——那正是格式变更退化成"未登录"的根因（上游注释）。
fn decode(value: &Value) -> Result<String, String> {
    let s = match value {
        Value::String(s) => s,
        _ => return Err("ZCode credential values must be strings".into()),
    };
    if !s.starts_with("enc:") {
        return Ok(s.clone());
    }
    if let Some(body) = s.strip_prefix("enc:v1:") {
        return open_envelope(body, &key());
    }
    let Some(rest) = s.strip_prefix("enc:v2:") else {
        return Err(format!("ZCode credential envelope is not recognised ({}…); the desktop app changed its cipher",
            &s[..s.len().min(12)]));
    };
    // rest = source:want:body（body 内含 '.' 不含 ':'，splitn(3) 正确切分）
    let mut it = rest.splitn(3, ':');
    let source = it.next().unwrap_or("");
    let want = it.next().unwrap_or("");
    let body = it.next().unwrap_or("");
    if (source != "os" && source != "env") || want.len() != 24 {
        return Err("Invalid encrypted ZCode credential".into());
    }
    let k = if source == "os" { os_key(want) } else { env_key(want) };
    let Some(k) = k else {
        return Err(format!("ZCode system key unavailable ({}/{}); unlock the OS credential store and retry", source, want));
    };
    open_envelope(body, &k)
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
    // 与 python run() 的 global ROOT 对齐：先按请求设数据根，decode/os_key/env_key 据此定位密钥环。
    let root = request.get("root").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_root);
    set_root(Some(root.clone()));
    let record = match request.get("record") {
        Some(r) if !r.is_null() => r.clone(),
        _ => {
            let path = root.join("v2").join("credentials.json");
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

    /// enc:v2 os-source：CredWriteW 写测试密钥环条目 → os_key 快路径解析 → AES-GCM 解密。
    /// 验证 Windows 凭据库 FFI（UTF-16LE blob）+ sha256(realpath) 条目名 + 指纹校验全链路。
    /// 用唯一测试条目名，测毕 CredDeleteW 清理，不碰真实 ZCode 凭据。
    #[test]
    #[cfg(windows)]
    fn enc_v2_os_source_round_trip() {
        use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
        // 32 字节合成密钥
        let key: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(7).wrapping_add(13));
        let fp = fingerprint(&key);
        // 临时 ROOT + credentials.json（os_key 快路径据其 realpath 派生条目名）
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("v2")).unwrap();
        let cred_path = root.join("v2").join("credentials.json");
        std::fs::write(&cred_path, b"{}").unwrap();
        let real = cred_path.canonicalize().unwrap();
        let real = crate::registry::strip_verbatim(&real);
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(real.to_string_lossy().as_bytes());
        let digest: String = h.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        let target = format!("{}{}", digest, KEYRING_SUFFIX);
        // 密钥的 base64url 文本写入凭据库（UTF-16LE blob）
        let key_text = b64url_encode(&key);
        wincred::write_test(&target, &key_text).expect("写测试凭据");
        // 加密一段明文
        let cipher = Aes256Gcm::new((&key).into());
        let nonce_bytes = [9u8; 12];
        let ct = cipher.encrypt(Nonce::from_slice(&nonce_bytes), b"bigmodel-os-source".as_ref()).unwrap();
        let (data, tag) = ct.split_at(ct.len() - 16);
        let envelope = format!("enc:v2:os:{}:{}.{}.{}", fp,
            b64url_encode(&nonce_bytes), b64url_encode(tag), b64url_encode(data));
        // 设 ROOT 后 decode 应经 os_key 快路径解析密钥并解密
        set_root(Some(root));
        let decoded = decode(&json!(envelope)).expect("os-source 解密应成功");
        assert_eq!(decoded, "bigmodel-os-source");
        // 清理测试凭据
        let deleted = wincred::delete_test(&target);
        assert!(deleted, "测试凭据应删除成功");
        // 快路径条目已删 → read 应 None（不依赖全局枚举状态：其它会话/真实 ZCode
        // 可能有同后缀条目，慢路径枚举属生产行为，不在本测试断言范围）。
        assert!(wincred::read(&target).is_none(), "删除后快路径不应再读到测试密钥");
        set_root(None);
    }

    /// enc:v2 未知信封绝不静默当明文（上游警告的退化根因）。
    #[test]
    fn unknown_envelope_errors_not_plaintext() {
        let err = decode(&json!("enc:v9:whatever")).unwrap_err();
        assert!(err.contains("not recognised"), "未知信封应报错而非返回明文：{err}");
        // enc:v2 但 source 非法
        let err2 = decode(&json!("enc:v2:bogus:0123456789abcdef01234567:aaa.bbb.ccc")).unwrap_err();
        assert!(err2.contains("Invalid"), "非法 source 应报错：{err2}");
    }

    /// v1 fallback 平台口径：py_platform() 在 Windows 必须是 win32（与 python sys.platform 一致）。
    #[test]
    fn v1_fallback_platform_matches_python() {
        if cfg!(windows) {
            assert_eq!(py_platform(), "win32");
        }
    }
}