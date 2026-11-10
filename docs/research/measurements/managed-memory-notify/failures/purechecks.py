"""Argument/arithmetic tests only: no launcher, child, FD or filesystem operations."""

import argparse
import contextlib
import io
import unittest

from launcher import (Config, HEADER_BYTES, INT_MAX, InputFault, U64_MAX,
                      absolute_path, add_u64, poll_millis, positive,
                      outer_deadline, remaining_ns, sha256_hex)


def arguments():
    return [
        "--binary", "/root-owned/frozen-fixture", "--binary-sha256", "ab" * 32,
        "--child-helper", "/root-owned/nofile-child", "--child-helper-sha256", "cd" * 32,
        "--record-dir", "/private/new-record", "--deadline-monotonic-ns", "100",
        "--writes", "1", "--read-bytes", str(HEADER_BYTES), "--nofile-soft", "4",
        "--outer-timeout-ns", "20", "--max-log-bytes", "2000",
        "--expect-operation", "inotify_init1", "--expect-errno", "24", "--expect-exit", "1",
    ]


class PureChecks(unittest.TestCase):
    def test_complete_explicit_interface(self):
        config = Config.parse(arguments())
        self.assertEqual(config.writes, 1)
        self.assertEqual(config.deadline_monotonic_ns, 100)
        self.assertEqual(config.expect_exit, 1)

    def test_no_defaults_abbreviations_or_silent_success(self):
        for alter in (lambda a: [],
                      lambda a: a[:-2],
                      lambda a: a[:-1] + ["0"],
                      lambda a: ["--bin" if v == "--binary" else v for v in a]):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                Config.parse(alter(arguments()))

    def test_decimal_grammar_and_u64_bound(self):
        self.assertEqual(positive(str(U64_MAX)), U64_MAX)
        self.assertEqual(positive("0001"), 1)
        for value in ("0", "000", "-1", "+1", " 1", "1 ", "١", str(U64_MAX + 1), "9" * 5000):
            with self.assertRaises(argparse.ArgumentTypeError):
                positive(value)

    def test_binary_identity_grammar(self):
        self.assertEqual(sha256_hex("AB" * 32), "ab" * 32)
        for value in ("a" * 63, "a" * 65, "gg" * 32, "\n" + "a" * 63):
            with self.assertRaises(argparse.ArgumentTypeError):
                sha256_hex(value)

    def test_new_path_grammar(self):
        self.assertEqual(str(absolute_path("/private/record")), "/private/record")
        for value in ("relative", "/", "//", "/private/../record", "/private/./record", "/x\0y"):
            with self.assertRaises(argparse.ArgumentTypeError):
                absolute_path(value)

    def test_header_and_exit_abi_refusals(self):
        for name, value in (("--read-bytes", str(HEADER_BYTES - 1)),
                            ("--expect-exit", "256"), ("--expect-errno", str(INT_MAX + 1))):
            args = arguments()
            args[args.index(name) + 1] = value
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                Config.parse(args)

    def test_absolute_outer_deadline_checked_once(self):
        self.assertEqual(add_u64(U64_MAX - 1, 1), U64_MAX)
        for left, right in ((U64_MAX, 1), (-1, 1), (0, U64_MAX + 1)):
            with self.assertRaises(InputFault):
                add_u64(left, right)
        self.assertEqual(remaining_ns(100, 99), 1)
        for now in (100, 101, -1):
            with self.assertRaises(InputFault):
                remaining_ns(100, now)

    def test_poll_unit_and_only_abi_clamp(self):
        self.assertEqual(poll_millis(1), 1)
        self.assertEqual(poll_millis(1_000_000), 1)
        self.assertEqual(poll_millis(1_000_001), 2)
        self.assertEqual(poll_millis(U64_MAX), INT_MAX)
        for duration in (0, -1, U64_MAX + 1):
            with self.assertRaises(InputFault):
                poll_millis(duration)

    def test_outer_window_does_not_extend_original_native_authority(self):
        self.assertEqual(outer_deadline(10, 30, 20), 40)
        for entered, timeout, native in ((10, 30, 10), (10, 30, 40), (10, 0, 20),
                                         (U64_MAX, 1, U64_MAX)):
            with self.assertRaises(InputFault):
                outer_deadline(entered, timeout, native)


if __name__ == "__main__":
    unittest.main()
