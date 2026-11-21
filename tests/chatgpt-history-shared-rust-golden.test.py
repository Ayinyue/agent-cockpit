# -*- coding: utf-8 -*-
"""T18 第六批 golden：shared migrate 管理（Rust chatgpt_history_shared::synchronize）
vs python chatgpt-history-shared.py synchronize。
场景（复用既有 sync fixture）：
  A 正常迁移：canonical 拥有 native-chat，home1 拥有 private-chat + 归档 + 孤儿 rollout，
    home2 空库 → 全部 union、junction、退役备份、stage 清理。
  B busy home：home2 标 busy → pending（Account window is running），不迁移。
  C 续写冲突：canonical 与 home1 同线程分叉 → pending，两侧字节原样保留。
  D 中断恢复：home1 带 marker 但残留 state_9.sqlite → pending（保守保留现场）。
每个场景独立 fixture；python 与 Rust 各跑一套（py/rs 基目录），报告归一时间戳与基目录后
逐字段一致；迁移后双侧全内容（库行/文件字节/链接拓扑/备份数）归一后一致。
合成数据，不碰真实凭据；auth.json 每侧校验原样未动。"""
import importlib.util
import json
import os
import pathlib
import shutil
import sqlite3
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = pathlib.Path(os.environ["USERPROFILE"]) / ".cargo" / "bin" / "cargo.exe"
TARGET = ROOT.parent / "agent-cockpit-health-review" / "cargo-target"
MARKER = '.agentcockpit-shared-history.json'


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


shared = load('shared_history', ROOT / 'src-tauri/scripts/chatgpt-history-shared.py')
fixture_mod = load('shared_fixture', ROOT / 'tests/chatgpt-history-sync.test.py')


def make_fixture(base):
    """在指定基目录下构造 3 homes（复用 HistoryReplication.setUp 的建库逻辑）。"""
    case = fixture_mod.HistoryReplication('runTest')
    import unittest.mock as mock
    with mock.patch('tempfile.TemporaryDirectory') as td:
        holder = mock.MagicMock()
        holder.name = str(base)
        td.return_value = holder
        case.setUp()
    case.base = base.resolve()
    case.homes = [case.base / n for n in ('first', 'second', 'third')]
    case.state = case.base / 'sync'
    return case


def scenario(fx, kind):
    c, h1, h2 = fx.homes
    if kind == 'migrate':
        fx.put(c, 'native-chat', ['native'])
        fx.put(h1, 'private-chat', ['private'])
        fx.put(h1, 'arch-chat', ['archived body'], archived=1)
        orphan = h1 / 'sessions' / 'orphan.jsonl'
        orphan.write_bytes(b'{"text":"orphan"}\n')
        with fixture_mod.database(h1 / 'state_5.sqlite') as con:
            con.execute("INSERT OR REPLACE INTO projects VALUES('p1','proj')")
            con.execute("INSERT OR REPLACE INTO project_roots VALUES('p1',0,?)", (str(h1),))
        return {'canonical': str(c), 'homes': [str(x) for x in fx.homes], 'busy_homes': []}
    if kind == 'busy':
        fx.put(c, 'native-chat', ['native'])
        fx.put(h1, 'private-chat', ['private'])
        return {'canonical': str(c), 'homes': [str(x) for x in fx.homes], 'busy_homes': [str(h2)]}
    if kind == 'conflict':
        fx.put(c, 'chat', ['first', 'shared branch'])
        fx.put(h1, 'chat', ['first', 'private branch'])
        return {'canonical': str(c), 'homes': [str(x) for x in fx.homes], 'busy_homes': []}
    if kind == 'interrupted':
        fx.put(c, 'native-chat', ['native'])
        fx.put(h1, 'private-chat', ['private'])
        request = {'canonical': str(c), 'homes': [str(x) for x in fx.homes], 'busy_homes': []}
        shared.synchronize(dict(request, state_dir=str(fx.state)))
        # 模拟中断残留：重新放一个旧库文件（marker 已在）。
        shutil.copyfile(c / 'state_5.sqlite', h1 / 'state_9.sqlite')
        return request
    raise ValueError(kind)


def db_rows(path):
    if not path.is_file():
        return None
    con = sqlite3.connect(path.as_uri() + '?mode=ro', uri=True)
    try:
        con.row_factory = sqlite3.Row
        tables = [r[0] for r in con.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
        out = {}
        for t in tables:
            rows_ = [dict(r) for r in con.execute(f'SELECT * FROM "{t}"')]
            rows_.sort(key=lambda r: json.dumps(r, sort_keys=True, default=str))
            out[t] = rows_
        return out
    finally:
        con.close()


def snapshot_tree(fx):
    """迁移后全内容快照：每 home 的库行 + 文件字节 + 链接拓扑 + 备份统计。"""
    out = {}
    for home in fx.homes:
        entry = {'files': {}, 'links': {}, 'dbs': {}}
        for p in sorted(home.rglob('*')):
            rel = str(p.relative_to(home)).replace('\\', '/')
            if p.name in ('state_5.sqlite', 'thread_history_1.sqlite') or p.name == 'codex-dev.db':
                entry['dbs'][rel] = db_rows(p)
                continue
            if p.is_dir():
                # junction 判定：os.path.realpath != 自身
                real = pathlib.Path(os.path.realpath(p))
                if real != p.resolve():
                    entry['links'][rel] = str(real)
                continue
            if p.is_file():
                if p.suffix in ('.jsonl', '.toml'):
                    entry['files'][rel] = p.read_bytes().decode('utf-8', 'replace')
                elif p.suffix == '.json':
                    # JSON 文件按键排序 + 时间戳归一后比对（serde_json 默认按键排序，
                    # python 按插入序——字节序不同属序列化口径，内容必须一致）。
                    try:
                        entry['files'][rel] = json.loads(p.read_text(encoding='utf-8'))
                    except ValueError:
                        entry['files'][rel] = p.read_bytes().decode('utf-8', 'replace')
                else:
                    entry['files'][rel] = '<bin:%d>' % p.stat().st_size
        out[home.name] = entry
    state = fx.state
    # 备份 tag = sha256(str(home))[:16]，含基目录必然不同——按 home 名归一，
    # 并解压 gz 比对库内容（比只比对文件名更强的回滚保障验证）。
    import gzip
    import hashlib
    backups = {}
    if state.exists():
        tagmap = {hashlib.sha256(str(h).encode()).hexdigest()[:16]: h.name for h in fx.homes}
        for p in sorted(state.rglob('*.gz')):
            rel = p.relative_to(state)
            tag = rel.parts[1] if len(rel.parts) > 2 else rel.parts[0]
            home_name = tagmap.get(tag, tag)
            with tempfile.NamedTemporaryFile(suffix='.sqlite', delete=False) as tf:
                tf.write(gzip.decompress(p.read_bytes()))
                tmp_db = pathlib.Path(tf.name)
            try:
                backups.setdefault(home_name, {})[p.name] = db_rows(tmp_db)
            finally:
                tmp_db.unlink()
    out['_backups'] = backups
    sj = state / 'shared.json'
    out['_shared'] = json.loads(sj.read_text(encoding='utf-8')) if sj.is_file() else None
    return out


def normalize(obj, base):
    base_s = str(base)
    base_esc = base_s.replace('\\', '\\\\')  # TOML/JSON 文本里的转义路径形态
    def norm(v):
        if isinstance(v, str):
            return v.replace(base_s, '<BASE>').replace(base_esc, '<BASE>')
        if isinstance(v, dict):
            return {k: ('<TIME>' if k in ('updated_at', 'migrated_at') else norm(x)) for k, x in v.items()}
        if isinstance(v, list):
            return [norm(x) for x in v]
        return v
    return norm(obj)


def run_case(kind, tmp):
    results = {}
    for side in ('py', 'rs'):
        base = tmp / kind / side
        base.mkdir(parents=True)
        fx = make_fixture(base)
        request = scenario(fx, kind)
        request['state_dir'] = str(fx.state)
        if side == 'py':
            report = shared.synchronize(request)
            for home in fx.homes:
                assert (home / 'auth.json').read_bytes() == b'private-' + home.name.encode(), 'auth 被动过'
            results['py'] = (normalize(report, base), normalize(snapshot_tree(fx), base), fx, request)
        else:
            py_report, py_tree, py_fx, _ = results['py']
            req_path = tmp / kind / 'req.json'
            py_report_path = tmp / kind / 'py_report.json'
            req_path.write_text(json.dumps(dict(request, py_report=str(py_report_path),
                py_base=str(tmp / kind / 'py'), rs_base=str(base), verify='driver'), ensure_ascii=False), encoding='utf-8')
            py_report_path.write_text(json.dumps(py_report, ensure_ascii=False), encoding='utf-8')
            env = dict(os.environ)
            env['CARGO_TARGET_DIR'] = str(TARGET)
            env['T18_SHARED_REQ'] = str(req_path)
            p = subprocess.run([str(CARGO), 'test', '--bin', 'agent-cockpit',
                                'chatgpt_history_shared::tests::golden_matches_python', '--', '--nocapture', '--exact'],
                               cwd=str(ROOT / 'src-tauri'), env=env, capture_output=True, timeout=900)
            out = (p.stdout + p.stderr).decode('utf-8', 'replace')
            for line in out.splitlines():
                if 'test result' in line or 'panicked' in line or 'mismatch' in line:
                    print(f'[{kind}]', line[:500])
            if p.returncode != 0:
                print(out[-3000:])
                return p.returncode
            for home in fx.homes:
                assert (home / 'auth.json').read_bytes() == b'private-' + home.name.encode(), 'auth 被动过'
            rs_tree = normalize(snapshot_tree(fx), base)
            if rs_tree != py_tree:
                # 定位首个差异：逐层下钻到具体键，打印完整值。
                def first_diff(a, b, path=''):
                    if isinstance(a, dict) and isinstance(b, dict):
                        for k2 in sorted(set(a) | set(b)):
                            if a.get(k2) != b.get(k2):
                                return first_diff(a.get(k2), b.get(k2), f'{path}.{k2}')
                        return None
                    return (path, a, b)
                d = first_diff(py_tree, rs_tree)
                if d:
                    print(f'[{kind}] 内容不一致 {d[0]}:')
                    print('  py=', json.dumps(d[1], ensure_ascii=False, default=str)[:1200])
                    print('  rs=', json.dumps(d[2], ensure_ascii=False, default=str)[:1200])
                return 1
            print(f'[{kind}] 报告+全内容一致；migrated={len(py_report.get("migrated", []))} '
                  f'pending={len(py_report.get("pending", []))} released={py_report.get("released_bytes")} '
                  f'backups={len(py_tree["_backups"])}')
    return 0


def main():
    # fixture 放会话 scratch（运行时保留区）：系统 temp 下宿主删除保护会拦
    # python 侧 shutil.rmtree(stage)（retention scan incomplete），属环境限制非产品缺陷。
    scratch = os.environ.get('BOX_AGENT_SCRATCH_DIR')
    parent = pathlib.Path(scratch) if scratch and pathlib.Path(scratch).is_dir() else None
    tmp = pathlib.Path(tempfile.mkdtemp(prefix='t18-shared-golden-', dir=str(parent) if parent else None))
    try:
        for kind in ('migrate', 'busy', 'conflict', 'interrupted'):
            code = run_case(kind, tmp)
            if code != 0:
                return code
        print('ALL SHARED GOLDEN OK')
        return 0
    finally:
        # junction 存在时 rmtree 默认不跟随（Windows），安全清理。
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == '__main__':
    sys.exit(main())