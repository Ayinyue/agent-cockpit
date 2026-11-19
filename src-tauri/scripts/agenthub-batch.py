"""T5：agenthub 批量取数 wrapper（编排优化，不改 vendored agenthub.py 一个字节）。

背景：autofetch 原先逐账号起一个 python 进程跑 `agenthub quota`，每账号一次冷启
（实测中位 ~1s）。T12 已把串行改受控并发（墙钟≈最慢账号）；本 wrapper 是更进一步的
编排路线——**一次进程冷启，导入 agenthub 模块，循环调 cmd_quota 处理多账号**，把 N 次
冷启压成 1 次。实测 3 账号批量 2306ms（对比串行 5133ms / 3 并发 2582ms）。

可行性依据（均已实测）：
  - agenthub.py 可干净导入（无 import 副作用，~536ms）；
  - quota 命令在 main 里走 `args.fn(None, args)`，cmd_quota 不用数据库（con=None）；
  - cmd_quota(con, args) 是模块级函数，args 用 argparse.Namespace 构造即可。

用法：
  python agenthub-batch.py <agenthub.py 路径> <targets JSON>
  targets = [{"product": "qoder-cn", "profile": "<档案目录>"}, ...]
  输出：每账号一行 JSON（与单独跑 `agenthub quota --json` 同形），用分隔符包裹便于 Rust 侧切分。

**本脚本是 T5 地基**：与 T12 并发是互斥的两条取数路线，集成到 autofetch 需单独批次
（替换取数架构），本轮不强行替换已验证的 T12 并发。vendored agenthub.py 字节不变。
"""
import argparse
import contextlib
import importlib.util
import io
import json
import sys


def main():
    ah_path = sys.argv[1]
    targets = json.loads(sys.argv[2])
    spec = importlib.util.spec_from_file_location("agenthub", ah_path)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    results = []
    for t in targets:
        args = argparse.Namespace(
            product=t.get("product"),
            profile=t.get("profile"),
            local=bool(t.get("local", False)),
            json=True,
            write=False,
        )
        buf = io.StringIO()
        err = None
        try:
            with contextlib.redirect_stdout(buf):
                m.cmd_quota(None, args)
        except Exception as e:  # noqa: BLE001
            err = f"{type(e).__name__}: {e}"
        results.append({
            "product": t.get("product"),
            "profile": t.get("profile"),
            "ok": err is None,
            "error": err,
            "data": buf.getvalue(),
        })
    # 每账号一条 JSON，外层数组；Rust 侧按行/数组解析
    print(json.dumps(results, ensure_ascii=False))


if __name__ == "__main__":
    main()