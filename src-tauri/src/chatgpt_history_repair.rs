//! T18 第三批：chatgpt-history 项目修复迁 Rust（与 python chatgpt-history.py 的
//! project_repair_plan / repair_projects 逐条对齐）。
//!
//! **范围红线**：本模块只迁「项目可见性修复」这一条生产路径（chatgpt_history.rs 的
//! queue_project_repair / apply_pending 调用的 repair_projects mode）。copy_history /
//! shared-history / sync 三条路径涉及目录 junction、整库复制、WAL 退役、凭据隔离，
//! 属数据丢失级，**仍走 python，本模块不触碰**。
//!
//! 安全规则（与 python 版一致）：
//! 1. 路径必须绝对、无 .. 跳转、不含任何目录/文件链接（ordinary）。
//! 2. source/target 不得重叠；marker 所有权必须匹配 repair source。
//! 3. dry_run 只算计划不写盘；apply 前校验 expected_plan 未变、目标 state 未变、
//!    项目未新增对话；先备份 state+db 再事务删除，失败回滚。
//! 4. 只删「旧工作区列表自动重建的无对话项目」，受保护/被引用/不确定的保留。

use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = ".codex-global-state.json";
const MARKER_FILE: &str = ".agentcockpit-history.json";
const LOCAL_PROJECT_MIGRATION: &str = "2026-07-13-local-projects";
pub const VISIBILITY_REPAIR_VERSION: u64 = 2;

/// 与 python ordinary 对齐：绝对、无 ..、自身及所有祖先不含链接。
pub fn ordinary(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err("History paths must be absolute and cannot traverse parents".into());
    }
    for part in path.ancestors() {
        if crate::registry::is_reparse_point(part) {
            return Err("History path contains a directory/file link".into());
        }
    }
    Ok(path.to_path_buf())
}

/// 与 python key 对齐：Windows 路径 normcase+normpath（小写、正斜杠、去冗余）。
pub fn key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    // normpath：折叠重复斜杠、去尾部斜杠（保留根）、解析 . 与 ..
    let mut parts: Vec<&str> = Vec::new();
    let drive_root = if normalized.len() >= 2 && normalized.as_bytes()[1] == b':' {
        Some(&normalized[..2])
    } else if normalized.starts_with("//") {
        Some("//")
    } else if normalized.starts_with('/') {
        Some("/")
    } else {
        None
    };
    let body = match drive_root {
        Some(d) => &normalized[d.len()..],
        None => normalized.as_str(),
    };
    for seg in body.split('/') {
        match seg {
            "" | "." => continue,
            ".." => { parts.pop(); }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    let out = match drive_root {
        Some(d) if d == "//" => format!("//{}", joined),
        Some(d) => format!("{}/{}", d, joined),
        None => joined,
    };
    out.to_lowercase()
}

/// 与 python read_state 对齐：utf-8-sig JSON object；空文件→{}；非 object 报错。
pub fn read_state(path: &Path) -> Result<Value, String> {
    ordinary(path)?;
    if !path.is_file() {
        return Err("History metadata is not a regular file".into());
    }
    let bytes = std::fs::read(path).map_err(|_| "History metadata is not a regular file".to_string())?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    let text = String::from_utf8_lossy(bytes);
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| "History project metadata is not an object".to_string())?;
    if !value.is_object() {
        return Err("History project metadata is not an object".into());
    }
    Ok(value)
}

/// 与 python project_visibility_state 对齐：已完成迁移的来源才剪枝旧工作区列表。
pub fn project_visibility_state(state: &Value) -> Value {
    let mut state = state.clone();
    let projects = state.get("local-projects").filter(|p| p.is_object()).cloned();
    let completed = state.get("electron-completed-local-data-migration-ids")
        .and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let has_migration = completed.iter().any(|v| v.as_str() == Some(LOCAL_PROJECT_MIGRATION));
    let projects = match projects {
        Some(p) if has_migration => p,
        _ => return state,
    };
    let mut active: BTreeSet<String> = BTreeSet::new();
    if let Some(o) = projects.as_object() {
        for p in o.values() {
            if let Some(roots) = p.get("rootPaths").and_then(|r| r.as_array()) {
                for path in roots {
                    if let Some(s) = path.as_str() { active.insert(key(s)); }
                }
            }
        }
    }
    for field in ["electron-saved-workspace-roots", "saved-workspace-roots", "workspace-root-order"] {
        if let Some(arr) = state.get(field).and_then(|v| v.as_array()).cloned() {
            let kept: Vec<Value> = arr.into_iter()
                .filter(|p| p.as_str().map(|s| active.contains(&key(s))).unwrap_or(false))
                .collect();
            state[field] = json!(kept);
        }
    }
    state
}

/// 与 python database_paths 对齐：根目录 state_N/thread_history_N/thread_catalog[_N].sqlite
/// + sqlite/ 子树（含 codex-dev.db）。只收 regular file，排序。
fn database_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if is_database_name(&p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()) {
                found.push(p);
            }
        }
    }
    let nested = root.join("sqlite");
    if nested.exists() || crate::registry::is_reparse_point(&nested) {
        ordinary(&nested)?;
        if !nested.is_dir() {
            return Err("History SQLite directory is not a directory".into());
        }
        let mut stack = vec![nested.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() { stack.push(p); }
                else {
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if is_database_name(&name) || p == nested.join("codex-dev.db") {
                        found.push(p);
                    }
                }
            }
        }
    }
    found.sort();
    for p in &found {
        ordinary(p)?;
        if !p.is_file() {
            return Err("History database is not a regular file".into());
        }
    }
    Ok(found)
}

/// DATABASE_NAME 正则：state_\d+ / thread_history_\d+ / thread_catalog(_\d+)? .sqlite（忽略大小写）。
fn is_database_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    let stem = match lower.strip_suffix(".sqlite") {
        Some(s) => s,
        None => return false,
    };
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

/// 与 python primary_state_db 对齐：根目录 state_N.sqlite 取版本号最大者。
fn primary_state_db(root: &Path) -> Result<Option<PathBuf>, String> {
    let mut best: Option<(u64, PathBuf)> = None;
    for p in database_paths(root)? {
        if p.parent() != Some(root) { continue; }
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let lower = name.to_lowercase();
        if let Some(num) = lower.strip_prefix("state_").and_then(|s| s.strip_suffix(".sqlite")) {
            if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(v) = num.parse::<u64>() {
                    if best.as_ref().map(|(bv, _)| v > *bv).unwrap_or(true) {
                        best = Some((v, p));
                    }
                }
            }
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// 只读打开 sqlite（mode=ro + query_only），与 python readonly 对齐。
fn readonly(path: &Path) -> Result<rusqlite::Connection, String> {
    ordinary(path)?;
    for suffix in ["-wal", "-shm"] {
        ordinary(&PathBuf::from(format!("{}{}", path.display(), suffix)))?;
    }
    let conn = rusqlite::Connection::open_with_flags(
        path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI)
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "query_only", "ON").ok();
    Ok(conn)
}

fn tables(conn: &rusqlite::Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>, String> {
    let sql = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// 与 python _project_rows 对齐：读 primary state db 的 projects/project_roots/threads 引用。
fn project_rows(root: &Path) -> Result<(HashMap<String, Value>, BTreeSet<String>, Option<PathBuf>), String> {
    let database = match primary_state_db(root)? {
        Some(d) => d,
        None => return Ok((HashMap::new(), BTreeSet::new(), None)),
    };
    let conn = readonly(&database)?;
    let names: BTreeSet<String> = tables(&conn)?.into_iter().collect();
    if !names.contains("projects") || !names.contains("project_roots") {
        return Ok((HashMap::new(), BTreeSet::new(), Some(database)));
    }
    let needed = ["id", "name", "metadata", "created_at_ms"];
    let cols = columns(&conn, "projects")?;
    if !needed.iter().all(|n| cols.iter().any(|c| c == n)) {
        return Ok((HashMap::new(), BTreeSet::new(), Some(database)));
    }
    let mut projects: HashMap<String, Value> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id,name,metadata,created_at_ms FROM projects")
            .map_err(|e| e.to_string())?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?, r.get::<_, Option<f64>>(3)?))
        }).map_err(|e| e.to_string())?;
        for row in rows.filter_map(|r| r.ok()) {
            let (id, name, metadata, created) = row;
            let mut roots: Vec<Value> = Vec::new();
            let mut rstmt = conn.prepare("SELECT path FROM project_roots WHERE project_id=?1 ORDER BY position")
                .map_err(|e| e.to_string())?;
            let rrows = rstmt.query_map([&id], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
            for rr in rrows.filter_map(|r| r.ok()) { roots.push(json!(rr)); }
            projects.insert(id, json!({"name": name, "metadata": metadata, "created": created, "roots": roots}));
        }
    }
    let mut used: BTreeSet<String> = BTreeSet::new();
    if names.contains("threads") && columns(&conn, "threads")?.iter().any(|c| c == "project_id") {
        let mut stmt = conn.prepare("SELECT DISTINCT project_id FROM threads WHERE project_id IS NOT NULL")
            .map_err(|e| e.to_string())?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
        for r in rows.filter_map(|r| r.ok()) { used.insert(r); }
    }
    Ok((projects, used, Some(database)))
}

/// 与 python project_repair_plan 对齐：只识别复制首启的旧工作区迁移残留项目。
pub fn project_repair_plan(source: &Path, target: &Path) -> Result<Value, String> {
    let source = ordinary(source)?.canonicalize().map_err(|e| e.to_string())?;
    let target = ordinary(target)?.canonicalize().map_err(|e| e.to_string())?;
    let source = crate::registry::strip_verbatim(&source);
    let target = crate::registry::strip_verbatim(&target);
    if source == target || target.starts_with(&source) || source.starts_with(&target) {
        return Err("Source and target history directories overlap".into());
    }
    let marker = read_state(&target.join(MARKER_FILE))?;
    if key(marker.get("source").and_then(|v| v.as_str()).unwrap_or("")) != key(&source.to_string_lossy()) {
        return Err("History copy ownership does not match the repair source".into());
    }
    let original = read_state(&source.join(STATE_FILE))?;
    let current = read_state(&target.join(STATE_FILE))?;
    let completed = original.get("electron-completed-local-data-migration-ids")
        .and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let base = original.get("local-projects").filter(|v| v.is_object()).cloned();
    let present = current.get("local-projects").filter(|v| v.is_object()).cloned();
    let ineligible = |msg: &str| json!({"eligible": false, "legacy_ids": [], "project_ids": [],
        "uncertain": 0, "message": msg});
    let has_migration = completed.iter().any(|v| v.as_str() == Some(LOCAL_PROJECT_MIGRATION));
    let (base, present) = match (base, present) {
        (Some(b), Some(p)) if has_migration => (b, p),
        _ => return Ok(ineligible("来源没有已完成的项目迁移记录，保留现有项目")),
    };
    let mut visible: BTreeSet<String> = BTreeSet::new();
    if let Some(o) = base.as_object() {
        for v in o.values() {
            if let Some(roots) = v.get("rootPaths").and_then(|r| r.as_array()) {
                for p in roots { if let Some(s) = p.as_str() { visible.insert(key(s)); } }
            }
        }
    }
    let mut stale: BTreeSet<String> = BTreeSet::new();
    if let Some(arr) = original.get("electron-saved-workspace-roots").and_then(|v| v.as_array()) {
        for p in arr {
            if let Some(s) = p.as_str() {
                let k = key(s);
                if !visible.contains(&k) { stale.insert(k); }
            }
        }
    }
    let (source_db, _, _) = project_rows(&source)?;
    let (target_db, mut referenced, database) = project_rows(&target)?;
    if database.is_none() || target_db.is_empty() {
        return Ok(ineligible("无法核对目标项目与对话引用，保留现有项目"));
    }
    if let Some(assigned) = current.get("thread-project-assignments").and_then(|v| v.as_object()) {
        for v in assigned.values() {
            if let Some(pid) = v.get("projectId").and_then(|p| p.as_str()) {
                if !pid.is_empty() { referenced.insert(pid.to_string()); }
            }
        }
    }
    let mut protected: BTreeSet<String> = BTreeSet::new();
    if let Some(arr) = current.get("pinned-project-ids").and_then(|v| v.as_array()) {
        for v in arr { if let Some(s) = v.as_str() { protected.insert(s.to_string()); } }
    }
    if let Some(pid) = current.get("selected-project").and_then(|v| v.get("projectId")).and_then(|p| p.as_str()) {
        if !pid.is_empty() { protected.insert(pid.to_string()); }
    }
    let created_at = marker.get("copied_at").and_then(|v| v.as_f64()).unwrap_or(0.0) * 1000.0;
    let mut candidates: Vec<String> = Vec::new();
    let mut modern: BTreeSet<String> = BTreeSet::new();
    let mut uncertain = 0u64;
    if let Some(present_obj) = present.as_object() {
        for (identifier, value) in present_obj {
            if base.get(identifier).is_some() || !value.is_object() { continue; }
            let roots = value.get("rootPaths").and_then(|r| r.as_array()).cloned().unwrap_or_default();
            let paths: BTreeSet<String> = roots.iter().filter_map(|p| p.as_str()).map(key).collect();
            if paths.is_empty() || !paths.iter().all(|p| stale.contains(p)) { continue; }
            let created = value.get("createdAt").and_then(|v| v.as_f64());
            let updated = value.get("updatedAt").and_then(|v| v.as_f64());
            let first_root = roots.first().and_then(|p| p.as_str()).unwrap_or("");
            let canonical_name = windows_basename(first_root);
            let matching: Vec<(&String, &Value)> = target_db.iter()
                .filter(|(pid, proj)| {
                    !source_db.contains_key(*pid) && {
                        let proj_paths: BTreeSet<String> = proj.get("roots").and_then(|r| r.as_array())
                            .map(|a| a.iter().filter_map(|p| p.as_str()).map(key).collect())
                            .unwrap_or_default();
                        proj_paths == paths
                    }
                }).collect();
            let mut safe = identifier.starts_with("local-")
                && !referenced.contains(identifier) && !protected.contains(identifier)
                && created.is_some() && updated.is_some()
                && created.map(|c| created_at - 30000.0 <= c && c <= created_at + 180000.0).unwrap_or(false)
                && created.zip(updated).map(|(c, u)| (u - c).abs() <= 1000.0).unwrap_or(false)
                && value.get("name").and_then(|v| v.as_str()) == Some(canonical_name.as_str());
            for (pid, project) in &matching {
                safe = safe && !referenced.contains(*pid) && !protected.contains(*pid)
                    && project.get("name").and_then(|v| v.as_str()) == value.get("name").and_then(|v| v.as_str());
                let meta_ok = project.get("metadata").map(|m| {
                    m.is_null() || m.as_str() == Some("{}") || m.as_str() == Some("")
                }).unwrap_or(false);
                let proj_created = project.get("created").and_then(|v| v.as_f64());
                safe = safe && meta_ok
                    && created.zip(proj_created).map(|(c, pc)| c <= pc && pc <= c + 180000.0).unwrap_or(false);
            }
            if safe {
                candidates.push(identifier.clone());
                for (pid, _) in matching { modern.insert(pid.clone()); }
            } else {
                uncertain += 1;
            }
        }
    }
    let mut modern_sorted: Vec<String> = modern.into_iter().collect();
    modern_sorted.sort();
    let legacy_count = candidates.len();
    let project_count = modern_sorted.len();
    Ok(json!({"eligible": true, "legacy_ids": candidates, "project_ids": modern_sorted,
        "uncertain": uncertain, "legacy_count": legacy_count, "project_count": project_count,
        "message": "已识别旧工作区列表自动重建的无对话项目"}))
}

/// 与 python pathlib.PureWindowsPath(root).name / Path(root).name 对齐：取路径最后一段。
fn windows_basename(root: &str) -> String {
    let normalized = root.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or("").to_string()
}

/// 与 python repair_projects 对齐：dry_run 只算计划；apply 备份+事务删除+失败回滚。
pub fn repair_projects(source: &Path, target: &Path, dry_run: bool, expected_plan: Option<&Value>) -> Result<Value, String> {
    let source = ordinary(source)?.canonicalize().map_err(|e| e.to_string())?;
    let target = ordinary(target)?.canonicalize().map_err(|e| e.to_string())?;
    let source = crate::registry::strip_verbatim(&source);
    let target = crate::registry::strip_verbatim(&target);
    let plan = project_repair_plan(&source, &target)?;
    if dry_run || plan["eligible"] != true {
        return Ok(plan);
    }
    if let Some(expected) = expected_plan {
        for k in ["legacy_ids", "project_ids"] {
            if plan.get(k) != expected.get(k) {
                return Err("Project metadata changed after repair was queued; review it again".into());
            }
        }
    }
    let state_file = target.join(STATE_FILE);
    let original_bytes = std::fs::read(&state_file).map_err(|e| e.to_string())?;
    let mut current: Value = serde_json::from_slice(&original_bytes).map_err(|e| e.to_string())?;
    let original = read_state(&source.join(STATE_FILE))?;
    let remove_legacy: BTreeSet<String> = plan["legacy_ids"].as_array().unwrap().iter()
        .filter_map(|v| v.as_str().map(String::from)).collect();
    let remove_modern: BTreeSet<String> = plan["project_ids"].as_array().unwrap().iter()
        .filter_map(|v| v.as_str().map(String::from)).collect();
    let remove_ids: BTreeSet<String> = remove_legacy.union(&remove_modern).cloned().collect();
    // local-projects：删 legacy
    if let Some(lp) = current.get("local-projects").and_then(|v| v.as_object()).cloned() {
        let kept: serde_json::Map<String, Value> = lp.into_iter()
            .filter(|(k, _)| !remove_legacy.contains(k)).collect();
        current["local-projects"] = Value::Object(kept);
    }
    // project-order / pinned-project-ids：删 legacy|modern
    for field in ["project-order", "pinned-project-ids"] {
        if let Some(arr) = current.get(field).and_then(|v| v.as_array()).cloned() {
            let kept: Vec<Value> = arr.into_iter()
                .filter(|v| v.as_str().map(|s| !remove_ids.contains(s)).unwrap_or(true)).collect();
            current[field] = json!(kept);
        }
    }
    // project-appearances / sidebar-project-thread-orders：删键
    for field in ["project-appearances", "sidebar-project-thread-orders"] {
        if let Some(obj) = current.get(field).and_then(|v| v.as_object()).cloned() {
            let kept: serde_json::Map<String, Value> = obj.into_iter()
                .filter(|(k, _)| !remove_ids.contains(k)).collect();
            current[field] = Value::Object(kept);
        }
    }
    // app-server-project-id-by-legacy-project-id-by-host：删 legacy 键 + 值为 modern 的项
    if let Some(hosts) = current.get("app-server-project-id-by-legacy-project-id-by-host")
        .and_then(|v| v.as_object()).cloned() {
        let mut new_hosts = serde_json::Map::new();
        for (host, mapping) in hosts {
            if let Some(m) = mapping.as_object() {
                let kept: serde_json::Map<String, Value> = m.iter()
                    .filter(|(k, v)| !remove_legacy.contains(*k)
                        && !(v.as_str().map(|s| remove_modern.contains(s)).unwrap_or(false)))
                    .map(|(k, v)| (k.clone(), v.clone())).collect();
                new_hosts.insert(host, Value::Object(kept));
            } else {
                new_hosts.insert(host, mapping);
            }
        }
        current["app-server-project-id-by-legacy-project-id-by-host"] = Value::Object(new_hosts);
    }
    // 迁移完成记录：保留本客户端新增的，合并来源的
    for field in ["electron-local-data-migration-id", "electron-completed-local-data-migration-ids",
                  "remote-project-connection-backfill-completed"] {
        if original.get(field).is_some() {
            if field.ends_with("migration-ids") {
                let mut merged: Vec<Value> = Vec::new();
                let mut seen: BTreeSet<String> = BTreeSet::new();
                let cur_arr = current.get(field).and_then(|v| v.as_array()).cloned().unwrap_or_default();
                let orig_arr = original.get(field).and_then(|v| v.as_array()).cloned().unwrap_or_default();
                for v in cur_arr.into_iter().chain(orig_arr.into_iter()) {
                    let k = serde_json::to_string(&v).unwrap_or_default();
                    if seen.insert(k) { merged.push(v); }
                }
                current[field] = json!(merged);
            } else if current.get(field).map(|v| v.is_null()).unwrap_or(true) {
                current[field] = original[field].clone();
            }
        }
    }
    let current = project_visibility_state(&current);
    let next_bytes = serde_json::to_vec(&current).map_err(|e| e.to_string())?;
    // 备份 state + db
    let backup_id = uuid_hex();
    let backup = target.join(".project-repair-backups").join(&backup_id);
    ordinary(&backup)?;
    std::fs::create_dir_all(&backup).map_err(|e| e.to_string())?;
    std::fs::write(backup.join(STATE_FILE), &original_bytes).map_err(|e| e.to_string())?;
    let database = primary_state_db(&target)?.ok_or("History database is missing")?;
    let db_name = database.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    {
        let origin = readonly(&database)?;
        let mut copied = rusqlite::Connection::open(backup.join(&db_name)).map_err(|e| e.to_string())?;
        let backup_api = rusqlite::backup::Backup::new(&origin, &mut copied).map_err(|e| e.to_string())?;
        backup_api.run_to_completion(256, std::time::Duration::from_millis(50), None)
            .map_err(|e| e.to_string())?;
    }
    // 事务删除 modern 项目 + 写 state，失败回滚
    let mut con = rusqlite::Connection::open(&database).map_err(|e| e.to_string())?;
    con.pragma_update(None, "foreign_keys", "ON").ok();
    con.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let mut state_written = false;
    let result = (|| -> Result<(), String> {
        if std::fs::read(&state_file).map_err(|e| e.to_string())? != original_bytes {
            return Err("Target project state changed; repair was not applied".into());
        }
        for identifier in &remove_modern {
            let has_thread: bool = con.query_row(
                "SELECT 1 FROM threads WHERE project_id=?1 LIMIT 1", [identifier], |_| Ok(true))
                .optional().map_err(|e| e.to_string())?.unwrap_or(false);
            if has_thread {
                return Err("A project gained a conversation; its registration was retained".into());
            }
            let tbls = tables(&con)?;
            for table in ["project_roots", "project_idempotency_keys"] {
                if tbls.iter().any(|t| t == table) {
                    con.execute(&format!("DELETE FROM \"{}\" WHERE project_id=?1", table), [identifier])
                        .map_err(|e| e.to_string())?;
                }
            }
            con.execute("DELETE FROM projects WHERE id=?1", [identifier]).map_err(|e| e.to_string())?;
        }
        atomic_bytes(&state_file, &next_bytes)?;
        state_written = true;
        con.execute_batch("COMMIT").map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = con.execute_batch("ROLLBACK");
        if state_written {
            if let Ok(now_bytes) = std::fs::read(&state_file) {
                if now_bytes == next_bytes {
                    let _ = atomic_bytes(&state_file, &original_bytes);
                }
            }
        }
        return Err(error);
    }
    let repaired_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_secs_f64();
    let mut result = plan.as_object().cloned().unwrap_or_default();
    result.insert("applied".into(), json!(true));
    result.insert("backup".into(), json!(backup.to_string_lossy()));
    result.insert("repaired_at".into(), json!(repaired_at));
    result.insert("visibility_version".into(), json!(VISIBILITY_REPAIR_VERSION));
    let result = Value::Object(result);
    atomic_bytes(&target.join(".agentcockpit-project-visibility.json"),
        &serde_json::to_vec(&result).map_err(|e| e.to_string())?)?;
    Ok(result)
}

/// 与 python _atomic_bytes 对齐：独占临时文件 + fsync + os.replace。
fn atomic_bytes(path: &Path, data: &[u8]) -> Result<(), String> {
    ordinary(path)?;
    let temporary = path.with_file_name(format!(".project-repair-{}.tmp", uuid_hex()));
    let write_result = (|| -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)
            .map_err(|e| e.to_string())?;
        use std::io::Write;
        f.write_all(data).map_err(|e| e.to_string())?;
        f.flush().map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return write_result;
    }
    let rename = std::fs::rename(&temporary, path).map_err(|e| e.to_string());
    if rename.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    rename
}

/// 简易 uuid hex（无新增依赖：时间戳+进程id+计数器的 sha256 前 32 位）。
fn uuid_hex() -> String {
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut h = Sha256::new();
    h.update(format!("{}:{}:{}", nanos, std::process::id(), n).as_bytes());
    let out = h.finalize();
    out.iter().take(16).map(|b| format!("{:02x}", b)).collect()
}

/// 与 python run(request) 的 repair_projects mode 对齐。
pub fn run(request: &Value) -> Result<Value, String> {
    let source = PathBuf::from(request.get("source").and_then(|v| v.as_str()).unwrap_or(""));
    let target = PathBuf::from(request.get("target").and_then(|v| v.as_str()).unwrap_or(""));
    let dry_run = request.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(true);
    let expected = request.get("expected_plan").filter(|v| !v.is_null());
    repair_projects(&source, &target, dry_run, expected)
}

/// rusqlite optional 扩展（query_row 无行→None）。
trait OptionalRow<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}
impl<T> OptionalRow<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个合成 source/target 对：source 有已完成迁移 + local-projects + 旧工作区列表；
    /// target 是复制首启重建了无对话项目（marker 所有权匹配）。
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        // CI runner 的 TEMP 可能含 8.3 短名段（如 RUNNER~1）：plan/apply 里 canonicalize
        // 会展开成长名，marker 若写原始拼写则 key 不等 → ownership 误判。
        // 与生产口径对齐（copy_history 写 marker 前也先 resolve），fixture 同样先展开。
        let source = source.canonicalize().unwrap();
        let target = target.canonicalize().unwrap();
        let source = crate::registry::strip_verbatim(&source);
        let target = crate::registry::strip_verbatim(&target);
        // source state：已完成迁移，local-projects 有 proj-a（rootPaths=C:/Work/A），
        // 旧工作区列表含 C:/Work/A 与 C:/Work/Stale（后者不在 local-projects → stale）
        let source_state = json!({
            "electron-completed-local-data-migration-ids": [LOCAL_PROJECT_MIGRATION],
            "local-projects": {"proj-a": {"rootPaths": ["C:/Work/A"], "name": "A"}},
            "electron-saved-workspace-roots": ["C:/Work/A", "C:/Work/Stale"],
            "saved-workspace-roots": ["C:/Work/A", "C:/Work/Stale"],
            "workspace-root-order": ["C:/Work/A", "C:/Work/Stale"],
        });
        std::fs::write(source.join(STATE_FILE), serde_json::to_vec(&source_state).unwrap()).unwrap();
        // source state_5.sqlite：projects 表有 proj-a
        let sdb = source.join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&sdb).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects(id TEXT PRIMARY KEY, name TEXT, metadata TEXT, created_at_ms REAL);
             CREATE TABLE project_roots(project_id TEXT, path TEXT, position INTEGER);
             CREATE TABLE threads(id TEXT PRIMARY KEY, project_id TEXT);
             INSERT INTO projects VALUES('proj-a','A','{}',1000.0);
             INSERT INTO project_roots VALUES('proj-a','C:/Work/A',0);").unwrap();
        drop(conn);
        // target marker：source 所有权 + copied_at
        let copied_at = 1_700_000_000.0_f64;
        let marker = json!({"source": source.to_string_lossy(), "copied_at": copied_at,
            "threads": 0, "projects": 0, "rollouts": 0});
        std::fs::write(target.join(MARKER_FILE), serde_json::to_vec(&marker).unwrap()).unwrap();
        // target state：复制首启重建了 local-stale（rootPaths=C:/Work/Stale，createdAt 在窗口内）
        let target_state = json!({
            "electron-completed-local-data-migration-ids": [LOCAL_PROJECT_MIGRATION],
            "local-projects": {
                "proj-a": {"rootPaths": ["C:/Work/A"], "name": "A"},
                "local-stale": {"rootPaths": ["C:/Work/Stale"], "name": "Stale",
                    "createdAt": copied_at * 1000.0 + 5000.0, "updatedAt": copied_at * 1000.0 + 5500.0}
            },
            "electron-saved-workspace-roots": ["C:/Work/A", "C:/Work/Stale"],
            "project-order": ["proj-a", "local-stale"],
            "pinned-project-ids": [],
            "thread-project-assignments": {},
        });
        std::fs::write(target.join(STATE_FILE), serde_json::to_vec(&target_state).unwrap()).unwrap();
        // target state_5.sqlite：projects 有 proj-a + modern-stale（对应 local-stale 的重建）
        let tdb = target.join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&tdb).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects(id TEXT PRIMARY KEY, name TEXT, metadata TEXT, created_at_ms REAL);
             CREATE TABLE project_roots(project_id TEXT, path TEXT, position INTEGER);
             CREATE TABLE threads(id TEXT PRIMARY KEY, project_id TEXT);
             INSERT INTO projects VALUES('proj-a','A','{}',1000.0);
             INSERT INTO projects VALUES('modern-stale','Stale','{}',1700000005000.0);
             INSERT INTO project_roots VALUES('proj-a','C:/Work/A',0);
             INSERT INTO project_roots VALUES('modern-stale','C:/Work/Stale',0);").unwrap();
        drop(conn);
        (tmp, source, target)
    }

    /// dry_run：识别 local-stale + modern-stale，不写盘。
    #[test]
    fn plan_dry_run_identifies_legacy() {
        let (_tmp, source, target) = setup();
        let state_before = std::fs::read(target.join(STATE_FILE)).unwrap();
        let plan = project_repair_plan(&source, &target).unwrap();
        assert_eq!(plan["eligible"], true);
        assert_eq!(plan["legacy_ids"], json!(["local-stale"]));
        assert_eq!(plan["project_ids"], json!(["modern-stale"]));
        assert_eq!(plan["legacy_count"], 1);
        assert_eq!(plan["project_count"], 1);
        // dry_run 不写盘
        let after = repair_projects(&source, &target, true, None).unwrap();
        assert_eq!(after["eligible"], true);
        assert!(after.get("applied").is_none());
        assert_eq!(std::fs::read(target.join(STATE_FILE)).unwrap(), state_before);
    }

    /// apply：删 legacy+modern，备份存在，state 剪枝，db 项目删除。
    #[test]
    fn apply_removes_and_backs_up() {
        let (_tmp, source, target) = setup();
        let plan = project_repair_plan(&source, &target).unwrap();
        let result = repair_projects(&source, &target, false, Some(&plan)).unwrap();
        assert_eq!(result["applied"], true);
        assert_eq!(result["visibility_version"], 2);
        let backup = PathBuf::from(result["backup"].as_str().unwrap());
        assert!(backup.join(STATE_FILE).is_file(), "state 备份应存在");
        assert!(backup.join("state_5.sqlite").is_file(), "db 备份应存在");
        // state：local-stale 没了，proj-a 保留
        let state = read_state(&target.join(STATE_FILE)).unwrap();
        assert!(state["local-projects"].get("local-stale").is_none());
        assert!(state["local-projects"].get("proj-a").is_some());
        assert_eq!(state["project-order"], json!(["proj-a"]));
        // db：modern-stale 删了，proj-a 保留
        let conn = readonly(&target.join("state_5.sqlite")).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM projects WHERE id='modern-stale'", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
        let count_a: i64 = conn.query_row("SELECT COUNT(*) FROM projects WHERE id='proj-a'", [], |r| r.get(0)).unwrap();
        assert_eq!(count_a, 1);
        // 可见性记录文件
        assert!(target.join(".agentcockpit-project-visibility.json").is_file());
    }

    /// expected_plan 变了 → 拒绝 apply。
    #[test]
    fn apply_rejects_changed_plan() {
        let (_tmp, source, target) = setup();
        let mut wrong = project_repair_plan(&source, &target).unwrap();
        wrong["legacy_ids"] = json!(["different"]);
        let err = repair_projects(&source, &target, false, Some(&wrong)).unwrap_err();
        assert!(err.contains("changed after repair was queued"));
    }

    /// 项目新增对话 → 拒绝删除（保留登记）。
    #[test]
    fn apply_rejects_project_with_thread() {
        let (_tmp, source, target) = setup();
        // 给 modern-stale 加一条对话
        let conn = rusqlite::Connection::open(target.join("state_5.sqlite")).unwrap();
        conn.execute("INSERT INTO threads VALUES('t1','modern-stale')", []).unwrap();
        drop(conn);
        let plan = project_repair_plan(&source, &target).unwrap();
        // plan 里 modern-stale 被引用 → 不再是候选
        assert_eq!(plan["project_ids"], json!([]));
    }

    /// apply 守卫：目标项目新增对话 → 拒绝删除（保留登记）。
    /// plan 阶段被引用的项目本就不进候选；此守卫防 plan→apply 之间新增对话的竞态。
    /// 加对话后重算 plan（模拟 apply 内部重算），legacy_ids 不变但 modern 被引用移出候选，
    /// expected_plan 校验先拦（plan 变了）；若绕过 expected_plan 直接 apply，has_thread 守卫拦。
    #[test]
    fn apply_guard_rejects_thread_added_after_plan() {
        let (_tmp, source, target) = setup();
        let plan = project_repair_plan(&source, &target).unwrap();
        assert_eq!(plan["project_ids"], json!(["modern-stale"]));
        // plan 之后给 modern-stale 加对话（模拟竞态）
        let conn = rusqlite::Connection::open(target.join("state_5.sqlite")).unwrap();
        conn.execute("INSERT INTO threads VALUES('t1','modern-stale')", []).unwrap();
        drop(conn);
        // 用旧 plan 作 expected_plan → expected_plan 校验拦（plan 已变）
        let err = repair_projects(&source, &target, false, Some(&plan)).unwrap_err();
        assert!(err.contains("changed after repair was queued"), "expected_plan 校验应拦：{err}");
        // 项目仍在（未删）
        let conn = readonly(&target.join("state_5.sqlite")).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM projects WHERE id='modern-stale'", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    /// apply has_thread 守卫：绕过 expected_plan（None），直接 apply 时项目已有对话 → 拒绝。
    #[test]
    fn apply_has_thread_guard_without_expected_plan() {
        let (_tmp, source, target) = setup();
        // 先给 modern-stale 加对话，但手工构造一个仍含 modern-stale 的 plan 绕过 plan 重算
        let conn = rusqlite::Connection::open(target.join("state_5.sqlite")).unwrap();
        conn.execute("INSERT INTO threads VALUES('t1','modern-stale')", []).unwrap();
        drop(conn);
        // 直接调 repair_projects dry_run=false expected_plan=None：内部重算 plan 会把 modern-stale
        // 移出候选（被引用），所以实际不会删它。验证项目保留。
        let result = repair_projects(&source, &target, false, None).unwrap();
        let conn = readonly(&target.join("state_5.sqlite")).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM projects WHERE id='modern-stale'", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "被引用的项目不应被删除");
        assert_eq!(result["project_ids"], json!([]));
    }

    /// marker 所有权不匹配 → 报错。
    #[test]
    fn plan_rejects_wrong_ownership() {
        let (_tmp, source, target) = setup();
        let marker = json!({"source": "C:/somewhere/else", "copied_at": 1.0});
        std::fs::write(target.join(MARKER_FILE), serde_json::to_vec(&marker).unwrap()).unwrap();
        let err = project_repair_plan(&source, &target).unwrap_err();
        assert!(err.contains("ownership does not match"));
    }

    /// source/target 重叠 → 报错。
    #[test]
    fn plan_rejects_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let err = project_repair_plan(&a, &a).unwrap_err();
        assert!(err.contains("overlap"));
    }

    /// key：Windows 路径 normcase（小写、正斜杠）。
    #[test]
    fn key_normalizes_windows_path() {
        assert_eq!(key("C:\\Work\\A"), "c:/work/a");
        assert_eq!(key("C:/Work/A/"), "c:/work/a");
        assert_eq!(key("C:\\Work\\..\\Other"), "c:/other");
    }

    /// project_visibility_state：未完成迁移 → 不剪枝。
    #[test]
    fn visibility_no_migration_no_prune() {
        let state = json!({
            "local-projects": {"p": {"rootPaths": ["C:/A"]}},
            "electron-saved-workspace-roots": ["C:/A", "C:/B"],
        });
        let out = project_visibility_state(&state);
        assert_eq!(out["electron-saved-workspace-roots"], json!(["C:/A", "C:/B"]));
    }

    /// T18 golden：dry_run plan 与 python project_repair_plan 逐字段比对。
    #[test]
    fn golden_plan_matches_python() {
        let Ok(source) = std::env::var("T18C_SOURCE") else {
            eprintln!("跳过 golden：未设 T18C_SOURCE");
            return;
        };
        let target = std::env::var("T18C_TARGET").unwrap();
        let py: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("T18C_PY_PLAN").unwrap()).unwrap()).unwrap();
        let rs = project_repair_plan(Path::new(&source), Path::new(&target)).unwrap();
        for k in ["eligible", "legacy_ids", "project_ids", "uncertain", "legacy_count", "project_count"] {
            assert_eq!(rs[k], py[k], "plan 字段 {k} 不一致：rust={} python={}", rs[k], py[k]);
        }
    }

    /// T18 golden：apply 后 state+db 与 python repair_projects 结果比对。
    #[test]
    fn golden_apply_matches_python() {
        let Ok(source) = std::env::var("T18C_SOURCE") else {
            eprintln!("跳过 golden：未设 T18C_SOURCE");
            return;
        };
        let target = std::env::var("T18C_TARGET").unwrap();
        let py: Value = serde_json::from_str(
            &std::fs::read_to_string(std::env::var("T18C_PY_APPLY").unwrap()).unwrap()).unwrap();
        let target_path = Path::new(&target);
        let plan = project_repair_plan(Path::new(&source), target_path).unwrap();
        let rs = repair_projects(Path::new(&source), target_path, false, Some(&plan)).unwrap();
        assert_eq!(rs["applied"], py["result"]["applied"], "applied 不一致");
        assert_eq!(rs["legacy_ids"], py["result"]["legacy_ids"], "legacy_ids 不一致");
        assert_eq!(rs["project_ids"], py["result"]["project_ids"], "project_ids 不一致");
        assert_eq!(rs["visibility_version"], py["result"]["visibility_version"], "visibility_version 不一致");
        // state 比对（local-projects / project-order / 工作区列表剪枝）
        let rs_state = read_state(&target_path.join(STATE_FILE)).unwrap();
        assert_eq!(rs_state["local-projects"], py["state"]["local-projects"], "local-projects 不一致");
        assert_eq!(rs_state["project-order"], py["state"]["project-order"], "project-order 不一致");
        assert_eq!(rs_state["electron-saved-workspace-roots"], py["state"]["electron-saved-workspace-roots"],
            "工作区列表剪枝不一致");
        // db projects 比对
        let conn = readonly(&target_path.join("state_5.sqlite")).unwrap();
        let mut stmt = conn.prepare("SELECT id,name,metadata FROM projects").unwrap();
        let rs_db: Value = {
            let mut obj = serde_json::Map::new();
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))
            }).unwrap();
            for row in rows.filter_map(|r| r.ok()) {
                obj.insert(row.0, json!({"name": row.1, "metadata": row.2}));
            }
            Value::Object(obj)
        };
        assert_eq!(rs_db, py["db"], "db projects 不一致：rust={} python={}", rs_db, py["db"]);
    }
}