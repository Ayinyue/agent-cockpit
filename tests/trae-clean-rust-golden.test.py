# -*- coding: utf-8 -*-
"""T18 golden：trae-gateway-clean 迁 Rust 跨语言一致性验证。
同一份合成 state.vscdb，python run vs Rust clean_db 输出逐字段比对 + 写回后库内容比对。
只比对数字与结构，不打印日志正文。合成数据，不碰真实凭据。"""
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

MAP_KEY = "uid:AI.agent.model.model_list_map"
SEL_KEY = "uid:AI.agent.model.session_selected_model"
RECENT_KEY = "uid:AI.agent.model.recent_user_selection_by_agent_label"


def build_fixture(db_path):
    con = sqlite3.connect(db_path)
    con.execute("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value TEXT)")
    map_doc = {
        "openai": [
            {"name": "glm-5.3", "config_name": "glm-5.3", "display_name": "GLM 5.3",
             "custom_model_id": "glm-5.3", "base_url": "http://127.0.0.1:8788/v1"},
            {"name": "my-custom", "base_url": "https://api.example.com/v1"},
            {"name": "claude-x", "base_url": "http://localhost:8788/v1"},
        ],
        "anthropic": [
            {"name": "claude-y", "base_url": "http://[::1]:8788/v1"},
        ],
        "empty-channel": [],
    }
    sel_doc = {"modelId": "glm-5.3", "effort": "high"}
    recent_doc = {"agent": ["glm-5.3", "my-custom"], "glm-5.3": 1, "mine": "keep"}
    con.execute("INSERT INTO ItemTable VALUES(?,?)", (MAP_KEY, json.dumps(map_doc, ensure_ascii=False)))
    con.execute("INSERT INTO ItemTable VALUES(?,?)", (SEL_KEY, json.dumps(sel_doc, ensure_ascii=False)))
    con.execute("INSERT INTO ItemTable VALUES(?,?)", (RECENT_KEY, json.dumps(recent_doc, ensure_ascii=False)))
    con.commit()
    con.close()


def db_snapshot(db_path):
    con = sqlite3.connect(db_path)
    rows = dict(con.execute("SELECT key, value FROM ItemTable").fetchall())
    con.close()
    return {k: json.loads(v) for k, v in rows.items()}


def main():
    spec = importlib.util.spec_from_file_location("t", ROOT / "src-tauri" / "scripts" / "trae-gateway-clean.py")
    py = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(py)
    hosts = ["127.0.0.1:8788", "localhost:8788", "[::1]:8788"]

    tmp = pathlib.Path(tempfile.mkdtemp(prefix="t18-trae-golden-"))
    py_db = tmp / "py.vscdb"
    rs_db = tmp / "rs.vscdb"
    build_fixture(py_db)
    build_fixture(rs_db)

    py_result = py.run(str(py_db), hosts)
    py_after = db_snapshot(py_db)

    # Rust 侧：临时 golden 测试读同一 fixture（环境变量传路径）
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = str(TARGET)
    env["T18_TRAE_FIXTURE"] = str(rs_db)
    env["T18_TRAE_HOSTS"] = json.dumps(hosts)
    env["T18_TRAE_PY_RESULT"] = str(tmp / "py_result.json")
    env["T18_TRAE_PY_AFTER"] = str(tmp / "py_after.json")
    (tmp / "py_result.json").write_text(json.dumps(py_result, ensure_ascii=False), encoding="utf-8")
    (tmp / "py_after.json").write_text(json.dumps(py_after, ensure_ascii=False), encoding="utf-8")
    p = subprocess.run(
        [str(CARGO), "test", "--bin", "agent-cockpit", "trae_clean::tests::golden_matches_python",
         "--", "--nocapture", "--exact"],
        cwd=str(ROOT / "src-tauri"), env=env, capture_output=True, timeout=600,
    )
    out = (p.stdout + p.stderr).decode("utf-8", "replace")
    for line in out.splitlines():
        if "test result" in line or "panicked" in line or "不一致" in line or "FAILED" in line:
            print(line[:400])
    print("python removed:", py_result["removed"], "keys:", len(py_result["keys"]), "cargo_exit:", p.returncode)
    return p.returncode


if __name__ == "__main__":
    sys.exit(main())