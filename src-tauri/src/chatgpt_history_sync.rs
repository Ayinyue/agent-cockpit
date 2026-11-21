//! T18 剩余生产路径①：chatgpt-history-sync 增量复制迁 Rust
//! （与 python chatgpt-history-sync.py 逐条对齐）。
//!
//! 注册的本机 Codex 档案之间做增量、追加式历史复制。绝不复制 auth/config、浏览器档案、
//!  jobs 或整份活数据库。SQLite 事务保护 WAL 写者。已有 rollout 只能按校验过的前缀增长；
//! 冲突的续写保留在各自原档案。
//!
//! 安全规则（与 python 版一致）：
//! 1. 路径必须绝对、无 ..、不含链接（plain）。
//! 2. rollout_path 只认 sessions/archived_sessions 下的 .jsonl，绝不借毒化路径读 auth/config。
//! 3. publish 独占发布（os.link），并发创建绝不覆盖；续写冲突报错保留原样。
//! 4. 跨进程文件锁（msvcrt/_locking）串行化同步 worker。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::chatgpt_history_repair::{atomic_bytes, key, ordinary, uuid_hex};

/// 候选 rollout（与 python candidates[thread] 的元组对齐）。
struct Cand { root: PathBuf, row: Value, path: PathBuf, relative: PathBuf, fp: Value }

/// JSON 值 → rusqlite 可绑定值（Null/整数/浮点/文本；数组/对象序列化为文本）。
pub(crate) fn to_sql(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => rusqlite::types::Value::Integer(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() { rusqlite::types::Value::Integer(i) }
            else { rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)) }
        }
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

/// 与 python qi 对齐：SQL 标识符双引号转义。
pub(crate) fn qi(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 与 python settings_only 对齐：event_msg + thread_settings_applied。
pub(crate) fn settings_only(line: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(line) else { return false };
    v.get("type").and_then(|t| t.as_str()) == Some("event_msg")
        && v.get("payload").and_then(|p| p.get("type")).and_then(|t| t.as_str())
            == Some("thread_settings_applied")
}

/// 与 python internal_thread 对齐：subagent/guardian_review 或 source 含 subagent。
pub(crate) fn internal_thread(row: &Value) -> bool {
    if let Some(s) = row.get("thread_source").and_then(|v| v.as_str()) {
        if s == "subagent" || s == "guardian_review" { return true; }
    }
    let src = row.get("source").and_then(|v| v.as_str()).unwrap_or("");
    if let Ok(origin) = serde_json::from_str::<Value>(src) {
        return origin.is_object() && origin.get("subagent").is_some();
    }
    false
}

/// 按 \n 切行并保留行尾（与 python splitlines(keepends=True) 的 JSONL 口径对齐）。
pub(crate) fn lines_keepends(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..data.len() {
        if data[i] == b'\n' {
            out.push(&data[start..=i]);
            start = i + 1;
        }
    }
    if start < data.len() { out.push(&data[start..]); }
    out
}

/// 与 python replace_offset 对齐：incoming 是 current 的追加 → 返回 len(current)；
/// 否则找公共前缀行，若 current 剩余行全是 settings_only → 返回公共前缀字节数；否则 None（冲突）。
pub(crate) fn replace_offset(current: &[u8], incoming: &[u8]) -> Option<usize> {
    if incoming.starts_with(current) {
        return Some(current.len());
    }
    let old = lines_keepends(current);
    let new = lines_keepends(incoming);
    let mut common = 0;
    for (a, b) in old.iter().zip(new.iter()) {
        if a != b { break; }
        common += 1;
    }
    if common > 0 && old[common..].iter().all(|l| settings_only(l)) {
        return Some(old[..common].iter().map(|l| l.len()).sum());
    }
    None
}

/// 与 python snapshot 对齐：读前后 stat 一致才认；去掉末尾不完整行；校验首末行 JSON。
pub(crate) fn snapshot(path: &Path) -> Result<Vec<u8>, String> {
    ordinary(path)?;
    let before = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let after = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let bm = before.modified().map_err(|e| e.to_string())?;
    let am = after.modified().map_err(|e| e.to_string())?;
    if before.len() != after.len() || bm != am {
        return Err("Rollout changed during snapshot".into());
    }
    let mut data = data;
    if !data.is_empty() && !data.ends_with(b"\n") {
        let cut = data.iter().rposition(|&b| b == b'\n').map(|p| p + 1).unwrap_or(0);
        data.truncate(cut);
    }
    if !data.is_empty() {
        let ls = lines_keepends(&data);
        serde_json::from_slice::<Value>(ls[0]).map_err(|_| "invalid first line".to_string())?;
        let trimmed = data.trim_ascii_end();
        let last = trimmed.iter().rposition(|&b| b == b'\n').map(|p| &trimmed[p + 1..]).unwrap_or(trimmed);
        serde_json::from_slice::<Value>(last).map_err(|_| "invalid last line".to_string())?;
    }
    Ok(data)
}

/// 与 python rollout_path 对齐：剥 \\?\ 前缀，只认 sessions/archived_sessions 下 .jsonl。
fn rollout_path(root: &Path, row: &Value) -> Result<(PathBuf, PathBuf), String> {
    let raw = row.get("rollout_path").and_then(|v| v.as_str()).unwrap_or("");
    let raw = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    let path = ordinary(Path::new(raw))?;
    let relative = path.strip_prefix(root).map_err(|_| "Rollout is outside this home history".to_string())?;
    let first = relative.components().next().and_then(|c| c.as_os_str().to_str()).unwrap_or("");
    if (first != "sessions" && first != "archived_sessions")
        || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return Err("Rollout is outside this home history".into());
    }
    Ok((path.clone(), relative.to_path_buf()))
}

/// 与 python connect 对齐：mode=ro（query_only）或 mode=rw。
pub(crate) fn connect(path: &Path, write: bool) -> Result<rusqlite::Connection, String> {
    ordinary(path)?;
    let flags = if write {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI
    };
    let conn = rusqlite::Connection::open_with_flags(path, flags).map_err(|e| e.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(2)).ok();
    if !write { conn.pragma_update(None, "query_only", "ON").ok(); }
    Ok(conn)
}

pub(crate) fn columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>, String> {
    let sql = format!("PRAGMA table_info({})", qi(table));
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(crate) fn row_to_json(row: &rusqlite::Row, cols: &[String]) -> Result<Value, rusqlite::Error> {
    let mut obj = serde_json::Map::new();
    for (i, name) in cols.iter().enumerate() {
        let v = row.get_ref(i)?;
        let val = match v {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => json!(n),
            rusqlite::types::ValueRef::Real(f) => json!(f),
            rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
            rusqlite::types::ValueRef::Blob(b) => json!(String::from_utf8_lossy(b)),
        };
        obj.insert(name.clone(), val);
    }
    Ok(Value::Object(obj))
}

/// 与 python rows 对齐：SELECT * FROM table [WHERE thread_id=?]，返回 JSON object 列表。
pub(crate) fn rows(conn: &rusqlite::Connection, table: &str, thread: Option<&str>) -> Result<Vec<Value>, String> {
    let cols = columns(conn, table)?;
    if cols.is_empty() { return Ok(Vec::new()); }
    let (sql, use_thread) = match thread {
        Some(t) => {
            if !cols.iter().any(|c| c == "thread_id") { return Ok(Vec::new()); }
            (format!("SELECT * FROM {} WHERE thread_id=?1", qi(table)), Some(t.to_string()))
        }
        None => (format!("SELECT * FROM {}", qi(table)), None),
    };
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mapped = if let Some(t) = use_thread {
        stmt.query_map([&t], |r| row_to_json(r, &cols)).map_err(|e| e.to_string())?
            .filter_map(|r| r.ok()).collect::<Vec<_>>()
    } else {
        stmt.query_map([], |r| row_to_json(r, &cols)).map_err(|e| e.to_string())?
            .filter_map(|r| r.ok()).collect::<Vec<_>>()
    };
    Ok(mapped)
}

/// 跨进程文件锁（与 python msvcrt.locking LK_NBLCK 同口径）。
/// 用 Win32 LockFileEx（独占+立即失败）——与 CRT _locking 共享同一套 OS 字节范围锁，
/// 因此 Rust 与 python 的锁互斥互通。不能用 _locking FFI：它要 CRT fd 而非 HANDLE。
#[cfg(windows)]
pub(crate) mod filelock {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct Overlapped {
        internal: usize, internal_high: usize,
        offset: u32, offset_high: u32, h_event: isize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LockFileEx(h: isize, flags: u32, reserved: u32, nlow: u32, nhigh: u32, ov: *mut Overlapped) -> i32;
        fn UnlockFileEx(h: isize, reserved: u32, nlow: u32, nhigh: u32, ov: *mut Overlapped) -> i32;
    }
    const EXCLUSIVE: u32 = 0x2;
    const FAIL_IMMEDIATELY: u32 = 0x1;

    pub struct Lock { file: File, nbytes: u32 }
    impl Lock {
        pub fn acquire(path: &std::path::Path) -> Result<Self, String> {
            Self::acquire_range(path, 1)
        }
        pub fn acquire_range(path: &std::path::Path, nbytes: u32) -> Result<Self, String> {
            let file = std::fs::OpenOptions::new().read(true).write(true).create(true)
                .open(path).map_err(|e| e.to_string())?;
            let mut ov = Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, h_event: 0 };
            let ok = unsafe { LockFileEx(file.as_raw_handle() as isize, EXCLUSIVE | FAIL_IMMEDIATELY, 0, nbytes, 0, &mut ov) };
            if ok == 0 { return Err("sync lock busy".into()); }
            Ok(Lock { file, nbytes })
        }
    }
    impl Drop for Lock {
        fn drop(&mut self) {
            let mut ov = Overlapped { internal: 0, internal_high: 0, offset: 0, offset_high: 0, h_event: 0 };
            unsafe { UnlockFileEx(self.file.as_raw_handle() as isize, 0, self.nbytes, 0, &mut ov); }
        }
    }
}
#[cfg(not(windows))]
pub(crate) mod filelock {
    pub struct Lock;
    impl Lock {
        pub fn acquire(_path: &std::path::Path) -> Result<Self, String> { Ok(Lock) }
        pub fn acquire_range(_path: &std::path::Path, _n: u32) -> Result<Self, String> { Ok(Lock) }
    }
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// 与 python Replicator 对齐：指纹缓存 + 备份 + 独占发布 + upsert + 投影/目录复制。
pub(crate) struct Replicator {
    pub(crate) directory: PathBuf,
    pub(crate) cache_file: PathBuf,
    pub(crate) cache: HashMap<String, Value>,
    pub(crate) report: Value,
    /// shared 侧目录重建（每 15s）不做整库备份，与 python `rep.backup = lambda: None` 对齐。
    pub(crate) no_backup: bool,
}

impl Replicator {
    pub(crate) fn new(state_dir: &Path) -> Result<Self, String> {
        let directory = ordinary(state_dir)?;
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let cache_file = directory.join("fingerprints.json");
        let cache = std::fs::read_to_string(&cache_file).ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, Value>>(&s).ok())
            .unwrap_or_default();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_secs_f64();
        let report = json!({"ok": true, "version": 1, "updated_at": now,
            "homes": 0, "threads": 0, "rollouts": 0, "catalog": 0,
            "conflicts": [], "deferred": [], "missing": []});
        Ok(Replicator { directory, cache_file, cache, report, no_backup: false })
    }

    fn backup(&self, root: &Path, path: &Path, database: bool) -> Result<(), String> {
        if self.no_backup { return Ok(()); }
        let tag = &sha256_hex(root.to_string_lossy().as_bytes())[..16];
        let rel = path.strip_prefix(root).map_err(|e| e.to_string())?;
        let destination = ordinary(&self.directory.join("backups-v1").join(tag).join(rel))?;
        if destination.exists() { return Ok(()); }
        if let Some(p) = destination.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
        if database {
            let source = connect(path, false)?;
            let mut target = rusqlite::Connection::open(&destination).map_err(|e| e.to_string())?;
            let bk = rusqlite::backup::Backup::new(&source, &mut target).map_err(|e| e.to_string())?;
            bk.run_to_completion(256, std::time::Duration::from_millis(50), None).map_err(|e| e.to_string())?;
        } else {
            let data = std::fs::read(path).map_err(|e| e.to_string())?;
            std::fs::OpenOptions::new().write(true).create_new(true).open(&destination)
                .and_then(|mut f| { use std::io::Write; f.write_all(&data) })
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn fingerprint(&mut self, path: &Path) -> Result<Value, String> {
        let info = std::fs::metadata(path).map_err(|e| e.to_string())?;
        let mtime = info.modified().map_err(|e| e.to_string())?
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as i64;
        let token = json!([info.len(), mtime]);
        let pkey = path.to_string_lossy().to_string();
        if let Some(old) = self.cache.get(&pkey) {
            if old.get("meaningful").is_some() && old.get("stat") == Some(&token) {
                return Ok(old.clone());
            }
        }
        let data = snapshot(path)?;
        let meaningful = lines_keepends(&data).iter().filter(|l| !settings_only(l)).count();
        let value = json!({"stat": token, "size": data.len(), "hash": sha256_hex(&data), "meaningful": meaningful});
        self.cache.insert(pkey, value.clone());
        Ok(value)
    }

    pub(crate) fn publish(&mut self, root: &Path, destination: &Path, data: &[u8]) -> Result<bool, String> {
        ordinary(destination)?;
        if let Some(p) = destination.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
        if !destination.exists() {
            let temp = destination.with_file_name(format!(".sync-{}", uuid_hex()));
            std::fs::write(&temp, data).map_err(|e| e.to_string())?;
            // 独占发布：并发客户端可能在检查与发布之间创建目标；绝不覆盖。
            let link = std::fs::hard_link(&temp, destination);
            let _ = std::fs::remove_file(&temp);
            link.map_err(|e| e.to_string())?;
            return Ok(true);
        }
        let current = snapshot(destination)?;
        if current == data { return Ok(false); }
        let offset = replace_offset(&current, data).ok_or("Conflicting continuation")?;
        self.backup(root, destination, false)?;
        // OS 锁下重查（含生产者的不完整尾），再 seek+write+truncate。
        let _lock = filelock::Lock::acquire(destination)?;
        let reread = std::fs::read(destination).map_err(|e| e.to_string())?;
        if reread != current { return Err("Destination has a concurrent writer".into()); }
        let mut file = std::fs::OpenOptions::new().read(true).write(true).open(destination).map_err(|e| e.to_string())?;
        use std::io::{Seek, SeekFrom, Write};
        file.seek(SeekFrom::Start(offset as u64)).map_err(|e| e.to_string())?;
        file.write_all(&data[offset..]).map_err(|e| e.to_string())?;
        file.set_len(data.len() as u64).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        Ok(true)
    }

    pub(crate) fn upsert(&self, conn: &rusqlite::Connection, table: &str, row: &Value, overwrite: bool) -> Result<bool, String> {
        let sql = format!("PRAGMA table_info({})", qi(table));
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let info: Vec<(String, i64)> = stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))
            .map_err(|e| e.to_string())?.filter_map(|r| r.ok()).collect();
        let row_obj = row.as_object().ok_or("row must be object")?;
        let cols: Vec<String> = info.iter().map(|(n, _)| n.clone()).filter(|n| row_obj.contains_key(n)).collect();
        let mut pk: Vec<(String, i64)> = info.iter().filter(|(_, p)| *p > 0).cloned().collect();
        pk.sort_by_key(|(_, p)| *p);
        let keys: Vec<String> = pk.into_iter().map(|(n, _)| n).collect();
        if cols.is_empty() || keys.is_empty() || !keys.iter().all(|k| row_obj.contains_key(k)) {
            return Ok(false);
        }
        let where_clause = keys.iter().enumerate().map(|(i, k)| format!("{}=?{}", qi(k), i + 1)).collect::<Vec<_>>().join(" AND ");
        // 按 keys 顺序绑定参数
        let key_vals: Vec<&Value> = keys.iter().map(|k| &row_obj[k]).collect();
        let old: Option<Value> = {
            let q = format!("SELECT * FROM {} WHERE {}", qi(table), where_clause);
            let mut st = conn.prepare(&q).map_err(|e| e.to_string())?;
            let cols_all = columns(conn, table)?;
            let mut rr = st.query_map(rusqlite::params_from_iter(key_vals.iter().map(|v| to_sql(v))), |r| row_to_json(r, &cols_all))
                .map_err(|e| e.to_string())?;
            rr.next().and_then(|r| r.ok())
        };
        if let Some(old) = &old {
            let old_obj = old.as_object().unwrap();
            if !overwrite || cols.iter().all(|c| old_obj.get(c) == row_obj.get(c)) {
                return Ok(false);
            }
            let set = cols.iter().map(|k| format!("{}=?", qi(k))).collect::<Vec<_>>().join(",");
            let where_upd = keys.iter().enumerate().map(|(i, k)| format!("{}=?{}", qi(k), cols.len() + i + 1)).collect::<Vec<_>>().join(" AND ");
            let q = format!("UPDATE {} SET {} WHERE {}", qi(table), set, where_upd);
            let mut params: Vec<Value> = cols.iter().map(|c| row_obj[c].clone()).collect();
            params.extend(keys.iter().map(|k| row_obj[k].clone()));
            conn.execute(&q, rusqlite::params_from_iter(params.iter().map(to_sql))).map_err(|e| e.to_string())?;
        } else {
            let placeholders = cols.iter().map(|_| "?".to_string()).collect::<Vec<_>>().join(",");
            let q = format!("INSERT INTO {}({}) VALUES({})", qi(table),
                cols.iter().map(|c| qi(c)).collect::<Vec<_>>().join(","), placeholders);
            let params: Vec<&Value> = cols.iter().map(|c| &row_obj[c]).collect();
            conn.execute(&q, rusqlite::params_from_iter(params.iter().map(|v| to_sql(v)))).map_err(|e| e.to_string())?;
        }
        Ok(true)
    }

    fn projection_valid(&self, source: &Path, thread: &str, length: usize) -> Result<bool, String> {
        let path = source.join("thread_history_1.sqlite");
        if !path.is_file() { return Ok(false); }
        let conn = connect(&path, false)?;
        let states = rows(&conn, "thread_history_projection_state", Some(thread))?;
        Ok(!states.is_empty() && states.iter().all(|r| {
            r.get("next_rollout_byte_offset").and_then(|v| v.as_i64()).unwrap_or(0) <= length as i64
        }))
    }

    fn copy_projection(&mut self, source: &Path, target: &Path, thread: &str, length: usize, valid: bool) -> Result<(), String> {
        let src = source.join("thread_history_1.sqlite");
        let dst = target.join("thread_history_1.sqlite");
        if !src.is_file() || !dst.is_file() { return Ok(()); }
        self.backup(target, &dst, true)?;
        let a = connect(&src, false)?;
        let mut b = connect(&dst, true)?;
        let tables = ["thread_turns", "thread_items", "thread_realtime_items", "thread_history_projection_state"];
        if !valid {
            for table in tables {
                if columns(&b, table)?.iter().any(|c| c == "thread_id") {
                    b.execute(&format!("DELETE FROM {} WHERE thread_id=?1", qi(table)), [thread]).map_err(|e| e.to_string())?;
                }
            }
            return Ok(());
        }
        // 源只读快照事务（python a.execute('BEGIN')）；目标写事务（python with b）。
        a.execute_batch("BEGIN").map_err(|e| e.to_string())?;
        for state in rows(&a, "thread_history_projection_state", Some(thread))? {
            if state.get("next_rollout_byte_offset").and_then(|v| v.as_i64()).unwrap_or(0) > length as i64 {
                return Err("Source projection advanced beyond the copied rollout".into());
            }
        }
        let tx = b.unchecked_transaction().map_err(|e| e.to_string())?;
        let result = (|| -> Result<(), String> {
            for table in tables {
                for row in rows(&a, table, Some(thread))? {
                    self.upsert(&b, table, &row, true)?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => { tx.commit().map_err(|e| e.to_string())?; Ok(()) }
            Err(e) => { let _ = tx.rollback(); Err(e) }
        }
    }

    pub(crate) fn catalog(&mut self, root: &Path, row: &Value) -> Result<(), String> {
        let path = root.join("sqlite").join("codex-dev.db");
        if !path.is_file() { return Ok(()); }
        self.backup(root, &path, true)?;
        let mut conn = connect(&path, true)?;
        if columns(&conn, "local_thread_catalog")?.is_empty() { return Ok(()); }
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let local_hosts: Vec<String> = {
            let mut st = conn.prepare("SELECT host_id FROM local_thread_catalog_hosts WHERE host_kind='local'").map_err(|e| e.to_string())?;
            let rr = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
            rr.filter_map(|r| r.ok()).collect()
        };
        let hosts = if local_hosts.is_empty() { vec!["local".to_string()] } else { local_hosts };
        let tid = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let archived = row.get("archived").map(|v| v.as_i64().unwrap_or(0) != 0 || v.as_bool().unwrap_or(false)).unwrap_or(false);
        for host in hosts {
            let changed = if archived || internal_thread(row) {
                conn.execute("DELETE FROM local_thread_catalog WHERE host_id=?1 AND thread_id=?2", [&host, tid]).map_err(|e| e.to_string())?
            } else {
                let num = |k: &str| row.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let created = if num("created_at_ms") != 0.0 { num("created_at_ms") / 1000.0 } else { num("created_at") };
                let updated = if num("updated_at_ms") != 0.0 { num("updated_at_ms") / 1000.0 } else { num("updated_at") };
                let recency = {
                    let r = row.get("recency_at_ms").and_then(|v| v.as_f64()).filter(|x| *x != 0.0)
                        .or_else(|| row.get("updated_at_ms").and_then(|v| v.as_f64()).filter(|x| *x != 0.0))
                        .unwrap_or_else(|| num("updated_at") * 1000.0);
                    r / 1000.0
                };
                let title = row.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                    .or_else(|| row.get("title").and_then(|v| v.as_str())).unwrap_or("");
                let source_kind = row.get("source").and_then(|v| v.as_str())
                    .filter(|s| ["cli", "vscode", "exec", "appServer"].contains(s)).unwrap_or("vscode");
                let cat_row = json!({
                    "host_id": host, "thread_id": tid, "display_title": title,
                    "source_created_at": created, "source_updated_at": updated, "source_recency_at": recency,
                    "cwd": row.get("cwd").cloned().unwrap_or(Value::Null),
                    "source_kind": source_kind, "source_detail": Value::Null,
                    "model_provider": row.get("model_provider").cloned().unwrap_or(Value::Null),
                    "git_branch": row.get("git_branch").cloned().unwrap_or(Value::Null),
                    "observation_sequence": 0, "missing_candidate": 0,
                    "thread_source": row.get("thread_source").cloned().unwrap_or(Value::Null),
                    "project_id": row.get("project_id").cloned().unwrap_or(Value::Null),
                });
                if self.upsert(&conn, "local_thread_catalog", &cat_row, true)? { 1 } else { 0 }
            };
            if changed > 0 {
                self.report["catalog"] = json!(self.report["catalog"].as_i64().unwrap_or(0) + changed as i64);
                conn.execute("UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1", []).map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) fn clean_internal_catalog(&mut self, root: &Path, threads: &HashMap<String, Value>) -> Result<(), String> {
        let path = root.join("sqlite").join("codex-dev.db");
        if !path.is_file() { return Ok(()); }
        self.backup(root, &path, true)?;
        let mut conn = connect(&path, true)?;
        if columns(&conn, "local_thread_catalog")?.is_empty() { return Ok(()); }
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let mut removed = 0i64;
        for row in threads.values() {
            if internal_thread(row) {
                let tid = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
                removed += conn.execute("DELETE FROM local_thread_catalog WHERE thread_id=?1 AND host_id IN (SELECT host_id FROM local_thread_catalog_hosts WHERE host_kind='local')", [tid]).map_err(|e| e.to_string())? as i64;
            }
        }
        if removed > 0 {
            conn.execute("UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1", []).map_err(|e| e.to_string())?;
            self.report["catalog"] = json!(self.report["catalog"].as_i64().unwrap_or(0) + removed);
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn bump(&mut self, field: &str) {
        self.report[field] = json!(self.report[field].as_i64().unwrap_or(0) + 1);
    }
    fn push_list(&mut self, field: &str, item: Value) {
        if let Some(a) = self.report[field].as_array_mut() { a.push(item); }
    }

    pub(crate) fn run(&mut self, homes_in: &[PathBuf]) -> Result<Value, String> {
        let mut homes: Vec<PathBuf> = Vec::new();
        for p in homes_in {
            let p = ordinary(p)?;
            if !homes.contains(&p) { homes.push(p); }
        }
        for a in &homes {
            for b in &homes {
                if a != b && (b.starts_with(a) || a.starts_with(b)) {
                    return Err("History homes overlap".into());
                }
            }
        }
        homes.retain(|p| p.join("state_5.sqlite").is_file());
        self.report["homes"] = json!(homes.len());

        let mut existing_map: HashMap<PathBuf, HashMap<String, Value>> = HashMap::new();
        let mut cand_order: Vec<String> = Vec::new();
        let mut candidates: HashMap<String, Vec<Cand>> = HashMap::new();
        for root in &homes {
            let ordered: Vec<(String, Value)> = {
                let conn = connect(&root.join("state_5.sqlite"), false)?;
                rows(&conn, "threads", None)?.into_iter()
                    .filter_map(|r| { let id = r.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()); id.map(|id| (id, r)) })
                    .collect()
            };
            let map: HashMap<String, Value> = ordered.iter().cloned().collect();
            self.clean_internal_catalog(root, &map)?;
            for (thread, row) in ordered {
                let result = (|| -> Result<(), String> {
                    let (path, relative) = rollout_path(root, &row)?;
                    if !path.is_file() { return Ok(()); }
                    let fp = self.fingerprint(&path)?;
                    if !candidates.contains_key(&thread) { cand_order.push(thread.clone()); }
                    candidates.entry(thread.clone()).or_default().push(Cand {
                        root: root.clone(), row: row.clone(), path, relative, fp });
                    Ok(())
                })();
                if let Err(e) = result {
                    self.push_list("deferred", json!({"thread": thread, "home": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), "reason": e}));
                }
            }
            existing_map.insert(root.clone(), map);
        }
        self.run_candidates(&homes, &existing_map, &cand_order, &mut candidates)?;
        let mut all_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for m in existing_map.values() { for k in m.keys() { all_ids.insert(k.clone()); } }
        for k in candidates.keys() { all_ids.remove(k); }
        self.report["missing"] = json!(all_ids.into_iter().collect::<Vec<_>>());
        let cache_val = Value::Object(self.cache.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        atomic_bytes(&self.cache_file, serde_json::to_string(&cache_val).unwrap_or_default().as_bytes())?;
        atomic_bytes(&self.directory.join("status.json"), serde_json::to_string(&self.report).unwrap_or_default().as_bytes())?;
        Ok(self.report.clone())
    }

    fn run_candidates(&mut self, homes: &[PathBuf], existing_map: &HashMap<PathBuf, HashMap<String, Value>>,
                      cand_order: &[String], candidates: &mut HashMap<String, Vec<Cand>>) -> Result<(), String> {
        for thread in cand_order {
            let group = &candidates[thread];
            let thread_result = (|| -> Result<(), String> {
                let mut winner_idx = 0;
                let mut best_key: (i64, i64, i64) = (-1, -1, -1);
                for (i, c) in group.iter().enumerate() {
                    let meaningful = c.fp["meaningful"].as_i64().unwrap_or(0);
                    let size = c.fp["size"].as_i64().unwrap_or(0);
                    let updated = c.row.get("updated_at_ms").and_then(|v| v.as_i64()).filter(|x| *x != 0)
                        .unwrap_or_else(|| c.row.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0) * 1000);
                    let k = (meaningful, size, updated);
                    if k > best_key { best_key = k; winner_idx = i; }
                }
                let win = &group[winner_idx];
                let source = win.root.clone();
                let row = win.row.clone();
                let path = win.path.clone();
                let relative = win.relative.clone();
                let fp = win.fp.clone();
                let fp_hash = fp["hash"].as_str().unwrap_or("").to_string();
                let fp_size = fp["size"].as_i64().unwrap_or(0) as usize;

                let mut data: Option<Vec<u8>> = None;
                let mut divergent = false;
                for other in group {
                    if other.fp["hash"].as_str() == Some(&fp_hash) { continue; }
                    if data.is_none() { data = Some(snapshot(&path)?); }
                    let other_data = snapshot(&other.path)?;
                    if replace_offset(&other_data, data.as_ref().unwrap()).is_none() { divergent = true; }
                }
                if divergent {
                    let names: Vec<String> = group.iter().map(|g| g.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).collect();
                    self.push_list("conflicts", json!({"thread": thread, "homes": names}));
                    return Ok(());
                }
                let valid_projection = self.projection_valid(&source, thread, fp_size)?;
                for target in homes {
                    let old = existing_map.get(target).and_then(|m| m.get(thread)).cloned();
                    let destination = match &old {
                        Some(o) => rollout_path(target, o)?.0,
                        None => target.join(&relative),
                    };
                    let mut imported = row.clone();
                    imported["rollout_path"] = json!(destination.to_string_lossy());
                    if !valid_projection && imported.get("history_mode").is_some() {
                        imported["history_mode"] = json!("legacy");
                    }
                    let needs_file = !destination.is_file()
                        || self.fingerprint(&destination)?["hash"].as_str() != Some(&fp_hash);
                    let needs_row = match &old {
                        None => true,
                        Some(o) => {
                            let oo = o.as_object().unwrap();
                            imported.as_object().unwrap().iter().any(|(k, v)| oo.get(k) != Some(v))
                        }
                    };
                    let completion_key = format!("applied:{}", destination.to_string_lossy());
                    let pending = self.cache.get(&completion_key).and_then(|v| v.as_str()) != Some(&fp_hash);
                    if !needs_file && !needs_row && !pending { continue; }
                    let target_result = (|| -> Result<(), String> {
                        if needs_file {
                            if data.is_none() { data = Some(snapshot(&path)?); }
                            if sha256_hex(data.as_ref().unwrap()) != fp_hash {
                                return Err("Source advanced; retry on next pass".into());
                            }
                            if self.publish(target, &destination, data.as_ref().unwrap())? { self.bump("rollouts"); }
                        }
                        let db = target.join("state_5.sqlite");
                        self.backup(target, &db, true)?;
                        self.sync_thread_db(&source, target, &db, thread, &row, &imported, &old)?;
                        if (source != *target || !valid_projection) && (needs_file || needs_row || pending) {
                            self.copy_projection(&source, target, thread, fp_size, valid_projection)?;
                        }
                        self.catalog(target, &imported)?;
                        self.cache.insert(completion_key, json!(fp_hash));
                        self.bump("threads");
                        Ok(())
                    })();
                    if let Err(e) = target_result {
                        self.push_list("deferred", json!({"thread": thread, "home": target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), "reason": e}));
                    }
                }
                Ok(())
            })();
            if let Err(e) = thread_result {
                self.push_list("deferred", json!({"thread": thread, "reason": e}));
            }
        }
        Ok(())
    }

    fn sync_thread_db(&mut self, source: &Path, target: &Path, db: &Path, thread: &str,
                      row: &Value, imported: &Value, old: &Option<Value>) -> Result<(), String> {
        let _ = target;
        let a = connect(&source.join("state_5.sqlite"), false)?;
        let mut b = connect(db, true)?;
        let tx = b.unchecked_transaction().map_err(|e| e.to_string())?;
        let result = (|| -> Result<(), String> {
        let current: Option<Value> = {
            let cols = columns(&b, "threads")?;
            let mut st = b.prepare("SELECT * FROM threads WHERE id=?1").map_err(|e| e.to_string())?;
            let mut rr = st.query_map([thread], |r| row_to_json(r, &cols)).map_err(|e| e.to_string())?;
            rr.next().and_then(|r| r.ok())
        };
        if current != *old { return Err("Thread metadata changed during synchronization".into()); }
        for (table, id_col, ident) in [
            ("projects", "id", row.get("project_id").and_then(|v| v.as_str())),
            ("thread_sections", "id", row.get("thread_section_id").and_then(|v| v.as_str())),
        ] {
            if let Some(identifier) = ident.filter(|s| !s.is_empty()) {
                if !columns(&a, table)?.is_empty() {
                    let cols = columns(&a, table)?;
                    let q = format!("SELECT * FROM {} WHERE {}=?1", qi(table), qi(id_col));
                    let mut st = a.prepare(&q).map_err(|e| e.to_string())?;
                    let rec = st.query_map([identifier], |r| row_to_json(r, &cols)).map_err(|e| e.to_string())?
                        .filter_map(|r| r.ok()).next();
                    if let Some(record) = rec { self.upsert(&b, table, &record, false)?; }
                }
            }
        }
        if let Some(pid) = row.get("project_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            if !columns(&a, "project_roots")?.is_empty() {
                let cols = columns(&a, "project_roots")?;
                let mut st = a.prepare("SELECT * FROM project_roots WHERE project_id=?1").map_err(|e| e.to_string())?;
                let recs: Vec<Value> = st.query_map([pid], |r| row_to_json(r, &cols)).map_err(|e| e.to_string())?
                    .filter_map(|r| r.ok()).collect();
                for record in recs { self.upsert(&b, "project_roots", &record, false)?; }
            }
        }
        self.upsert(&b, "threads", imported, true)?;
        for table in ["thread_dynamic_tools", "thread_attachments", "thread_spawn_edges"] {
            let recs: Vec<Value> = if table == "thread_spawn_edges" {
                if columns(&a, table)?.is_empty() { Vec::new() } else {
                    let cols = columns(&a, table)?;
                    let mut st = a.prepare("SELECT * FROM thread_spawn_edges WHERE child_thread_id=?1").map_err(|e| e.to_string())?;
                    let recs: Vec<Value> = st.query_map([thread], |r| row_to_json(r, &cols)).map_err(|e| e.to_string())?
                        .filter_map(|r| r.ok()).collect();
                    recs
                }
            } else { rows(&a, table, Some(thread))? };
            for record in recs { self.upsert(&b, table, &record, true)?; }
        }
        Ok(())
        })();
        match result {
            Ok(()) => { tx.commit().map_err(|e| e.to_string())?; Ok(()) }
            Err(e) => { let _ = tx.rollback(); Err(e) }
        }
    }
}

/// 与 python synchronize 对齐：sync.lock 串行化；shared.json 存在则跳过（共享存储模式）。
pub fn synchronize(homes: &[PathBuf], state_dir: &Path) -> Result<Value, String> {
    let state_dir = ordinary(state_dir)?;
    std::fs::create_dir_all(&state_dir).map_err(|e| e.to_string())?;
    let _lock = filelock::Lock::acquire(&state_dir.join("sync.lock"))?;
    if state_dir.join("shared.json").exists() {
        return Ok(json!({"ok": true, "storage": "shared", "skipped": true}));
    }
    let mut rep = Replicator::new(&state_dir)?;
    rep.run(homes)
}

/// 与 python __main__ 对齐：stdin {homes, state_dir} → stdout report。
pub fn run(request: &Value) -> Result<Value, String> {
    let homes: Vec<PathBuf> = request.get("homes").and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(PathBuf::from)).collect())
        .unwrap_or_default();
    let state_dir = PathBuf::from(request.get("state_dir").and_then(|v| v.as_str()).unwrap_or(""));
    synchronize(&homes, &state_dir)
}

/// T18 生产接线：shared 每轮的侧栏目录重建（chatgpt-history-shared.py run() 尾段）。
/// 只重建各 home 的 local_thread_catalog（小缓存），绝不复制消息正文/整库/会话文件。
/// 与 python `rep.backup = lambda: None` 对齐：不做整库备份。持 sync.lock 与 python worker 互斥。
/// 返回 ok=true 时 python 侧跳过自己的重建循环；失败则 python 全量重建（幂等回退）。
pub fn rebuild_catalog(canonical: &Path, homes: &[PathBuf], state_dir: &Path) -> Result<Value, String> {
    let canonical = ordinary(canonical)?;
    let state_dir = ordinary(state_dir)?;
    std::fs::create_dir_all(&state_dir).map_err(|e| e.to_string())?;
    let _lock = filelock::Lock::acquire(&state_dir.join("sync.lock"))?;
    let db = canonical.join("state_5.sqlite");
    if !db.is_file() {
        return Ok(json!({"ok": true, "skipped": true, "reason": "no canonical state"}));
    }
    let mut rep = Replicator::new(&state_dir)?;
    rep.no_backup = true;
    let threads: HashMap<String, Value> = {
        let conn = connect(&db, false)?;
        rows(&conn, "threads", None)?.into_iter()
            .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()).map(|id| (id, r)))
            .collect()
    };
    let mut rebuilt = 0i64;
    for home in homes {
        let home = ordinary(home)?;
        // 与 python 一致：非 canonical 且无 marker 的 home 跳过（尚未接入共享）。
        if home != canonical && !home.join(".agentcockpit-shared-history.json").is_file() {
            continue;
        }
        rep.clean_internal_catalog(&home, &threads)?;
        for row in threads.values() {
            rep.catalog(&home, row)?;
        }
        rebuilt += 1;
    }
    Ok(json!({"ok": true, "homes": rebuilt, "catalog": rep.report["catalog"]}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// golden 比对用：把各侧基目录绝对路径归一为 <BASE>（python 导入时按目标 home
    /// 重写 rollout_path，属正确行为，两侧基目录必然不同）。其余字符串逐字保留。
    fn normalize_paths(v: Value, base: &str) -> Value {
        match v {
            Value::String(s) => Value::String(s.replace(base, "<BASE>")),
            Value::Object(m) => Value::Object(m.into_iter().map(|(k, x)| (k, normalize_paths(x, base))).collect()),
            Value::Array(a) => Value::Array(a.into_iter().map(|x| normalize_paths(x, base)).collect()),
            other => other,
        }
    }

    /// replace_offset：追加→len(current)；settings-only 后缀→公共前缀字节数；冲突→None。
    #[test]
    fn replace_offset_prefix_settings_conflict() {
        let a = b"{\"text\":\"one\"}\n";
        let b = b"{\"text\":\"one\"}\n{\"text\":\"two\"}\n";
        assert_eq!(replace_offset(a, b), Some(a.len()));
        // current 比 incoming 多一个 settings-only 行 → 返回公共前缀字节数
        let settings = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n";
        let cur = [a.as_slice(), settings].concat();
        assert_eq!(replace_offset(&cur, a), Some(a.len()));
        // 冲突续写 → None
        let c = b"{\"text\":\"DIFFERENT\"}\n";
        assert_eq!(replace_offset(a, c), None);
    }

    /// settings_only / internal_thread 口径。
    #[test]
    fn settings_and_internal_detection() {
        assert!(settings_only(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}"));
        assert!(!settings_only(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"other\"}}"));
        assert!(internal_thread(&json!({"thread_source": "subagent"})));
        assert!(internal_thread(&json!({"thread_source": "guardian_review"})));
        assert!(internal_thread(&json!({"source": "{\"subagent\":{}}"})));
        assert!(!internal_thread(&json!({"source": "{}"})));
        assert!(!internal_thread(&json!({"thread_source": "user"})));
    }

    /// T18 golden：与 python chatgpt-history-sync.py synchronize 输出 + 同步后库内容比对。
    /// 由 tests/chatgpt-history-sync-rust-golden.test.py 触发；未设环境变量时跳过。
    #[test]
    fn golden_matches_python() {
        let Ok(req_path) = std::env::var("T18_SYNC_REQ") else {
            eprintln!("跳过 golden：未设 T18_SYNC_REQ");
            return;
        };
        let request: Value = serde_json::from_str(&std::fs::read_to_string(&req_path).unwrap()).unwrap();
        let py: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("T18_SYNC_PY").unwrap()).unwrap()).unwrap();
        let rs = run(&request).expect("Rust synchronize 应成功");
        eprintln!("DIAG rust report: {}", rs);
        // report 计数与列表逐字段比对（updated_at 时间戳除外）
        for k in ["ok", "version", "homes", "threads", "rollouts", "catalog", "conflicts", "deferred", "missing"] {
            assert_eq!(rs[k], py[k], "report 字段 {k} 不一致：rust={} python={}", rs[k], py[k]);
        }
        // 同步后各 home 的 threads 行 + rollout 内容 + catalog 比对（由驱动写入 py_after）
        if let Ok(after_path) = std::env::var("T18_SYNC_PY_AFTER") {
            let py_after: Value = serde_json::from_str(&std::fs::read_to_string(after_path).unwrap()).unwrap();
            let homes: Vec<PathBuf> = request["homes"].as_array().unwrap().iter()
                .filter_map(|v| v.as_str().map(PathBuf::from)).collect();
            // 与驱动同口径：各侧基目录（homes 的父目录）归一为 <BASE>，其余字段逐字严格比对。
            let base = homes[0].parent().unwrap().to_string_lossy().to_string();
            let mut rs_after = serde_json::Map::new();
            for root in &homes {
                let name = root.file_name().unwrap().to_string_lossy().to_string();
                let conn = connect(&root.join("state_5.sqlite"), false).unwrap();
                let mut threads = rows(&conn, "threads", None).unwrap();
                // python 快照按 ORDER BY id；rusqlite 自然序不保证，比对前同口径排序。
                threads.sort_by(|a, b| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or("")));
                let mut contents = serde_json::Map::new();
                for t in &threads {
                    let tid = t["id"].as_str().unwrap();
                    let rp = t["rollout_path"].as_str().unwrap();
                    let data = std::fs::read(rp).unwrap_or_default();
                    contents.insert(tid.to_string(), json!(String::from_utf8_lossy(&data)));
                }
                rs_after.insert(name, json!({"threads": threads, "contents": contents}));
            }
            let rs_after = normalize_paths(Value::Object(rs_after), &base);
            if rs_after != py_after {
                for (name, pv) in py_after.as_object().unwrap() {
                    let rv = &rs_after[name];
                    let pt = pv["threads"].as_array().unwrap();
                    let rt = rv["threads"].as_array().unwrap();
                    if pt.len() != rt.len() { eprintln!("DIAG {name}: threads len rs={} py={}", rt.len(), pt.len()); }
                    for p in pt {
                        let tid = p["id"].as_str().unwrap_or("?");
                        let r = rt.iter().find(|x| x["id"] == p["id"]).unwrap_or(&Value::Null);
                        if r != p {
                            if let (Some(ro), Some(po)) = (r.as_object(), p.as_object()) {
                                for k in po.keys() {
                                    if ro.get(k) != po.get(k) {
                                        eprintln!("DIAG {name}/{tid}.{k}: rs={:?} py={:?}", ro.get(k), po.get(k));
                                    }
                                }
                            } else { eprintln!("DIAG {name}/{tid}: 行结构不同 rs={r} py={p}"); }
                        }
                    }
                    let pc = pv["contents"].as_object().unwrap();
                    let rc = rv["contents"].as_object().unwrap();
                    for (tid, pcontent) in pc {
                        if rc.get(tid) != Some(pcontent) {
                            eprintln!("DIAG {name}/{tid} content: rs={:?} py={:?}", rc.get(tid), pcontent);
                        }
                    }
                }
            }
            assert_eq!(rs_after, py_after, "同步后库内容不一致");
        }
    }

    /// T18 生产路径 golden：rebuild_catalog 与 python shared 尾段重建逐字段一致。
    /// 由 tests/chatgpt-history-rebuild-rust-golden.test.py 触发；未设环境变量时跳过。
    #[test]
    fn golden_rebuild_matches_python() {
        let Ok(req_path) = std::env::var("T18_REBUILD_REQ") else {
            eprintln!("跳过 rebuild golden：未设 T18_REBUILD_REQ");
            return;
        };
        let request: Value = serde_json::from_str(&std::fs::read_to_string(&req_path).unwrap()).unwrap();
        let canonical = PathBuf::from(request["canonical"].as_str().unwrap());
        let homes: Vec<PathBuf> = request["homes"].as_array().unwrap().iter()
            .filter_map(|v| v.as_str().map(PathBuf::from)).collect();
        let state_dir = PathBuf::from(request["state_dir"].as_str().unwrap());
        let rs = rebuild_catalog(&canonical, &homes, &state_dir).expect("Rust rebuild_catalog 应成功");
        assert_eq!(rs["ok"], json!(true), "rebuild 应 ok：{rs}");
        // 与 python 重建后的各 home codex-dev.db 目录内容比对（驱动写入 py_after）。
        let py_after: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("T18_REBUILD_PY_AFTER").unwrap()).unwrap()).unwrap();
        let base = canonical.parent().unwrap().to_string_lossy().to_string();
        let mut rs_after = serde_json::Map::new();
        for home in &homes {
            let name = home.file_name().unwrap().to_string_lossy().to_string();
            let db = home.join("sqlite").join("codex-dev.db");
            if !db.is_file() { rs_after.insert(name, json!(null)); continue; }
            let conn = connect(&db, false).unwrap();
            let mut cat = rows(&conn, "local_thread_catalog", None).unwrap();
            cat.sort_by(|a, b| {
                let ka = (a["host_id"].as_str().unwrap_or(""), a["thread_id"].as_str().unwrap_or(""));
                let kb = (b["host_id"].as_str().unwrap_or(""), b["thread_id"].as_str().unwrap_or(""));
                ka.cmp(&kb)
            });
            let rev: i64 = conn.query_row("SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id=1", [], |r| r.get(0)).unwrap_or(-1);
            rs_after.insert(name, json!({"catalog": cat, "revision": rev}));
        }
        let rs_after = normalize_paths(Value::Object(rs_after), &base);
        assert_eq!(rs_after, py_after, "rebuild 后目录内容不一致");
    }
}