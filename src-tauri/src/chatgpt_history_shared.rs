//! T18 第六批（最后一块）：chatgpt-history-shared 的 migrate 管理迁 Rust
//! （与 python chatgpt-history-shared.py 逐条对齐）。
//!
//! 一份本机历史库，凭据与浏览器偏好保持独立。已在运行的原生 CODEX_HOME 拥有物理
//! rollout 目录与 SQLite WAL 库；私有档案经 sqlite_home 指向共享库，sessions/
//! archived_sessions 用窄校验的目录 junction 接入。绝不链接 auth 目录。
//!
//! 安全规则（与 python 版一致）：
//! 1. 迁移前两份物理副本并存时对账（rep.run 增量合并 + merge_projects + union 校验）。
//! 2. 目录先 rename 成 stage 再挂 junction；junction 建后必须 exact_link 验证；
//!    任何一步失败按逆序回滚 junction/stage/config，私有文件绝不丢。
//! 3. 退役私有库前 check_database_union；在线备份 → gzip → 解压回读 sha256 比对
//!    通过才落备份，再删本体与 -wal/-shm。
//! 4. cleanup_stages 逐文件确认已保全（或可安全丢弃的 settings-only 续写）才递归删除；
//!    删除目标必须是 home 直接子目录且带 .history-shared-stage- 前缀。
//! 5. 中断恢复保守：残留旧库/stage 时报错保留现场，交人工核查。
//! 6. sync.lock 跨进程互斥（与 python msvcrt/_locking 同字节区间）。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::chatgpt_history_copy::{history_state, py_dumps, Relocator};
use crate::chatgpt_history_repair::{key, ordinary, uuid_hex};
use crate::chatgpt_history_sync::{connect, rows, snapshot, replace_offset, filelock, Replicator};

const DIRECTORIES: [&str; 2] = ["sessions", "archived_sessions"];
const LOCK_DIRECTORY: &str = "thread-writer-locks";
const MARKER: &str = ".agentcockpit-shared-history.json";

/// 与 python atomic_json 对齐：utf-8 JSON（ensure_ascii=False 口径）原子落盘。
fn atomic_json(path: &Path, value: &Value) -> Result<(), String> {
    ordinary(path)?;
    if let Some(p) = path.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
    let temp = path.with_file_name(format!("{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), uuid_hex()));
    let result = (|| -> Result<(), String> {
        std::fs::write(&temp, py_dumps(value).as_bytes()).map_err(|e| e.to_string())?;
        std::fs::rename(&temp, path).map_err(|e| e.to_string())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temp); }
    result
}

/// 与 python write_bytes 对齐：<name>.<uuid>.tmp 写入后 os.replace。
fn write_bytes(path: &Path, data: &[u8]) -> Result<(), String> {
    let temp = path.with_file_name(format!("{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), uuid_hex()));
    let result = (|| -> Result<(), String> {
        std::fs::write(&temp, data).map_err(|e| e.to_string())?;
        std::fs::rename(&temp, path).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_secs_f64()
}

/// 与 python config_bytes 对齐：读 utf-8-sig TOML；sqlite_home 已一致则原样返回；
/// 否则剔除根表旧 sqlite_home 行、在最前插入新行，并用 TOML 解析回验其余键完全保留。
fn config_bytes(path: &Path, canonical: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    let original = if path.is_file() { std::fs::read(path).map_err(|e| e.to_string())? } else { Vec::new() };
    let text = String::from_utf8(original.clone()).map_err(|e| e.to_string())?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let config: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let canon = canonical.to_string_lossy().to_string();
    if config.get("sqlite_home").and_then(|v| v.as_str()) == Some(&canon) {
        return Ok((original.clone(), original));
    }
    // splitlines(keepends) 口径：按 \n 切、保留行尾（与 TOML 行式重写一致）。
    let mut lines: Vec<String> = Vec::new();
    let mut in_root = true;
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut segments: Vec<&str> = Vec::new();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' { segments.push(&text[start..=i]); start = i + 1; }
    }
    if start < text.len() { segments.push(&text[start..]); }
    for line in segments {
        if line.trim_start().starts_with('[') { in_root = false; }
        if in_root && sqlite_home_line(line) { continue; }
        lines.push(line.to_string());
    }
    let updated = format!("sqlite_home = {}\n{}", py_dumps(&json!(canon)), lines.concat());
    // expected = dict(config, sqlite_home=canonical)；解析回验必须完全相等。
    let parsed: toml::Value = toml::from_str(&updated).map_err(|e| e.to_string())?;
    let mut expected = config.clone();
    match &mut expected {
        toml::Value::Table(e) => { e.insert("sqlite_home".into(), toml::Value::String(canon.clone())); }
        _ => return Err("Account configuration could not be preserved".into()),
    }
    if parsed != expected {
        return Err("Account configuration could not be preserved".into());
    }
    Ok((original, updated.into_bytes()))
}

/// 根表 sqlite_home 赋值行判定（等价 python re：\s*(?:sqlite_home|"sqlite_home"|'sqlite_home')\s*=）。
fn sqlite_home_line(line: &str) -> bool {
    let t = line.trim_start();
    for form in ["sqlite_home", "\"sqlite_home\"", "'sqlite_home'"] {
        if let Some(rest) = t.strip_prefix(form) {
            if rest.trim_start().starts_with('=') { return true; }
        }
    }
    false
}

/// 与 python link_directory 对齐：Windows 用 EncodedCommand PowerShell New-Item Junction
/// （账号名里的元字符保持字面量）；其余平台目录符号链接。20 秒超时。
fn link_directory(path: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let literal = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "''"));
        let command = format!(
            "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
            literal(path), literal(target));
        let utf16: Vec<u8> = command.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let encoded = crate::secrets::b64_encode(&utf16);
        let mut cmd = std::process::Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-EncodedCommand", &encoded]);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
        let child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .spawn().map_err(|e| e.to_string())?;
        let started = std::time::Instant::now();
        let mut child = child;
        loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(_) => break,
                None => {
                    if started.elapsed() > std::time::Duration::from_secs(20) {
                        let _ = child.kill(); let _ = child.wait();
                        return Err("Junction creation timed out".into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("Junction creation failed: {}", String::from_utf8_lossy(&out.stderr).chars().take(400).collect::<String>()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, path).map_err(|e| e.to_string())
    }
}

/// 与 python exact_link 对齐：存在 + resolve 相等 + absolute 不等；OSError → false。
/// resolve 用 std::fs::canonicalize（Windows 去 \\?\ 前缀后与 python Path.resolve 同口径）。
fn exact_link(path: &Path, target: &Path) -> bool {
    if !path.exists() { return false; }
    let resolve = |p: &Path| -> Option<PathBuf> {
        let c = std::fs::canonicalize(p).ok()?;
        let s = c.to_string_lossy().to_string();
        Some(PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()))
    };
    match (resolve(path), resolve(target)) {
        (Some(a), Some(b)) => key(&a.to_string_lossy()) == key(&b.to_string_lossy())
            && key(&path.to_string_lossy()) != key(&target.to_string_lossy()),
        _ => false,
    }
}

/// 与 python private_files 对齐：os.walk(followlinks=False)，目录与文件都产出；
/// 链接目录本身产出但不深入。返回 (path, is_dir)。
fn private_files(directory: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    let mut out = Vec::new();
    let mut stack = vec![directory.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        let mut subdirs = Vec::new();
        for p in entries {
            let meta = std::fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
            let is_link = crate::registry::is_reparse_point(&p) || meta.file_type().is_symlink();
            if meta.is_dir() {
                out.push((p.clone(), true));
                if !is_link { subdirs.push(p); }
            } else {
                out.push((p, false));
            }
        }
        // 深度优先与 os.walk 一致的方向不影响正确性（调用方逐文件独立处理）。
        for s in subdirs.into_iter().rev() { stack.push(s); }
    }
    Ok(out)
}

/// state_N.sqlite / thread_history_N.sqlite 精确匹配（等价 python re.fullmatch）。
fn retire_database_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".sqlite") else { return false };
    for prefix in ["state_", "thread_history_"] {
        if let Some(num) = stem.strip_prefix(prefix) {
            return !num.is_empty() && num.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

pub(crate) struct SharedHistory {
    rep: Replicator,
    canonical: PathBuf,
    state_dir: PathBuf,
    report: Value,
    rust_catalog: bool,
}

impl SharedHistory {
    pub(crate) fn new(canonical: &Path, state_dir: &Path, rust_catalog: bool) -> Result<Self, String> {
        let canonical = ordinary(canonical)?;
        let state_dir = ordinary(state_dir)?;
        std::fs::create_dir_all(&state_dir).map_err(|e| e.to_string())?;
        let rep = Replicator::new(&state_dir)?;
        let report = json!({"ok": true, "version": 2, "storage": "shared",
            "canonical": canonical.to_string_lossy(), "updated_at": now_secs(),
            "homes": 1, "migrated": [], "pending": [], "conflicts": [], "released_bytes": 0});
        for name in DIRECTORIES {
            let p = ordinary(&canonical.join(name))?;
            std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
        }
        let locks = ordinary(&canonical.join(LOCK_DIRECTORY))?;
        std::fs::create_dir_all(&locks).map_err(|e| e.to_string())?;
        Ok(SharedHistory { rep, canonical, state_dir, report, rust_catalog })
    }

    fn marker(&self, home: &Path) -> Result<Option<Value>, String> {
        let path = home.join(MARKER);
        if !path.exists() { return Ok(None); }
        ordinary(&path)?;
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if value.get("canonical").and_then(|v| v.as_str()) != Some(self.canonical.to_string_lossy().as_ref()) {
            return Err("Account is linked to a different history store".into());
        }
        Ok(Some(value))
    }

    fn verify_links(&self, home: &Path) -> Result<(), String> {
        for name in DIRECTORIES {
            if !exact_link(&home.join(name), &self.canonical.join(name)) {
                return Err("Shared history link is missing or points elsewhere".into());
            }
        }
        let text = std::fs::read_to_string(home.join("config.toml")).map_err(|e| e.to_string())?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let config: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
        if config.get("sqlite_home").and_then(|v| v.as_str()) != Some(self.canonical.to_string_lossy().as_ref()) {
            return Err("Account SQLite state is not shared".into());
        }
        Ok(())
    }

    fn ensure_lock_directory(&self, home: &Path) -> Result<(), String> {
        let source = home.join(LOCK_DIRECTORY);
        let target = self.canonical.join(LOCK_DIRECTORY);
        if exact_link(&source, &target) { return Ok(()); }
        ordinary(&source)?;
        let old = home.join(format!(".history-locks-before-shared-{}", uuid_hex()));
        let existed = source.exists();
        if existed { std::fs::rename(&source, &old).map_err(|e| e.to_string())?; }
        let result = (|| -> Result<(), String> {
            link_directory(&source, &target)?;
            if !exact_link(&source, &target) {
                return Err("Shared writer lock link verification failed".into());
            }
            Ok(())
        })();
        if result.is_err() {
            if exact_link(&source, &target) { let _ = std::fs::remove_dir(&source); }
            if existed && !source.exists() { let _ = std::fs::rename(&old, &source); }
        }
        result
    }

    fn normalize_paths(&self, homes: &[PathBuf]) -> Result<(), String> {
        let db = self.canonical.join("state_5.sqlite");
        if !db.exists() { return Ok(()); }
        let con = connect(&db, true)?;
        let tx = con.unchecked_transaction().map_err(|e| e.to_string())?;
        let pairs: Vec<(String, String)> = {
            let mut st = con.prepare("SELECT id,rollout_path FROM threads").map_err(|e| e.to_string())?;
            let rr = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
            rr.filter_map(|r| r.ok()).collect()
        };
        for (id, raw) in pairs {
            let stripped = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
            let path = Path::new(stripped);
            for home in homes {
                let Ok(relative) = path.strip_prefix(home) else { continue };
                let first = relative.components().next().map(|c| c.as_os_str().to_string_lossy().to_string());
                if let Some(first) = first.filter(|f| DIRECTORIES.contains(&f.as_str())) {
                    let _ = first;
                    let destination = self.canonical.join(relative);
                    let dest_str = destination.to_string_lossy().to_string();
                    if destination.is_file() && dest_str != raw {
                        con.execute("UPDATE threads SET rollout_path=?1 WHERE id=?2 AND rollout_path=?3",
                            [&dest_str, &id, &raw]).map_err(|e| e.to_string())?;
                    }
                    break;
                }
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    fn merge_projects(&self, home: &Path) -> Result<(), String> {
        let src = home.join("state_5.sqlite");
        let dst = self.canonical.join("state_5.sqlite");
        if !src.exists() || !dst.exists() { return Ok(()); }
        let a = connect(&src, false)?;
        let b = connect(&dst, true)?;
        let tx = b.unchecked_transaction().map_err(|e| e.to_string())?;
        for table in ["projects", "project_roots", "thread_sections"] {
            for row in rows(&a, table, None)? {
                self.rep.upsert(&b, table, &row, false)?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    fn merge_files(&mut self, home: &Path) -> Result<(), String> {
        // 孤儿与归档 rollout 一并合并，不只索引里的线程。
        for name in DIRECTORIES {
            let source = home.join(name);
            if !source.exists() { continue; }
            ordinary(&source)?;
            for (path, directory) in private_files(&source)? {
                ordinary(&path)?;
                if directory { continue; }
                let relative = path.strip_prefix(&source).map_err(|e| e.to_string())?;
                let destination = self.canonical.join(name).join(relative);
                ordinary(&destination)?;
                if !destination.exists() {
                    if let Some(p) = destination.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
                    std::fs::copy(&path, &destination).map_err(|e| e.to_string())?;
                } else if std::fs::read(&path).map_err(|e| e.to_string())? != std::fs::read(&destination).map_err(|e| e.to_string())? {
                    if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                        return Err(format!("An archive has different content in two homes: {}",
                            path.file_name().unwrap_or_default().to_string_lossy()));
                    }
                    let old = snapshot(&path)?;
                    let new = snapshot(&destination)?;
                    if replace_offset(&old, &new).is_some() { continue; }
                    if replace_offset(&new, &old).is_none() {
                        return Err(format!("Conflicting conversation continuations: {}",
                            path.file_name().unwrap_or_default().to_string_lossy()));
                    }
                    self.rep.publish(&self.canonical.clone(), &destination, &old)?;
                }
            }
        }
        Ok(())
    }

    fn initialize_preferences(&self, home: &Path) -> Result<(), String> {
        // 每窗口设置保持本地；已有偏好优先。
        let target = home.join(".codex-global-state.json");
        let original = self.canonical.join(".codex-global-state.json");
        if !target.exists() && original.is_file() {
            let relocate = Relocator { source: self.canonical.clone(), target: home.to_path_buf() };
            let state = history_state(&original, &relocate)?;
            atomic_json(&target, &state)?;
        }
        Ok(())
    }

    fn check_database_union(&self, home: &Path) -> Result<(), String> {
        let db = home.join("state_5.sqlite");
        let shared = self.canonical.join("state_5.sqlite");
        if !db.is_file() { return Ok(()); }
        let a = connect(&db, false)?;
        let b = connect(&shared, false)?;
        let ids: std::collections::HashSet<String> = {
            let mut st = b.prepare("SELECT id FROM threads").map_err(|e| e.to_string())?;
            let rr = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
            rr.filter_map(|r| r.ok()).collect()
        };
        let private: Vec<String> = {
            let mut st = a.prepare("SELECT id FROM threads").map_err(|e| e.to_string())?;
            let rr = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
            rr.filter_map(|r| r.ok()).collect()
        };
        if private.iter().any(|id| !ids.contains(id)) {
            return Err("Some private threads are not in the shared database".into());
        }
        Ok(())
    }

    fn retire_databases(&mut self, home: &Path) -> Result<(), String> {
        self.check_database_union(home)?;
        let tag = crate::chatgpt_history_sync::sha256_hex(home.to_string_lossy().as_bytes());
        let tag = &tag[..16];
        let backup_root = self.state_dir.join("before-shared").join(tag);
        let mut entries: Vec<PathBuf> = std::fs::read_dir(home).map_err(|e| e.to_string())?
            .filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if !retire_database_name(&name) { continue; }
            ordinary(&path)?;
            std::fs::create_dir_all(&backup_root).map_err(|e| e.to_string())?;
            let backup = backup_root.join(format!("{}.gz", name));
            if !backup.exists() {
                let snap = backup_root.join(format!("{}.snapshot-{}", name, uuid_hex()));
                let packed = backup.with_file_name(format!("{}.gz.{}.tmp", name, uuid_hex()));
                let result = (|| -> Result<(), String> {
                    {
                        let src = connect(&path, false)?;
                        let mut dst = rusqlite::Connection::open(&snap).map_err(|e| e.to_string())?;
                        let bk = rusqlite::backup::Backup::new(&src, &mut dst).map_err(|e| e.to_string())?;
                        bk.run_to_completion(256, std::time::Duration::from_millis(50), None).map_err(|e| e.to_string())?;
                    }
                    let raw = std::fs::read(&snap).map_err(|e| e.to_string())?;
                    {
                        use std::io::Write;
                        let f = std::fs::File::create(&packed).map_err(|e| e.to_string())?;
                        let mut gz = flate2::write::GzEncoder::new(f, flate2::Compression::new(6));
                        gz.write_all(&raw).map_err(|e| e.to_string())?;
                        gz.finish().map_err(|e| e.to_string())?;
                    }
                    {
                        use std::io::Read;
                        let f = std::fs::File::open(&packed).map_err(|e| e.to_string())?;
                        let mut gz = flate2::read::GzDecoder::new(f);
                        let mut roundtrip = Vec::new();
                        gz.read_to_end(&mut roundtrip).map_err(|e| e.to_string())?;
                        if crate::chatgpt_history_sync::sha256_hex(&roundtrip) != crate::chatgpt_history_sync::sha256_hex(&raw) {
                            return Err("History rollback backup failed verification".into());
                        }
                    }
                    std::fs::rename(&packed, &backup).map_err(|e| e.to_string())
                })();
                let _ = std::fs::remove_file(&snap);
                let _ = std::fs::remove_file(&packed);
                result?;
            }
            // 私有客户端已停止且 sqlite_home 已切换。
            for suffix in ["-wal", "-shm", ""] {
                let item = PathBuf::from(format!("{}{}", path.to_string_lossy(), suffix));
                ordinary(&item)?;
                if item.exists() {
                    let size = std::fs::metadata(&item).map_err(|e| e.to_string())?.len();
                    std::fs::remove_file(&item).map_err(|e| e.to_string())?;
                    self.report["released_bytes"] = json!(self.report["released_bytes"].as_i64().unwrap_or(0) + size as i64);
                }
            }
        }
        Ok(())
    }

    fn cleanup_stages(&mut self, home: &Path, value: &mut Value) -> Result<(), String> {
        let stages: Vec<(String, String)> = value.get("stages").and_then(|v| v.as_object())
            .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_default();
        for (name, stage_name) in stages {
            let stage = home.join(&stage_name);
            ordinary(&stage)?;
            // 先校验最终的绝对递归删除目标。
            let parent_ok = stage.parent().and_then(|p| std::fs::canonicalize(p).ok())
                .zip(std::fs::canonicalize(home).ok())
                .map(|(a, b)| key(&a.to_string_lossy()) == key(&b.to_string_lossy()))
                .unwrap_or(false);
            if !parent_ok || !stage_name.starts_with(".history-shared-stage-") {
                return Err("Invalid history migration cleanup path".into());
            }
            if !stage.exists() { continue; }
            let files = private_files(&stage)?;
            for (path, directory) in &files {
                ordinary(path)?;
                if *directory { continue; }
                let relative = path.strip_prefix(&stage).map_err(|e| e.to_string())?;
                let shared = self.canonical.join(&name).join(relative);
                if !shared.is_file() {
                    return Err("A private file has not been preserved".into());
                }
                let a = std::fs::read(path).map_err(|e| e.to_string())?;
                let b = std::fs::read(&shared).map_err(|e| e.to_string())?;
                if a != b && (path.extension().and_then(|s| s.to_str()) != Some("jsonl")
                    || replace_offset(&snapshot(path)?, &snapshot(&shared)?).is_none()) {
                    return Err("A private continuation cannot be discarded".into());
                }
            }
            let released: u64 = files.iter().filter(|(_, d)| !d)
                .filter_map(|(p, _)| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
            remove_tree(&stage)?;
            self.report["released_bytes"] = json!(self.report["released_bytes"].as_i64().unwrap_or(0) + released as i64);
        }
        value["stages"] = json!({});
        atomic_json(&home.join(MARKER), value)
    }
}

/// 与 shutil.rmtree 对齐的递归删除：链接目录只删链接本身不深入（private_files 同口径）。
fn remove_tree(root: &Path) -> Result<(), String> {
    for (path, directory) in private_files(root)? {
        if directory {
            if crate::registry::is_reparse_point(&path) {
                let _ = std::fs::remove_dir(&path);
            }
        } else {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
    }
    // 自底向上删空目录。
    fn prune(dir: &Path) -> Result<(), String> {
        if crate::registry::is_reparse_point(dir) { return std::fs::remove_dir(dir).map_err(|e| e.to_string()); }
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if std::fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(false) { prune(&p)?; }
            }
        }
        std::fs::remove_dir(dir).map_err(|e| e.to_string())
    }
    prune(root)
}

/// 中断恢复检查用的 glob 口径：state_*.sqlite / thread_history_*.sqlite（前缀+后缀，不限数字）。
fn glob_database(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix) && name.ends_with(".sqlite") && name.len() > prefix.len() + ".sqlite".len()
}

impl SharedHistory {
    fn migrate(&mut self, home_in: &Path) -> Result<(), String> {
        let home = ordinary(home_in)?;
        if home == self.canonical { return Ok(()); }
        std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        let previous = self.marker(&home)?;
        if let Some(mut previous) = previous {
            self.verify_links(&home)?;
            // 中断恢复保守：旧库与 staged rollout 保留供人工审计。
            let leftover = std::fs::read_dir(&home).map_err(|e| e.to_string())?
                .filter_map(|e| e.ok())
                .any(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    glob_database(&n, "state_") || glob_database(&n, "thread_history_")
                });
            let has_stages = previous.get("stages").and_then(|v| v.as_object()).map(|m| !m.is_empty()).unwrap_or(false);
            if leftover || has_stages {
                return Err("Interrupted migration needs history verification; previous files retained".into());
            }
            self.ensure_lock_directory(&home)?;
            self.retire_databases(&home)?;
            self.cleanup_stages(&home, &mut previous)?;
            return Ok(());
        }
        let (original, updated) = config_bytes(&home.join("config.toml"), &self.canonical)?;
        // 两份物理副本并存时对账全部遗留内容。
        if home.join("state_5.sqlite").exists() && self.canonical.join("state_5.sqlite").exists() {
            let merged = self.rep.run(&[self.canonical.clone(), home.clone()])?;
            if merged["conflicts"].as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                return Err("Conflicting history remains in this account".into());
            }
            if merged["deferred"].as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                return Err("History is still being written or could not be merged; retry later".into());
            }
            self.merge_projects(&home)?;
            self.check_database_union(&home)?;
        }
        self.merge_files(&home)?;
        self.initialize_preferences(&home)?;
        let mut stages: serde_json::Map<String, Value> = serde_json::Map::new();
        let mut linked: Vec<&str> = Vec::new();
        let attempt = (|| -> Result<(), String> {
            for name in DIRECTORIES {
                let source = home.join(name);
                if source.exists() {
                    ordinary(&source)?;
                    let stage_name = format!(".history-shared-stage-{}-{}", name, uuid_hex());
                    std::fs::rename(&source, home.join(&stage_name)).map_err(|e| e.to_string())?;
                    stages.insert(name.to_string(), json!(stage_name));
                }
                link_directory(&source, &self.canonical.join(name))?;
                linked.push(name);
                if !exact_link(&source, &self.canonical.join(name)) {
                    return Err("Shared directory link verification failed".into());
                }
            }
            if original != updated {
                let before = home.join(".shared-history-config.previous.toml");
                if !before.exists() { std::fs::write(&before, &original).map_err(|e| e.to_string())?; }
                write_bytes(&home.join("config.toml"), &updated)?;
            }
            self.verify_links(&home)?;
            self.ensure_lock_directory(&home)?;
            Ok(())
        })();
        if let Err(error) = attempt {
            for name in linked.iter().rev() {
                let link = home.join(name);
                if exact_link(&link, &self.canonical.join(name)) {
                    let _ = std::fs::remove_dir(&link);
                }
            }
            for (name, stage) in &stages {
                if !home.join(name).exists() {
                    if let Some(s) = stage.as_str() {
                        let _ = std::fs::rename(home.join(s), home.join(name));
                    }
                }
            }
            if original != updated {
                let _ = write_bytes(&home.join("config.toml"), &original);
            }
            return Err(error);
        }
        let mut value = json!({"version": 1, "canonical": self.canonical.to_string_lossy(),
            "migrated_at": now_secs(), "stages": Value::Object(stages)});
        atomic_json(&home.join(MARKER), &value)?;
        self.normalize_paths(&[home.clone(), self.canonical.clone()])?;
        self.retire_databases(&home)?;
        self.cleanup_stages(&home, &mut value)?;
        if let Some(a) = self.report["migrated"].as_array_mut() {
            a.push(json!(home.to_string_lossy()));
        }
        Ok(())
    }

    pub(crate) fn run(&mut self, homes_in: &[PathBuf], busy: &[PathBuf]) -> Result<Value, String> {
        let mut homes: Vec<PathBuf> = Vec::new();
        for p in homes_in {
            let p = ordinary(p)?;
            if !homes.contains(&p) { homes.push(p); }
        }
        if !homes.contains(&self.canonical) { homes.insert(0, self.canonical.clone()); }
        for a in &homes {
            for b in &homes {
                if a != b && (b.starts_with(a) || a.starts_with(b)) {
                    return Err("History homes overlap".into());
                }
            }
        }
        let busy_keys: Vec<String> = busy.iter().map(|h| key(&h.to_string_lossy())).collect();
        for home in &homes {
            if *home == self.canonical { continue; }
            if busy_keys.contains(&key(&home.to_string_lossy())) {
                let item = match self.marker(home) {
                    Ok(Some(_)) => {
                        match self.verify_links(home) {
                            Ok(()) => {
                                if !exact_link(&home.join(LOCK_DIRECTORY), &self.canonical.join(LOCK_DIRECTORY)) {
                                    Some(json!({"home": home.to_string_lossy(), "reason": "Close the account window to enable the shared writer lock"}))
                                } else { None }
                            }
                            Err(e) => Some(json!({"home": home.to_string_lossy(), "reason": e})),
                        }
                    }
                    Ok(None) => Some(json!({"home": home.to_string_lossy(), "reason": "Account window is running"})),
                    Err(e) => Some(json!({"home": home.to_string_lossy(), "reason": e})),
                };
                if let Some(item) = item {
                    if let Some(a) = self.report["pending"].as_array_mut() { a.push(item); }
                }
                continue;
            }
            if let Err(error) = self.migrate(home) {
                if let Some(a) = self.report["pending"].as_array_mut() {
                    a.push(json!({"home": home.to_string_lossy(), "reason": error}));
                }
            }
        }
        self.normalize_paths(&homes)?;
        // 只重建每窗口的小侧栏缓存；迁移后绝不复制消息正文、SQLite 库或会话文件。
        let db = self.canonical.join("state_5.sqlite");
        if db.is_file() && !self.rust_catalog {
            let threads: HashMap<String, Value> = {
                let conn = connect(&db, false)?;
                rows(&conn, "threads", None)?.into_iter()
                    .filter_map(|r| { let id = r.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()); id.map(|id| (id, r)) })
                    .collect()
            };
            // 与 python rep.backup = lambda: None 对齐：目录重建不做整库备份。
            let mut cat = Replicator::new(&self.state_dir)?;
            cat.no_backup = true;
            for home in &homes {
                if *home != self.canonical && self.marker(home)?.is_none() { continue; }
                cat.clean_internal_catalog(home, &threads)?;
                for row in threads.values() { cat.catalog(home, row)?; }
            }
        }
        self.report["homes"] = json!(homes.len());
        self.report["updated_at"] = json!(now_secs());
        atomic_json(&self.state_dir.join("shared.json"),
            &json!({"version": 2, "canonical": self.canonical.to_string_lossy(),
                "homes": homes.iter().map(|h| h.to_string_lossy()).collect::<Vec<_>>()}))?;
        atomic_json(&self.state_dir.join("status.json"), &self.report)?;
        Ok(self.report.clone())
    }
}

/// 与 python synchronize 对齐：sync.lock 串行化整个共享迁移/对账过程。
pub fn synchronize(canonical: &Path, homes: &[PathBuf], busy: &[PathBuf], state_dir: &Path, rust_catalog: bool) -> Result<Value, String> {
    let state_dir = ordinary(state_dir)?;
    std::fs::create_dir_all(&state_dir).map_err(|e| e.to_string())?;
    let _lock = filelock::Lock::acquire(&state_dir.join("sync.lock"))?;
    let mut shared = SharedHistory::new(canonical, &state_dir, rust_catalog)?;
    shared.run(homes, busy)
}

/// 生产入口：request {canonical, homes, busy_homes, state_dir, rust_catalog} → report。
pub fn run(request: &Value) -> Result<Value, String> {
    let canonical = PathBuf::from(request.get("canonical").and_then(|v| v.as_str()).ok_or("canonical required")?);
    let state_dir = PathBuf::from(request.get("state_dir").and_then(|v| v.as_str()).ok_or("state_dir required")?);
    let paths = |k: &str| -> Vec<PathBuf> {
        request.get(k).and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(PathBuf::from)).collect())
            .unwrap_or_default()
    };
    synchronize(&canonical, &paths("homes"), &paths("busy_homes"), &state_dir,
        request.get("rust_catalog").and_then(|v| v.as_bool()).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rewrite_preserves_other_keys() {
        let dir = std::env::temp_dir().join(format!("t18-shared-cfg-{}", uuid_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        let canon = Path::new(r"C:\shared\home");
        std::fs::write(&cfg, "model = \"gpt\"\nsqlite_home = 'C:\\old'\n\n[profiles.a]\nmodel = \"m\"\n").unwrap();
        let (original, updated) = config_bytes(&cfg, canon).unwrap();
        assert_ne!(original, updated);
        let text = String::from_utf8(updated).unwrap();
        assert!(text.starts_with("sqlite_home = \"C:\\\\shared\\\\home\"\n"));
        assert!(!text.contains("C:\\old"));
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed.get("model").and_then(|v| v.as_str()), Some("gpt"));
        assert_eq!(parsed["profiles"]["a"]["model"].as_str(), Some("m"));
        // 幂等：已一致 → 原样返回。
        std::fs::write(&cfg, config_bytes(&cfg, canon).unwrap().1).unwrap();
        let (a, b) = config_bytes(&cfg, canon).unwrap();
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn database_name_patterns() {
        assert!(retire_database_name("state_5.sqlite"));
        assert!(retire_database_name("thread_history_12.sqlite"));
        assert!(!retire_database_name("state_x.sqlite"));
        assert!(!retire_database_name("state_5.sqlite-wal"));
        assert!(!retire_database_name("codex-dev.db"));
        assert!(glob_database("state_any.sqlite", "state_"));
        assert!(!glob_database("state_.sqlite", "state_"));
        assert!(sqlite_home_line("  sqlite_home = 'x'"));
        assert!(sqlite_home_line("\"sqlite_home\"=\"x\""));
        assert!(!sqlite_home_line("other = 1"));
        assert!(!sqlite_home_line("sqlite_home_suffix = 1"));
    }

    /// T18 golden：与 python chatgpt-history-shared.py synchronize 输出比对
    /// （迁移后双侧库/文件内容由 python 驱动逐字节比对）。
    /// 由 tests/chatgpt-history-shared-rust-golden.test.py 触发；未设环境变量时跳过。
    #[test]
    fn golden_matches_python() {
        let Ok(req_path) = std::env::var("T18_SHARED_REQ") else {
            eprintln!("跳过 golden：未设 T18_SHARED_REQ");
            return;
        };
        let request: Value = serde_json::from_str(&std::fs::read_to_string(&req_path).unwrap()).unwrap();
        let py: Value = serde_json::from_str(&std::fs::read_to_string(request["py_report"].as_str().unwrap()).unwrap()).unwrap();
        let canonical = PathBuf::from(request["canonical"].as_str().unwrap());
        let state_dir = PathBuf::from(request["state_dir"].as_str().unwrap());
        let homes: Vec<PathBuf> = request["homes"].as_array().unwrap().iter()
            .map(|v| PathBuf::from(v.as_str().unwrap())).collect();
        let busy: Vec<PathBuf> = request.get("busy_homes").and_then(|v| v.as_array())
            .map(|a| a.iter().map(|v| PathBuf::from(v.as_str().unwrap())).collect()).unwrap_or_default();
        let rs = synchronize(&canonical, &homes, &busy, &state_dir, false).expect("rust synchronize");
        // 报告：去时间戳 + 双侧基目录归一后逐字段一致。
        let base_py = request["py_base"].as_str().unwrap();
        let base_rs = request["rs_base"].as_str().unwrap();
        fn walk(v: &mut Value, base: &str) {
            match v {
                Value::String(s) => { *s = s.replace(base, "<BASE>"); }
                Value::Object(m) => { for (k, x) in m.iter_mut() { if k != "migrated_at" { walk(x, base); } } }
                Value::Array(a) => { for x in a.iter_mut() { walk(x, base); } }
                _ => {}
            }
        }
        let norm = |mut v: Value, base: &str| -> Value {
            if let Some(o) = v.as_object_mut() { o.remove("updated_at"); }
            walk(&mut v, base);
            v
        };
        assert_eq!(norm(rs.clone(), base_rs), norm(py.clone(), base_py), "report mismatch:\nrs={rs}\npy={py}");
    }
}