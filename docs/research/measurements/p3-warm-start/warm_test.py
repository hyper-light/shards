"""warm.py's own statistics held to their sources, with no build and no VM:

    python3 -I warm_test.py
"""

import importlib.util
import math
import os
import tempfile
import unittest
from fractions import Fraction

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("warm", os.path.join(HERE, "warm.py"))
warm = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(warm)

# Half of 1 - CONFIDENCE, exactly: each tail an interval leaves out.
TAIL = (1 - Fraction(95, 100)) / 2


def cdf(n, q):
    """P(Binomial(n, q) <= i) for each i, exactly, apart from `interval`'s sums in logs."""
    a, b = q.numerator, q.denominator
    total, out, whole = 0, [], b**n
    for i in range(n + 1):
        total += math.comb(n, i) * a**i * (b - a) ** (n - i)
        out.append(Fraction(total, whole))
    return out


def sample(kind, usable):
    return f"IMAGE-START-SAMPLE round 0 {kind} from {kind} boot_us 7 usable_us {usable} stop_us 9\n"


class Statistics(unittest.TestCase):
    def test_sampling_starts_where_a_p99_first_has_an_interval(self):
        # X(n) lies above the p99 with probability 1 - 0.99^n at most, so the p99's first upper
        # bound is the largest sample, once 0.99^n is within the tail (Le Boudec 2010, Theorem 2.1).
        first = math.ceil(math.log(TAIL) / math.log(warm.QUANTILE))
        self.assertEqual((warm.least_n(), first), (368, 368))
        self.assertIsNone(warm.interval(first - 1, warm.QUANTILE))
        self.assertEqual(warm.interval(first, warm.QUANTILE)[1], first)

    def test_each_interval_is_the_narrowest_whose_tails_are_each_within_half_the_rest(self):
        # The p99's at the first three rounds' n, and the medians the verdict and the paired
        # difference take, against exact binomial sums.
        for n, q in ((368, Fraction(99, 100)), (736, Fraction(99, 100)), (1472, Fraction(99, 100)),
                     (6, Fraction(1, 2)), (20, Fraction(1, 2)), (368, Fraction(1, 2))):
            b = cdf(n, q)
            j, k = warm.interval(n, float(q))
            self.assertTrue(b[j - 1] <= TAIL < b[j], f"n {n} q {q}: X({j}) is not the highest lower bound")
            self.assertTrue(
                1 - TAIL <= b[k - 1] and (k == 1 or b[k - 2] < 1 - TAIL),
                f"n {n} q {q}: X({k}) is not the lowest upper bound",
            )

    def test_a_median_has_an_interval_from_six_samples(self):
        # 0.5^6 = 1/64 is within the tail of 1/40, and 0.5^5 = 1/32 is not; at 20, X(6) and X(15).
        self.assertIsNone(warm.interval(5, 0.5))
        self.assertEqual(warm.interval(6, 0.5), (1, 6))
        self.assertEqual(warm.interval(20, 0.5), (6, 15))

    def test_quantiles_are_nearest_rank_as_common_measured_takes_them(self):
        x = list(range(1, 369))
        self.assertEqual([warm.ab.nearest(x, p) for p in (50, 90, 99)], [184, 332, 365])
        self.assertEqual(warm.ab.nearest([7], 99), 7)
        p99, (lo, hi) = warm.bounded(x, 99)
        self.assertEqual((p99, hi), (365, 368))
        self.assertLess(lo, p99)

    def test_a_round_pairs_its_warm_start_with_its_cold_whichever_came_first(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.join(d, "quiet-0.log")
            with open(log, "w") as f:
                f.write(sample("Restored", 900) + "running 1 test\n" + sample("Warm", 250))
                f.write(sample("Warm", 260) + sample("Restored", 1000))
            spans, paired = warm.samples([log])
        self.assertEqual(warm.USABLE, 1)
        self.assertEqual(spans["Warm"][warm.USABLE], [250, 260])
        self.assertEqual(spans["Restored"][warm.USABLE], [900, 1000])
        self.assertEqual(spans["Restored"][0], [7, 7])
        self.assertEqual(paired, [250 - 900, 260 - 1000])

    def test_a_start_of_a_kind_never_asked_ends_the_harness(self):
        with tempfile.TemporaryDirectory() as d:
            log = os.path.join(d, "quiet-0.log")
            with open(log, "w") as f:
                f.write(sample("Booted", 900))
            with self.assertRaises(SystemExit):
                warm.samples([log])

    def test_sampling_stops_once_each_p99_lies_within_its_interval_of_the_round_before(self):
        now = [(5, (4, 9)), (7, (1, 8))]
        self.assertFalse(warm.stable(now, None))
        self.assertTrue(warm.stable(now, [(5, 6), (7, 7)]))
        self.assertFalse(warm.stable(now, [(5, 6), (8, 9)]))

    def test_the_conditions_take_turns_going_first(self):
        self.assertEqual(
            [warm.ab.order(r, warm.CONDITIONS) for r in range(4)],
            [["quiet", "loaded"], ["loaded", "quiet"], ["quiet", "loaded"], ["loaded", "quiet"]],
        )

    def test_the_bar_is_a_p50_of_at_most_300_ms_over_20_starts(self):
        self.assertTrue(warm.met([300_000] * 20))
        self.assertFalse(warm.met([300_001] * 20))
        self.assertIsNone(warm.met([1] * 19))
        # Nearest rank: of 20, the 10th.
        self.assertTrue(warm.met([100_000] * 10 + [400_000] * 10))


if __name__ == "__main__":
    unittest.main()
