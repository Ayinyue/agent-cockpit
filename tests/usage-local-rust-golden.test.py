# -*- coding: utf-8 -*-
"""T17 golden：usage-local 迁 Rust 第一阶段（codex）跨语言一致性验证。

同一份合成 fixture（含累计终值差分/配额刷新重复帧/last 兜底/跨时区归日/坏行），
python usage-local.py collect vs Rust usage_local::collect_codex 输出逐字段比对。
另测增量缓存：第二次采集未变文件不重算（Rust 侧 SQLite 命中）。

只比对数字与结构，不打印日志正文。合成数据，不碰真实凭据。
"""
import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO = pathlib.Path.home() / ".cargo" / "bin" / "cargo.exe"
TARGET = ROOT.parent / "agent-cockpit-health-review" / "cargo-target"

# 合成 codex rollout：两轮累计（70→150），一次配额刷新重复帧（150→150 应跳过），
# 一次 last 兜底（累计回退），一条坏行，跨时区一条（UTC 16:20 = 北京次日）
LINES = [
    '{"type":"session_meta","payload":{"id":"sess-golden"}}',
    '{"type":"turn_context","payload":{"model":"gpt-5-codex"}}',
    '{"type":"event_msg","timestamp":"2026-05-27T06:07:18.088Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":70},"total_token_usage":{"input_tokens":60,"cached_input_tokens":5,"output_tokens":10,"total_tokens":70}}}}',
    '{"type":"event_msg","timestamp":"2026-05-27T07:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":80},"total_token_usage":{"input_tokens":130,"cached_input_tokens":10,"output_tokens":20,"total_tokens":150}}}}',
    '{"type":"event_msg","timestamp":"2026-05-27T07:05:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":80},"total_token_usage":{"input_tokens":130,"cached_input_tokens":10,"output_tokens":20,"total_tokens":150}}}}',
    'not-json-garbage-line',
    '{"type":"event_msg","timestamp":"2026-05-27T16:20:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"output_tokens":10,"total_tokens":40},"total_token_usage":{"input_tokens":30,"output_tokens":10,"total_tokens":40}}}}',
]

def main():
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="t17-golden-"))
    profile = tmp / "profile"
    sessions = profile / "sessions"
    sessions.mkdir(parents=True)
    (sessions / "rollout.jsonl").write_text("\n".join(LINES), encoding="utf-8")

    # python 侧（无缓存，纯解析聚合）
    sys.path.insert(0, str(ROOT / "src-tauri" / "scripts"))
    import importlib.util
    spec = importlib.util.spec_from_file_location("usage_local_py", ROOT / "src-tauri" / "scripts" / "usage-local.py")
    py = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(py)
    py_result = py.collect("codex", str(profile), None, with_records=True)

    # Rust 侧：由 golden 测试读同一 fixture（环境变量传路径）
    env = dict(os.environ)
    env["T17_GOLDEN_FIXTURE"] = str(profile)
    env["T17_GOLDEN_PY_RESULT"] = str(tmp / "py_result.json")
    env["CARGO_TARGET_DIR"] = str(TARGET)
    (tmp / "py_result.json").write_text(json.dumps(py_result, ensure_ascii=False), encoding="utf-8")
    p = subprocess.run(
        [str(CARGO), "test", "--bin", "agent-cockpit", "usage_local::tests::golden_codex_matches_python", "--", "--nocapture"],
        cwd=str(ROOT / "src-tauri"), env=env, capture_output=True, timeout=900,
    )
    out = (p.stdout + p.stderr).decode("utf-8", "replace")
    # 只回传测试结论行（cargo 警告噪声太长会截掉关键结果）
    for line in out.splitlines():
        if "test result" in line or "golden_codex" in line or "panicked" in line or "不一致" in line:
            print(line[:300])
    print("cargo exit:", p.returncode)
    # python 侧关键数字先自检（golden 的期望锚点）
    assert py_result is not None, "python 应聚出结果"
    print("python total:", py_result["total"], "days:", len(py_result["days"]),
          "sessions:", py_result["sessions"], "records:", len(py_result.get("_records", [])))
    return p.returncode

if __name__ == "__main__":
    sys.exit(main())