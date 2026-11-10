#!/usr/bin/env python3
"""Pure synthetic reporting checks; imports run.py without invoking its main."""

import copy
from pathlib import Path
import runpy
import unittest

summarize = runpy.run_path(str(Path(__file__).with_name("run.py")))["summarize"]


def case(index, count, elapsed):
    return {"case": index, "ready_count": count, "rounds_requested": 1,
            "rounds_completed": 1, "complete": True,
            "waits": [{"wait_ns": elapsed, "observations": [
                {"write_completed_to_wait_return_ns": elapsed,
                 "wait_return_to_drained_ns": elapsed}
                for _ in range(count)]}]}


class SummaryTests(unittest.TestCase):
    def test_each_case_stays_separate_including_repeated_ready_counts(self):
        raw = [case(0, 1, 10), case(1, 2, 100), case(2, 1, 30)]
        original = copy.deepcopy(raw)
        result = summarize(raw, 0, len(raw))
        self.assertTrue(result["complete"])
        self.assertEqual(result["recorded_waits"], 3)
        self.assertEqual([item["case"] for item in result["case_summaries"]], [0, 1, 2])
        self.assertEqual([item["ready_count"] for item in result["case_summaries"]], [1, 2, 1])
        self.assertEqual([item["wait_ns"]["p50"] for item in result["case_summaries"]], [10, 100, 30])
        self.assertEqual([item["write_completed_to_wait_return_ns"]["n"]
                          for item in result["case_summaries"]], [1, 2, 1])
        self.assertEqual(raw, original)

    def test_failure_gap_or_parse_error_suppresses_all_success_distributions(self):
        raw = [case(0, 1, 10), case(1, 2, 100)]
        failed = copy.deepcopy(raw)
        failed[1]["complete"] = False
        for cases, status, expected, error in [
            (raw, 1, 2, None), (raw, "Timeout", 2, None),
            (raw[:1], 0, 2, None), (raw, 0, 2, "invalid JSONL"),
            (failed, 0, 2, None),
        ]:
            with self.subTest(status=status, expected=expected, error=error):
                result = summarize(cases, status, expected, error)
                self.assertFalse(result["complete"])
                for item in result["case_summaries"]:
                    for field in ["wait_ns", "write_completed_to_wait_return_ns", "wait_return_to_drained_ns"]:
                        self.assertEqual(item[field], {"n": 0, "p50": None, "p90": None,
                                                       "p99": None, "max": None})


if __name__ == "__main__":
    unittest.main()
