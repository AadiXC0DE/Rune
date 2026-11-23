"""Offline tests of the Harbor adapter's log parser (no Harbor import)."""

import json
import os
import tempfile
import unittest

from rune_harbor import parse

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURE = os.path.join(HERE, "testdata", "rune.jsonl")


def without_final(records):
    return [record for record in records if record.get("type") != "final"]


class ParseTests(unittest.TestCase):
    def setUp(self):
        self.records = parse.read_records(FIXTURE)

    def test_usage_comes_from_the_final_record(self):
        totals = parse.usage_totals(self.records)
        self.assertEqual(
            totals,
            {"input_tokens": 2100, "cached_tokens": 900, "output_tokens": 30, "cost_usd": 0.000123},
        )

    def test_usage_falls_back_to_step_totals_without_a_final_record(self):
        totals = parse.usage_totals(without_final(self.records))
        self.assertEqual(
            totals,
            {"input_tokens": 2100, "cached_tokens": 900, "output_tokens": 30, "cost_usd": None},
        )
        self.assertIsNone(parse.usage_totals([]))
        trajectory = parse.trajectory_dict(
            without_final(self.records), instruction="x", model_name="m", version="v")
        self.assertIsNotNone(trajectory, "a killed run still has a trajectory")

    def test_trajectory_has_a_user_step_and_one_agent_step_per_model_step(self):
        trajectory = parse.trajectory_dict(
            self.records, instruction="Fix the configuration.", model_name="deepseek/flash", version="")
        self.assertEqual(trajectory["schema_version"], "ATIF-v1.2")
        self.assertEqual(trajectory["session_id"], "sess123")
        self.assertEqual(trajectory["agent"], {"name": "rune", "version": "0.3.0", "model_name": "deepseek/flash"})
        sources = [step["source"] for step in trajectory["steps"]]
        self.assertEqual(sources, ["user", "agent", "agent"])
        self.assertEqual([step["step_id"] for step in trajectory["steps"]], [1, 2, 3])
        first, second = trajectory["steps"][1:]
        self.assertEqual(first["message"], "Reading the configuration.")
        self.assertEqual(first["metrics"], {"prompt_tokens": 1000, "completion_tokens": 20, "cached_tokens": 0})
        self.assertEqual(first["extra"], {"finish": "tool_calls"})
        self.assertEqual(second["message"], "Set max_items to 10.")
        self.assertNotIn("tool_calls", second)
        self.assertEqual(
            trajectory["final_metrics"],
            {"total_prompt_tokens": 2100, "total_completion_tokens": 30,
             "total_cached_tokens": 900, "total_cost_usd": 0.000123, "total_steps": 3},
        )
        json.dumps(trajectory)

    def test_tool_calls_and_observations_are_paired_by_id(self):
        trajectory = parse.trajectory_dict(self.records, instruction="x", model_name="m", version="v")
        step = trajectory["steps"][1]
        self.assertEqual(
            step["tool_calls"],
            [{"tool_call_id": "c1", "function_name": "read_file", "arguments": {"path": "config/limits.conf"}}],
        )
        self.assertEqual(
            step["observation"]["results"],
            [{"source_call_id": "c1", "content": "max_items = 5",
              "extra": {"ok": True, "executed": True, "elapsed_ms": 3, "bytes": 40}}],
        )
        raw = parse.trajectory_dict(
            [{"type": "step_start", "step": 1},
             {"type": "tool_call", "id": "c9", "name": "shell", "arguments": "not json"}],
            instruction="x", model_name="m", version="v")
        self.assertEqual(raw["steps"][1]["tool_calls"][0]["arguments"], {"raw": "not json"})

    def test_unparseable_lines_are_skipped(self):
        with open(FIXTURE, encoding="utf-8") as handle:
            lines = [line for line in handle if line.strip()]
        self.assertEqual(len(self.records), len(lines) - 1)
        with tempfile.NamedTemporaryFile("wb", suffix=".jsonl", delete=False) as handle:
            handle.write(b'{"type":"step_start","step":1}\n\xff\xfe broken\n')
            path = handle.name
        try:
            self.assertEqual(parse.read_records(path), [{"type": "step_start", "step": 1}])
        finally:
            os.unlink(path)

    def test_no_steps_means_no_trajectory(self):
        self.assertIsNone(parse.trajectory_dict([], instruction="x", model_name="m", version="v"))
        start_only = [record for record in self.records if record.get("type") == "start"]
        self.assertIsNone(parse.trajectory_dict(start_only, instruction="x", model_name="m", version="v"))
        self.assertEqual(parse.read_records(os.path.join(HERE, "missing.jsonl")), [])

    def test_compaction_and_retries_are_recorded(self):
        records = [
            {"type": "step_start", "step": 1},
            {"type": "retry", "step": 1, "attempt": 2, "wait_ms": 250, "kind": "rate_limited"},
            {"type": "step_restart", "step": 1},
            {"type": "text_delta", "step": 1, "delta": "kept"},
            {"type": "step_finish", "step": 1, "finish": "stop", "usage": {"input_tokens": 5, "output_tokens": 1}},
            {"type": "compaction", "before_tokens": 5000, "after_tokens": 1200},
            {"type": "step_start", "step": 2},
        ]
        trajectory = parse.trajectory_dict(records, instruction="x", model_name="m", version="v")
        agent, system, after = trajectory["steps"][1:]
        self.assertEqual(agent["message"], "kept")
        self.assertEqual(agent["extra"]["retries"], [{"attempt": 2, "wait_ms": 250, "kind": "rate_limited"}])
        self.assertEqual(system, {"step_id": 3, "source": "system",
                                  "message": "Context compacted from 5000 to 1200 tokens"})
        self.assertEqual(after["step_id"], 4)


if __name__ == "__main__":
    unittest.main()
