#!/usr/bin/env python3
"""Read-only JSONL efficiency counters. Never emit prompts, outputs or credentials.

This counts stored model responses and tool results, not API retry attempts or
wall-clock latency. Rejected results are matched by known harness prefixes;
they are evidence-free replies, not necessarily unjustified refusals.
"""
import argparse
import collections
import json
from pathlib import Path

REFUSALS = (
    "Already Grep'd a similar pattern this turn.",
    "Grep budget for this turn is used.",
    "Search budget for this turn is used.",
    "Already searched a similar",
    "This file was already read",
)


def measure(path):
    counts = collections.Counter()
    tools = collections.Counter()
    refusals = collections.Counter()
    usage = collections.Counter()
    measured_cache_input = 0
    cache_samples = 0
    channel = ""
    for line in path.open(encoding="utf-8", errors="replace"):
        try:
            event = json.loads(line)
        except (json.JSONDecodeError, ValueError):
            counts["malformed_lines"] += 1
            continue
        if not isinstance(event, dict):
            counts["malformed_lines"] += 1
            continue
        kind = event.get("type", "")
        counts[kind] += 1
        if kind == "session/start":
            channel = event.get("channel", "")
        elif kind == "assistant":
            prompt = event.get("prompt_tokens", 0) or 0
            usage["prompt_tokens"] += prompt
            usage["completion_tokens"] += event.get("completion_tokens", 0) or 0
            cached = event.get("cached_tokens")
            if cached is not None:
                cache_samples += 1
                measured_cache_input += prompt
                usage["cached_tokens"] += cached
        elif kind == "tool":
            tools[event.get("name", "unknown")] += 1
            output = event.get("output", "")
            if isinstance(output, str):
                for prefix in REFUSALS:
                    if output.startswith(prefix):
                        refusals[prefix] += 1
                        break
    total = counts["tool"]
    return {
        "session_id": path.stem,
        "channel": channel,
        "user_messages": counts["user"],
        "model_responses": counts["assistant"],
        "tool_results": total,
        "compactions": counts["session/compact"],
        "known_refusals": sum(refusals.values()),
        "refusal_share": round(sum(refusals.values()) / total, 4) if total else None,
        "tools": dict(tools),
        "refusals": dict(refusals),
        "usage": dict(usage),
        "cache_usage_samples": cache_samples,
        "cache_hit_share_measured_input": round(usage["cached_tokens"] / measured_cache_input, 4) if measured_cache_input else None,
        "malformed_lines": counts["malformed_lines"],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session-dir", type=Path, default=Path.home() / ".grok-hyper/sessions")
    parser.add_argument("--include-subagents", action="store_true")
    parser.add_argument("--limit", type=int, default=10)
    args = parser.parse_args()
    if not args.session_dir.is_dir():
        parser.error("session directory does not exist")
    rows = [measure(p) for p in sorted(args.session_dir.glob("*.jsonl"))]
    if not args.include_subagents:
        rows = [r for r in rows if r["channel"] != "subagent"]
    rows.sort(key=lambda r: r["tool_results"], reverse=True)
    print(json.dumps(rows[:max(args.limit, 0)], ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
