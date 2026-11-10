"""Pure evidence validation only: no Windows socket/handle API is called."""

import copy
import importlib.util
import json
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("handle_caller", Path(__file__).with_name("caller.py"))
C = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(C)


class EvidenceTests(unittest.TestCase):
    def records(self, mode="success", counts=(10, 17, 19, 21, 23, 15)):
        rows = [{"kind": "identity", "mode": mode, "pid": 42, "started": 123456}]
        phases = [("cold", None), ("setup", None), ("after-operation", 0),
                  ("after-operation", 1), ("after-operations", None), ("post-join", None)]
        rows += [{"kind": "count", "phase": phase, "iteration": iteration, "returned": 1,
                  "count": n, "error": None} for (phase, iteration), n in zip(phases, counts)]
        for n in range(2):
            rows.append({"kind": "operation", "iteration": n,
                         "api": {"success": "connected", "wrong-start": "different-start", "withheld": "read-expired"}[mode],
                         "server_receipt": "no-dial" if mode == "wrong-start" else "closed",
                         "server_eof_and_close_confirmed": mode != "wrong-start",
                         "close_returned": None if mode == "wrong-start" else 0,
                         "close_error": None, "no_dial_error": C.WSAEWOULDBLOCK if mode == "wrong-start" else None,
                         "raw_debug_hex": "61637475616c"})
        rows.append({"kind": "result", "mode": mode, "functional_pass": True,
                     "server_joined": True, "counts_known": True, "listener_close_rc": None,
                     "handle_stability": "raw-point-counts-only"})
        return rows

    def log(self, rows, summary="test result: ok. 1 passed; 0 failed; 0 ignored; 7 filtered out;"):
        return ("\n".join("BORROWED_HANDLES " + json.dumps(r) for r in rows) + "\n" + summary).encode()

    def issues(self, rows, mode="success", code=0):
        return C.validate(mode, 2, code, self.log(rows))[1]

    def test_functional_pass_does_not_normalize_or_judge_unequal_raw_counts(self):
        rows = self.records()
        parsed, issues = C.validate("success", 2, 0, self.log(rows))
        self.assertEqual(issues, [])
        self.assertEqual([r["count"] for r in parsed if r["kind"] == "count"], [10, 17, 19, 21, 23, 15])

    def test_all_three_exact_modes_have_distinct_required_receipts(self):
        for mode in C.MODES:
            with self.subTest(mode=mode):
                self.assertEqual(self.issues(self.records(mode), mode), [])

    def test_bool_and_out_of_range_are_not_native_dword_counts(self):
        for value in (True, -1, 1 << 32, "17"):
            rows = self.records()
            rows[1]["count"] = value
            with self.subTest(value=value):
                self.assertTrue(self.issues(rows))

    def test_failed_count_is_unknown_and_never_zero_or_success(self):
        rows = self.records()
        rows[1].update(returned=0, count=None, error=5)
        self.assertTrue(self.issues(rows))
        rows[1]["count"] = 0
        self.assertTrue(self.issues(rows))

    def test_missing_cold_reordered_or_duplicate_samples_are_rejected(self):
        rows = self.records()
        for changed in (rows[:1] + rows[2:], rows[:1] + [rows[2], rows[1]] + rows[3:], rows + [rows[1]]):
            self.assertTrue(self.issues(changed))

    def test_close_failure_and_boolean_close_are_not_closed_receipts(self):
        for value in (-1, False, None):
            rows = self.records()
            next(r for r in rows if r["kind"] == "operation")["close_returned"] = value
            with self.subTest(value=value):
                self.assertTrue(self.issues(rows))

    def test_join_request_or_unknown_is_not_actual_join(self):
        for value in (False, None, 1):
            rows = self.records()
            rows[-1]["server_joined"] = value
            with self.subTest(value=value):
                self.assertTrue(self.issues(rows))

    def test_zero_filtered_pass_and_loader_only_output_are_rejected(self):
        output = self.log(self.records(), "test result: ok. 0 passed; 0 failed; 0 ignored;")
        self.assertTrue(C.validate("success", 2, 0, output)[1])
        self.assertTrue(C.validate("success", 2, 0, b"launcher complete\n")[1])

    def test_timeout_failure_wrong_mode_and_leak_claim_are_rejected(self):
        for code in (101, 124, -9):
            self.assertTrue(self.issues(self.records(), code=code))
        self.assertTrue(self.issues(self.records("withheld")))
        rows = self.records()
        rows[-1]["handle_stability"] = "leak-free"
        self.assertTrue(self.issues(rows))

    def test_duplicate_json_keys_and_malformed_records_are_rejected(self):
        for data in (b'BORROWED_HANDLES {"kind":"count","kind":"result"}', b'BORROWED_HANDLES false'):
            self.assertTrue(C.parse_native(data)[1])

    def test_no_observed_listener_close_code_is_fabricated(self):
        rows = self.records()
        rows[-1]["listener_close_rc"] = 0
        self.assertTrue(self.issues(rows))

    def test_positive_original_budgets_reserve_cleanup(self):
        self.assertIsNone(C.budget_error(100, 60, 40, 1))
        for values in ((100, 0, 40, 1), (100, 61, 40, 1), (100, 20, 100, 1),
                       (1 << 31, 20, 40, 3e6), (100, 20, 40, .1), (100, 20, 40, float("inf"))):
            with self.subTest(values=values):
                self.assertIsNotNone(C.budget_error(*values))


if __name__ == "__main__":
    unittest.main()
