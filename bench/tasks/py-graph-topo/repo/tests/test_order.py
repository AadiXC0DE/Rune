import unittest

from buildorder import CycleError, order


class VisibleTests(unittest.TestCase):
    def test_chain(self):
        deps = {"app": ["lib"], "lib": ["core"], "core": []}
        self.assertEqual(order(deps), ["core", "lib", "app"])

    def test_sample_project(self):
        deps = {
            "app": ["ui", "server"],
            "ui": ["core", "assets"],
            "server": ["core", "libc"],
            "core": ["libc"],
            "assets": [],
        }
        self.assertEqual(order(deps), ["libc", "core", "server", "assets", "ui", "app"])

    def test_cycle_names_its_members(self):
        deps = {"a": ["b"], "b": ["c"], "c": ["a"]}
        with self.assertRaises(CycleError) as ctx:
            order(deps)
        self.assertIn("a -> b -> c -> a", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
