#!/usr/bin/env python3
"""Offline tests for best_of_n.py Phase 1 helpers.

No Incus, no container, no binary, no provider calls — everything here runs
against pure functions and a fake exec backend, keeping
`python3 -m unittest discover scripts/eval -p 'test_*.py'` green on machines
without Incus (spec §11).
"""

from __future__ import annotations

import io
import sys
import tarfile
import unittest
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import best_of_n  # noqa: E402
from best_of_n import build_report  # noqa: E402


def _record(task: str, upstream: str, repeat: int = 0, **kw) -> dict:
    """A minimal evaluable attempt record (verify-passing, in-scope)."""
    base = {
        "run_id": f"bon-{task}-{upstream}-{repeat}-test",
        "task": task,
        "upstream": upstream,
        "model": f"free/{upstream}/m",
        "repeat": repeat,
        "ts": "2026-09-06T00:00:00.000Z",
        "run_error": None,
        "timed_out": False,
        "ttft_ms": 100,
        "total_ms": 1000 + repeat,
        "response_chars": 50,
        "tool_calls": 3,
        "upstream_ids": [upstream],
        "attribution": {"cost_usd": 0.0},
        "stderr_tail": "",
        "pin_excluded": False,
        "verify_rc": 0,
        "verify_pass": True,
        "verify_tail": "OK",
        "fs_diff": {"added": [], "deleted": [], "modified": []},
        "scope_violations": [],
    }
    base.update(kw)
    return base


class ManifestTests(unittest.TestCase):
    # Realistic sha256sum output uses full 64-hex digests; the parser is
    # intentionally strict about the format it will diff on.
    D1 = "a" * 64
    D2 = "b" * 64
    D3 = "c" * 64

    def test_parse_manifest_standard_sha256sum_output(self) -> None:
        text = (
            f"{self.D1}  ./a.txt\n"
            f"{self.D2}  ./sub/b.txt\n"
        )
        self.assertEqual(
            best_of_n.parse_manifest(text),
            {"./a.txt": self.D1, "./sub/b.txt": self.D2},
        )

    def test_parse_manifest_rejects_short_digests(self) -> None:
        # A truncated digest would produce false "modified" diffs — refuse it.
        text = "abc123  ./a.txt\n"
        self.assertEqual(best_of_n.parse_manifest(text), {})

    def test_parse_manifest_handles_binary_indicator_and_blank_lines(self) -> None:
        text = (
            f"{self.D1} *./a.bin\n"
            "\n"
            f"{self.D2}  ./b.txt\n"
        )
        parsed = best_of_n.parse_manifest(text)
        self.assertEqual(parsed, {"./a.bin": self.D1, "./b.txt": self.D2})

    def test_diff_manifests_all_buckets(self) -> None:
        before = {"a.txt": "1", "gone.txt": "2", "same.txt": "3"}
        after = {"a.txt": "9", "same.txt": "3", "new.txt": "4"}
        diff = best_of_n.diff_manifests(before, after)
        self.assertEqual(diff, {
            "added": ["new.txt"],
            "deleted": ["gone.txt"],
            "modified": ["a.txt"],
        })

    def test_diff_manifests_empty_sides(self) -> None:
        self.assertEqual(best_of_n.diff_manifests({}, {}), {"added": [], "deleted": [], "modified": []})
        self.assertEqual(best_of_n.diff_manifests({}, {"a.txt": "1"}), {"added": ["a.txt"], "deleted": [], "modified": []})
        self.assertEqual(best_of_n.diff_manifests({"a.txt": "1"}, {}), {"added": [], "deleted": ["a.txt"], "modified": []})


class StreamCheckVerdictTests(unittest.TestCase):
    def marker_lines(self, staggered: bool) -> list[tuple[float, str]]:
        lines: list[tuple[float, str]] = []
        for i in range(best_of_n.STREAM_LINES):
            ts = i * best_of_n.STREAM_DELAY_SECS if staggered else 2.0
            lines.append((ts, f"{i + 1} {best_of_n.STREAM_MARKER}"))
        return lines

    def test_pass_on_interleaved_stream_with_json_tail(self) -> None:
        lines = self.marker_lines(staggered=True)
        lines.append((2.0, '{"type":"result","cost_usd":0}'))
        verdict = best_of_n.stream_check_verdict(lines, 0, False, started=0.0)
        self.assertEqual(verdict["verdict"], "pass")
        self.assertTrue(verdict["interleaved"])
        self.assertTrue(verdict["json_tail_parsed"])

    def test_fail_when_timestamps_collapse(self) -> None:
        lines = self.marker_lines(staggered=False)
        lines.append((2.0, '{"type":"result","cost_usd":0}'))
        verdict = best_of_n.stream_check_verdict(lines, 0, False, started=0.0)
        self.assertEqual(verdict["verdict"], "fail: line timestamps collapsed — incus exec buffered the stream")

    def test_fail_on_timeout(self) -> None:
        verdict = best_of_n.stream_check_verdict([], None, True, started=0.0)
        self.assertEqual(verdict["verdict"], "fail: streaming timed out")

    def test_fail_on_missing_markers(self) -> None:
        lines = [(0.1, f"1 {best_of_n.STREAM_MARKER}")]
        verdict = best_of_n.stream_check_verdict(lines, 0, False, started=0.0)
        self.assertIn("fail:", verdict["verdict"])

    def test_fail_on_nonzero_exit(self) -> None:
        lines = self.marker_lines(staggered=True)
        verdict = best_of_n.stream_check_verdict(lines, 1, False, started=0.0)
        self.assertIn("fail:", verdict["verdict"])

    def test_fail_on_unparseable_tail(self) -> None:
        lines = self.marker_lines(staggered=True)
        lines.append((2.0, "not json at all"))
        verdict = best_of_n.stream_check_verdict(lines, 0, False, started=0.0)
        self.assertEqual(verdict["verdict"], "fail: stream-json-shaped tail did not parse")

    def test_trailing_blank_lines_are_ignored(self) -> None:
        lines = self.marker_lines(staggered=True)
        lines.append((2.0, '{"type":"result","cost_usd":0}'))
        lines.append((2.1, ""))
        verdict = best_of_n.stream_check_verdict(lines, 0, False, started=0.0)
        self.assertEqual(verdict["verdict"], "pass")


class EmptyCompletionTests(unittest.TestCase):
    def test_all_zero_outputs_is_a_flake(self) -> None:
        self.assertTrue(best_of_n.is_empty_completion(0, 0, 0))

    def test_any_output_is_evaluable(self) -> None:
        self.assertFalse(best_of_n.is_empty_completion(5, 0, 0))   # text
        self.assertFalse(best_of_n.is_empty_completion(0, 2, 0))   # tools
        self.assertFalse(best_of_n.is_empty_completion(0, 0, 43))  # tokens


class VerifyRcTests(unittest.TestCase):
    def test_parse_verify_rc_last_wins(self) -> None:
        text = "some output\n__VERIFY_RC=0\n"
        self.assertEqual(best_of_n.parse_verify_rc(text), 0)
        self.assertEqual(best_of_n.parse_verify_rc("earlier __VERIFY_RC=1\nreal\n__VERIFY_RC=0"), 0)
        self.assertEqual(best_of_n.parse_verify_rc("__VERIFY_RC=1"), 1)
        self.assertIsNone(best_of_n.parse_verify_rc("no marker here"))
        self.assertIsNone(best_of_n.parse_verify_rc("__VERIFY_RC=notanumber"))


class PinFilterTests(unittest.TestCase):
    def test_pin_held_single_turn(self) -> None:
        record = _record("t", "groq")
        served = record["upstream_ids"][0]
        self.assertEqual(served, "groq")
        self.assertFalse(record.get("pin_excluded"))

    def test_pin_fallthrough_marks_record(self) -> None:
        # Mirror run_attempt's logic: a served id different from the pin sets
        # pin_excluded AND run_error (the record never counts as evaluable).
        record = _record("t", "groq", upstream_ids=["cerebras"])
        served = record["upstream_ids"][0]
        pin_held = served == "groq"
        if not pin_held:
            record["pin_excluded"] = True
            record["run_error"] = f"pin fell through: served by '{served}'"
        self.assertTrue(record["pin_excluded"])
        self.assertTrue(record["run_error"].startswith("pin fell through"))


class BuildReportTests(unittest.TestCase):
    def test_two_upstreams_one_failing_gives_uplift(self) -> None:
        records = [
            _record("t1", "groq"),                       # pass
            _record("t1", "cerebras", verify_pass=False),  # fail
            _record("t2", "groq", verify_pass=False),     # fail
            _record("t2", "cerebras"),                   # pass
        ]
        report = build_report("tag", "img", 1, records)
        agg = report["aggregate"]
        self.assertEqual(agg["single_attempt_pass_rate"], {"cerebras": 0.5, "groq": 0.5})
        self.assertEqual(agg["best_of_n_pass_rate"], 1.0)
        self.assertEqual(agg["uplift"], 0.5)
        self.assertTrue(report["per_task"]["t1"]["any_pass"])
        self.assertEqual(report["per_task"]["t1"]["passing_upstreams"], ["groq"])
        self.assertEqual(report["per_task"]["t2"]["picked"], "cerebras")

    def test_scope_violation_fails_a_clean_verify(self) -> None:
        records = [_record("t1", "groq", scope_violations=["./notes.md"])]
        report = build_report("tag", "img", 1, records)
        self.assertFalse(report["per_task"]["t1"]["any_pass"])
        self.assertEqual(report["aggregate"]["single_attempt_pass_rate"], {"groq": 0.0})
        self.assertEqual(report["aggregate"]["best_of_n_pass_rate"], 0.0)

    def test_unevaluable_attempts_are_counted_not_failed(self) -> None:
        records = [
            _record("t1", "groq"),
            _record("t1", "cerebras", run_error="provider dead", verify_pass=False),
        ]
        report = build_report("tag", "img", 1, records)
        agg = report["aggregate"]
        self.assertEqual(agg["unevaluable"], 1)
        self.assertEqual(agg["single_attempt_pass_rate"], {"groq": 1.0})  # cerebras excluded
        self.assertTrue(report["per_task"]["t1"]["any_pass"])
        self.assertEqual(agg["best_of_n_pass_rate"], 1.0)

    def test_pin_excluded_counted_in_aggregate(self) -> None:
        records = [
            _record("t1", "groq"),
            _record("t1", "cerebras", run_error="pin fell through: served by 'groq'", pin_excluded=True, verify_pass=False),
        ]
        report = build_report("tag", "img", 1, records)
        self.assertEqual(report["aggregate"]["pin_excluded"], 1)
        self.assertEqual(report["aggregate"]["unevaluable"], 1)

    def test_all_unevaluable_gives_none_rates(self) -> None:
        records = [_record("t1", "groq", run_error="dead", verify_pass=False)]
        report = build_report("tag", "img", 1, records)
        agg = report["aggregate"]
        self.assertEqual(agg["single_attempt_pass_rate"], {})
        self.assertIsNone(agg["best_of_n_pass_rate"])
        self.assertIsNone(agg["uplift"])

    def test_picked_is_fastest_passing_upstream(self) -> None:
        records = [
            _record("t1", "groq", total_ms=5000),
            _record("t1", "cerebras", total_ms=1000),
        ]
        report = build_report("tag", "img", 1, records)
        self.assertEqual(report["per_task"]["t1"]["picked"], "cerebras")

    def test_diversity_string_counts_multi_winner_tasks(self) -> None:
        records = [
            _record("t1", "groq"),
            _record("t1", "cerebras"),
        ]
        report = build_report("tag", "img", 1, records)
        self.assertIn("1/1 tasks", report["aggregate"]["diversity"])


class RateLimitRetryTests(unittest.TestCase):
    def test_rate_limit_messages_are_retryable(self) -> None:
        self.assertTrue(best_of_n._is_rate_limit_error(
            "clawde exited 1 [rate_limited]: [groq] Rate limited; retry after 1s"))
        self.assertTrue(best_of_n._is_rate_limit_error(
            "clawde exited 1: free-mode upstreams exhausted: zai [rate_limited]: [zai] Rate limited"))

    def test_stderr_only_rate_limit_is_retryable(self) -> None:
        # The uplift-v2 run: the error event reached stderr but not the
        # parsed stream, so run_error was the generic exit-1 message. The
        # retry classifier must consider stderr_tail too.
        combined = "clawde exited 1 " + '{"type":"error","error":"API error: [free] Server error: free-mode upstreams exhausted: google [rate_limited]: [google] Rate limited"}'
        self.assertTrue(best_of_n._is_rate_limit_error(combined))

    def test_other_errors_are_not_retryable(self) -> None:
        self.assertFalse(best_of_n._is_rate_limit_error(
            "clawde exited 1 [quota_exhausted]: [cerebras] Error 402: Payment required"))
        self.assertFalse(best_of_n._is_rate_limit_error(
            "clawde exited 1 [invalid_credential]: [mistral] tier_not_allowed"))
        self.assertFalse(best_of_n._is_rate_limit_error(
            "empty completion: no text, no tool calls, 0 output tokens (provider flake)"))
        self.assertFalse(best_of_n._is_rate_limit_error(""))

    def test_retry_pass_monkeypatched(self) -> None:
        # Drive run_attempt's retry branch without Incus: stub the pieces it
        # touches, feed a rate-limit error on the first call, success on the
        # second, and verify the retry carries attempt_index + the flag.
        calls = {"n": 0, "kwargs": []}

        class FakeUpstream(dict):
            def __init__(self):
                super().__init__({"id": "groq", "default_model": "m"})

        class FakeTask(dict):
            pass

        real_sleep = best_of_n.time.sleep
        best_of_n.time.sleep = lambda _s: None
        launched = []
        try:
            def fake_launch(image, run_id, *, keep):
                launched.append(run_id)

            def fake_seed(home, auth, sabotage):
                pass

            def fake_pin(home, up):
                pass

            def fake_push_home(home, name):
                pass

            def fake_push_binary(binary, name):
                pass

            def fake_deps(name):
                return {"verdict": "pass"}

            def fake_push_task(name, files):
                pass

            def fake_exec(name, argv, *, timeout=15, check=False):
                class P:
                    # The verify gate parses __VERIFY_RC from this output;
                    # manifest execs simply find no sha256 lines (harmless).
                    stdout = "__VERIFY_RC=0\n"
                    stderr = ""
                    returncode = 0
                return P()

            def fake_stream(name, argv, *, timeout):
                calls["n"] += 1
                if calls["n"] == 1:
                    return [
                        (0.0, '{"type":"error","error":"API error: [free] Server error: '
                              'free-mode upstreams exhausted: groq [rate_limited]: '
                              '[groq] Rate limited; retry after 1s"}'),
                    ], "", 1, False, 0.0
                # Success: real text + attribution + usage so the empty-
                # completion guard does not fire.
                return [
                    (0.1, '{"type":"text_delta","text":"done"}'),
                    (0.2, '{"type":"provider_attribution","provider_id":"free",'
                          '"upstream_id":"groq","model":"m","retries":0}'),
                    (0.3, '{"type":"result","usage":{"input_tokens":10,'
                          '"output_tokens":5},"cost_usd":0.01,"provider":"free",'
                          '"upstream":"groq","model":"m"}'),
                ], "", 0, False, 0.0

            orig = (
                best_of_n.launch, best_of_n.seed_home, best_of_n.write_pin_settings,
                best_of_n.push_home, best_of_n.push_binary, best_of_n.ensure_binary_deps,
                best_of_n.push_task_files, best_of_n.exec_capture, best_of_n.exec_stream_lines,
            )
            (best_of_n.launch, best_of_n.seed_home, best_of_n.write_pin_settings,
             best_of_n.push_home, best_of_n.push_binary, best_of_n.ensure_binary_deps,
             best_of_n.push_task_files, best_of_n.exec_capture, best_of_n.exec_stream_lines) = (
                fake_launch, fake_seed, fake_pin,
                fake_push_home, fake_push_binary, fake_deps,
                fake_push_task, fake_exec, fake_stream,
            )
            try:
                rec = best_of_n.run_attempt(
                    FakeTask({"name": "t", "prompt": "p", "files": {}, "verify_cmd": "true",
                              "verify_timeout_secs": 5, "allowed_paths": []}),
                    FakeUpstream(), 0,
                    image="img", binary=__import__("pathlib").Path("/bin/true"),
                    auth_file=__import__("pathlib").Path("/dev/null"),
                    max_turns=2, timeout=10,
                    artifacts_dir=__import__("pathlib").Path("/tmp/bon-test-artifacts"),
                    model_override=None, keep=False, attempt_index=4,
                )
            finally:
                (best_of_n.launch, best_of_n.seed_home, best_of_n.write_pin_settings,
                 best_of_n.push_home, best_of_n.push_binary, best_of_n.ensure_binary_deps,
                 best_of_n.push_task_files, best_of_n.exec_capture, best_of_n.exec_stream_lines) = orig
        finally:
            best_of_n.time.sleep = real_sleep

        self.assertEqual(calls["n"], 2)
        self.assertTrue(rec["rate_limited_once"])
        self.assertEqual(rec["attempt_index"], 4)
        self.assertIsNone(rec["run_error"])
        self.assertTrue(rec["verify_pass"])


class ScheduleRotationTests(unittest.TestCase):
    def test_schedule_interleaves_and_rotates(self) -> None:
        # 2 upstreams x 3 tasks x 1 repeat: each upstream sees the tasks in a
        # rotated order, so quota burn spreads instead of one task always
        # running last on every upstream.
        sched = best_of_n.build_schedule(["a", "b"], ["t1", "t2", "t3"], 1)
        a_order = [t for t, u, r, i in sched if u == "a"]
        b_order = [t for t, u, r, i in sched if u == "b"]
        self.assertEqual(a_order, ["t1", "t2", "t3"])
        self.assertEqual(b_order, ["t2", "t3", "t1"])
        # Attempt indices are stable 0..n.
        self.assertEqual([i for _t, _u, _r, i in sched], list(range(6)))

    def test_schedule_repeats_extend(self) -> None:
        sched = best_of_n.build_schedule(["a"], ["t1"], 3)
        self.assertEqual([(t, u, r) for t, u, r, _ in sched], [("t1", "a", 0), ("t1", "a", 1), ("t1", "a", 2)])


class TarRoundTripTests(unittest.TestCase):
    def test_tar_round_trip_member_names(self) -> None:
        # Mirror phase_manifest_and_pull's pull path: bytes -> tarfile -> names.
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w") as tf:
            for name, content in (("a.txt", b"mutated\n"), ("sub/b.txt", b"two\n")):
                info = tarfile.TarInfo(f"./{name}")
                info.size = len(content)
                tf.addfile(info, io.BytesIO(content))
        with tarfile.open(fileobj=io.BytesIO(buf.getvalue())) as tf:
            names = sorted(m.name.lstrip("./") for m in tf.getmembers() if m.isfile())
        self.assertEqual(names, ["a.txt", "sub/b.txt"])

    def test_container_name_uses_prefix(self) -> None:
        run_id = f"{uuid.uuid4().hex[:8]}"
        name = best_of_n.container_name(run_id)
        self.assertTrue(name.startswith(best_of_n.CONTAINER_PREFIX + "-"))
        self.assertIn(run_id, name)


if __name__ == "__main__":
    unittest.main()
