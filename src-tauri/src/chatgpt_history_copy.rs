//! T18 第五批：chatgpt-history.py 的 copy_history 迁 Rust
//! （与 python copy_history 逐条对齐；repair_projects 已在第三批迁 chatgpt_history_repair）。
//!
//! 把本机 Codex 历史复制进空的私有 CODEX_HOME：SQLite 在线备份含已提交 WAL 页；
//! rollout 保持字节复制；只有复制出的库/索引/项目元数据做路径与 local host 重定位。
//! auth/config/浏览器档案绝不作为导入输入。
//!
//! 安全规则（与 python 版一致）：
//! 1. 路径绝对、无 ..、不含链接（ordinary）；source/target 不得重叠。
//! 2. 目标已有真实历史 → preserved_existing，绝不动目标。
//! 3. staging 独占发布：目标文件只在 initialized 指纹集合内才备份替换；
//!    发布中途出现新文件 → 报错回滚；回滚失败保留 staging 供人工恢复。
//! 4. 快照期间源文件变化 → 重试 3 次后报错；180s 预算超时保留源历史。

use serde_json::{json, Value};
use rusqlite::OptionalExtension;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::chatgpt_history_repair::{key, ordinary, project_visibility_state, read_state, uuid_hex};

const STATE_FILE: &str = ".codex-global-state.json";
const MARKER_FILE: &str = ".agentcockpit-history.json";
const ROLLOUT_DIRS: [&str; 2] = ["sessions", "archived_sessions"];
const INDEX_FILES: [&str; 2] = ["session_index.jsonl", "history.jsonl"];
const VISIBILITY_REPAIR_VERSION: i64 = 2;

const STATE_KEYS: &[&str] = &[
    "active-workspace-roots", "saved-workspace-roots", "workspace-root-order",
    "workspace-root-labels", "electron-saved-workspace-roots", "local-projects",
    "pinned-project-ids", "project-order", "project-appearances", "selected-project",
    "sidebar-project-thread-orders", "thread-project-assignments",
    "thread-project-membership-host-ids", "projectless-thread-ids",
    "pinned-thread-ids", "thread-titles", "thread-workspace-root-hints",
    "thread-writable-roots", "thread-projectless-output-directories",
    "electron-thread-read-state-v1", "app-server-projects-migration-by-host",
    "app-server-project-id-by-legacy-project-id-by-host",
    "app-server-migrated-pinned-thread-ids-by-host",
    "app-server-pending-project-deletions-by-host",
    "electron-local-data-migration-id", "electron-completed-local-data-migration-ids",
    "remote-project-connection-backfill-completed",
    "electron-workspace-root-labels", "project-writable-roots", "project-files",
    "hidden-project-ids", "archived-project-ids", "deleted-project-ids",
];
const ATOM_KEYS: &[&str] = &[
    "sidebar-custom-sections-v3", "sidebar-collapsed-sections-v1",
    "flat-project-sidebar-preferences-v1", "sidebar-project-list-expanded-v1",
    "thread-descriptions-v1", "unified-sidebar-project-order-v1",
];
const SECRET_FIELDS: &[&str] = &[
    "auth", "authentication", "authorization", "config", "credentials", "cookies",
    "apikey", "accesstoken", "refreshtoken", "idtoken", "token", "password", "secret",
];
const HOST_COLUMNS: &[&str] = &["host_id", "local_host_id", "source_host_id"];
const PATH_COLUMNS: &[&str] = &[
    "rollout_path", "rollout_file_path", "path", "cwd", "working_directory",
    "workspace_root", "root_path", "codex_home", "home_path",
];
const RELOCATED_COLUMNS: &[&str] = &["rollout_path", "rollout_file_path", "codex_home", "home_path"];
const HISTORY_TABLES: &[&str] = &[
    "threads", "thread_items", "projects", "sessions", "messages", "conversations",
    "thread_history", "thread_catalog", "local_thread_catalog", "thread_timeline_ledger",
    "thread_turns", "thread_realtime_items", "thread_history_projection_state",
];
const REAL_STATE_KEYS: &[&str] = &[
    "local-projects", "pinned-project-ids", "project-order", "pinned-thread-ids",
    "projectless-thread-ids", "thread-titles", "sidebar-project-thread-orders",
    "thread-project-assignments", "thread-project-membership-host-ids",
];
const CATALOG_TABLES: &[&str] = &[
    "local_thread_catalog", "local_thread_catalog_hosts", "local_thread_catalog_metadata",
    "local_thread_catalog_sync_state", "local_thread_catalog_scan_checkpoints",
    "local_thread_catalog_scan_entries", "thread_timeline_ledger", "codex_schema_migrations",
];
const TASK_TABLES: &[&str] = &[
    "agent_jobs", "agent_job_items", "agent_job_states", "automations",
    "automation_runs", "background_jobs", "background_job_queue", "task_queue",
];

/// JSON → python json.dumps(ensure_ascii=False) 字节口径：紧凑冒号 + ", " 项分隔 + 非 ASCII 原样。
/// serde_json 默认无空格且键排序，与 python 插入序不同；state/索引/marker 落盘必须用本函数。
pub(crate) fn py_dumps(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let items: Vec<String> = map.iter()
                .map(|(k, x)| format!("{}: {}", py_dumps(&json!(k)), py_dumps(x))).collect();
            format!("{{{}}}", items.join(", "))
        }
        Value::Array(arr) => format!("[{}]", arr.iter().map(py_dumps).collect::<Vec<_>>().join(", ")),
        Value::String(s) => py_json_string(s),
        other => other.to_string(),
    }
}

/// python json.dumps 字符串转义口径：只转义 " \ 与控制字符（<0x20），非 ASCII 原样。
fn py_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn is_link(path: &Path) -> bool {
    crate::registry::is_reparse_point(path)
}

/// 与 python DATABASE_NAME 正则对齐（state_N/thread_history_N/thread_catalog[_N].sqlite，忽略大小写）。
fn is_database_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    let Some(stem) = lower.strip_suffix(".sqlite") else { return false };
    if let Some(num) = stem.strip_prefix("state_") {
        return !num.is_empty() && num.chars().all(|c| c.is_ascii_digit());
    }
    if let Some(num) = stem.strip_prefix("thread_history_") {
        return !num.is_empty() && num.chars().all(|c| c.is_ascii_digit());
    }
    if stem == "thread_catalog" { return true; }
    if let Some(num) = stem.strip_prefix("thread_catalog_") {
        return !num.is_empty() && num.chars().all(|c| c.is_ascii_digit());
    }
    false
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// 与 python readonly 对齐：ordinary + wal/shm 校验 + mode=ro + query_only + 10s busy。
fn readonly(path: &Path) -> Result<rusqlite::Connection, String> {
    ordinary(path)?;
    for suffix in ["-wal", "-shm"] {
        ordinary(&PathBuf::from(format!("{}{}", path.display(), suffix)))?;
    }
    let conn = rusqlite::Connection::open_with_flags(
        path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI)
        .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(10)).ok();
    conn.pragma_update(None, "query_only", "ON").ok();
    Ok(conn)
}

fn tables(conn: &rusqlite::Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'").map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>, String> {
    let sql = format!("PRAGMA table_info({})", quote(table));
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// 与 python walk_files 对齐：不跟链接，目录逐个 ordinary 校验，只产出文件。
fn walk_files(base: &Path) -> Result<Vec<PathBuf>, String> {
    ordinary(base)?;
    let mut out = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                ordinary(&p)?;
                if !is_link(&p) { stack.push(p); }
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 与 python database_paths 对齐：根目录匹配名 + sqlite/ 子树（含 codex-dev.db），排序 + 校验。
fn database_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if is_database_name(&name) { found.push(p); }
        }
    }
    let nested = root.join("sqlite");
    if nested.exists() || is_link(&nested) {
        ordinary(&nested)?;
        if !nested.is_dir() { return Err("History SQLite directory is not a directory".into()); }
        for p in walk_files(&nested)? {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if is_database_name(&name) || p == nested.join("codex-dev.db") { found.push(p); }
        }
    }
    found.sort();
    for p in &found {
        ordinary(p)?;
        if !p.is_file() { return Err("History database is not a regular file".into()); }
    }
    Ok(found)
}

fn state_version(name: &str) -> Option<u64> {
    let lower = name.to_lowercase();
    lower.strip_prefix("state_").and_then(|s| s.strip_suffix(".sqlite"))
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
}

/// 与 python history_count 对齐：所有库 threads 最大值；根目录最高版 state_N 优先。
fn history_count(root: &Path) -> Result<i64, String> {
    let mut count = 0i64;
    let mut primary: Option<(u64, i64)> = None;
    for path in database_paths(root)? {
        let conn = readonly(&path)?;
        if tables(&conn)?.iter().any(|t| t == "threads") {
            let current: i64 = conn.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0)).map_err(|e| e.to_string())?;
            count = count.max(current);
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if path.parent() == Some(root) {
                if let Some(v) = state_version(&name) {
                    if primary.as_ref().map(|(pv, _)| v > *pv).unwrap_or(true) { primary = Some((v, current)); }
                }
            }
        }
    }
    Ok(primary.map(|(_, c)| c).unwrap_or(count))
}

/// 与 python existing_history 对齐：真实记录（库行/索引非空行/真实 state 键/marker 计数/rollout 文件）才算已有历史。
fn existing_history(root: &Path) -> Result<bool, String> {
    for database in database_paths(root)? {
        let conn = readonly(&database)?;
        for table in tables(&conn)? {
            if HISTORY_TABLES.contains(&table.as_str()) {
                let sql = format!("SELECT 1 FROM {} LIMIT 1", quote(&table));
                let found = conn.query_row(&sql, [], |_| Ok(())).optional().map_err(|e| e.to_string())?.is_some();
                if found { return Ok(true); }
            }
        }
    }
    for name in INDEX_FILES {
        let path = root.join(name);
        if path.exists() || is_link(&path) {
            ordinary(&path)?;
            if !path.is_file() { return Ok(true); }
            let text = read_text_sig(&path)?;
            if text.lines().any(|l| !l.trim().is_empty()) { return Ok(true); }
        }
    }
    let state_file = root.join(STATE_FILE);
    if state_file.exists() || is_link(&state_file) {
        let state = read_state(&state_file)?;
        if REAL_STATE_KEYS.iter().any(|k| truthy(state.get(*k))) { return Ok(true); }
    }
    let marker = root.join(MARKER_FILE);
    if marker.exists() || is_link(&marker) {
        let value = read_state(&marker)?;
        if ["threads", "projects", "rollouts"].iter().any(|k| truthy(value.get(*k))) { return Ok(true); }
    }
    for name in ROLLOUT_DIRS {
        let base = root.join(name);
        if base.exists() || is_link(&base) {
            ordinary(&base)?;
            if !base.is_dir() { return Ok(true); }
            if !walk_files(&base)?.is_empty() { return Ok(true); }
        }
    }
    Ok(false)
}

/// python 真值口径：null/false/0/""/[]/{} 为假。
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn read_text_sig(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).to_string())
}

/// 与 python initialized_files 对齐：目标初始化文件（库+wal+索引+state+marker）的 sha256 指纹。
fn initialized_files(root: &Path) -> Result<HashMap<PathBuf, String>, String> {
    let mut files = database_paths(root)?;
    for database in files.clone() {
        let wal = PathBuf::from(format!("{}-wal", database.display()));
        if wal.exists() || is_link(&wal) {
            ordinary(&wal)?;
            files.push(wal);
        }
    }
    for name in INDEX_FILES.iter().chain([STATE_FILE, MARKER_FILE].iter()) {
        let path = root.join(name);
        if path.exists() || is_link(&path) {
            ordinary(&path)?;
            files.push(path);
        }
    }
    let mut fingerprints = HashMap::new();
    for path in files {
        ordinary(&path)?;
        let data = std::fs::read(&path).map_err(|e| e.to_string())?;
        let digest = sha256_hex(&data);
        let rel = path.strip_prefix(root).map_err(|e| e.to_string())?.to_path_buf();
        fingerprints.insert(rel, digest);
    }
    Ok(fingerprints)
}

/// 与 python relative_home 对齐：识别两种斜杠拼写的 CODEX_HOME 前缀，返回 home 内相对路径。
/// 相等→"."；非前缀→None；含 .. 或绝对→报错。
fn relative_home(value: &str, source: &Path) -> Result<Option<PathBuf>, String> {
    let mut old = value.replace('\\', "/");
    let mut home = source.to_string_lossy().replace('\\', "/");
    while home.ends_with('/') { home.pop(); }
    if let Some(s) = old.strip_prefix("//?/") { old = s.to_string(); }
    if let Some(s) = home.strip_prefix("//?/") { home = s.to_string(); }
    // python：有盘符或 nt 才 casefold；本产品生产路径均 Windows，与 key() 同口径 lowercase。
    let (old_c, home_c) = (old.to_lowercase(), home.to_lowercase());
    if old_c == home_c { return Ok(Some(PathBuf::from("."))); }
    if !old_c.starts_with(&format!("{}/", home_c)) { return Ok(None); }
    let rest = &old[home.len() + 1..];
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    if parts.iter().any(|p| *p == "..") || rest.starts_with('/') {
        return Err("A history path traverses outside its home".into());
    }
    let mut rel = PathBuf::new();
    for p in parts { rel.push(p); }
    Ok(Some(rel))
}

/// 与 python make_relocator 对齐：text 处理 local: 前缀与 RELOCATED_COLUMNS 路径；
/// relocate 递归处理 dict（键也过 text）/list/str。
pub(crate) struct Relocator { pub(crate) source: PathBuf, pub(crate) target: PathBuf }

impl Relocator {
    fn text(&self, value: &str, relocate_path: bool) -> Result<String, String> {
        if let Some(rest) = value.strip_prefix("local:") {
            return Ok(if key(rest) == key(&self.source.to_string_lossy()) {
                format!("local:{}", self.target.display())
            } else { value.to_string() });
        }
        if relocate_path {
            if let Some(rel) = relative_home(value, &self.source)? {
                return Ok(self.target.join(rel).to_string_lossy().to_string());
            }
        }
        Ok(value.to_string())
    }

    pub(crate) fn relocate(&self, value: &Value, field: &str) -> Result<Value, String> {
        match value {
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, v) in map {
                    out.insert(self.text(k, false)?, self.relocate(v, k)?);
                }
                Ok(Value::Object(out))
            }
            Value::Array(arr) => Ok(Value::Array(arr.iter().map(|v| self.relocate(v, field)).collect::<Result<_, _>>()?)),
            Value::String(s) => Ok(json!(self.text(s, RELOCATED_COLUMNS.contains(&field))?)),
            other => Ok(other.clone()),
        }
    }
}

/// 与 python history_state 对齐：STATE_KEYS 白名单 + visibility 剪枝 + atom 白名单 + 秘密字段剔除 + relocate。
pub(crate) fn history_state(path: &Path, relocator: &Relocator) -> Result<Value, String> {
    let state = read_state(path)?;
    fn scrub(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(map.iter()
                .filter(|(k, _)| {
                    let normalized: String = k.chars().filter(|c| *c != '-' && *c != '_').collect::<String>().to_lowercase();
                    !SECRET_FIELDS.contains(&normalized.as_str())
                })
                .map(|(k, v)| (k.clone(), scrub(v))).collect()),
            Value::Array(arr) => Value::Array(arr.iter().map(scrub).collect()),
            other => other.clone(),
        }
    }
    let mut selected = serde_json::Map::new();
    if let Some(obj) = state.as_object() {
        for k in STATE_KEYS {
            if let Some(v) = obj.get(*k) { selected.insert(k.to_string(), v.clone()); }
        }
    }
    let mut selected = project_visibility_state(&Value::Object(selected));
    if let Some(atoms) = state.get("electron-persisted-atom-state").and_then(|v| v.as_object()) {
        let mut keep = serde_json::Map::new();
        for (k, v) in atoms {
            if ATOM_KEYS.contains(&k.as_str()) || k.starts_with("sidebar-project-expanded-v1-") {
                keep.insert(k.clone(), v.clone());
            }
        }
        if !keep.is_empty() {
            selected.as_object_mut().unwrap().insert("electron-persisted-atom-state".into(), Value::Object(keep));
        }
    }
    relocator.relocate(&scrub(&selected), "")
}

/// 与 python copy_bytes 对齐：按 fstat 尺寸读满 + 前后 (size,mtime_ns) 一致才收；重试 3 次。
fn copy_bytes(src: &Path, dst: &Path, deadline: Instant) -> Result<(), String> {
    ordinary(src)?;
    if let Some(p) = dst.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
    for _ in 0..3 {
        budget(deadline)?;
        let mut origin = std::fs::File::open(src).map_err(|e| e.to_string())?;
        let before = stat_token(&origin)?;
        let mut remaining = before.0;
        let mut output = std::fs::File::create(dst).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 1024 * 1024];
        while remaining > 0 {
            budget(deadline)?;
            let want = (1024 * 1024).min(remaining as usize);
            let n = origin.read(&mut buf[..want]).map_err(|e| e.to_string())?;
            if n == 0 { return Err("A history file changed during its snapshot".into()); }
            output.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            remaining -= n as u64;
        }
        let after = stat_token(&origin)?;
        drop(output);
        if before == after {
            copy_stat(src, dst)?;
            return Ok(());
        }
    }
    Err("A history file is still changing; retry the snapshot".into())
}

fn budget(deadline: Instant) -> Result<(), String> {
    if Instant::now() > deadline {
        return Err("History copy timed out; source history was retained".into());
    }
    Ok(())
}

/// (size, mtime_ns) 快照令牌，与 python fstat 口径一致。
fn stat_token(file: &std::fs::File) -> Result<(u64, u128), String> {
    let meta = file.metadata().map_err(|e| e.to_string())?;
    let mtime = meta.modified().map_err(|e| e.to_string())?
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    Ok((meta.len(), mtime))
}

/// 与 python shutil.copystat(follow_symlinks=False) 对齐：复制 atime/mtime（权限 Windows 语义有限）。
fn copy_stat(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| e.to_string())?;
    let times = std::fs::FileTimes::new();
    let times = if let Ok(a) = meta.accessed() { times.set_accessed(a) } else { times };
    let times = if let Ok(m) = meta.modified() { times.set_modified(m) } else { times };
    let f = std::fs::OpenOptions::new().write(true).open(dst).map_err(|e| e.to_string())?;
    f.set_times(times).map_err(|e| e.to_string())?;
    Ok(())
}

/// 与 python _publish_file 对齐：Windows rename 拒绝已存在目标；POSIX 用独占 hard link。
fn publish_file(source: &Path, destination: &Path) -> Result<(), String> {
    #[cfg(windows)]
    { std::fs::rename(source, destination).map_err(|e| e.to_string()) }
    #[cfg(not(windows))]
    {
        std::fs::hard_link(source, destination).map_err(|e| e.to_string())?;
        std::fs::remove_file(source).map_err(|e| e.to_string())
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).iter().map(|b| format!("{:02x}", b)).collect()
}/// 与 python copy_history 对齐的完整复制流程。返回 report（不含 ok，由 run 包装）。
pub fn copy_history(source: &Path, target: &Path) -> Result<Value, String> {
    let source = ordinary(source)?;
    let target = ordinary(target)?;
    let source = std::fs::canonicalize(&source).map_err(|e| e.to_string())?;
    let target = std::fs::canonicalize(&target).map_err(|e| e.to_string())?;
    let source = crate::registry::strip_verbatim(&source);
    let target = crate::registry::strip_verbatim(&target);
    if source == target || target.starts_with(&source) || source.starts_with(&target) {
        return Err("Source and target history directories overlap".into());
    }
    if !source.is_dir() || !target.is_dir() { return Err("History directory is missing".into()); }
    if existing_history(&target)? {
        return Ok(json!({"threads": history_count(&target)?, "preserved_existing": true}));
    }
    let initialized = initialized_files(&target)?;
    let stage = target.join(format!(".history-stage-{}", uuid_hex()));
    std::fs::create_dir(&stage).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let backup = target.join(format!(".history-before-copy-{}", uuid_hex()));
    let mut published: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut created_directories: Vec<PathBuf> = Vec::new();
    let mut backed_up: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut retain_stage = false;

    let result = copy_inner(&source, &target, &stage, &backup, deadline, &initialized,
                            &mut published, &mut created_directories, &mut backed_up);
    match result {
        Ok(v) => {
            cleanup_stage(&stage, &target, &backup, false);
            Ok(v)
        }
        Err(error) => {
            // 与 python 回滚对齐：published 逆序搬回 stage、backed_up 逆序还原、新建目录逆序删。
            let mut rollback_errors: Vec<String> = Vec::new();
            for (dest, item) in published.iter().rev() {
                let r = (|| -> Result<(), String> {
                    ordinary(dest)?;
                    if dest.exists() { std::fs::rename(dest, item).map_err(|e| e.to_string())?; }
                    Ok(())
                })();
                if let Err(e) = r { rollback_errors.push(e); }
            }
            for (dest, old) in backed_up.iter().rev() {
                let r = (|| -> Result<(), String> {
                    ordinary(dest)?;
                    std::fs::rename(old, dest).map_err(|e| e.to_string())?;
                    Ok(())
                })();
                if let Err(e) = r { rollback_errors.push(e); }
            }
            for directory in created_directories.iter().rev() {
                if let Err(e) = std::fs::remove_dir(directory) { rollback_errors.push(e.to_string()); }
            }
            if !rollback_errors.is_empty() {
                retain_stage = true;
                cleanup_stage(&stage, &target, &backup, retain_stage);
                return Err(format!("History publication stopped; staging was retained for recovery: {} ({error})", stage.display()));
            }
            cleanup_stage(&stage, &target, &backup, retain_stage);
            Err(error)
        }
    }
}

/// 只删本次调用的独占 staging；回滚失败保留。backup 只清空目录（成功替换的原文件保留）。
fn cleanup_stage(stage: &Path, target: &Path, backup: &Path, retain: bool) {
    if retain { return; }
    if stage.exists() && stage.parent() == Some(target)
        && stage.file_name().map(|n| n.to_string_lossy().starts_with(".history-stage-")).unwrap_or(false)
        && ordinary(stage).is_ok()
    {
        let _ = std::fs::remove_dir_all(stage);
    }
    if backup.exists() {
        // 自底向上只删空目录；有文件的（成功替换保留的原件）不动。
        let mut dirs = vec![backup.to_path_buf()];
        let mut stack = vec![backup.to_path_buf()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.filter_map(|e| e.ok()) {
                    let p = e.path();
                    if p.is_dir() { dirs.push(p.clone()); stack.push(p); }
                }
            }
        }
        dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for d in dirs { let _ = std::fs::remove_dir(d); }
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_inner(
    source: &Path, target: &Path, stage: &Path, backup: &Path, deadline: Instant,
    initialized: &HashMap<PathBuf, String>,
    published: &mut Vec<(PathBuf, PathBuf)>,
    created_directories: &mut Vec<PathBuf>,
    backed_up: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<Value, String> {
    let relocator = Relocator { source: source.to_path_buf(), target: target.to_path_buf() };
    // 1) SQLite 在线备份（含已提交 WAL 页）。
    for src in database_paths(source)? {
        budget(deadline)?;
        let rel = src.strip_prefix(source).map_err(|e| e.to_string())?;
        let dst = stage.join(rel);
        if let Some(p) = dst.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
        let origin = readonly(&src)?;
        let mut dest = rusqlite::Connection::open(&dst).map_err(|e| e.to_string())?;
        {
            let bk = rusqlite::backup::Backup::new(&origin, &mut dest).map_err(|e| e.to_string())?;
            bk.run_to_completion(256, Duration::from_millis(50), None).map_err(|e| e.to_string())?;
        }
        dest.execute_batch("COMMIT").ok();
    }
    // 2) rollout 字节复制（含压缩档案），记录同名索引。
    let mut copied = 0i64;
    let mut rollout_by_name: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for directory in ROLLOUT_DIRS {
        let base = source.join(directory);
        if !base.exists() && !is_link(&base) { continue; }
        ordinary(&base)?;
        if !base.is_dir() { return Err("History sessions path is not a directory".into()); }
        for src in walk_files(&base)? {
            budget(deadline)?;
            let name = src.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !name.ends_with(".jsonl") && !name.ends_with(".jsonl.zst") && !name.ends_with(".jsonl.gz") { continue; }
            let rel = src.strip_prefix(source).map_err(|e| e.to_string())?.to_path_buf();
            copy_bytes(&src, &stage.join(&rel), deadline)?;
            rollout_by_name.entry(name).or_default().push(rel);
            copied += 1;
        }
    }
    // 3) 压缩 rollout 还原（gz/zstd），已有同名明文则跳过。
    let mut restored_compressed = 0i64;
    for directory in ROLLOUT_DIRS {
        let base = stage.join(directory);
        if !base.is_dir() { continue; }
        for compressed in walk_files(&base)? {
            let name = compressed.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            let is_gz = name.ends_with(".jsonl.gz");
            let is_zst = name.ends_with(".jsonl.zst");
            if !is_gz && !is_zst { continue; }
            let plain = compressed.with_extension("");
            if plain.exists() { continue; }
            budget(deadline)?;
            let data = std::fs::read(&compressed).map_err(|e| e.to_string())?;
            let decoded: Vec<u8> = if is_gz {
                let mut decoder = flate2::read::GzDecoder::new(&data[..]);
                let mut out = Vec::new();
                decoder.read_to_end(&mut out).map_err(|e| e.to_string())?;
                out
            } else {
                zstd::stream::decode_all(&data[..]).map_err(|e| e.to_string())?
            };
            let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(&plain).map_err(|e| e.to_string())?;
            output.write_all(&decoded).map_err(|e| e.to_string())?;
            let plain_name = plain.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            let rel = plain.strip_prefix(stage).map_err(|e| e.to_string())?.to_path_buf();
            rollout_by_name.entry(plain_name).or_default().push(rel);
            restored_compressed += 1;
        }
    }
    // 4) 索引文件复制 + HOST/PATH 列重定位重写。
    for name in INDEX_FILES {
        let src = source.join(name);
        if src.exists() || is_link(&src) {
            ordinary(&src)?;
            let copied_index = stage.join(name);
            copy_bytes(&src, &copied_index, deadline)?;
            let text = read_text_sig(&copied_index)?;
            let mut out = String::new();
            for line in text.lines() {
                budget(deadline)?;
                if line.trim().is_empty() { continue; }
                let mut row: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
                if let Some(obj) = row.as_object_mut() {
                    for (k, v) in obj.iter_mut() {
                        let is_host = HOST_COLUMNS.contains(&k.as_str());
                        let is_path = PATH_COLUMNS.contains(&k.as_str());
                        if (is_host || is_path) && v.is_string() {
                            *v = json!(relocator.text(v.as_str().unwrap(), RELOCATED_COLUMNS.contains(&k.as_str()))?);
                        }
                    }
                }
                out.push_str(&py_dumps(&row));
                out.push('\n');
            }
            let rewritten = stage.join(format!("{}.rewritten", name));
            std::fs::write(&rewritten, out).map_err(|e| e.to_string())?;
            std::fs::rename(&rewritten, &copied_index).map_err(|e| e.to_string())?;
        }
    }
    // 5) 全局 state 白名单合并（既有非空值优先）。
    let state_path = source.join(STATE_FILE);
    if state_path.exists() || is_link(&state_path) {
        let state = history_state(&state_path, &relocator)?;
        if truthy(Some(&state)) {
            let target_state = target.join(STATE_FILE);
            let mut previous = if target_state.exists() { read_state(&target_state)? } else { json!({}) };
            if let (Some(prev), Some(next)) = (previous.as_object_mut(), state.as_object()) {
                for (name, value) in next {
                    if !truthy(prev.get(name)) { prev.insert(name.clone(), value.clone()); }
                }
            }
            std::fs::write(stage.join(STATE_FILE), py_dumps(&previous))
                .map_err(|e| e.to_string())?;
        }
    }
    // 6) 权威库归档名单（保留悬空归档记录，不发明内容也不删索引）。
    let roots: Vec<PathBuf> = database_paths(stage)?.into_iter()
        .filter(|p| p.parent() == Some(stage))
        .filter(|p| state_version(&p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).is_some())
        .collect();
    let authoritative = roots.iter().max_by_key(|p| state_version(&p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).unwrap_or(0)).cloned();
    let mut archived_names: HashSet<String> = HashSet::new();
    let mut missing_archived: HashSet<String> = HashSet::new();
    if let Some(auth) = &authoritative {
        let conn = rusqlite::Connection::open(auth).map_err(|e| e.to_string())?;
        let names = tables(&conn)?;
        if names.iter().any(|t| t == "threads") {
            let cols = columns(&conn, "threads")?;
            if cols.iter().any(|c| c == "archived") && cols.iter().any(|c| c == "rollout_path") {
                let mut stmt = conn.prepare("SELECT rollout_path FROM threads WHERE archived=1 AND rollout_path IS NOT NULL").map_err(|e| e.to_string())?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
                for path in rows.filter_map(|r| r.ok()) {
                    archived_names.insert(basename_normcase(&path));
                }
            }
        }
    }
    let rollout_relative = |old: &str, missing_archived: &mut HashSet<String>| -> Result<PathBuf, String> {
        let rel = relative_home(old, source)?;
        let name = basename_normcase(old);
        let mut rel = rel;
        let exists = rel.as_ref().map(|r| stage.join(r).is_file()).unwrap_or(false);
        if rel.is_none() || !exists {
            let choices = rollout_by_name.get(&name).cloned().unwrap_or_default();
            if choices.is_empty() {
                if let Some(r) = &rel {
                    let first = r.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
                    if archived_names.contains(&name) && ROLLOUT_DIRS.contains(&first.as_str()) {
                        missing_archived.insert(name);
                        return Ok(r.clone());
                    }
                }
                return Err("A thread rollout could not be copied completely".into());
            }
            if choices.len() != 1 { return Err("A thread rollout could not be copied completely".into()); }
            rel = Some(choices[0].clone());
        }
        let r = rel.unwrap();
        let first = r.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
        if r.components().any(|c| matches!(c, std::path::Component::ParentDir))
            || first.is_empty() || !ROLLOUT_DIRS.contains(&first.as_str()) || !stage.join(&r).is_file()
        {
            return Err("A thread rollout is missing from the private copy".into());
        }
        Ok(r)
    };
    // 7) staging 库清洗（任务表/codex-dev 非目录表）+ HOST/PATH/ledger 重定位 + quick_check + DELETE 日志 + VACUUM。
    let mut threads = 0i64;
    let mut projects = 0i64;
    let mut primary_counts: Option<(u64, i64, i64)> = None;
    for path in database_paths(stage)? {
        budget(deadline)?;
        let conn = rusqlite::Connection::open(&path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "trusted_schema", "OFF").ok();
        let names = tables(&conn)?;
        let mut clear: BTreeSet<String> = names.iter().filter(|t| TASK_TABLES.contains(&t.as_str())).cloned().collect();
        let file_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if file_name == "codex-dev.db" {
            for t in &names {
                if !CATALOG_TABLES.contains(&t.as_str()) && !t.starts_with("sqlite_") { clear.insert(t.clone()); }
            }
        }
        if !clear.is_empty() {
            conn.pragma_update(None, "secure_delete", "ON").ok();
            for table in &clear {
                conn.execute_batch(&format!("DELETE FROM {}", quote(table))).map_err(|e| e.to_string())?;
            }
        }
        if names.iter().any(|t| t == "threads") {
            let c: i64 = conn.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0)).map_err(|e| e.to_string())?;
            threads = threads.max(c);
        }
        if names.iter().any(|t| t == "projects") {
            let c: i64 = conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0)).map_err(|e| e.to_string())?;
            projects = projects.max(c);
        }
        if path.parent() == Some(stage) {
            if let Some(v) = state_version(&file_name) {
                if primary_counts.as_ref().map(|(pv, _, _)| v > *pv).unwrap_or(true) {
                    let t: i64 = if names.iter().any(|t| t == "threads") { conn.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0)).map_err(|e| e.to_string())? } else { 0 };
                    let p: i64 = if names.iter().any(|t| t == "projects") { conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0)).map_err(|e| e.to_string())? } else { 0 };
                    primary_counts = Some((v, t, p));
                }
            }
        }
        for table in &names {
            if table.starts_with("sqlite_") { continue; }
            for col in columns(&conn, table)? {
                if table == "thread_timeline_ledger" && col == "payload_json" {
                    let (qt, qc) = (quote(table), quote(&col));
                    let olds: Vec<String> = {
                        let mut stmt = conn.prepare(&format!("SELECT DISTINCT {qc} FROM {qt} WHERE {qc} IS NOT NULL")).map_err(|e| e.to_string())?;
                        let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0)).map_err(|e| e.to_string())?;
                        rows.filter_map(|r| r.ok()).flatten().collect()
                    };
                    for old in olds {
                        let parsed: Value = serde_json::from_str(&old).map_err(|e| e.to_string())?;
                        let new = py_dumps(&relocator.relocate(&parsed, "")?);
                        conn.execute(&format!("UPDATE {qt} SET {qc}=?1 WHERE {qc}=?2"), [&new, &old]).map_err(|e| e.to_string())?;
                    }
                    continue;
                }
                let is_host = HOST_COLUMNS.contains(&col.as_str());
                let is_path = PATH_COLUMNS.contains(&col.as_str());
                if !is_host && !is_path { continue; }
                let (qt, qc) = (quote(table), quote(&col));
                let olds: Vec<String> = {
                    let mut stmt = conn.prepare(&format!("SELECT DISTINCT {qc} FROM {qt} WHERE {qc} IS NOT NULL")).map_err(|e| e.to_string())?;
                    let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0)).map_err(|e| e.to_string())?;
                    rows.filter_map(|r| r.ok()).flatten().collect()
                };
                for old in olds {
                    budget(deadline)?;
                    if old.is_empty() { continue; }
                    let new = if col.starts_with("rollout") {
                        target.join(rollout_relative(&old, &mut missing_archived)?).to_string_lossy().to_string()
                    } else {
                        relocator.text(&old, RELOCATED_COLUMNS.contains(&col.as_str()))?
                    };
                    if new != old {
                        conn.execute(&format!("UPDATE {qt} SET {qc}=?1 WHERE {qc}=?2"), [&new, &old]).map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        conn.execute_batch("COMMIT").ok();
        let check: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA quick_check").map_err(|e| e.to_string())?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if check != vec!["ok".to_string()] { return Err("Copied history database failed validation".into()); }
        conn.pragma_update(None, "journal_mode", "DELETE").ok();
        if !clear.is_empty() { conn.execute_batch("VACUUM").map_err(|e| e.to_string())?; }
    }
    if let Some((_, t, p)) = primary_counts { threads = t; projects = p; }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    let marker = json!({
        "source": source.to_string_lossy(), "threads": threads, "projects": projects,
        "rollouts": copied, "restored_compressed": restored_compressed,
        "missing_archived_rollouts": missing_archived.len(), "copied_at": now,
        "visibility_version": VISIBILITY_REPAIR_VERSION,
    });
    std::fs::write(stage.join(MARKER_FILE), py_dumps(&marker))
        .map_err(|e| e.to_string())?;
    // 8) 发布前复查：目标出现真实历史→保留；初始化指纹变化→报错重试。
    if existing_history(target)? {
        return Ok(json!({"threads": history_count(target)?, "preserved_existing": true}));
    }
    if initialized_files(target)? != *initialized {
        return Err("Target initialization files changed during the snapshot; retry with this account closed".into());
    }
    // 9) 独占发布：marker 最后；每个叶子无替换发布，已初始化文件先备份。
    let mut files = walk_files(stage)?;
    files.sort_by_key(|p| (p.file_name().map(|n| n == MARKER_FILE).unwrap_or(false), p.to_string_lossy().to_string()));
    let staged: HashSet<PathBuf> = files.iter().filter_map(|p| p.strip_prefix(stage).ok().map(|r| r.to_path_buf())).collect();
    for relative in initialized.keys() {
        let name = relative.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if is_database_name(&name) && !staged.contains(relative) {
            return Err("Target has a different initialized history database version; an empty history home is required".into());
        }
    }
    for item in &files {
        budget(deadline)?;
        let rel = item.strip_prefix(stage).map_err(|e| e.to_string())?;
        let dest = target.join(rel);
        ordinary(&dest)?;
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut parent = dest.parent().map(|p| p.to_path_buf());
        while let Some(p) = parent {
            if p == target || p.exists() { break; }
            missing.push(p.clone());
            parent = p.parent().map(|x| x.to_path_buf());
        }
        for p in missing.iter().rev() {
            std::fs::create_dir(p).map_err(|e| e.to_string())?;
            created_directories.push(p.clone());
        }
        if dest.exists() {
            if !initialized.contains_key(rel) {
                return Err("A target history file appeared during publication".into());
            }
            let old = backup.join(rel);
            if let Some(p) = old.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
            std::fs::rename(&dest, &old).map_err(|e| e.to_string())?;
            backed_up.push((dest.clone(), old));
        }
        let dest_name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if is_database_name(&dest_name) || dest_name == "codex-dev.db" {
            for suffix in ["-wal", "-shm"] {
                let side = PathBuf::from(format!("{}{}", dest.display(), suffix));
                if side.exists() || is_link(&side) {
                    ordinary(&side)?;
                    let rel_side = side.strip_prefix(target).map_err(|e| e.to_string())?;
                    let old = backup.join(rel_side);
                    if let Some(p) = old.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
                    std::fs::rename(&side, &old).map_err(|e| e.to_string())?;
                    backed_up.push((side, old));
                }
            }
        }
        publish_file(item, &dest)?;
        published.push((dest, item.clone()));
    }
    Ok(json!({
        "threads": marker["threads"], "projects": marker["projects"], "rollouts": marker["rollouts"],
        "restored_compressed": marker["restored_compressed"],
        "missing_archived_rollouts": marker["missing_archived_rollouts"],
        "visibility_version": marker["visibility_version"], "preserved_existing": false,
    }))
}

/// os.path.normcase(basename)：反斜杠转正斜杠取末段 + lowercase（nt 口径）。
fn basename_normcase(path: &str) -> String {
    path.replace('\\', "/").rsplit('/').next().unwrap_or("").to_lowercase()
}

/// 与 python __main__ 对齐：{source,target} → {ok:true,...report}。
pub fn run(request: &Value) -> Result<Value, String> {
    let source = PathBuf::from(request.get("source").and_then(|v| v.as_str()).unwrap_or(""));
    let target = PathBuf::from(request.get("target").and_then(|v| v.as_str()).unwrap_or(""));
    let report = copy_history(&source, &target)?;
    let mut out = serde_json::Map::new();
    out.insert("ok".into(), json!(true));
    if let Some(obj) = report.as_object() {
        for (k, v) in obj { out.insert(k.clone(), v.clone()); }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// relative_home：相等→"."、前缀→相对、非前缀→None、..→报错、\\?\ 与斜杠拼写。
    #[test]
    fn relative_home_spellings_and_traversal() {
        let src = PathBuf::from(r"C:\Users\me\.codex");
        assert_eq!(relative_home(r"C:\Users\me\.codex", &src).unwrap(), Some(PathBuf::from(".")));
        assert_eq!(relative_home("c:/users/me/.codex/sessions/a.jsonl", &src).unwrap(),
                   Some(PathBuf::from("sessions/a.jsonl")));
        assert_eq!(relative_home(r"\\?\C:\Users\me\.codex\sessions\b.jsonl", &src).unwrap(),
                   Some(PathBuf::from("sessions/b.jsonl")));
        assert_eq!(relative_home(r"D:\other\sessions\a.jsonl", &src).unwrap(), None);
        assert!(relative_home(r"C:\Users\me\.codex\..\evil.jsonl", &src).is_err());
        // 前缀相似但不同目录（.codex2）不得误判为 home 内。
        assert_eq!(relative_home(r"C:\Users\me\.codex2\sessions\a.jsonl", &src).unwrap(), None);
    }

    /// relocator：local: 前缀按 key 归一比较；RELOCATED_COLUMNS 才做路径重定位；dict 键也过 text。
    #[test]
    fn relocator_local_prefix_and_fields() {
        let r = Relocator { source: PathBuf::from(r"C:\src"), target: PathBuf::from(r"C:\dst") };
        assert_eq!(r.text("local:c:/SRC", false).unwrap(), r"local:C:\dst");
        assert_eq!(r.text("local:C:/other", false).unwrap(), "local:C:/other");
        assert_eq!(r.text(r"C:\src\sessions\a.jsonl", true).unwrap(), r"C:\dst\sessions\a.jsonl");
        assert_eq!(r.text(r"C:\src\sessions\a.jsonl", false).unwrap(), r"C:\src\sessions\a.jsonl");
        let v = json!({"local:C:/src": {"rollout_path": "C:/src/sessions/a.jsonl", "cwd": "C:/src/work"}});
        let out = r.relocate(&v, "").unwrap();
        let obj = out.as_object().unwrap();
        let inner = obj.get(r"local:C:\dst").unwrap();
        assert_eq!(inner["rollout_path"], json!(r"C:\dst\sessions\a.jsonl"));
        // cwd 不在 RELOCATED_COLUMNS：保持原值。
        assert_eq!(inner["cwd"], json!("C:/src/work"));
    }

    /// truthy 与 python 真值口径一致。
    #[test]
    fn truthy_matches_python() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&json!(null))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!(""))));
        assert!(!truthy(Some(&json!([]))));
        assert!(!truthy(Some(&json!({}))));
        assert!(!truthy(Some(&json!(false))));
        assert!(truthy(Some(&json!(1))));
        assert!(truthy(Some(&json!("x"))));
        assert!(truthy(Some(&json!([0]))));
    }

    /// is_database_name 边界：合法版本号/无版本号/相似名。
    #[test]
    fn database_name_boundaries() {
        assert!(is_database_name("state_5.sqlite"));
        assert!(is_database_name("STATE_12.SQLITE"));
        assert!(is_database_name("thread_history_1.sqlite"));
        assert!(is_database_name("thread_catalog.sqlite"));
        assert!(is_database_name("thread_catalog_3.sqlite"));
        assert!(!is_database_name("state_.sqlite"));
        assert!(!is_database_name("state_x.sqlite"));
        assert!(!is_database_name("codex-dev.db"));
        assert!(!is_database_name("state_5.sqlite-wal"));
    }

    /// T18 第五批 golden：与 python copy_history 输出 + 复制后目标内容比对。
    /// 由 tests/chatgpt-history-copy-rust-golden.test.py 触发；未设环境变量时跳过。
    #[test]
    fn golden_copy_matches_python() {
        let Ok(req_path) = std::env::var("T18_COPY_REQ") else {
            eprintln!("跳过 copy golden：未设 T18_COPY_REQ");
            return;
        };
        let request: Value = serde_json::from_str(&std::fs::read_to_string(&req_path).unwrap()).unwrap();
        let py: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("T18_COPY_PY").unwrap()).unwrap()).unwrap();
        let rs = run(&request).expect("Rust copy_history 应成功");
        // report 逐字段比对（copied_at 不在 report 内）。
        assert_eq!(rs, py, "copy report 不一致：rust={rs} python={py}");
        // 复制后目标内容比对（驱动写入 py_after；路径/时间戳归一）。
        if let Ok(after_path) = std::env::var("T18_COPY_PY_AFTER") {
            let py_after: Value = serde_json::from_str(&std::fs::read_to_string(after_path).unwrap()).unwrap();
            let target = PathBuf::from(request["target"].as_str().unwrap());
            let base = target.parent().unwrap().to_string_lossy().to_string();
            let rs_after = snapshot_target(&target);
            let rs_after = normalize(rs_after, &base);
            if rs_after != py_after {
                let ro = rs_after.as_object().unwrap();
                let po = py_after.as_object().unwrap();
                for k in po.keys() {
                    if !ro.contains_key(k) { eprintln!("DIAG 缺文件 rs 无 {k}"); }
                    else if ro[k] != po[k] {
                        eprintln!("DIAG 文件不同 {k}");
                        if let (Some(rdb), Some(pdb)) = (ro[k].as_object(), po[k].as_object()) {
                            for t in pdb.keys() {
                                if rdb.get(t) != pdb.get(t) {
                                    eprintln!("DIAG   表 {t}: rs={:?} py={:?}", rdb.get(t), pdb.get(t));
                                }
                            }
                        } else {
                            eprintln!("DIAG   rs={:.400} py={:.400}", ro[k], po[k]);
                        }
                    }
                }
                for k in ro.keys() {
                    if !po.contains_key(k) { eprintln!("DIAG 多文件 py 无 {k}"); }
                }
            }
            assert_eq!(rs_after, py_after, "复制后目标内容不一致");
        }
    }

    /// 目标快照：全部文件相对路径 + 内容（sqlite 转储为表→行 JSON；文本原样；marker 去时间戳）。
    fn snapshot_target(target: &Path) -> Value {
        fn normalize(v: Value, base: &str) -> Value {
            match v {
                Value::String(s) => Value::String(s.replace(base, "<BASE>")),
                Value::Object(m) => Value::Object(m.into_iter().map(|(k, x)| (normalize(Value::String(k), base).as_str().unwrap().to_string(), normalize(x, base))).collect()),
                Value::Array(a) => Value::Array(a.into_iter().map(|x| normalize(x, base)).collect()),
                other => other,
            }
        }
        let mut out = serde_json::Map::new();
        let files = walk_files(target).unwrap();
        for f in files {
            let rel = f.strip_prefix(target).unwrap().to_string_lossy().replace('\\', "/");
            // 成功替换的原件保留在 .history-before-copy-*（python finally 只删空目录），
            // 属恢复资产而非复制产物，且目录名含随机 uuid——比对时排除。
            if rel.starts_with(".history-before-copy-") { continue; }
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            if name.ends_with(".sqlite") || name == "codex-dev.db" {
                let conn = rusqlite::Connection::open(&f).unwrap();
                let mut db = serde_json::Map::new();
                for t in tables(&conn).unwrap() {
                    if t.starts_with("sqlite_") { continue; }
                    let cols = columns(&conn, &t).unwrap();
                    let mut stmt = conn.prepare(&format!("SELECT * FROM {}", quote(&t))).unwrap();
                    let rows: Vec<Value> = stmt.query_map([], |r| {
                        let mut obj = serde_json::Map::new();
                        for (i, c) in cols.iter().enumerate() {
                            let v = match r.get_ref(i)? {
                                rusqlite::types::ValueRef::Null => Value::Null,
                                rusqlite::types::ValueRef::Integer(n) => json!(n),
                                rusqlite::types::ValueRef::Real(x) => json!(x),
                                rusqlite::types::ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
                                rusqlite::types::ValueRef::Blob(b) => json!(String::from_utf8_lossy(b)),
                            };
                            obj.insert(c.clone(), v);
                        }
                        Ok(Value::Object(obj))
                    }).unwrap().filter_map(|r| r.ok()).collect();
                    let mut rows = rows;
                    rows.sort_by_key(|v| v.to_string());
                    // payload_json 内嵌 JSON 字符串含绝对路径：解析→归一→再比（外层 normalize 只处理独立字符串值）。
                    if t == "thread_timeline_ledger" {
                        for row in &mut rows {
                            if let Some(s) = row.get("payload_json").and_then(|v| v.as_str()) {
                                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                                    row["payload_json"] = parsed;
                                }
                            }
                        }
                    }
                    db.insert(t, Value::Array(rows));
                }
                out.insert(rel, Value::Object(db));
            } else if name.ends_with(".jsonl.gz") || name.ends_with(".jsonl.zst") {
                // 压缩档案按字节 sha256 比对（二进制不能按文本读）。
                out.insert(rel, json!(sha256_hex(&std::fs::read(&f).unwrap())));
            } else if name == MARKER_FILE || name == STATE_FILE || name.ends_with(".jsonl") {
                // JSON 文本按解析后结构比对（分隔符/键序属序列化细节，语义一致即可）。
                let text = std::fs::read_to_string(&f).unwrap_or_default();
                if name.ends_with(".jsonl") {
                    let rows: Vec<Value> = text.lines().filter(|l| !l.trim().is_empty())
                        .map(|l| serde_json::from_str(l).unwrap()).collect();
                    out.insert(rel, Value::Array(rows));
                } else {
                    let mut v: Value = serde_json::from_str(&text).unwrap();
                    if name == MARKER_FILE { v.as_object_mut().unwrap().remove("copied_at"); }
                    out.insert(rel, v);
                }
            } else {
                let text = std::fs::read_to_string(&f).unwrap_or_default();
                out.insert(rel, json!(text));
            }
        }
        Value::Object(out)
    }

    fn normalize(v: Value, base: &str) -> Value {
        match v {
            Value::String(s) => Value::String(s.replace(base, "<BASE>")),
            Value::Object(m) => Value::Object(m.into_iter().map(|(k, x)| (k.replace(base, "<BASE>"), normalize(x, base))).collect()),
            Value::Array(a) => Value::Array(a.into_iter().map(|x| normalize(x, base)).collect()),
            other => other,
        }
    }
}