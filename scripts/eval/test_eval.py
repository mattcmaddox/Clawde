#!/usr/bin/env python3
"""Offline tests for scripts/eval.

These tests never invoke Clawde or a provider. They cover the harness contracts
that should remain deterministic even when live free-tier services are flaky.
"""

import json
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from derive_catalog_facts import CATALOG_RS, parse_catalog  # noqa: E402
from run_eval import (  # noqa: E402
    append_result,
    fixture_label,
    load_catalog_facts,
    parse_judge_output,
    parse_stream_events,
    run_assertions,
    run_process_stream,
    validate_expected,
)
from summarize import normalize_fixture  # noqa: E402


class EvalHarnessTests(unittest.TestCase):
    def test_catalog_parser_resolves_string_constants(self):
        entries = parse_catalog(CATALOG_RS)
        cloudflare = next(entry for entry in entries if entry["id"] == "cloudflare")
        self.assertEqual(cloudflare["default_model"], "@cf/qwen/qwen3-30b-a3b-fp8")
        self.assertEqual(len(entries), 14)

    def test_runtime_catalog_facts_match_checked_in_snapshot(self):
        facts = load_catalog_facts()
        self.assertEqual(len(facts["ids"]), 14)
        self.assertEqual(facts["ids"][0], "github-copilot")
        self.assertEqual(facts["upstreams"][5]["default_model"], "@cf/qwen/qwen3-30b-a3b-fp8")

    def test_fixture_schema_validation(self):
        fixture_root = Path(__file__).resolve().parent / "fixtures"
        for fixture in fixture_root.iterdir():
            expected = json.loads((fixture / "expected.json").read_text())
            self.assertEqual(validate_expected(expected), [], fixture.name)
        self.assertTrue(validate_expected({"assert": [{"type": "unknown"}]}))

    def test_mentions_upstreams_uses_identifier_boundaries(self):
        assertions = [{"type": "mentions-upstreams", "min": 1}]
        run = {"tools_used": []}
        catalog = {"ids": ["groq"]}
        self.assertTrue(run_assertions(assertions, "The groq provider is available.", run, catalog)[0]["passed"])
        self.assertFalse(run_assertions(assertions, "This is groqish output.", run, catalog)[0]["passed"])

    def test_stream_events_capture_text_tools_and_attribution(self):
        lines = [
            (100.05, '{"type":"provider_attribution","provider_id":"free","upstream_id":"groq"}'),
            (100.10, '{"type":"tool_start","tool":"Read"}'),
            (100.20, '{"type":"text_delta","text":"hello"}'),
            (100.30, '{"type":"result","cost_usd":0.01,"upstream":"groq"}'),
        ]
        run = parse_stream_events(lines, 100.0)
        self.assertEqual(run["response_text"], "hello")
        self.assertEqual(run["first_text_delta_ms"], 200)
        self.assertEqual(run["tool_sequence"], ["Read"])
        self.assertEqual(run["upstream_id"], "groq")
        self.assertEqual(run["cost_usd"], 0.01)

    def test_judge_parser_accepts_supported_scales_and_rejects_empty(self):
        self.assertEqual(parse_judge_output("SCORE=0.85\nREASON=complete")[0], 0.85)
        self.assertEqual(parse_judge_output('{"score": 8, "reason": "good"}')[0], 0.8)
        self.assertIsNone(parse_judge_output("SCORE=.\nREASON=bad")[0])

    def test_process_timeout_drains_both_pipes_and_kills_child(self):
        command = [
            sys.executable,
            "-c",
            "import sys,time; print('partial', flush=True); sys.stderr.write('e'*100000); sys.stderr.flush(); time.sleep(5)",
        ]
        started = time.monotonic()
        lines, stderr, exit_code, timed_out, elapsed_ms, _ = run_process_stream(
            command, env=dict(), timeout=0.2
        )
        elapsed = time.monotonic() - started
        self.assertTrue(timed_out)
        self.assertIsNone(exit_code)
        self.assertLess(elapsed, 2.0)
        self.assertTrue(lines)
        self.assertGreater(len(stderr), 0)
        self.assertLess(elapsed_ms, 2000)

    def test_result_append_is_valid_jsonl(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nested" / "results.jsonl"
            append_result(path, {"ok": True})
            append_result(path, {"ok": False})
            records = [json.loads(line) for line in path.read_text().splitlines()]
        self.assertEqual(records, [{"ok": True}, {"ok": False}])

    def test_summary_normalizes_legacy_absolute_fixture_paths(self):
        fixture = Path(__file__).resolve().parent / "fixtures" / "trajectory"
        self.assertEqual(normalize_fixture(str(fixture)), "scripts/eval/fixtures/trajectory")

    def test_fixture_label_is_repo_stable(self):
        fixture = Path(__file__).resolve().parent / "fixtures" / "trajectory"
        self.assertEqual(fixture_label(fixture), "scripts/eval/fixtures/trajectory")
        self.assertIsNone(fixture_label(None))


if __name__ == "__main__":
    unittest.main()
