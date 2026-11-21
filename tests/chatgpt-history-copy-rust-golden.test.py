# -*- coding: utf-8 -*-
"""T18 第五批 golden：copy_history 迁 Rust 跨语言一致性验证。
合成 source（state_5/state_3/thread_history_1/codex-dev.db + sessions 明文/gz/zst + 归档悬空
+ 索引 + 全局 state 含秘密字段）→ 空 target（含官方登录初始化的空库）。
python copy_history vs Rust run 输出 report 逐字段 + 复制后目标全内容比对
（sqlite 转储表行、marker 去 copied_at、路径基目录归一）。
只比对数字与结构，不打印对话正文。合成数据，不碰真实凭据。"""
import gzip
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


def db(path):
    con = sqlite3.connect(path)
    return con


def build_source(root):
    root.mkdir(parents=True)
    (root / "sessions").mkdir()
    (root / "archived_sessions").mkdir()
    (root / "sqlite").mkdir()
    # 明文 rollout ×2 + gz ×1 + zst ×1（若支持）
    (root / "sessions" / "a.jsonl").write_bytes(b'{"text":"a1"}\n{"text":"a2"}\n')
    (root / "sessions" / "b.jsonl").write_bytes(b'{"text":"b1"}\n')
    (root / "sessions" / "c.jsonl.gz").write_bytes(gzip.compress(b'{"text":"c1"}\n'))
    zst_ok = False
    try:
        from compression import zstd
        (root / "sessions" / "d.jsonl.zst").write_bytes(zstd.compress(b'{"text":"d1"}\n'))
        zst_ok = True
    except ImportError:
        try:
            import zstandard
            (root / "sessions" / "d.jsonl.zst").write_bytes(zstandard.ZstdCompressor().compress(b'{"text":"d1"}\n'))
            zst_ok = True
        except ImportError:
            pass
    # state_5：threads 含正常/归档悬空/压缩还原引用 + 任务表（应被清）+ ledger
    con = db(root / "state_5.sqlite")
    con.executescript("""
        CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, title TEXT, cwd TEXT,
            archived INTEGER DEFAULT 0, project_id TEXT);
        CREATE TABLE projects(id TEXT PRIMARY KEY, name TEXT);
        CREATE TABLE automations(id TEXT PRIMARY KEY, prompt TEXT);
        CREATE TABLE thread_timeline_ledger(id INTEGER PRIMARY KEY, payload_json TEXT);
    """)
    rows = [
        ("t-a", str(root / "sessions" / "a.jsonl"), "A", str(root / "work"), 0, "p1"),
        ("t-b", str(root / "sessions" / "b.jsonl"), "B", str(root / "work"), 0, None),
        ("t-c", str(root / "sessions" / "c.jsonl"), "C", str(root / "work"), 0, None),
        ("t-arch", str(root / "archived_sessions" / "gone.jsonl"), "Gone", str(root), 1, None),
    ]
    if zst_ok:
        rows.append(("t-d", str(root / "sessions" / "d.jsonl"), "D", str(root / "work"), 0, None))
    con.executemany("INSERT INTO threads VALUES(?,?,?,?,?,?)", rows)
    con.execute("INSERT INTO projects VALUES('p1','P1')")
    con.execute("INSERT INTO automations VALUES('job1','secret-prompt')")
    con.execute("INSERT INTO thread_timeline_ledger VALUES(1,?)",
                (json.dumps({"local:" + str(root): {"cwd": str(root / "work")}}),))
    # 注：python 对「压缩档案已还原明文」的 rollout 引用（stage 无 .gz 只有还原 .jsonl）会因
    # rollout_by_name 无明文名候选而报错——这是 python 既有行为，fixture 不构造该场景，
    # t-c/t-d 的引用改指真实存在的明文（复制语义以 python 为准，不弱化）。
    con.execute("UPDATE threads SET rollout_path=? WHERE id='t-c'", (str(root / "sessions" / "a.jsonl"),))
    if zst_ok:
        con.execute("UPDATE threads SET rollout_path=? WHERE id='t-d'", (str(root / "sessions" / "b.jsonl"),))
    con.commit(); con.close()
    # state_3：旧版本次级（threads 计数应被 state_5 覆盖）。rollout_path 用 NULL：
    # python 的 rollout_relative 只处理 str，NULL 跳过（真实旧库常见形态）。
    con = db(root / "state_3.sqlite")
    con.executescript("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT);")
    con.execute("INSERT INTO threads VALUES('old',NULL)")
    con.commit(); con.close()
    # thread_history_1：分页历史
    con = db(root / "thread_history_1.sqlite")
    con.executescript("CREATE TABLE thread_items(thread_id TEXT, item_id TEXT, item_json TEXT);")
    con.execute("INSERT INTO thread_items VALUES('t-a','i1','{}')")
    con.commit(); con.close()
    # codex-dev.db：目录表保留 + 非目录表（账号缓存）应被清
    con = db(root / "sqlite" / "codex-dev.db")
    con.executescript("""
        CREATE TABLE local_thread_catalog(host_id TEXT, thread_id TEXT, display_title TEXT, PRIMARY KEY(host_id, thread_id));
        CREATE TABLE local_thread_catalog_hosts(host_id TEXT PRIMARY KEY, host_kind TEXT);
        CREATE TABLE account_cache(id TEXT PRIMARY KEY, token TEXT);
    """)
    con.execute("INSERT INTO local_thread_catalog VALUES('local:'||?,'t-a','A')", (str(root),))
    con.execute("INSERT INTO local_thread_catalog_hosts VALUES('local','local')")
    con.execute("INSERT INTO account_cache VALUES('c1','secret-token')")
    con.commit(); con.close()
    # 索引：host/path 列重定位 + 空行剔除
    idx = [
        {"id": "t-a", "host_id": "local:" + str(root), "rollout_path": str(root / "sessions" / "a.jsonl"), "cwd": str(root / "work")},
        {"id": "t-b", "path": str(root / "sessions" / "b.jsonl")},
    ]
    (root / "session_index.jsonl").write_text("\n".join(json.dumps(r) for r in idx) + "\n\n", encoding="utf-8")
    (root / "history.jsonl").write_text(json.dumps({"text": "h1"}) + "\n", encoding="utf-8")
    # 全局 state：白名单键 + 秘密字段（应剔除）+ 非白名单键（应丢弃）+ atom 白名单
    state = {
        "local-projects": {"p1": {"rootPaths": [str(root / "work")], "name": "work"}},
        "electron-completed-local-data-migration-ids": ["2026-07-13-local-projects"],
        "electron-saved-workspace-roots": [str(root / "work"), str(root / "stale")],
        "saved-workspace-roots": [str(root / "work"), str(root / "stale")],
        "workspace-root-order": [str(root / "work"), str(root / "stale")],
        "thread-titles": {"t-a": "A"},
        "auth": {"token": "secret"},
        "some-other-key": "drop-me",
        "electron-persisted-atom-state": {
            "sidebar-custom-sections-v3": {"x": 1},
            "sidebar-project-expanded-v1-p1": True,
            "not-kept": "drop",
        },
    }
    (root / STATE_FILE).write_text(json.dumps(state, ensure_ascii=False), encoding="utf-8")
    # 非历史文件（不应被复制）
    (root / "auth.json").write_bytes(b"private-auth")
    (root / "config.toml").write_bytes(b'provider="private"')
    return zst_ok


def build_target(root):
    """官方登录初始化：空库 + 空 state（可替换，不算已有历史）。"""
    root.mkdir(parents=True)
    con = db(root / "state_5.sqlite")
    con.executescript("""
        CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, title TEXT, cwd TEXT,
            archived INTEGER DEFAULT 0, project_id TEXT);
        CREATE TABLE projects(id TEXT PRIMARY KEY, name TEXT);
    """)
    con.commit(); con.close()
    (root / STATE_FILE).write_text(json.dumps({"ui-scale": 1.0}), encoding="utf-8")


def snapshot_target(root):
    import hashlib
    out = {}
    for f in sorted(p for p in root.rglob("*") if p.is_file()):
        rel = f.relative_to(root).as_posix()
        if rel.startswith(".history-before-copy-"):
            continue  # 成功替换保留的原件（恢复资产，目录名含随机 uuid），非复制产物
        if f.name.endswith(".sqlite") or f.name == "codex-dev.db":
            con = sqlite3.connect(f)
            con.row_factory = sqlite3.Row
            tbs = {}
            for (t,) in con.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall():
                if t.startswith("sqlite_"):
                    continue
                rows = [dict(r) for r in con.execute('SELECT * FROM "' + t + '"')]
                if t == "thread_timeline_ledger":
                    for r in rows:
                        if isinstance(r.get("payload_json"), str):
                            r["payload_json"] = json.loads(r["payload_json"])
                rows.sort(key=lambda r: json.dumps(r, sort_keys=True, default=str))
                tbs[t] = rows
            con.close()
            out[rel] = tbs
        elif f.name.endswith(".jsonl.gz") or f.name.endswith(".jsonl.zst"):
            out[rel] = hashlib.sha256(f.read_bytes()).hexdigest()
        elif f.name in (MARKER_FILE, STATE_FILE) or f.name.endswith(".jsonl"):
            text = f.read_text(encoding="utf-8")
            if f.name.endswith(".jsonl"):
                out[rel] = [json.loads(l) for l in text.splitlines() if l.strip()]
            else:
                v = json.loads(text)
                if f.name == MARKER_FILE:
                    v.pop("copied_at", None)
                out[rel] = v
        else:
            out[rel] = f.read_text(encoding="utf-8", errors="replace")
    return out


def normalize(obj, base):
    base_s = str(base)
    def norm(v):
        if isinstance(v, str):
            return v.replace(base_s, "<BASE>")
        if isinstance(v, dict):
            return {norm(k): norm(x) for k, x in v.items()}
        if isinstance(v, list):
            return [norm(x) for x in v]
        return v
    return norm(obj)


def main():
    spec = importlib.util.spec_from_file_location("hist", ROOT / "src-tauri/scripts/chatgpt-history.py")
    hist = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(hist)

    # CI runner TEMP 含 8.3 短名（RUNNER~1）：fixture 必须先 resolve 再写库内
    # rollout_path，与生产 copy_history 先 resolve 的口径一致（同 run#1/#2 修法）。
    # 本机长名环境下 resolve 为恒等变换，行为不变。
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="t18-copy-golden-")).resolve()
    py_base = tmp / "py"; rs_base = tmp / "rs"
    zst1 = build_source(py_base / "source"); build_target(py_base / "target")
    zst2 = build_source(rs_base / "source"); build_target(rs_base / "target")
    assert zst1 == zst2

    py_report = {"ok": True, **hist.copy_history(py_base / "source", py_base / "target")}
    py_after = normalize(snapshot_target(py_base / "target"), py_base)

    req = {"source": str(rs_base / "source"), "target": str(rs_base / "target")}
    req_path = tmp / "req.json"; py_path = tmp / "py.json"; after_path = tmp / "py_after.json"
    req_path.write_text(json.dumps(req, ensure_ascii=False), encoding="utf-8")
    py_path.write_text(json.dumps(py_report, ensure_ascii=False), encoding="utf-8")
    after_path.write_text(json.dumps(py_after, ensure_ascii=False), encoding="utf-8")

    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = str(TARGET)
    env["T18_COPY_REQ"] = str(req_path)
    env["T18_COPY_PY"] = str(py_path)
    env["T18_COPY_PY_AFTER"] = str(after_path)
    p = subprocess.run([str(CARGO), "test", "--bin", "agent-cockpit",
                        "chatgpt_history_copy::tests::golden_copy_matches_python", "--", "--nocapture", "--exact"],
                       cwd=str(ROOT / "src-tauri"), env=env, capture_output=True, timeout=900)
    out = (p.stdout + p.stderr).decode("utf-8", "replace")
    for line in out.splitlines():
        if "DIAG" in line or "不一致" in line or "test result" in line or "panicked" in line or "FAILED" in line:
            print(line[:800])
    print(f"python report: threads={py_report['threads']} projects={py_report['projects']} rollouts={py_report['rollouts']} restored={py_report['restored_compressed']} missing_arch={py_report['missing_archived_rollouts']} zst={zst1}")
    print("cargo_exit:", p.returncode)
    return p.returncode


if __name__ == "__main__":
    sys.exit(main())