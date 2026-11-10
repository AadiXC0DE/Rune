import os
import unittest

from importer import load, parse_price

DATA = os.path.join(os.path.dirname(__file__), "..", "data", "orders.jsonl")


class ImportTests(unittest.TestCase):
    def test_price_text(self):
        self.assertEqual(parse_price("12.50"), 1250)
        self.assertEqual(parse_price("$3.05"), 305)

    def test_sample_data_set_imports(self):
        records = []
        for record in load(DATA):
            print("imported record", record["id"], record["name"], record["price_cents"])
            records.append(record)
        self.assertEqual(len(records), 500)
        self.assertEqual([r["id"] for r in records], list(range(1, 501)))
        priced = [r["price_cents"] for r in records if r["price_cents"] is not None]
        self.assertEqual(len(priced), 499)
        self.assertEqual(sum(priced), TOTAL)


TOTAL = 2395097


if __name__ == "__main__":
    unittest.main()
