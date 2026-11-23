"""Turns a `rune ask --jsonl` log into Harbor usage figures and an ATIF trajectory.

Standard library only, so it can be tested without Harbor installed.
"""

import json
import uuid

SCHEMA_VERSION = "ATIF-v1.2"


def read_records(path):
    """Returns one dict per parseable line of the log; other lines are skipped."""
    records = []
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except ValueError:
                    continue
                if isinstance(record, dict):
                    records.append(record)
    except OSError:
        return []
    return records


def _number(value):
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) else 0


def _cost(value):
    try:
        return float(value) if value is not None else None
    except (TypeError, ValueError):
        return None


def usage_totals(records):
    """Token and cost totals for the run, or None when the log has none.

    The last `final` record is authoritative. A run killed before it wrote one
    still has its `step_finish` records, whose sums are used, with no cost.
    """
    finals = [record for record in records if record.get("type") == "final"]
    if finals:
        result = finals[-1].get("result") or {}
        usage = result.get("usage") or {}
        return {
            "input_tokens": _number(usage.get("input_tokens")),
            "cached_tokens": _number(usage.get("cache_read_tokens")),
            "output_tokens": _number(usage.get("output_tokens")),
            "cost_usd": _cost(result.get("cost_usd")),
        }
    steps = [record for record in records if record.get("type") == "step_finish"]
    if not steps:
        return None
    totals = {"input_tokens": 0, "cached_tokens": 0, "output_tokens": 0, "cost_usd": None}
    for step in steps:
        usage = step.get("usage") or {}
        totals["input_tokens"] += _number(usage.get("input_tokens"))
        totals["cached_tokens"] += _number(usage.get("cached_tokens"))
        totals["output_tokens"] += _number(usage.get("output_tokens"))
    return totals


def _arguments(value):
    return value if isinstance(value, dict) else {"raw": value if isinstance(value, str) else json.dumps(value)}


def trajectory_dict(records, *, instruction, model_name, version):
    """An ATIF trajectory as a dict, or None when no model step started."""
    if not any(record.get("type") == "step_start" for record in records):
        return None
    start = next((record for record in records if record.get("type") == "start"), {})
    steps = [{"step_id": 1, "source": "user", "message": instruction}]
    current = None

    def close():
        nonlocal current
        if current is None:
            return
        step = {
            "step_id": len(steps) + 1,
            "source": "agent",
            "model_name": model_name,
            "message": "".join(current["text"]),
        }
        if current["calls"]:
            step["tool_calls"] = current["calls"]
        if current["results"]:
            step["observation"] = {"results": current["results"]}
        if current["metrics"] is not None:
            step["metrics"] = current["metrics"]
        extra = {}
        if current["reasoning"]:
            extra["reasoning"] = "\n".join(current["reasoning"])
        if current["finish"] is not None:
            extra["finish"] = current["finish"]
        if current["retries"]:
            extra["retries"] = current["retries"]
        if extra:
            step["extra"] = extra
        steps.append(step)
        current = None

    def fresh():
        return {"text": [], "calls": [], "results": [], "metrics": None,
                "reasoning": [], "finish": None, "retries": []}

    for record in records:
        kind = record.get("type")
        if kind == "step_start":
            close()
            current = fresh()
        elif kind == "step_restart" and current is not None:
            # The failed attempt's text is discarded; retries stay recorded.
            current["text"] = []
        elif kind == "text_delta" and current is not None:
            current["text"].append(record.get("delta") or "")
        elif kind == "reasoning" and current is not None:
            current["reasoning"].append(record.get("text") or "")
        elif kind == "retry" and current is not None:
            current["retries"].append({key: record[key] for key in ("attempt", "wait_ms", "kind") if key in record})
        elif kind == "step_finish" and current is not None:
            usage = record.get("usage") or {}
            current["finish"] = record.get("finish")
            metrics = {
                "prompt_tokens": _number(usage.get("input_tokens")),
                "completion_tokens": _number(usage.get("output_tokens")),
                "cached_tokens": _number(usage.get("cached_tokens")),
            }
            extra = {key: usage[key] for key in ("cache_write_tokens", "reasoning_tokens") if usage.get(key) is not None}
            if extra:
                metrics["extra"] = extra
            current["metrics"] = metrics
        elif kind == "tool_call" and current is not None:
            call = {
                "tool_call_id": record.get("id", ""),
                "function_name": record.get("name", ""),
                "arguments": _arguments(record.get("arguments")),
            }
            current["calls"].append(call)
        elif kind == "tool_result" and current is not None:
            current["results"].append({
                "source_call_id": record.get("id", ""),
                "content": record.get("preview", ""),
                "extra": {key: record[key] for key in ("ok", "executed", "elapsed_ms", "bytes") if key in record},
            })
        elif kind == "compaction":
            close()
            steps.append({
                "step_id": len(steps) + 1,
                "source": "system",
                "message": "Context compacted from {} to {} tokens".format(
                    record.get("before_tokens", 0), record.get("after_tokens", 0)),
            })
    close()

    totals = usage_totals(records) or {"input_tokens": 0, "cached_tokens": 0, "output_tokens": 0, "cost_usd": None}
    return {
        "schema_version": SCHEMA_VERSION,
        "session_id": start.get("session_id") or "rune-{}".format(uuid.uuid4()),
        "agent": {"name": "rune", "version": version or start.get("version", ""), "model_name": model_name},
        "steps": steps,
        "final_metrics": {
            "total_prompt_tokens": totals["input_tokens"],
            "total_completion_tokens": totals["output_tokens"],
            "total_cached_tokens": totals["cached_tokens"],
            "total_cost_usd": totals["cost_usd"],
            "total_steps": len(steps),
        },
    }
