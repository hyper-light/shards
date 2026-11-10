"""Pure rejection checks: never executes the native/fault fixture."""

import argparse
import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest


SPEC = importlib.util.spec_from_file_location("terminal_order_caller", Path(__file__).with_name("caller.py"))
CALLER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CALLER)


class CallerEvidenceTests(unittest.TestCase):
    def complete_log(self, case="completion-lock"):
        return (
            f"TERMINAL_ORDER event=main-enter case={case} semantics=empty-unsubscribed\n"
            "TERMINAL_ORDER event=driver-return result=Ok(())\n"
            f"TERMINAL_ORDER result=pass case={case} bytes=64\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 15 filtered out;\n"
        ).encode()

    def test_exact_fixture_receipts_and_independent_bytes_are_required(self):
        log = self.complete_log()
        self.assertEqual(CALLER.verdict("completion-lock", 0, log, CALLER.ENDED, CALLER.ENDED), [])
        self.assertTrue(CALLER.verdict("completion-lock", 0, log, CALLER.ABORTED, CALLER.ENDED))

    def test_zero_filtered_pass_is_not_a_native_execution(self):
        log = self.complete_log().replace(b"1 passed;", b"0 passed;")
        self.assertTrue(CALLER.verdict("completion-lock", 0, log, CALLER.ENDED, CALLER.ENDED))

    def test_launcher_success_does_not_prove_native_main_ran(self):
        log = b"TERMINAL_ORDER event=launcher-limit fsize=8 scope=owned-child\n"
        self.assertTrue(CALLER.verdict("file-limit", 0, log, CALLER.CANONICAL[:8], CALLER.CANONICAL[:8]))

    def test_wrong_case_receipts_are_rejected(self):
        self.assertTrue(CALLER.verdict("completion-lock", 0, self.complete_log("readonly-file"), b"", b""))

    def test_timeout_and_native_failure_are_never_passes(self):
        for code in (124, 101, -9):
            with self.subTest(code=code):
                self.assertTrue(CALLER.verdict("completion-lock", code, self.complete_log(), CALLER.ENDED, CALLER.ENDED))

    def test_driver_request_without_actual_return_is_rejected(self):
        log = self.complete_log().replace(b"event=driver-return result=Ok(())", b"event=stop-requested")
        self.assertTrue(CALLER.verdict("completion-lock", 0, log, CALLER.ENDED, CALLER.ENDED))

    def test_partial_prefix_does_not_become_a_complete_append(self):
        for prefix in (1, len(CALLER.CANONICAL) - 1):
            with self.subTest(prefix=prefix):
                self.assertEqual(CALLER.expected_bytes("file-limit", prefix), CALLER.CANONICAL[:prefix])
                self.assertNotEqual(CALLER.expected_bytes("file-limit", prefix), CALLER.CANONICAL)
        for prefix in (None, 0, len(CALLER.CANONICAL), len(CALLER.CANONICAL) + 1):
            with self.subTest(prefix=prefix), self.assertRaises(ValueError):
                CALLER.expected_bytes("file-limit", prefix)

    def test_file_limit_never_belongs_to_ordinary_cases(self):
        for case in set(CALLER.CASES) - {"file-limit"}:
            with self.subTest(case=case), self.assertRaises(ValueError):
                CALLER.expected_bytes(case, 1)

    def test_command_selects_one_exact_ignored_case_with_one_test_thread(self):
        command = CALLER.test_command(Path("/owned/test-binary"), "readonly-file")
        self.assertEqual(command[0], "/owned/test-binary")
        self.assertIn("--exact", command)
        self.assertIn("--ignored", command)
        self.assertIn("--test-threads=1", command)
        self.assertEqual(command[1], "daemon::managed::terminal_order_tests::" + CALLER.CASES["readonly-file"])

    def test_deadline_inputs_reject_nonfinite_and_nonpositive(self):
        for value in ("nan", "inf", "0", "-1"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                CALLER.positive_seconds(value)

    def test_isolated_child_cli_rejects_invalid_prefix_before_limits_or_exec(self):
        # Invalid input exits before the limit call and before any native code.
        result = subprocess.run(
            [sys.executable, "-B", str(Path(__file__).with_name("caller.py")),
             "--_exec-limited", "/unused/native-binary", "0"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"strict nonempty canonical-row prefix", result.stderr)
        self.assertNotIn(b"event=launcher-limit", result.stdout)


if __name__ == "__main__":
    unittest.main()
