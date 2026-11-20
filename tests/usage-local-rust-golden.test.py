# -*- coding: utf-8 -*-
"""T17 golden：usage-local 迁 Rust 跨语言一致性验证。

第一阶段（codex）：合成 fixture（累计终值差分/配额刷新重复帧/last 兜底/跨时区归日/坏行）。
第二阶段（messages 系）：claude/workbuddy/minimax 三种合成 fixture——
  session:messageId 分组、fragment 去重、user 输入估算累计、runtime-config 模型、
  usage 取组内最大帧、无 usage 可见文本估算、claude/minimax/standard 三种 style。
另支持真实数据抽样：T17_REAL_APPS=claude,qoder-cn,... 逐个跑 python↔Rust 全量比对
（with_records=True 时逐事件 id + 字段；大目录可用 T17_REAL_LIGHT=1 只比汇总）。

只比对数字与结构，不打印日志正文。合成数据不碰真实凭据；真实抽样只读不写源。
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
CODEX_LINES = [
    '{"type":"session_meta","payload":{"id":"sess-golden"}}',
    '{"type":"turn_context","payload":{"model":"gpt-5-codex"}}',
    '{"type":"event_msg","timestamp":"2026-05-27T06:07:18.088Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":70},"total_token_usage":{"input_tokens":60,"cached_input_tokens":5,"output_tokens":10,"total_tokens":70}}}}',
    '{"type":"event_msg","timestamp":"2026-05-27T07:00:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":80},"total_token_usage":{"input_tokens":130,"cached_input_tokens":10,"output_tokens":20,"total_tokens":150}}}}',
    '{"type":"event_msg","timestamp":"2026-05-27T07:05:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"total_tokens":80},"total_token_usage":{"input_tokens":130,"cached_input_tokens":10,"output_tokens":20,"total_tokens":150}}}}',
    'not-json-garbage-line',
    '{"type":"event_msg","timestamp":"2026-05-27T16:20:00.000Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"output_tokens":10,"total_tokens":40},"total_token_usage":{"input_tokens":30,"output_tokens":10,"total_tokens":40}}}}',
]


def j(obj):
    return json.dumps(obj, ensure_ascii=False)


def claude_lines():
    """claude 风格：ISO-Z 时间戳、message.id 分组、fragment uuid 去重、
    无 usage 估算、claude style（cache 单列）、runtime-config 模型兜底。"""
    return [
        j({"type": "runtime-config", "model": "claude-sonnet-4", "sessionId": "sess-claude"}),
        # user 输入：中文 4 字 + ascii 8 字符 → estimate = 4 + 2 = 6
        j({"type": "user", "sessionId": "sess-claude", "timestamp": "2026-05-27T06:00:00.000Z",
           "message": {"role": "user", "content": [{"type": "text", "text": "你好世界abcdefgh"}]}}),
        # assistant 第一帧（同 message.id，usage 较小）+ 第二帧（usage 较大应胜出）
        j({"type": "assistant", "sessionId": "sess-claude", "timestamp": "2026-05-27T06:00:05.000Z",
           "message": {"role": "assistant", "id": "msg-1", "model": "claude-sonnet-4",
                       "content": [{"type": "text", "text": "第一段回答"}],
                       "usage": {"input_tokens": 10, "output_tokens": 5}}}),
        j({"type": "assistant", "sessionId": "sess-claude", "timestamp": "2026-05-27T06:00:06.000Z",
           "message": {"role": "assistant", "id": "msg-1", "model": "claude-sonnet-4",
                       "content": [{"type": "text", "text": "第一段回答"}, {"type": "text", "text": "续写部分"}],
                       "usage": {"input_tokens": 10, "output_tokens": 20,
                                 "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5}}}),
        # 重复 uuid 的 fragment（同文本不同行，归入无 usage 的 msg-2 组）——去重后估算不翻倍
        j({"type": "assistant", "sessionId": "sess-claude", "uuid": "frag-dup",
           "timestamp": "2026-05-27T07:00:10.000Z",
           "message": {"role": "assistant", "id": "msg-2", "content": [{"type": "text", "text": "重复片段"}]}}),
        # 同 uuid 但文本更新（后帧覆盖前帧）——fragment 去重失效时此帧会被重复计入而撞红
        j({"type": "assistant", "sessionId": "sess-claude", "uuid": "frag-dup",
           "timestamp": "2026-05-27T07:00:11.000Z",
           "message": {"role": "assistant", "id": "msg-2", "content": [{"type": "text", "text": "重复片段更长一些"}]}}),
        # 无 usage 的 assistant：走可见文本估算（user 输入 6 + 输出 "abcd"=1）
        j({"type": "user", "sessionId": "sess-claude", "timestamp": "2026-05-27T07:00:00.000Z",
           "message": {"role": "user", "content": "你好世界abcdefgh"}}),
        j({"type": "assistant", "sessionId": "sess-claude", "timestamp": "2026-05-27T07:00:09.000Z",
           "message": {"role": "assistant", "id": "msg-2",
                       "content": [{"type": "text", "text": "abcd"},
                                   {"type": "image", "source": {"data": "base64-hidden"}}]}}),
        # tool_result 可见文本 + thinking 不计
        j({"type": "assistant", "sessionId": "sess-claude", "timestamp": "2026-05-27T08:00:00.000Z",
           "message": {"role": "assistant", "id": "msg-3", "model": "claude-opus-4",
                       "content": [{"type": "tool_result", "content": [{"type": "text", "text": "工具结果"}]},
                                   {"type": "thinking", "thinking": "hidden-reasoning"}],
                       "usage": {"input_tokens": 50, "output_tokens": 8, "total_tokens": 58}}}),
        # 跨时区归日：UTC 16:20 = 北京次日
        j({"type": "assistant", "sessionId": "sess-claude", "timestamp": "2026-05-27T16:20:00.000Z",
           "message": {"role": "assistant", "id": "msg-4", "content": "ok",
                       "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}),
        "garbage-line-not-json",
        j({"type": "ai-title", "sessionId": "sess-claude", "title": "should-be-ignored"}),
    ]


def workbuddy_lines():
    """workbuddy 风格：毫秒整数时间戳、providerData.messageId 分组、
    function_call/model-usage 也计、standard style（cache 不另加）。"""
    return [
        j({"type": "message", "sessionId": "sess-wb", "timestamp": 1779861600000,
           "message": {"role": "user", "content": "输入文本abcd"}}),
        j({"type": "message", "sessionId": "sess-wb", "timestamp": 1779861605000,
           "providerData": {"messageId": "wb-1", "model": "wb-model", "usage": {"input_tokens": 30, "output_tokens": 12}},
           "message": {"role": "assistant", "content": [{"type": "text", "text": "回答一"}]}}),
        # function_call 类型也计入（is_wb 分支），同 messageId 归一组
        j({"type": "function_call", "sessionId": "sess-wb", "timestamp": 1779861606000,
           "providerData": {"messageId": "wb-1", "usage": {"input_tokens": 30, "output_tokens": 25, "total_tokens": 55}},
           "message": {"role": "assistant", "content": [{"type": "text", "text": "回答一"}, {"type": "text", "text": "追加"}]}}),
        # model-usage 独立帧
        j({"type": "model-usage", "sessionId": "sess-wb", "timestamp": 1779861700000,
           "providerData": {"messageId": "wb-2", "requestModelId": "wb-model-2"},
           "usage": {"prompt_tokens": 100, "completion_tokens": 40, "prompt_tokens_details": {"cached_tokens": 60}}}),
        # 无 id 的 assistant（index 兜底分组）+ 无 usage 估算
        j({"type": "message", "sessionId": "sess-wb", "timestamp": 1779861800000,
           "message": {"role": "assistant", "content": "你好"}}),
        j({"type": "reasoning", "sessionId": "sess-wb", "timestamp": 1779861801000,
           "message": {"role": "reasoning", "content": "不应计入"}}),
    ]


def minimax_lines():
    """minimax 风格：v2/sessions/**/messages.jsonl 布局、minimax style（cache 单列）、
    session 取父目录名。"""
    return [
        j({"type": "user", "timestamp": "2026-05-27T06:00:00.000Z",
           "message": {"role": "user", "content": "问题文本"}}),
        j({"type": "assistant", "timestamp": "2026-05-27T06:00:03.000Z",
           "message": {"role": "assistant", "id": "mm-1", "model": "abab7",
                       "content": [{"type": "output_text", "text": "答案"}],
                       "usage": {"input_tokens": 20, "output_tokens": 9,
                                 "cache_read_input_tokens": 30}}}),
        # total_tokens 显式给出时优先（不重算）
        j({"type": "assistant", "timestamp": "2026-05-27T06:05:00.000Z",
           "message": {"role": "assistant", "id": "mm-2", "model": "abab7",
                       "content": "ok",
                       "usage": {"input_tokens": 5, "output_tokens": 5, "total_tokens": 999}}}),
    ]


def load_py_module():
    import importlib.util
    spec = importlib.util.spec_from_file_location("usage_local_py", ROOT / "src-tauri" / "scripts" / "usage-local.py")
    py = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(py)
    return py


def run_rust_golden(test_name, env_extra, timeout=900):
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


def golden_case(py, app, profile, lines, rel_path, test_name, env_prefix):
    """写 fixture → python collect → Rust golden 比对。返回 (rc, py_result)。"""
    target = profile / rel_path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("\n".join(lines), encoding="utf-8")
    py_result = py.collect(app, str(profile), None, with_records=True)
    assert py_result is not None, f"{app}: python 应聚出结果"
    tmp = profile.parent
    py_path = tmp / f"py_result_{app}.json"
    py_path.write_text(json.dumps(py_result, ensure_ascii=False), encoding="utf-8")
    rc = run_rust_golden(test_name, {
        f"{env_prefix}_FIXTURE": str(profile),
        f"{env_prefix}_PY_RESULT": str(py_path),
        **({"T17M_GOLDEN_APP": app} if env_prefix == "T17M_GOLDEN" else {}),
    })
    print(f"[{app}] python total={py_result['total']} days={len(py_result['days'])} "
          f"sessions={py_result['sessions']} records={len(py_result.get('_records', []))} cargo_exit={rc}")
    return rc, py_result


def real_sample(py, app, root, light):
    """真实数据全量比对：python collect vs Rust collect（同一真实目录，只读）。"""
    with_records = not light
    py_result = py.collect(app, str(root), None, with_records=with_records)
    if py_result is None:
        print(f"[real:{app}] 无数据，跳过")
        return 0
    tmp = pathlib.Path(tempfile.mkdtemp(prefix=f"t17-real-{app}-"))
    py_path = tmp / "py_result.json"
    py_path.write_text(json.dumps(py_result, ensure_ascii=False), encoding="utf-8")
    other_apps = {"zcode", "opencode", "marvis", "astudio", "dsh", "gemini", "gemini-cli"}
    if app in other_apps:
        test_name = "usage_local::tests::golden_other_matches_python"
        env_extra = {"T17O_GOLDEN_FIXTURE": str(root), "T17O_GOLDEN_PY_RESULT": str(py_path),
                     "T17O_GOLDEN_APP": app}
    else:
        test_name = "usage_local::tests::golden_messages_matches_python"
        env_extra = {"T17M_GOLDEN_FIXTURE": str(root), "T17M_GOLDEN_PY_RESULT": str(py_path),
                     "T17M_GOLDEN_APP": app}
    rc = run_rust_golden(test_name, env_extra, timeout=1200)
    print(f"[real:{app}] files={py_result['files']} total={py_result['total']} "
          f"sessions={py_result['sessions']} light={light} cargo_exit={rc}")
    return rc


def main():
    py = load_py_module()
    failures = 0

    # 第一阶段：codex 合成
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="t17-golden-"))
    profile = tmp / "profile"
    rc, _ = golden_case(py, "codex", profile, CODEX_LINES, "sessions/rollout.jsonl",
                        "usage_local::tests::golden_codex_matches_python", "T17_GOLDEN")
    failures += (rc != 0)

    # 第二阶段：messages 合成（三种 style + 分组/去重/估算）
    for app, lines, rel in [
        ("claude", claude_lines(), "projects/proj-a/conv.jsonl"),
        ("workbuddy", workbuddy_lines(), "projects/proj-wb/conv.jsonl"),
        ("minimax", minimax_lines(), "v2/sessions/sess-mm/messages.jsonl"),
    ]:
        tmp_m = pathlib.Path(tempfile.mkdtemp(prefix=f"t17m-golden-{app}-"))
        rc, _ = golden_case(py, app, tmp_m / "profile", lines, rel,
                            "usage_local::tests::golden_messages_matches_python", "T17M_GOLDEN")
        failures += (rc != 0)

    # 真实数据抽样（可选）：T17_REAL_APPS=claude,qoder-cn,...；T17_REAL_LIGHT=1 只比汇总
    real_apps = os.environ.get("T17_REAL_APPS", "").strip()
    if real_apps:
        light = os.environ.get("T17_REAL_LIGHT", "") == "1"
        home = pathlib.Path.home()
        dir_map = {"claude": ".claude", "claude-code": ".claude", "qoder": ".qoder",
                   "qoder-cn": ".qoder-cn", "qoderwork": ".qoderwork", "qwenwork": ".qwenwork",
                   "qwenworkcn": ".qwenworkcn", "workbuddy": ".workbuddy",
                   "workbuddy-intl": ".workbuddy-intl", "codebuddy": ".codebuddy",
                   "codebuddy-cn": ".codebuddy-cn", "minimax": ".minimax",
                   "zcode": ".zcode", "opencode": ".opencode", "marvis": ".marvis",
                   "astudio": ".astudio", "dsh": ".dsh", "gemini": ".gemini", "gemini-cli": ".gemini"}
        for app in [a.strip() for a in real_apps.split(",") if a.strip()]:
            root = home / dir_map.get(app, "." + app)
            if not root.is_dir():
                print(f"[real:{app}] 目录不存在，跳过")
                continue
            failures += (real_sample(py, app, root, light) != 0)

    print("golden failures:", failures)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())