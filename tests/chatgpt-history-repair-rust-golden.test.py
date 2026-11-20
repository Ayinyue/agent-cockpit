# -*- coding: utf-8 -*-
"""T18 第三批 golden：chatgpt-history 项目修复迁 Rust 跨语言一致性验证。
同一份合成 source/target（含 state_5.sqlite + state JSON + marker），
python project_repair_plan/repair_projects vs Rust 输出逐字段比对 + apply 后库内容比对。
只比对数字与结构，不打印对话正文。合成数据，不碰真实凭据。"""
import importlib.util
import json
import os
import pathlib
import sqlite3
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = pathlib.Path(os.environ["USERPROFILE"]) / ".cargo" / "bin" / "cargo.exe"
TARGET = ROOT.parent / "agent-cockpit-health-review" / "cargo-target"
STATE_FILE = ".codex-global-state.json"
MARKER_FILE = ".agentcockpit-history.json"
MIGRATION = "2026-07-13-local-projects"
COPIED_AT = 1_700_000_000.0


def build_pair(base):
    source = base / "source"
    target = base / "target"
    source.mkdir(parents=True)
    target.mkdir(parents=True)
    source_state = {
        "electron-completed-local-data-migration-ids": [MIGRATION],
        "local-projects": {"proj-a": {"rootPaths": ["C:/Work/A"], "name": "A"}},
        "electron-saved-workspace-roots": ["C:/Work/A", "C:/Work/Stale"],
        "saved-workspace-roots": ["C:/Work/A", "C:/Work/Stale"],
        "workspace-root-order": ["C:/Work/A", "C:/Work/Stale"],
    }
    (source / STATE_FILE).write_text(json.dumps(source_state, ensure_ascii=False), encoding="utf-8")
    con = sqlite3.connect(source / "state_5.sqlite")
    con.executescript(
        "CREATE TABLE projects(id TEXT PRIMARY KEY, name TEXT, metadata TEXT, created_at_ms REAL);"
        "CREATE TABLE project_roots(project_id TEXT, path TEXT, position INTEGER);"
        "CREATE TABLE threads(id TEXT PRIMARY KEY, project_id TEXT);"
        "INSERT INTO projects VALUES('proj-a','A','{}',1000.0);"
        "INSERT INTO project_roots VALUES('proj-a','C:/Work/A',0);")
    con.commit(); con.close()
    marker = {"source": str(source), "copied_at": COPIED_AT, "threads": 0, "projects": 0, "rollouts": 0}
    (target / MARKER_FILE).write_text(json.dumps(marker, ensure_ascii=False), encoding="utf-8")
    target_state = {
        "electron-completed-local-data-migration-ids": [MIGRATION],
        "local-projects": {
            "proj-a": {"rootPaths": ["C:/Work/A"], "name": "A"},
            "local-stale": {"rootPaths": ["C:/Work/Stale"], "name": "Stale",
                            "createdAt": COPIED_AT * 1000 + 5000, "updatedAt": COPIED_AT * 1000 + 5500},
        },
        "electron-saved-workspace-roots": ["C:/Work/A", "C:/Work/Stale"],
        "project-order": ["proj-a", "local-stale"],
        "pinned-project-ids": [],
        "thread-project-assignments": {},
    }
    (target / STATE_FILE).write_text(json.dumps(target_state, ensure_ascii=False), encoding="utf-8")
    con = sqlite3.connect(target / "state_5.sqlite")
    con.executescript(
        "CREATE TABLE projects(id TEXT PRIMARY KEY, name TEXT, metadata TEXT, created_at_ms REAL);"
        "CREATE TABLE project_roots(project_id TEXT, path TEXT, position INTEGER);"
        "CREATE TABLE threads(id TEXT PRIMARY KEY, project_id TEXT);"
        "INSERT INTO projects VALUES('proj-a','A','{}',1000.0);"
        "INSERT INTO projects VALUES('modern-stale','Stale','{}',1700000005000.0);"
        "INSERT INTO project_roots VALUES('proj-a','C:/Work/A',0);"
        "INSERT INTO project_roots VALUES('modern-stale','C:/Work/Stale',0);")
    con.commit(); con.close()
    return source, target


def db_projects(db):
    con = sqlite3.connect(db)
    rows = {r[0]: {"name": r[1], "metadata": r[2]} for r in con.execute("SELECT id,name,metadata FROM projects")}
    con.close()
    return rows


def run_rust(test_name, env_extra, timeout=600):
    env = dict(os.environ)
    env.update(env_extra)
    env["CARGO_TARGET_DIR"] = str(TARGET)
    p = subprocess.run(
        [str(CARGO), "test", "--bin", "agent-cockpit", test_name, "--", "--nocapture", "--exact"],
        cwd=str(ROOT / "src-tauri"), env=env, capture_output=True, timeout=timeout,
    )
    out = (p.stdout + p.stderr).decode("utf-8", "replace")
    for line in out.splitlines():
        if "test result" in line or "panicked" in line or "不一致" in line or "FAILED" in line:
            print(line[:400])
    return p.returncode


def main():
    spec = importlib.util.spec_from_file_location("h", ROOT / "src-tauri" / "scripts" / "chatgpt-history.py")
    py = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(py)
    failures = 0

    # 1) dry_run plan 比对
    tmp1 = pathlib.Path(tempfile.mkdtemp(prefix="t18c-plan-"))
    src1, tgt1 = build_pair(tmp1)
    py_plan = py.project_repair_plan(str(src1), str(tgt1))
    rs_src, rs_tgt = build_pair(pathlib.Path(tempfile.mkdtemp(prefix="t18c-plan-rs-")))
    (tmp1 / "py_plan.json").write_text(json.dumps(py_plan, ensure_ascii=False), encoding="utf-8")
    rc = run_rust("chatgpt_history_repair::tests::golden_plan_matches_python", {
        "T18C_SOURCE": str(rs_src), "T18C_TARGET": str(rs_tgt),
        "T18C_PY_PLAN": str(tmp1 / "py_plan.json"),
    })
    print(f"[plan] python eligible={py_plan['eligible']} legacy={py_plan['legacy_ids']} cargo_exit={rc}")
    failures += (rc != 0)

    # 2) apply 比对：python apply 一份，Rust apply 一份，比对 state+db 结果
    tmp2 = pathlib.Path(tempfile.mkdtemp(prefix="t18c-apply-"))
    src2, tgt2 = build_pair(tmp2)
    py_plan2 = py.project_repair_plan(str(src2), str(tgt2))
    py_result = py.repair_projects(str(src2), str(tgt2), dry_run=False, expected_plan=py_plan2)
    py_state = json.loads((tgt2 / STATE_FILE).read_text(encoding="utf-8"))
    py_db = db_projects(tgt2 / "state_5.sqlite")
    rs_src2, rs_tgt2 = build_pair(pathlib.Path(tempfile.mkdtemp(prefix="t18c-apply-rs-")))
    (tmp2 / "py_apply.json").write_text(json.dumps(
        {"result": {k: py_result[k] for k in ("applied", "legacy_ids", "project_ids", "visibility_version")},
         "state": py_state, "db": py_db}, ensure_ascii=False), encoding="utf-8")
    rc = run_rust("chatgpt_history_repair::tests::golden_apply_matches_python", {
        "T18C_SOURCE": str(rs_src2), "T18C_TARGET": str(rs_tgt2),
        "T18C_PY_APPLY": str(tmp2 / "py_apply.json"),
    })
    print(f"[apply] python applied={py_result['applied']} cargo_exit={rc}")
    failures += (rc != 0)

    print("golden failures:", failures)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())