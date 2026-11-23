import unittest

from ledger import Ledger, summarize


class HiddenTests(unittest.TestCase):
    def test_many_small_movements_do_not_drift(self):
        ledger = Ledger()
        for i in range(1, 10001):
            ledger.record("tape", 1, 0.1)
            if i % 7 == 0 or i in (9, 10, 11, 46, 47):
                self.assertEqual(summarize(ledger)["tape"]["revenue_cents"], i * 10)
        report = summarize(ledger)["tape"]
        self.assertEqual(report["revenue_cents"], 100000)
        self.assertEqual(report["units_sold"], 10000)

    def test_return_after_a_sale(self):
        ledger = Ledger()
        ledger.record("lamp", 3, 19.99)
        ledger.record("lamp", -2, 19.99)
        report = summarize(ledger)["lamp"]
        self.assertEqual(report["units_sold"], 3)
        self.assertEqual(report["units_returned"], 2)
        self.assertEqual(report["revenue_cents"], 1999)

    def test_more_returned_than_sold_goes_negative(self):
        ledger = Ledger()
        ledger.record("lamp", -1, 5.0)
        report = summarize(ledger)["lamp"]
        self.assertEqual(report["units_sold"], 0)
        self.assertEqual(report["units_returned"], 1)
        self.assertEqual(report["revenue_cents"], -500)

    def test_prices_given_as_text_or_floats(self):
        ledger = Ledger()
        ledger.record("a", 3, "0.29")
        ledger.record("b", 3, 0.29)
        report = summarize(ledger)
        self.assertEqual(report["a"]["revenue_cents"], 87)
        self.assertEqual(report["b"]["revenue_cents"], 87)

    def test_skus_stay_separate(self):
        ledger = Ledger()
        ledger.record("a", 1, 1.1)
        ledger.record("b", -1, 2.2)
        report = summarize(ledger)
        self.assertEqual(report["a"]["revenue_cents"], 110)
        self.assertEqual(report["b"]["revenue_cents"], -220)


if __name__ == "__main__":
    unittest.main()
