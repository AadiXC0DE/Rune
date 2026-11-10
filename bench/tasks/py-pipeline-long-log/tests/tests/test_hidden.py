import json
import os
import tempfile
import unittest

from importer import load, parse_price, parse_record


class HiddenTests(unittest.TestCase):
    def test_numeric_prices(self):
        self.assertEqual(parse_price(7.25), 725)
        self.assertEqual(parse_price(12), 1200)
        self.assertEqual(parse_price(0.1), 10)
        self.assertEqual(parse_price(19.995), 2000)

    def test_missing_price_is_none(self):
        self.assertIsNone(parse_price(None))
        record = parse_record({"id": 3, "name": " lamp "})
        self.assertEqual(record, {"id": 3, "name": "lamp", "price_cents": None})
        self.assertIsNone(parse_record({"id": 4, "name": "x", "price": None})["price_cents"])

    def test_currency_suffix(self):
        self.assertEqual(parse_price("19.99 EUR"), 1999)
        self.assertEqual(parse_price("5 usd"), 500)
        self.assertEqual(parse_price("$2.50 GBP"), 250)

    def test_text_forms_still_work(self):
        self.assertEqual(parse_price("12.50"), 1250)
        self.assertEqual(parse_price(" $3.05 "), 305)
        self.assertEqual(parse_price("0"), 0)

    def test_garbage_is_still_refused(self):
        for bad in ("abc", "", "12.50.1", "EUR 5", True):
            with self.assertRaises(ValueError, msg=repr(bad)):
                parse_price(bad)

    def test_load_a_mixed_file(self):
        rows = [
            {"id": 1, "name": "a", "price": "1.10"},
            {"id": 2, "name": "b", "price": 2.2},
            {"id": 3, "name": "c", "price": "3.30 CHF"},
            {"id": 4, "name": "d"},
        ]
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "in.jsonl")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write("\n\n".join(json.dumps(r) for r in rows) + "\n")
            got = [r["price_cents"] for r in load(path)]
        self.assertEqual(got, [110, 220, 330, None])


if __name__ == "__main__":
    unittest.main()
