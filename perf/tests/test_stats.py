import unittest

from pgperf import stats


class TestPercentile(unittest.TestCase):
    def test_empty_returns_none(self):
        self.assertIsNone(stats.percentile([], 95))

    def test_single_value(self):
        self.assertEqual(stats.percentile([42.0], 95), 42.0)

    def test_nearest_rank_definition(self):
        # Nearest-rank: idx = ceil(p/100 * n) - 1, clamped to [0, n-1].
        # n=10, p=95 -> ceil(9.5)-1 = 9 -> the max.
        values = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0]
        self.assertEqual(stats.percentile(values, 95), 10.0)
        self.assertEqual(stats.percentile(values, 50), 5.0)
        self.assertEqual(stats.percentile(values, 100), 10.0)

    def test_p95_of_five_samples_is_the_max(self):
        # Documented consequence of N=5 (spec 8.1): p95 and p99 are the max.
        # This test exists so nobody "fixes" it into interpolation by accident.
        values = [10.0, 20.0, 30.0, 40.0, 500.0]
        self.assertEqual(stats.percentile(values, 95), 500.0)
        self.assertEqual(stats.percentile(values, 99), 500.0)

    def test_input_is_not_mutated(self):
        values = [3.0, 1.0, 2.0]
        stats.percentile(values, 50)
        self.assertEqual(values, [3.0, 1.0, 2.0])


class TestSummarize(unittest.TestCase):
    def test_empty(self):
        got = stats.summarize([])
        self.assertEqual(got["n"], 0)
        for key in ("min", "median", "p95", "p99", "max", "mean", "stdev", "cov"):
            self.assertIsNone(got[key], f"{key} should be None for empty input")

    def test_single_value_has_no_spread(self):
        got = stats.summarize([7.0])
        self.assertEqual(got["n"], 1)
        self.assertEqual(got["mean"], 7.0)
        self.assertIsNone(got["stdev"], "stdev undefined for n=1")
        self.assertIsNone(got["cov"], "cov undefined for n=1")

    def test_known_values(self):
        got = stats.summarize([2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0])
        self.assertEqual(got["n"], 8)
        self.assertEqual(got["min"], 2.0)
        self.assertEqual(got["max"], 9.0)
        self.assertEqual(got["mean"], 5.0)
        # statistics.stdev is the sample (n-1) standard deviation.
        self.assertAlmostEqual(got["stdev"], 2.1381, places=3)
        self.assertAlmostEqual(got["cov"], 0.4276, places=3)

    def test_cov_is_none_when_mean_is_zero(self):
        got = stats.summarize([0.0, 0.0, 0.0])
        self.assertEqual(got["mean"], 0.0)
        self.assertIsNone(got["cov"], "cov is undefined when mean is 0")


if __name__ == "__main__":
    unittest.main()
