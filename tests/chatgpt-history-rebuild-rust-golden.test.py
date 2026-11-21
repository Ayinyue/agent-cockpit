# -*- coding: utf-8 -*-
"""T18 生产路径 golden：rebuild_catalog（Rust）vs python shared 尾段重建。
fixture：canonical + 2 带 marker home + 1 无 marker home（应跳过）。
预置陈旧/内部/归档目录项，验证两侧重建后 local_thread_catalog + revision 逐字段一致。
只比对数字与结构，不打印对话正文。合成数据，不碰真实凭据。"""
import importlib.util
import json
import os
import pathlib
import sqlite3
import subprocess
import sys
import tempfile
from contextlib import contextmanager

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = pathlib.Path(os.environ["USERPROFILE"]) / ".cargo" / "bin" / "cargo.exe"
TARGET = ROOT.parent / "agent-cockpit-health-review" / "cargo-target"
MARKER = '.agentcockpit-shared-history.json'


@contextmanager
def database(path):
    con = sqlite3.connect(path)
    try:
        with con:
            yield con
    finally:
        con.close()


def build_home(root, canonical, with_marker):
    root.mkdir(parents=True)
    (root / 'sqlite').mkdir()
    with database(root / 'state_5.sqlite') as con:
        con.executescript('''CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT NOT NULL,
            title TEXT,name TEXT,created_at INTEGER,updated_at INTEGER,model_provider TEXT,
            cwd TEXT,archived INTEGER DEFAULT 0,project_id TEXT,thread_section_id TEXT,
            thread_source TEXT,source TEXT,git_branch TEXT);''')
    with database(root / 'sqlite' / 'codex-dev.db') as con:
        con.executescript('''CREATE TABLE local_thread_catalog(host_id TEXT,thread_id TEXT,display_title TEXT,
            source_created_at REAL,source_updated_at REAL,source_recency_at REAL,cwd TEXT,source_kind TEXT,
            source_detail TEXT,model_provider TEXT,git_branch TEXT,observation_sequence INTEGER,
            missing_candidate INTEGER,thread_source TEXT,project_id TEXT,PRIMARY KEY(host_id,thread_id));
            CREATE TABLE local_thread_catalog_hosts(host_id TEXT PRIMARY KEY,host_kind TEXT);
            INSERT INTO local_thread_catalog_hosts VALUES('local','local');
            CREATE TABLE local_thread_catalog_metadata(id INTEGER PRIMARY KEY,catalog_revision INTEGER);
            INSERT INTO local_thread_catalog_metadata VALUES(1,7);''')
    if with_marker:
        (root / MARKER).write_text(json.dumps({'version': 1, 'canonical': str(canonical)}), encoding='utf-8')


def put_thread(root, tid, title, archived=0, thread_source=None, source='vscode'):
    with database(root / 'state_5.sqlite') as con:
        con.execute('INSERT OR REPLACE INTO threads(id,rollout_path,title,name,created_at,updated_at,model_provider,cwd,archived,thread_source,source) VALUES(?,?,?,?,?,?,?,?,?,?,?)',
                    (tid, str(root / 'sessions' / (tid + '.jsonl')), title, title, 1000, 2000, 'openai', str(root), archived, thread_source, source))


def seed_stale_catalog(root, entries):
    """预置陈旧目录项：含应被清理的内部线程、应被删除的归档项、应被更新的旧标题。"""
    with database(root / 'sqlite' / 'codex-dev.db') as con:
        for host, tid, title in entries:
            con.execute('INSERT OR REPLACE INTO local_thread_catalog(host_id,thread_id,display_title,source_created_at,source_updated_at,observation_sequence) VALUES(?,?,?,?,?,?)',
                        (host, tid, title, 1.0, 2.0, 0))


def snapshot_catalog(homes):
    out = {}
    for root in homes:
        name = root.name
        db = root / 'sqlite' / 'codex-dev.db'
        if not db.is_file():
            out[name] = None
            continue
        with database(db) as con:
            con.row_factory = sqlite3.Row
            cat = [dict(r) for r in con.execute('SELECT * FROM local_thread_catalog ORDER BY host_id,thread_id')]
            rev = con.execute('SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id=1').fetchone()[0]
        out[name] = {'catalog': cat, 'revision': rev}
    return out


def normalize(obj, base):
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

    tmp = pathlib.Path(tempfile.mkdtemp(prefix='t18-rebuild-golden-'))
    py_base = tmp / 'py'; rs_base = tmp / 'rs'

    def build_set(base):
        canonical = base / 'canonical'
        h1 = base / 'home1'; h2 = base / 'home2'; h3 = base / 'home3-nomarker'
        build_home(canonical, canonical, False)
        build_home(h1, canonical, True)
        build_home(h2, canonical, True)
        build_home(h3, canonical, False)  # 无 marker，应跳过
        # canonical 拥有全部线程（共享库）
        put_thread(canonical, 't-normal', '正常对话')
        put_thread(canonical, 't-arch', '归档对话', archived=1)
        put_thread(canonical, 't-sub', '内部子代理', thread_source='subagent')
        put_thread(canonical, 't-cli', 'CLI 对话', source='cli')
        # 各 home 预置陈旧目录项
        for h in (canonical, h1, h2, h3):
            seed_stale_catalog(h, [
                ('local', 't-normal', '旧标题'),       # 应被更新为新标题
                ('local', 't-arch', '归档旧项'),       # 应被删除（archived）
                ('local', 't-sub', '内部旧项'),        # 应被删除（internal）
                ('local', 't-ghost', '幽灵项'),        # 不在 threads，保留（python 不删非内部幽灵）
            ])
        return canonical, [canonical, h1, h2, h3]

    py_canonical, py_homes = build_set(py_base)
    rs_canonical, rs_homes = build_set(rs_base)

    # python 侧：直接调用 Replicator 的 clean_internal_catalog + catalog（镜像 shared 尾段，no-backup）
    py_state = tmp / 'py-state'; py_state.mkdir()
    rep = sync.Replicator(str(py_state))
    rep.backup = lambda *a, **k: None
    with database(py_canonical / 'state_5.sqlite') as con:
        con.row_factory = sqlite3.Row
        threads = {r['id']: dict(r) for r in con.execute('SELECT * FROM threads')}
    for home in py_homes:
        if home != py_canonical and not (home / MARKER).is_file():
            continue
        rep.clean_internal_catalog(home, threads)
        for row in threads.values():
            rep.catalog(home, row)
    py_after = snapshot_catalog(py_homes)

    req = {'canonical': str(rs_canonical), 'homes': [str(h) for h in rs_homes], 'state_dir': str(tmp / 'rs-state')}
    req_path = tmp / 'req.json'; after_path = tmp / 'py_after.json'
    req_path.write_text(json.dumps(req, ensure_ascii=False), encoding='utf-8')
    after_path.write_text(json.dumps(normalize(py_after, py_base), ensure_ascii=False), encoding='utf-8')

    env = dict(os.environ)
    env['CARGO_TARGET_DIR'] = str(TARGET)
    env['T18_REBUILD_REQ'] = str(req_path)
    env['T18_REBUILD_PY_AFTER'] = str(after_path)
    p = subprocess.run([str(CARGO), 'test', '--bin', 'agent-cockpit',
                        'chatgpt_history_sync::tests::golden_rebuild_matches_python', '--', '--nocapture', '--exact'],
                       cwd=str(ROOT / 'src-tauri'), env=env, capture_output=True, timeout=600)
    out = (p.stdout + p.stderr).decode('utf-8', 'replace')
    for line in out.splitlines():
        if '不一致' in line or 'test result' in line or 'panicked' in line or 'FAILED' in line or 'rebuild 应' in line:
            print(line[:600])
    print('python catalog counts:', {k: (len(v['catalog']) if v else 0) for k, v in py_after.items()})
    print('cargo_exit:', p.returncode)
    return p.returncode


if __name__ == '__main__':
    sys.exit(main())