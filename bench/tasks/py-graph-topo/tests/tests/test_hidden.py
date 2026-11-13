import unittest

from buildorder import CycleError, order


class HiddenTests(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(order({}), [])

    def test_diamond(self):
        deps = {"a": ["b", "c"], "b": ["d"], "c": ["d"], "d": []}
        self.assertEqual(order(deps), ["d", "b", "c", "a"])

    def test_wide_diamond_chain_is_not_a_cycle(self):
        deps = {"top": ["m1", "m2", "m3"], "m1": ["base"], "m2": ["base"], "m3": ["base", "m1"]}
        self.assertEqual(order(deps), ["base", "m1", "m2", "m3", "top"])

    def test_undeclared_names_are_included(self):
        self.assertEqual(order({"a": ["x", "y"]}), ["x", "y", "a"])

    def test_self_loop(self):
        with self.assertRaises(CycleError) as ctx:
            order({"a": ["a"]})
        self.assertEqual(ctx.exception.path, ["a", "a"])
        self.assertIn("a -> a", str(ctx.exception))

    def test_cycle_message_lists_the_path_in_order(self):
        deps = {"root": ["x"], "x": ["y"], "y": ["z"], "z": ["x"]}
        with self.assertRaises(CycleError) as ctx:
            order(deps)
        self.assertEqual(ctx.exception.path, ["x", "y", "z", "x"])
        self.assertIn("x -> y -> z -> x", str(ctx.exception))
        self.assertNotIn("root", str(ctx.exception))

    def test_cycle_is_found_after_a_finished_branch(self):
        deps = {"a": ["b"], "b": [], "c": ["d"], "d": ["c"]}
        with self.assertRaises(CycleError) as ctx:
            order(deps)
        self.assertEqual(ctx.exception.path, ["c", "d", "c"])

    def test_input_is_not_modified(self):
        deps = {"b": ["a"], "a": []}
        order(deps)
        self.assertEqual(deps, {"b": ["a"], "a": []})

    def test_long_chain(self):
        deps = {"n%03d" % i: ["n%03d" % (i + 1)] for i in range(300)}
        names = order(deps)
        self.assertEqual(names[0], "n300")
        self.assertEqual(names[-1], "n000")


if __name__ == "__main__":
    unittest.main()
