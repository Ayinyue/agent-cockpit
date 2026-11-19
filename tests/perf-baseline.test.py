# -*- coding: utf-8 -*-
"""性能基准回归：消费 baseline_perf.json，让性能退化能被闸门拦住。

为什么要有这条（综合评估报告短板 #6 / PERFORMANCE_PLAN §5.1）：
    项目原先只有注释里的历史耗时，没有基准文件、没有任何测试消费它——
    「有数据、没机制」，性能退化不会被任何闸门发现。

这条测试守的是**基准本身的完整性与口径**，不是「今天必须比昨天快」：
    1. baseline_perf.json 必须存在、结构完整、由实测产生（有 samples、有 conditions）。
    2. 口径必须是 median + repeat>=3（不许改成取最好值来美化数字）。
    3. 数字必须自洽（roundtrip = read+loads+dumps，偏差不超过舍入误差）。
    4. 若本机存在用量缓存，则实测值不得比基准显著恶化（阈值宽松，避免机器负载误红）。

第 4 条只在能测到缓存时生效；测不到就跳过并说明，绝不假装通过。
"""
import importlib.util
import json
import os
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BASELINE = ROOT / "baseline_perf.json"

spec = importlib.util.spec_from_file_location("perf_baseline", ROOT / "tools/perf_baseline.py")
perf = importlib.util.module_from_spec(spec)
spec.loader.exec_module(perf)

# 恶化容忍倍数：跨机器/负载差异很大，这里只拦「数量级退化」，不当微调旋钮。
REGRESSION_FACTOR = 3.0


class BaselineContract(unittest.TestCase):
    def setUp(self):
        self.assertTrue(BASELINE.is_file(),
                        "缺 baseline_perf.json：先跑 python tools/perf_baseline.py 生成")
        self.data = json.loads(BASELINE.read_text(encoding="utf-8"))

    def test_schema_and_measurement_provenance(self):
        self.assertEqual(self.data.get("schema"), "agent-cockpit-perf/1")
        self.assertTrue(self.data.get("measured_at"), "必须记录测量时间，否则无法判断可比性")
        conditions = self.data.get("conditions") or {}
        for key in ("python", "platform", "cpu_count"):
            self.assertIn(key, conditions, f"conditions 缺 {key}：跨机比较要先看环境")

    def test_aggregation_is_median_of_at_least_three(self):
        self.assertEqual(self.data.get("aggregation"), "median",
                         "口径必须是中位数；取最好值是美化数字")
        self.assertGreaterEqual(int(self.data.get("repeat", 0)), 3, "至少测 3 次")

    def test_numbers_are_self_consistent(self):
        cache = self.data.get("usage_cache") or {}
        self.assertIn("file_count", cache)
        self.assertIn("total_bytes", cache)
        for item in cache.get("top3") or []:
            for key in ("read_ms", "loads_ms", "dumps_ms", "roundtrip_ms", "bytes"):
                self.assertIsInstance(item.get(key), (int, float), f"{key} 必须是实测数字")
                self.assertGreaterEqual(item[key], 0)
            expected = item["read_ms"] + item["loads_ms"] + item["dumps_ms"]
            self.assertAlmostEqual(item["roundtrip_ms"], expected, delta=0.5,
                                   msg="roundtrip 必须等于三段之和（口径不许漂）")
            samples = item.get("samples") or {}
            for key in ("read", "loads", "dumps"):
                self.assertGreaterEqual(len(samples.get(key) or []), 3,
                                        f"{key} 必须留下全部样本，便于复核中位数")

    def test_entries_counted_for_dict_shaped_cache(self):
        # files 是字典；早先按列表数会让 entries 恒为 None。
        for item in (self.data.get("usage_cache") or {}).get("top3") or []:
            if item["bytes"] > 1024:
                self.assertIsNotNone(item.get("entries"), "非空缓存必须数出条目数")
                self.assertGreater(item["entries"], 0)


class NoSignificantRegression(unittest.TestCase):
    """能测到本机缓存时，比对基准；测不到就跳过（不假装通过）。"""

    def test_current_machine_not_dramatically_slower(self):
        if not BASELINE.is_file():
            self.skipTest("没有基准文件")
        baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
        largest = (baseline.get("usage_cache") or {}).get("largest")
        if not largest:
            self.skipTest("基准里没有缓存样本（生成时本机无用量缓存）")
        cache_dir = perf.data_root() / "usage-cache"
        target = cache_dir / largest["name"]
        if not target.is_file():
            self.skipTest(f"本机没有基准所用的缓存文件 {largest['name'][:12]}…，无法同口径比对")
        current = perf.measure_cache_file(target)
        # 文件长大了按体积归一，避免把「数据变多」误判成「代码变慢」
        base_per_mb = largest["roundtrip_ms"] / max(1.0, largest["bytes"] / 1e6)
        cur_per_mb = current["roundtrip_ms"] / max(1.0, current["bytes"] / 1e6)
        self.assertLess(
            cur_per_mb, base_per_mb * REGRESSION_FACTOR,
            f"每 MB 往返耗时从 {base_per_mb:.1f}ms 恶化到 {cur_per_mb:.1f}ms（超过 {REGRESSION_FACTOR}×）；"
            f"先确认机器负载，再查是否真的引入了性能退化")


if __name__ == "__main__":
    unittest.main()