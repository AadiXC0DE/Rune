import unittest

from ledger import Ledger, summarize


class VisibleTests(unittest.TestCase):
    def test_sales_add_up(self):
        ledger = Ledger()
        ledger.record("pen", 3, 1.5)
        ledger.record("pen", 2, 1.5)
        ledger.record("cup", 1, 4.25)
        report = summarize(ledger)
        self.assertEqual(report["pen"], {"units_sold": 5, "units_returned": 0, "revenue_cents": 750})
        self.assertEqual(report["cup"]["revenue_cents"], 425)

    def test_return_reduces_revenue(self):
        ledger = Ledger()
        ledger.record("pen", 4, 2.0)
        ledger.record("pen", -1, 2.0)
        report = summarize(ledger)
        self.assertEqual(report["pen"], {"units_sold": 4, "units_returned": 1, "revenue_cents": 600})

    def test_zero_quantity_is_refused(self):
        with self.assertRaises(ValueError):
            Ledger().record("pen", 0, 1.0)


if __name__ == "__main__":
    unittest.main()
