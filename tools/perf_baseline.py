#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""性能基准：把「注释级实测」变成「闸门级实测」。

为什么要有这个文件（docs/PERFORMANCE_PLAN.md §5.1 + 综合评估报告短板 #6）：
    优化前项目只有注释里的历史耗时（get_state 300ms+ / 档案计数 250-300ms），
    没有基准文件、没有回归消费——性能退化不会被任何闸门拦住，属"有数据、没机制"。
    本脚本按附录 B 的统一口径实测并落盘 baseline_perf.json，
    tests/perf-baseline.test.py 消费它做回归断言。

口径（不许改，否则两次测量不可比）：
    1. 每项测 REPEAT 次取**中位数**，不取最好值——最好值是运气，中位数才是日常。
    2. 只测**纯计算**部分（read / loads / dumps），不含进程启动，避免把冷启混进来。
    3. 记录测量环境（机器负载无法控制，所以把 conditions 一起落盘，让人能判断可比性）。
    4. 缓存文件是**只读**打开：本脚本跑一百遍也不会弄脏用户数据。

用法：
    python tools/perf_baseline.py                    # 实测并写 baseline_perf.json
    python tools/perf_baseline.py --json 别的.json   # 换个落盘位置
    python tools/perf_baseline.py --print            # 只打印不落盘
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import time
from pathlib import Path

REPEAT = 3
ROOT = Path(__file__).resolve().parents[1]


def data_root() -> Path:
    """数据根：与 registry::base_dir 同一套判定（指针文件优先，其次 AGENT_COCKPIT_HOME）。"""
    env = os.environ.get("AGENT_COCKPIT_HOME")
    if env:
        return Path(env)
    pointer = Path.home() / ".agent-cockpit-root"
    if pointer.is_file():
        try:
            text = pointer.read_text(encoding="utf-8").strip()
            if text:
                return Path(text)
        except OSError:
            pass
    return Path.home() / ".agent-cockpit"


def median_ms(fn, repeat=REPEAT):
    """跑 repeat 次取中位数（毫秒）。返回 (中位数, 全部样本)。"""
    samples = []
    for _ in range(repeat):
        started = time.perf_counter()
        fn()
        samples.append((time.perf_counter() - started) * 1000.0)
    return round(statistics.median(samples), 1), [round(s, 1) for s in samples]


def measure_cache_file(path: Path) -> dict:
    """单个用量缓存文件的 read / loads / dumps 三段耗时（附录 B 第 1 项）。"""
    raw_holder = {}

    def do_read():
        raw_holder["raw"] = path.read_bytes()

    read_ms, read_samples = median_ms(do_read)
    raw = raw_holder["raw"]
    obj_holder = {}

    def do_loads():
        obj_holder["obj"] = json.loads(raw)

    loads_ms, loads_samples = median_ms(do_loads)
    obj = obj_holder["obj"]

    def do_dumps():
        json.dumps(obj)

    dumps_ms, dumps_samples = median_ms(do_dumps)
    # 缓存结构是 {"version":N,"files":{相对路径:{stamp,records}}}——files 是**字典**，
    # 早先按列表数导致 entries 恒为 None（工具报"无" ≠ 事实为"无"）。
    files = obj.get("files") if isinstance(obj, dict) else None
    entries = len(files) if isinstance(files, (list, dict)) else None
    return {
        "name": path.name,
        "bytes": path.stat().st_size,
        "entries": entries,
        "read_ms": read_ms,
        "loads_ms": loads_ms,
        "dumps_ms": dumps_ms,
        "roundtrip_ms": round(read_ms + loads_ms + dumps_ms, 1),
        "samples": {"read": read_samples, "loads": loads_samples, "dumps": dumps_samples},
    }


def collect(repeat=REPEAT) -> dict:
    cache_dir = data_root() / "usage-cache"
    files = sorted(cache_dir.glob("*.json"), key=lambda p: p.stat().st_size, reverse=True) if cache_dir.is_dir() else []
    measured = [measure_cache_file(p) for p in files[:3]]
    total_bytes = sum(p.stat().st_size for p in files)
    return {
        "schema": "agent-cockpit-perf/1",
        "measured_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
        "repeat": repeat,
        "aggregation": "median",
        "conditions": {
            "note": "纯计算耗时，不含进程启动；机器负载未受控，跨机比较前先看 conditions",
            "python": sys.version.split()[0],
            "platform": sys.platform,
            "cpu_count": os.cpu_count(),
        },
        "usage_cache": {
            # 相对数据根的描述，不落绝对路径：baseline_perf.json 要随仓库分发，
            # 绝对路径含开发者家目录（privacy_check 的 personal-home-path 会拦），
            # 且换机器后本来就没有可比性。测试契约不消费 dir，回归比对用
            # perf.data_root() 现场解析（见 perf-baseline.test.py 的同口径比对）。
            "dir": "<数据根>/usage-cache",
            "file_count": len(files),
            "total_bytes": total_bytes,
            "largest": measured[0] if measured else None,
            "top3": measured,
        },
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="性能基准实测（附录 B 口径）")
    ap.add_argument("--json", default=str(ROOT / "baseline_perf.json"), help="落盘位置")
    ap.add_argument("--print", action="store_true", help="只打印不落盘")
    ap.add_argument("--repeat", type=int, default=REPEAT)
    args = ap.parse_args()

    result = collect(args.repeat)
    cache = result["usage_cache"]
    print("用量缓存: %d 个文件 / %.1f MB" % (cache["file_count"], cache["total_bytes"] / 1e6))
    for item in cache["top3"]:
        print("  %-16s %7.1f MB  read=%6.1fms loads=%7.1fms dumps=%7.1fms  合计=%7.1fms  条目=%s" % (
            item["name"][:16], item["bytes"] / 1e6, item["read_ms"], item["loads_ms"],
            item["dumps_ms"], item["roundtrip_ms"], item["entries"]))
    if not args.print:
        target = Path(args.json)
        target.write_text(json.dumps(result, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
        print("已写出:", target)
    return 0


if __name__ == "__main__":
    sys.exit(main())