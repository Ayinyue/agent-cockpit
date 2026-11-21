// 凭据保管箱：磁盘上不留明文。
//
// 两层：
//   第一层 Windows DPAPI（CurrentUser）——密文只有这台机器这个账户能解，日常无感；
//   第二层可选主密码——用 PBKDF2-SHA512 把主密码派生成 entropy 一起交给 DPAPI，
//   于是必须**同时**有这台机器的 Windows 账户和正确的主密码才解得开。
// 只保管驾驶舱自己的东西（站点 Key、AI Key、账号 token），各 Agent 的登录态在它们
// 自己的加密库里，我们不碰也碰不了。
use crate::registry::base_dir;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const ROUNDS: i64 = 150_000;
const KDF: &str = "pbkdf2-sha512";

/// 解密后的明文库（只在内存里）；None = 还没读进来或已锁定
static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
/// 当前会话的主密码派生密钥；None = 只靠 DPAPI 那一层
static ENTROPY: Mutex<Option<Vec<u8>>> = Mutex::new(None);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// T15：测试专用开关——置 true 时 run_ps 直接报错，证明原生路径独立完成
/// （不依赖 PowerShell 回退）。生产恒 false；仅 cfg(test) 下可置位。
#[cfg(test)]
static PS_DISABLED_FOR_TEST: AtomicBool = AtomicBool::new(false);

pub fn file() -> PathBuf {
    base_dir().join("vault.json")
}

/// 保管箱能不能用（DPAPI 只在 Windows 上有）
pub fn supported() -> bool {
    cfg!(windows)
}

// ---------------------------------------------------------------- PowerShell 通道

/// 脚本一律纯 ASCII，正文用 -EncodedCommand(UTF-16LE base64) 送进去，
/// 密钥、密文、派生盐这些可能带特殊字符的东西走环境变量，绝不拼进命令行。
fn run_ps(script: &str, envs: &[(&str, &str)]) -> Result<String, String> {
    if !supported() {
        return Err("这台机器不是 Windows，没有 DPAPI".into());
    }
    #[cfg(test)]
    if PS_DISABLED_FOR_TEST.load(Ordering::SeqCst) {
        return Err("T15 测试：PowerShell 通道已禁用".into());
    }
    // -EncodedCommand 要的是 UTF-16LE 的 base64：绕开控制台代码页，脚本里的引号也不会被吞
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let enc = b64_encode(&utf16);
    let mut c = std::process::Command::new("powershell");
    c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &enc]);
    for (k, v) in envs {
        c.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let out = c.output().map_err(|e| format!("起 powershell 失败：{}", e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(clean_err(&String::from_utf8_lossy(&out.stderr)))
    }
}

fn clean_err(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .last()
        .unwrap_or("PowerShell 返回失败")
        .trim()
        .to_string()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(src: &[u8]) -> String {
    let mut s = String::with_capacity((src.len() + 2) / 3 * 4);
    for chunk in src.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        s.push(B64[(n >> 18) as usize & 63] as char);
        s.push(B64[(n >> 12) as usize & 63] as char);
        s.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    s
}

pub fn b64_decode(src: &str) -> Result<Vec<u8>, String> {
    let mut vals: Vec<u8> = Vec::with_capacity(src.len());
    for c in src.chars() {
        if c == '=' || c.is_whitespace() {
            continue;
        }
        let i = B64
            .iter()
            .position(|b| *b as char == c)
            .ok_or_else(|| format!("不是 base64 字符：{}", c))?;
        vals.push(i as u8);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        let mut n: u32 = 0;
        for (i, v) in chunk.iter().enumerate() {
            n |= (*v as u32) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

const PS_PROTECT: &str = r#"
$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Security
$in=[Convert]::FromBase64String($env:COCKPIT_IN)
$ent=if($env:COCKPIT_ENTROPY){[Convert]::FromBase64String($env:COCKPIT_ENTROPY)}else{$null}
$out=[Security.Cryptography.ProtectedData]::Protect($in,$ent,[Security.Cryptography.DataProtectionScope]::CurrentUser)
[Console]::Out.Write([Convert]::ToBase64String($out))
"#;

const PS_UNPROTECT: &str = r#"
$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Security
$in=[Convert]::FromBase64String($env:COCKPIT_IN)
$ent=if($env:COCKPIT_ENTROPY){[Convert]::FromBase64String($env:COCKPIT_ENTROPY)}else{$null}
$out=[Security.Cryptography.ProtectedData]::Unprotect($in,$ent,[Security.Cryptography.DataProtectionScope]::CurrentUser)
[Console]::Out.Write([Convert]::ToBase64String($out))
"#;

const PS_DERIVE: &str = r#"
$ErrorActionPreference='Stop'
$p=New-Object System.Security.Cryptography.Rfc2898DeriveBytes($env:COCKPIT_PWD,[Convert]::FromBase64String($env:COCKPIT_SALT),[int]$env:COCKPIT_ROUNDS,[System.Security.Cryptography.HashAlgorithmName]::SHA512)
[Console]::Out.Write([Convert]::ToBase64String($p.GetBytes(32)))
"#;

const PS_RANDOM: &str = r#"
$ErrorActionPreference='Stop'
$b=New-Object byte[] 32
$r=New-Object System.Security.Cryptography.RNGCryptoServiceProvider
$r.GetBytes($b)
[Console]::Out.Write([Convert]::ToBase64String($b))
"#;

fn protect(plain: &[u8], entropy: &[u8]) -> Result<String, String> {
    // T15 切生产：原生 CryptProtectData 优先（进程内，免起 PowerShell）；
    // 失败回退 PowerShell 通道（同口径 CurrentUser，golden+真实 vault 双通道已验证互解一致）。
    #[cfg(windows)]
    if let Ok(blob) = crate::dpapi_native::protect(plain, entropy) {
        return Ok(b64_encode(&blob));
    }
    let e = if entropy.is_empty() {
        String::new()
    } else {
        b64_encode(entropy)
    };
    run_ps(
        PS_PROTECT,
        &[("COCKPIT_IN", &b64_encode(plain)), ("COCKPIT_ENTROPY", &e)],
    )
}

fn unprotect(blob: &str, entropy: &[u8]) -> Result<Vec<u8>, String> {
    // T15 切生产：原生 CryptUnprotectData 优先。失败（含错 entropy/换机器）回退
    // PowerShell 再试一次——两通道任一成功即可，语义与旧版完全一致；
    // 都失败时返回 PowerShell 的错误（与旧版报错口径相同，load() 据此保持锁定）。
    #[cfg(windows)]
    if let Ok(raw) = b64_decode(blob) {
        if let Ok(plain) = crate::dpapi_native::unprotect(&raw, entropy) {
            return Ok(plain);
        }
    }
    let e = if entropy.is_empty() {
        String::new()
    } else {
        b64_encode(entropy)
    };
    let out = run_ps(PS_UNPROTECT, &[("COCKPIT_IN", blob), ("COCKPIT_ENTROPY", &e)])?;
    b64_decode(&out)
}

/// 只解一段 DPAPI 密文（CurrentUser、无附加 entropy）。
/// Qoder 系的 `Local State` 里那把 os_crypt 主密钥就是这么包的，调用方拿到的是原始 32 字节。
pub fn dpapi_open(b64: &str) -> Result<Vec<u8>, String> {
    if !supported() {
        return Err("这台机器没有 DPAPI".into());
    }
    unprotect(b64.trim(), &[])
}

fn derive(pwd: &str, salt_b64: &str, rounds: i64) -> Result<Vec<u8>, String> {
    // T15 切生产：进程内 PBKDF2-HMAC-SHA512（agentcrypt，golden 已钉与标准实现逐字节一致；
    // .NET Rfc2898DeriveBytes 对字符串密码同样按 UTF-8 取字节，口径一致）。
    // rounds 超界/盐解码失败回退 PowerShell（错误语义与旧版相同）。
    if let (Ok(salt), Ok(n)) = (b64_decode(salt_b64), u32::try_from(rounds)) {
        if n > 0 {
            return Ok(crate::agentcrypt::pbkdf2_sha512(pwd.as_bytes(), &salt, n, 32));
        }
    }
    let out = run_ps(
        PS_DERIVE,
        &[
            ("COCKPIT_PWD", pwd),
            ("COCKPIT_SALT", salt_b64),
            ("COCKPIT_ROUNDS", &rounds.to_string()),
        ],
    )?;
    b64_decode(&out)
}

fn random_b64() -> Result<String, String> {
    // T15 切生产：BCryptGenRandom（系统首选 CSPRNG，与 antigravity/remote 生产同款）
    // 优先；失败回退 PowerShell RNGCryptoServiceProvider。绝不退回可预测随机数。
    #[cfg(windows)]
    {
        let mut bytes = [0u8; 32];
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            fn BCryptGenRandom(algorithm: *mut std::ffi::c_void, buffer: *mut u8, len: u32, flags: u32) -> i32;
        }
        let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), 32, 2) };
        if status >= 0 {
            return Ok(b64_encode(&bytes));
        }
    }
    run_ps(PS_RANDOM, &[])
}

// ---------------------------------------------------------------- 落盘格式

fn meta() -> Value {
    fs::read_to_string(file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({ "format": 1 }))
}

fn entropy_now() -> Vec<u8> {
    ENTROPY.lock().ok().and_then(|g| g.clone()).unwrap_or_default()
}

/// 内存里的明文库重新加密写盘。整包一次 DPAPI，省得几十个 key 弹几十次进程。
/// 锁定状态（CACHE 是 None）必须拒绝写：那时内存里没有原有凭据，一写就把整包覆盖了。
pub fn flush() -> Result<(), String> {
    // T6：先清 dirty 再写。写失败由调用方感知；写成功后台不再重复。
    // 若写盘期间又有 put 置脏，后台下一轮会再写一次，不会丢。
    DIRTY.store(false, Ordering::SeqCst);
    LAST_FLUSH_MS.store(now_ms(), Ordering::SeqCst);
    let map = match CACHE.lock() {
        Ok(g) => match g.as_ref() {
            Some(m) => m.clone(),
            None => return Err("保管箱现在是锁着的，没写".into()),
        },
        Err(_) => return Err("保管箱状态取不到".into()),
    };
    let body = serde_json::to_vec(&map).map_err(|e| e.to_string())?;
    let m = meta();
    let blob = protect(&body, &entropy_now())?;
    let out = json!({
        "format": 1,
        "kdf": KDF,
        "rounds": m["rounds"].as_i64().unwrap_or(ROUNDS),
        "salt": m["salt"].as_str().unwrap_or(""),
        "has_password": m["has_password"].as_bool().unwrap_or(false),
        "hint": m["hint"].as_str().unwrap_or(""),
        "updated": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "count": map.len(),
        "plaintext_left": plaintext_left(),
        "blob": blob,
    });
    let _ = fs::create_dir_all(base_dir());
    fs::write(file(), serde_json::to_string_pretty(&out).unwrap_or_default())
        .map_err(|e| e.to_string())
}

/// 锁定期间留在文档里的明文条数，解锁后由 sweep 收进保管箱并清零
static PLAIN_LEFT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn plaintext_left() -> usize {
    PLAIN_LEFT.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_plaintext_left(n: usize) {
    PLAIN_LEFT.store(n, std::sync::atomic::Ordering::Relaxed);
}

fn load_map(text: &[u8]) {
    if let Ok(v) = serde_json::from_slice::<Value>(text) {
        if let Some(o) = v.as_object() {
            let mut m = HashMap::new();
            for (k, x) in o {
                if let Some(s) = x.as_str() {
                    m.insert(k.clone(), s.to_string());
                }
            }
            if let Ok(mut g) = CACHE.lock() {
                *g = Some(m);
            }
            // T6：刚从磁盘读进来的内容与磁盘一致，没有未落盘修改。
            DIRTY.store(false, Ordering::SeqCst);
        }
    }
}

/// 启动时读一次保管箱。没设主密码就当场解出来进内存（DPAPI 免输密码）；
/// 设了主密码则保持锁定，等界面输入。
pub fn load() {
    if !supported() {
        return;
    }
    let m = meta();
    let blob = match m["blob"].as_str() {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => {
            // 还没有保管箱：先建一个空的（只走 DPAPI），后面 sweep 导入时直接写
            if let Ok(mut g) = CACHE.lock() {
                if g.is_none() {
                    *g = Some(HashMap::new());
                }
            }
            let _ = flush();
            return;
        }
    };
    if m["has_password"].as_bool().unwrap_or(false) {
        // 有主密码：只有本会话已经解过一次才用得上那份 entropy
        if ENTROPY.lock().map(|g| g.is_some()).unwrap_or(false) {
            if let Ok(text) = unprotect(&blob, &entropy_now()) {
                load_map(&text);
            }
        }
        return;
    }
    match unprotect(&blob, &[]) {
        Ok(text) => load_map(&text),
        Err(e) => {
            // 解不开一般是换了 Windows 账户。**保持锁定**（CACHE 留 None）：
            // 这时候要是塞个空表进去，下一次 flush 就把真凭据整包覆盖没了。
            let mut s = LAST_ERR.lock().unwrap_or_else(|p| p.into_inner());
            *s = e;
        }
    }
}

static LAST_ERR: Mutex<String> = Mutex::new(String::new());

// ---------------------------------------------------------------- 对外接口

/// 内存里有明文库 = 可用（没设主密码，或本会话已解锁）。
/// 不可用时**绝不能**写保管箱：那时 CACHE 是空的，一写就把原有凭据整包覆盖没了。
pub fn available() -> bool {
    CACHE.lock().map(|g| g.is_some()).unwrap_or(false)
}

pub fn locked() -> bool {
    meta()["has_password"].as_bool().unwrap_or(false) && !available()
}

pub fn get(id: &str) -> Option<String> {
    CACHE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|m| m.get(id).cloned()))
}

/// 收一条凭据。返回 false 表示保管箱锁着，调用方**别把明文从文档里删掉**，
/// 标一下等解锁后由 sweep 收回。
pub fn put(id: &str, value: &str) -> bool {
    match CACHE.lock() {
        Ok(mut g) => match g.as_mut() {
            Some(m) => {
                m.insert(id.to_string(), value.to_string());
                mark_dirty();
                crate::proxy::invalidate_legacy_pool(); // T7：token 变更影响路由可用性
                true
            }
            None => false,
        },
        Err(_) => false,
    }
}

pub fn remove(id: &str) {
    if let Ok(mut g) = CACHE.lock() {
        if let Some(m) = g.as_mut() {
            if m.remove(id).is_some() {
                mark_dirty();
                crate::proxy::invalidate_legacy_pool(); // T7：token 移除影响路由可用性
            }
        }
    }
}

// ---------------------------------------------------------------- T6 批量落盘
//
// 原先每次 put/remove 后调用方都立刻 flush()：整包重新 DPAPI 加密 + 起一个
// PowerShell（高负载实测冷启 3~5s）。批量登记 10 个账号就是 10 次进程。
// 现在 put/remove 只置脏，由三条路收口：
//   1. 后台线程每 ≤1s 看一眼，脏了且距上次写 ≥5s 才 flush（连续写合并成一次）；
//   2. 关键路径（改名/导入等要求立刻持久化的）仍直接调 flush()，语义不变；
//   3. 退出钩子（lifecycle::exit）强制 flush，崩溃丢写窗口 ≤5s。
// 锁定（CACHE=None）时 flush 本就拒绝写，置脏无副作用。

/// 有未落盘的修改。put/remove 置位；flush 开头清位。
static DIRTY: AtomicBool = AtomicBool::new(false);
/// 上次成功进入 flush 的时刻（毫秒 epoch）；后台据此做 ≥5s 合并窗口。
static LAST_FLUSH_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 后台 flush 线程只起一次。
static FLUSH_THREAD: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mark_dirty() {
    DIRTY.store(true, Ordering::SeqCst);
    ensure_flush_thread();
}

/// 是否有未落盘修改（测试与诊断用）。
pub fn is_dirty() -> bool {
    DIRTY.load(Ordering::SeqCst)
}

/// 起后台收口线程（幂等）。首次置脏时自动拉起；也可在启动时显式调用。
/// 测试进程里不拉：避免单测的 put 触发后台真写用户 vault.json。
pub fn ensure_flush_thread() {
    if cfg!(test) {
        return;
    }
    FLUSH_THREAD.get_or_init(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if !DIRTY.load(Ordering::SeqCst) {
                continue;
            }
            let last = LAST_FLUSH_MS.load(Ordering::SeqCst);
            if last != 0 && now_ms().saturating_sub(last) < 5_000 {
                continue; // 合并窗口内：让连续写攒成一次
            }
            let _ = flush();
        });
    });
}

/// 退出钩子用：有脏就立刻写，不等合并窗口。锁定状态 flush 自己会拒绝。
pub fn flush_on_exit() {
    if DIRTY.load(Ordering::SeqCst) {
        let _ = flush();
    }
}

pub fn count() -> usize {
    CACHE.lock().ok().and_then(|g| g.as_ref().map(|m| m.len())).unwrap_or(0)
}

/// 保管箱本身存了多少条密文，用于界面说"已收 N 个凭据"
pub fn status_json() -> Value {
    let m = meta();
    json!({
        "ok": true,
        "supported": supported(),
        "file": file().to_string_lossy(),
        "has_password": m["has_password"].as_bool().unwrap_or(false),
        "locked": locked(),
        "hint": m["hint"].as_str().unwrap_or(""),
        "count": m["count"].as_u64().map(|x| x as usize).unwrap_or_else(count),
        "plaintext_left": m["plaintext_left"]
            .as_u64()
            .map(|x| x as usize)
            .unwrap_or_else(plaintext_left),
        "available": available(),
        "rounds": m["rounds"].as_i64().unwrap_or(ROUNDS),
        "kdf": KDF,
        "updated": m["updated"].as_str().unwrap_or(""),
        "error": LAST_ERR.lock().map(|s| s.clone()).unwrap_or_default(),
    })
}

/// 设 / 改主密码。要求此刻拿得到明文（没设过密码，或本会话已解锁），
/// 否则连库都解不开，改了密码等于把数据锁死。
pub fn set_password(pwd: &str, again: &str, hint: &str) -> (bool, String) {
    if !supported() {
        return (false, "这台机器不是 Windows，用不了 DPAPI".into());
    }
    if pwd.len() < 6 {
        return (false, "主密码至少 6 位".into());
    }
    if pwd != again {
        return (false, "两次输入不一样".into());
    }
    if !available() {
        return (false, "保管箱现在是锁着的，先用旧密码解锁再改。".into());
    }
    let salt = match random_b64() {
        Ok(s) => s,
        Err(e) => return (false, format!("生成随机盐失败：{}", e)),
    };
    let key = match derive(pwd, &salt, ROUNDS) {
        Ok(k) => k,
        Err(e) => return (false, format!("派生密钥失败：{}", e)),
    };
    let m = meta();
    let was = m["has_password"].as_bool().unwrap_or(false);
    let map = CACHE
        .lock()
        .map(|g| g.clone().unwrap_or_default())
        .unwrap_or_default();
    // 先按新 entropy 加密，成功后才改元数据——中间任何一步炸了，旧的那份还能解
    let body = match serde_json::to_vec(&map) {
        Ok(b) => b,
        Err(e) => return (false, e.to_string()),
    };
    let blob = match protect(&body, &key) {
        Ok(b) => b,
        Err(e) => return (false, format!("加密失败：{}", e)),
    };
    let out = json!({
        "format": 1, "kdf": KDF, "rounds": ROUNDS, "salt": salt,
        "has_password": true, "hint": hint.trim(),
        "updated": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "count": map.len(), "blob": blob,
    });
    let n = map.len();
    if let Err(e) = fs::write(file(), serde_json::to_string_pretty(&out).unwrap_or_default()) {
        return (false, format!("写保管箱失败：{}", e));
    }
    if let Ok(mut g) = ENTROPY.lock() {
        *g = Some(key);
    }
    // T6：本函数已按新 entropy 整包写盘，清掉脏标志——
    // 否则后台 flush 可能拿旧时序的脏状态再写一遍，徒增一次 PowerShell。
    DIRTY.store(false, Ordering::SeqCst);
    LAST_FLUSH_MS.store(now_ms(), Ordering::SeqCst);
    (
        true,
        format!(
            "已{}主密码，{} 个凭据都重新加密过（本次会话仍解锁）。每次开机要输一次才用得了 Key。",
            if was { "改过" } else { "设上" },
            n
        ),
    )
}

pub fn unlock(pwd: &str) -> (bool, String) {
    let m = meta();
    if !m["has_password"].as_bool().unwrap_or(false) {
        return (false, "没设主密码，不用解锁。".into());
    }
    let salt = m["salt"].as_str().unwrap_or("").to_string();
    let rounds = m["rounds"].as_i64().unwrap_or(ROUNDS);
    let key = match derive(pwd, &salt, rounds) {
        Ok(k) => k,
        Err(e) => return (false, format!("派生失败：{}", e)),
    };
    let blob = m["blob"].as_str().unwrap_or("").to_string();
    match unprotect(&blob, &key) {
        Ok(text) => {
            if let Ok(mut g) = ENTROPY.lock() {
                *g = Some(key);
            }
            load_map(&text);
            let _ = flush_count();
            (true, format!("已解锁，{} 个凭据可用。", count()))
        }
        Err(e) => (
            false,
            format!("解不开：主密码不对（或这份保管箱不是这台机器写的）。{}", e),
        ),
    }
}

/// 只更新 count 字段，不动 blob
fn flush_count() -> Result<(), String> {
    let mut m = meta();
    if let Some(o) = m.as_object_mut() {
        o.insert("count".into(), json!(count()));
    }
    fs::write(file(), serde_json::to_string_pretty(&m).unwrap_or_default()).map_err(|e| e.to_string())
}

pub fn lock() -> String {
    if let Ok(mut g) = CACHE.lock() {
        *g = None;
    }
    if let Ok(mut g) = ENTROPY.lock() {
        *g = None;
    }
    "已锁定：内存里的明文都清了，界面上看不到也复制不了任何 Key。".to_string()
}

pub fn remove_password() -> (bool, String) {
    let m = meta();
    if !m["has_password"].as_bool().unwrap_or(false) {
        return (false, "本来就没设主密码。".into());
    }
    if !available() {
        return (false, "先解锁才能移除主密码（不然解不开这份密文）。".into());
    }
    let map = CACHE
        .lock()
        .map(|g| g.clone().unwrap_or_default())
        .unwrap_or_default();
    let body = match serde_json::to_vec(&map) {
        Ok(b) => b,
        Err(e) => return (false, e.to_string()),
    };
    // entropy 传空 = 只剩 DPAPI 那一层
    let blob = match protect(&body, &[]) {
        Ok(b) => b,
        Err(e) => return (false, format!("重新加密失败：{}", e)),
    };
    let out = json!({
        "format": 1, "kdf": KDF, "rounds": ROUNDS, "salt": "",
        "has_password": false, "hint": "",
        "updated": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "count": map.len(), "blob": blob,
    });
    if let Err(e) = fs::write(file(), serde_json::to_string_pretty(&out).unwrap_or_default()) {
        return (false, format!("写保管箱失败：{}", e));
    }
    if let Ok(mut g) = ENTROPY.lock() {
        *g = None;
    }
    // T6：同 set_password——已整包写盘，清脏，避免后台重复加密。
    DIRTY.store(false, Ordering::SeqCst);
    LAST_FLUSH_MS.store(now_ms(), Ordering::SeqCst);
    (
        true,
        format!("已移除主密码，{} 个凭据退回只靠 Windows 账户这一层保护。", map.len()),
    )
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_handles_padding() {
        for s in ["", "a", "ab", "abc", "abcd", "中文key-123"] {
            let b = s.as_bytes();
            let enc = b64_encode(b);
            let dec = b64_decode(&enc).unwrap();
            assert_eq!(dec, b, "{} 编解码要能回到原样", s);
        }
    }

    /// DPAPI 是真调用，Windows CI/本机上跑得通；非 Windows 上跳过
    #[test]
    fn dpapi_roundtrip_when_supported() {
        if !supported() {
            return;
        }
        let blob = protect(b"sk-test-1234", b"").unwrap();
        assert!(!blob.is_empty());
        assert_ne!(String::from_utf8_lossy(&b64_decode(&blob).unwrap()), "sk-test-1234".to_string());
        let back = unprotect(&blob, b"").unwrap();
        assert_eq!(back, b"sk-test-1234");
        // 加了 entropy 就得用同一段才解得开
        let b2 = protect(b"sk-test-1234", b"entropy-from-password").unwrap();
        assert!(unprotect(&b2, b"wrong-entropy").is_err());
        assert_eq!(unprotect(&b2, b"entropy-from-password").unwrap(), b"sk-test-1234");
    }

    #[test]
    fn pbkdf2_is_stable_and_password_sensitive() {
        if !supported() {
            return;
        }
        let salt = random_b64().unwrap();
        let a = derive("hunter2", &salt, 1000).unwrap();
        let b = derive("hunter2", &salt, 1000).unwrap();
        let c = derive("hunter3", &salt, 1000).unwrap();
        assert_eq!(a, b, "同盐同密码要出同一把钥匙");
        assert_eq!(a.len(), 32);
        assert_ne!(a, c, "差一个字符就该完全不同");
    }

    // ---------------- T15 切生产：golden 桥（由 tests/t15-secrets-golden.test.py 触发） ----------------
    // emit：走生产 protect/derive/random_b64（原生优先路径）产出向量供 PowerShell 验证；
    // verify：走生产 unprotect 解 PowerShell 密文（含错 entropy 必须拒绝）。
    #[test]
    fn t15_golden_bridge() {
        if !supported() {
            return;
        }
        let Ok(req_path) = std::env::var("T15_SECRETS_REQ") else {
            eprintln!("跳过 golden：未设 T15_SECRETS_REQ");
            return;
        };
        let req: Value = serde_json::from_str(&fs::read_to_string(&req_path).unwrap()).unwrap();
        let out_path = req["out"].as_str().unwrap();
        match req["mode"].as_str().unwrap() {
            "emit" => {
                let plain = b64_decode(req["plain_b64"].as_str().unwrap()).unwrap();
                let entropy = b64_decode(req["entropy_b64"].as_str().unwrap()).unwrap();
                let pwd = req["pwd"].as_str().unwrap();
                let salt = req["salt_b64"].as_str().unwrap();
                let v = json!({
                    "blob_b64": protect(&plain, &entropy).unwrap(),
                    "blob_noent_b64": protect(&plain, &[]).unwrap(),
                    "entropy_b64": req["entropy_b64"],
                    "salt_b64": salt,
                    "derive_1000": b64_encode(&derive(pwd, salt, 1000).unwrap()),
                    "derive_150000": b64_encode(&derive(pwd, salt, 150_000).unwrap()),
                    "random1": random_b64().unwrap(),
                    "random2": random_b64().unwrap(),
                });
                fs::write(out_path, v.to_string()).unwrap();
            }
            "verify" => {
                let entropy = b64_decode(req["entropy_b64"].as_str().unwrap()).unwrap();
                let wrong = b64_decode(req["wrong_entropy_b64"].as_str().unwrap()).unwrap();
                let expect = b64_decode(req["expect_plain_b64"].as_str().unwrap()).unwrap();
                let ent_ok = unprotect(req["blob_b64"].as_str().unwrap(), &entropy)
                    .map(|p| p == expect).unwrap_or(false);
                let noent_ok = unprotect(req["blob_noent_b64"].as_str().unwrap(), &[])
                    .map(|p| p == expect).unwrap_or(false);
                let wrong_rejected = unprotect(req["blob_b64"].as_str().unwrap(), &wrong).is_err();
                let v = json!({"ent_ok": ent_ok, "noent_ok": noent_ok, "wrong_rejected": wrong_rejected});
                fs::write(out_path, v.to_string()).unwrap();
                assert!(ent_ok && noent_ok && wrong_rejected, "生产 unprotect 校验失败：{v}");
            }
            other => panic!("未知 golden mode：{other}"),
        }
    }

    // ---------------- T15 切生产：原生路径独立性（禁 PowerShell 仍全通） ----------------
    // 证明生产四原语真的走原生（而非永远靠 PS 回退兜底）。
    // 注意：PS_DISABLED_FOR_TEST 是进程级全局，本测试独占使用并在结尾还原；
    // 同进程其它 DPAPI 测试若并行运行会受影响——用串行锁把它们隔开。
    static T15_SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn t15_native_path_works_without_powershell() {
        if !supported() {
            return;
        }
        let _g = T15_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        PS_DISABLED_FOR_TEST.store(true, Ordering::SeqCst);
        let result = (|| {
            // protect + unprotect（含 entropy）全走原生
            let ent = b"t15-native-only-entropy";
            let blob = protect(b"t15-native-only-payload", ent)?;
            assert_eq!(unprotect(&blob, ent)?, b"t15-native-only-payload");
            assert!(unprotect(&blob, b"wrong").is_err(), "错 entropy 必须拒绝（无 PS 兜底）");
            // 无 entropy 路径
            let b2 = protect(b"no-ent", &[])?;
            assert_eq!(unprotect(&b2, &[])?, b"no-ent");
            // derive 进程内 PBKDF2
            let salt = b64_encode(b"0123456789abcdef");
            let k = derive("t15-pwd", &salt, 1000)?;
            assert_eq!(k.len(), 32);
            assert_eq!(k, derive("t15-pwd", &salt, 1000)?);
            // random BCryptGenRandom
            let r1 = b64_decode(&random_b64()?)?;
            let r2 = b64_decode(&random_b64()?)?;
            assert_eq!(r1.len(), 32);
            assert_ne!(r1, r2);
            Ok::<(), String>(())
        })();
        PS_DISABLED_FOR_TEST.store(false, Ordering::SeqCst);
        result.expect("原生路径应独立完成全部四原语");
    }

    // ---------------- T6 批量落盘：机制测试（不触发 DPAPI/PowerShell） ----------------
    // CACHE/DIRTY 是全局静态，cargo 同进程并行跑测试——三个断言段必须串在
    // 一条测试里顺序执行，拆成三条会互相踩状态。

    /// 1) put/remove 置脏（后台/退出收口能拿到活的前提；删 mark_dirty 即红）；
    /// 2) 锁定态 put 拒绝且不置脏（否则退出钩子白跑注定失败的 flush）；
    /// 3) flush 进入即清脏并记时刻（锁定态早退路径同样清）。
    #[test]
    fn t6_dirty_semantics() {
        // 段 1：可用态置脏
        {
            let mut g = CACHE.lock().unwrap();
            if g.is_none() {
                *g = Some(HashMap::new());
            }
        }
        DIRTY.store(false, Ordering::SeqCst);
        assert!(put("t6-probe-key", "v"));
        assert!(DIRTY.load(Ordering::SeqCst), "put 后必须置脏（T6 回退即红）");
        DIRTY.store(false, Ordering::SeqCst);
        remove("t6-probe-key");
        assert!(DIRTY.load(Ordering::SeqCst), "remove 命中后必须置脏");
        remove("t6-probe-key"); // 清理测试键

        // 段 2：锁定态拒绝且不置脏
        let saved = CACHE.lock().unwrap().take();
        DIRTY.store(false, Ordering::SeqCst);
        assert!(!put("t6-locked-key", "v"), "锁定必须拒绝");
        assert!(!DIRTY.load(Ordering::SeqCst), "锁定拒绝不得置脏");

        // 段 3：flush 进入即清脏（锁定态早退，不触发 DPAPI）
        DIRTY.store(true, Ordering::SeqCst);
        let r = flush();
        assert!(r.is_err(), "锁定态 flush 应拒绝写");
        assert!(!DIRTY.load(Ordering::SeqCst), "flush 进入即清脏");
        assert!(LAST_FLUSH_MS.load(Ordering::SeqCst) > 0, "应记录 flush 时刻");

        // 还原全局状态，避免污染同进程其它测试
        *CACHE.lock().unwrap() = saved;
        DIRTY.store(false, Ordering::SeqCst);
    }
}
