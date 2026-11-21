# -*- coding: utf-8 -*-
"""T18 golden：chatgpt-history-sync 迁 Rust 跨语言一致性验证。
同一份合成 fixture（3 homes + state_5/thread_history_1/codex-dev.db + rollouts），
python synchronize vs Rust synchronize 输出 report 逐字段比对 + 同步后库内容比对。
python 与 Rust 各用独立 home 副本（避免互相污染），初始状态完全一致。
只比对数字与结构，不打印对话正文。合成数据，不碰真实凭据。"""
import importlib.util
import json
import os
import pathlib
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from contextlib import contextmanager

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = pathlib.Path(os.environ["USERPROFILE"]) / ".cargo" / "bin" / "cargo.exe"
TARGET = ROOT.parent / "agent-cockpit-health-review" / "cargo-target"


@contextmanager
def database(path):
    con = sqlite3.connect(path)
    try:
        with con:
            yield con
    finally:
        con.close()


def build_homes(base):
    homes = [base / n for n in ('first', 'second', 'third')]
    for root in homes:
        root.mkdir(parents=True)
        (root / 'auth.json').write_bytes(b'private-' + root.name.encode())
        (root / 'config.toml').write_bytes(b'provider="private"')
        with database(root / 'state_5.sqlite') as con:
            con.executescript('''CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,
                title TEXT, name TEXT, created_at INTEGER,updated_at INTEGER,model_provider TEXT,
                cwd TEXT, archived INTEGER DEFAULT 0,project_id TEXT,thread_section_id TEXT);
                CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT);
                CREATE TABLE project_roots(project_id TEXT,position INTEGER,path TEXT,PRIMARY KEY(project_id,position));
                CREATE TABLE automations(id TEXT PRIMARY KEY,prompt TEXT);''')
        with database(root / 'thread_history_1.sqlite') as con:
            con.executescript('''CREATE TABLE thread_items(thread_id TEXT,item_id TEXT,item_json TEXT,PRIMARY KEY(thread_id,item_id));
                CREATE TABLE thread_history_projection_state(thread_id TEXT PRIMARY KEY,next_rollout_byte_offset INTEGER);''')
        (root / 'sqlite').mkdir()
        with database(root / 'sqlite/codex-dev.db') as con:
            con.executescript('''CREATE TABLE local_thread_catalog(host_id TEXT,thread_id TEXT,display_title TEXT,
                source_created_at REAL,source_updated_at REAL,source_kind TEXT,observation_sequence INTEGER,
                PRIMARY KEY(host_id,thread_id));
                CREATE TABLE local_thread_catalog_hosts(host_id TEXT PRIMARY KEY,host_kind TEXT);
                INSERT INTO local_thread_catalog_hosts VALUES('local','local');
                CREATE TABLE local_thread_catalog_metadata(id INTEGER PRIMARY KEY,catalog_revision INTEGER);
                INSERT INTO local_thread_catalog_metadata VALUES(1,0);''')
    return homes


def put(root, thread, messages, updated=1, archived=0):
    relative = pathlib.Path('archived_sessions' if archived else 'sessions') / (thread + '.jsonl')
    path = root / relative
    path.parent.mkdir(exist_ok=True)
    data = b''.join(json.dumps({'text': m}).encode() + b'\n' for m in messages)
    path.write_bytes(data)
    with database(root / 'state_5.sqlite') as con:
        con.execute('INSERT OR REPLACE INTO threads VALUES(?,?,?,?,?,?,?,?,?,?,?)',
                    (thread, str(path), thread, thread, 1, updated, 'openai', str(root), archived, None, None))
    with database(root / 'thread_history_1.sqlite') as con:
        con.execute('INSERT OR REPLACE INTO thread_items VALUES(?,?,?)', (thread, 'last', messages[-1]))
        con.execute('INSERT OR REPLACE INTO thread_history_projection_state VALUES(?,?)', (thread, len(data)))


def snapshot_after(homes):
    out = {}
    for root in homes:
        with database(root / 'state_5.sqlite') as con:
            con.row_factory = sqlite3.Row
            threads = [dict(r) for r in con.execute('SELECT * FROM threads ORDER BY id')]
        contents = {}
        for t in threads:
            contents[t['id']] = pathlib.Path(t['rollout_path']).read_text(encoding='utf-8')
        out[root.name] = {'threads': threads, 'contents': contents}
    return out


def normalize_after(obj, base):
    """rollout_path/cwd 含各侧基目录绝对路径（python 导入时按目标 home 重写，属正确行为）。
    比对前把基目录归一为 <BASE>；其余字段保持逐字严格比对。"""
    base_s = str(base)
    def norm(v):
        if isinstance(v, str):
            return v.replace(base_s, '<BASE>')
        if isinstance(v, dict):
            return {k: norm(x) for k, x in v.items()}
        if isinstance(v, list):
            return [norm(x) for x in v]
        return v
    return norm(obj)


def main():
    spec = importlib.util.spec_from_file_location('sync', ROOT / 'src-tauri/scripts/chatgpt-history-sync.py')
    sync = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(sync)

    tmp = pathlib.Path(tempfile.mkdtemp(prefix='t18-sync-golden-'))
    # python 与 Rust 各用独立但初始一致的副本
    py_base = tmp / 'py'; rs_base = tmp / 'rs'
    py_homes = build_homes(py_base); rs_homes = build_homes(rs_base)
    # 场景：双向 union + 续写 + 冲突 + 归档
    for homes in (py_homes, rs_homes):
        put(homes[0], 'one', ['first'])
        put(homes[1], 'two', ['second'])
        put(homes[0], 'arch', ['archived-msg'], archived=1)
    py_state = tmp / 'py-state'; rs_state = tmp / 'rs-state'

    py_report = sync.synchronize([str(h) for h in py_homes], str(py_state))
    py_after = snapshot_after(py_homes)

    req = {'homes': [str(h) for h in rs_homes], 'state_dir': str(rs_state)}
    req_path = tmp / 'req.json'; py_path = tmp / 'py.json'; after_path = tmp / 'py_after.json'
    req_path.write_text(json.dumps(req, ensure_ascii=False), encoding='utf-8')
    py_path.write_text(json.dumps(py_report, ensure_ascii=False), encoding='utf-8')
    after_path.write_text(json.dumps(normalize_after(py_after, py_base), ensure_ascii=False), encoding='utf-8')

    env = dict(os.environ)
    env['CARGO_TARGET_DIR'] = str(TARGET)
    env['T18_SYNC_REQ'] = str(req_path)
    env['T18_SYNC_PY'] = str(py_path)
    env['T18_SYNC_PY_AFTER'] = str(after_path)
    env['T18_SYNC_DEBUG'] = '1'
    p = subprocess.run([str(CARGO), 'test', '--bin', 'agent-cockpit',
                        'chatgpt_history_sync::tests::golden_matches_python', '--', '--nocapture', '--exact'],
                       cwd=str(ROOT / 'src-tauri'), env=env, capture_output=True, timeout=600)
    out = (p.stdout + p.stderr).decode('utf-8', 'replace')
    for line in out.splitlines():
        if 'DIAG' in line or 'test result' in line or 'panicked' in line or '不一致' in line or 'FAILED' in line:
            print(line[:600])
    print(f"python report: homes={py_report['homes']} threads={py_report['threads']} rollouts={py_report['rollouts']} catalog={py_report['catalog']} conflicts={len(py_report['conflicts'])} deferred={len(py_report['deferred'])}")
    print('cargo_exit:', p.returncode)
    return p.returncode


if __name__ == '__main__':
    sys.exit(main())