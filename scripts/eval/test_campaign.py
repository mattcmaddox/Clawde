#!/usr/bin/env python3
"""Offline tests for the campaign orchestration layer."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from campaign import (  # noqa: E402
    aggregate,
    build_comparison,
    compare_summaries,
    inferred_workdir,
    validate_manifest,
)


class CampaignTests(unittest.TestCase):
    def test_default_manifest_shape_is_valid(self):
        manifest = {
            "repeats": 2,
            "min_evaluable": 1,
            "gates": {"max_pass_rate_drop": 0, "max_score_drop": 0.1},
            "fixtures": [{"name": "one", "path": "scripts/eval/fixtures/trajectory"}],
        }
        self.assertEqual(validate_manifest(manifest), [])

    def test_manifest_validation_reports_bad_fixture(self):
        errors = validate_manifest({"fixtures": [{"name": "x", "path": "x", "repeats": 0}]})
        self.assertTrue(any("repeats" in error for error in errors))

    def test_infers_workspace_for_debug_binary(self):
        binary = Path("/tmp/checkouts/baseline/src-rust/target/debug/clawde")
        self.assertEqual(inferred_workdir(binary), Path("/tmp/checkouts/baseline/src-rust"))

    def test_aggregate_keeps_quality_failures_evaluable(self):
        records = [
            {"score": 1.0, "passed": True, "error": None, "ttft_ms": 100},
            {"score": 0.5, "passed": False, "error": None, "ttft_ms": 200},
            {"score": None, "passed": None, "error": "timeout", "ttft_ms": None},
        ]
        summary = aggregate(records, 1)
        self.assertEqual(summary["evaluable"], 2)
        self.assertEqual(summary["infra_failures"], 1)
        self.assertEqual(summary["pass_rate"], 0.5)
        self.assertEqual(summary["score_mean"], 0.75)

    def test_compare_detects_pass_rate_regression(self):
        baseline = aggregate(
            [{"score": 1.0, "passed": True, "error": None}],
            1,
        )
        candidate = aggregate(
            [{"score": 0.5, "passed": False, "error": None}],
            1,
        )
        comparison = compare_summaries(
            baseline,
            candidate,
            max_pass_rate_drop=0,
            max_score_drop=0.1,
        )
        self.assertEqual(comparison["status"], "regression")
        self.assertTrue(comparison["reasons"])

    def test_compare_treats_missing_runs_as_infrastructure(self):
        comparison = compare_summaries(
            aggregate([], 1),
            aggregate([], 1),
            max_pass_rate_drop=0,
            max_score_drop=0.1,
        )
        self.assertEqual(comparison["status"], "infrastructure")

    def test_build_comparison_includes_manifest_fixture_with_no_records(self):
        manifest = {"min_evaluable": 1, "gates": {}, "fixtures": [{"name": "missing", "path": "x"}]}
        comparison = build_comparison([], manifest)
        self.assertEqual(comparison["status"], "infrastructure")
        self.assertEqual(comparison["infrastructure"], ["missing"])


if __name__ == "__main__":
    unittest.main()
