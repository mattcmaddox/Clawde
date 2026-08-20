#!/usr/bin/env python3
"""Run a bounded baseline-versus-candidate Clawde evaluation campaign.

This is the orchestration layer above ``run_eval.py``. It runs the same fixture
suite against two binaries, stores every report in an isolated campaign
folder, compares evaluable quality metrics, and distinguishes infrastructure
failures from answer-quality regressions.

Exit codes:
  0 = candidate passed the configured gates
  1 = an evaluable quality regression was detected
  2 = the campaign could not establish a comparison (provider/timeout/setup)

No source files, git refs, commits, or deployments are modified.
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from statistics import mean

REPO_ROOT = Path(__file__).resolve().parents[2]
EVAL_DIR = Path(__file__).resolve().parent
RUN_EVAL = EVAL_DIR / "run_eval.py"
DEFAULT_MANIFEST = EVAL_DIR / "campaign.json"
DEFAULT_AUTH = Path(os.environ.get("HOME", "~")) / ".clawde" / "auth.json"


def validate_manifest(manifest: dict) -> list[str]:
    """Return actionable errors for a campaign manifest."""
    errors: list[str] = []
    if not isinstance(manifest, dict):
        return ["manifest must be a JSON object"]

    repeats = manifest.get("repeats", 1)
    if not isinstance(repeats, int) or repeats < 1:
        errors.append("repeats must be a positive integer")

    min_evaluable = manifest.get("min_evaluable", 1)
    if not isinstance(min_evaluable, int) or min_evaluable < 1:
        errors.append("min_evaluable must be a positive integer")

    gates = manifest.get("gates", {})
    if not isinstance(gates, dict):
        errors.append("gates must be an object")
        gates = {}
    for key in ("max_pass_rate_drop", "max_score_drop"):
        value = gates.get(key, 0.0 if key == "max_pass_rate_drop" else 0.10)
        if not isinstance(value, (int, float)) or not 0 <= value <= 1:
            errors.append(f"gates.{key} must be between 0 and 1")

    fixtures = manifest.get("fixtures")
    if not isinstance(fixtures, list) or not fixtures:
        errors.append("fixtures must be a non-empty array")
        return errors
    names: set[str] = set()
    for index, fixture in enumerate(fixtures):
        prefix = f"fixtures[{index}]"
        if not isinstance(fixture, dict):
            errors.append(f"{prefix} must be an object")
            continue
        name = fixture.get("name")
        path = fixture.get("path")
        if not isinstance(name, str) or not name.strip():
            errors.append(f"{prefix}.name must be a non-empty string")
        elif name in names:
            errors.append(f"{prefix}.name duplicates {name!r}")
        else:
            names.add(name)
        if not isinstance(path, str) or not path.strip():
            errors.append(f"{prefix}.path must be a non-empty string")
        fixture_repeats = fixture.get("repeats", repeats)
        if not isinstance(fixture_repeats, int) or fixture_repeats < 1:
            errors.append(f"{prefix}.repeats must be a positive integer")
        if "judge" in fixture and not isinstance(fixture["judge"], bool):
            errors.append(f"{prefix}.judge must be boolean")
        sabotage = fixture.get("sabotage", [])
        if not isinstance(sabotage, list) or not all(isinstance(item, str) for item in sabotage):
            errors.append(f"{prefix}.sabotage must be an array of strings")
    return errors


def load_manifest(path: Path) -> dict:
    path = Path(path)
    try:
        manifest = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"could not load campaign manifest {path}: {error}") from error
    errors = validate_manifest(manifest)
    if errors:
        raise ValueError("invalid campaign manifest:\n" + "\n".join(f"  - {error}" for error in errors))
    return manifest


def _safe_name(value: str) -> str:
    return "".join(char if char.isalnum() or char in "-_" else "_" for char in value)


def _report_record(
    phase: str,
    fixture: dict,
    repetition: int,
    report_path: Path | None,
    exit_code: int,
    stderr: str = "",
) -> dict:
    report = None
    if report_path and report_path.exists():
        try:
            report = json.loads(report_path.read_text())
        except (OSError, json.JSONDecodeError):
            report = None

    run = report.get("run", {}) if isinstance(report, dict) else {}
    return {
        "phase": phase,
        "fixture": fixture["name"],
        "repetition": repetition,
        "exit_code": exit_code,
        "report_path": str(report_path) if report_path else None,
        "passed": report.get("passed") if isinstance(report, dict) else None,
        "score": report.get("score") if isinstance(report, dict) else None,
        "error": run.get("error") if isinstance(run, dict) else None,
        "response_chars": run.get("response_chars") if isinstance(run, dict) else None,
        "upstream": run.get("upstream_id") if isinstance(run, dict) else None,
        "ttft_ms": run.get("first_text_delta_ms") if isinstance(run, dict) else None,
        "total_ms": run.get("total_ms") if isinstance(run, dict) else None,
        "stderr_tail": "\n".join(stderr.splitlines()[-5:]),
    }


def inferred_workdir(binary: Path) -> Path:
    """Infer a checkout's Rust workspace from a conventional debug binary path."""
    resolved = binary.resolve()
    if resolved.parent.name == "debug" and resolved.parent.parent.name == "target":
        return resolved.parent.parent.parent
    return REPO_ROOT / "src-rust"


def run_fixture(
    phase: str,
    binary: Path,
    fixture: dict,
    repetition: int,
    *,
    output_dir: Path,
    auth_file: Path,
    timeout: float,
    model: str | None,
    cwd: Path,
    permission_mode: str,
) -> dict:
    fixture_path = (REPO_ROOT / fixture["path"]).resolve()
    run_dir = output_dir / phase / _safe_name(fixture["name"]) / str(repetition)
    run_dir.mkdir(parents=True, exist_ok=True)
    command = [
        sys.executable,
        str(RUN_EVAL),
        "--fixture",
        str(fixture_path),
        "--binary",
        str(binary),
        "--auth-file",
        str(auth_file),
        "--output",
        str(run_dir),
        "--no-results",
        "--quiet",
        "--timeout",
        str(timeout),
        "--cwd",
        str(cwd),
        "--permission-mode",
        permission_mode,
        "--tag",
        f"campaign-{phase}-{fixture['name']}-{repetition}",
    ]
    if model:
        command.extend(["--model", model])
    if fixture.get("judge", False):
        command.append("--judge")
    for upstream in fixture.get("sabotage", []):
        command.extend(["--sabotage", upstream])

    try:
        completed = subprocess.run(
            command,
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=timeout + 30,
        )
        exit_code = completed.returncode
        stderr = completed.stderr or completed.stdout
    except subprocess.TimeoutExpired as error:
        exit_code = 2
        stderr = f"campaign runner timeout after {timeout + 30:.0f}s: {error}"

    report_path = run_dir / "report.json"
    return _report_record(phase, fixture, repetition, report_path, exit_code, stderr)


def run_offline_tests(output_dir: Path, timeout: float) -> dict:
    """Run the local eval tests before spending provider quota."""
    command = [sys.executable, "-m", "unittest", "discover", "scripts/eval", "-p", "test_*.py"]
    try:
        completed = subprocess.run(
            command,
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        output = (completed.stdout or "") + (completed.stderr or "")
        status = "passed" if completed.returncode == 0 else "failed"
        exit_code = completed.returncode
    except subprocess.TimeoutExpired as error:
        output = f"offline test timeout: {error}"
        status = "failed"
        exit_code = 2
    path = output_dir / "offline-tests.txt"
    path.write_text(output[-20_000:])
    return {"status": status, "exit_code": exit_code, "output_path": str(path)}


def aggregate(records: list[dict], min_evaluable: int) -> dict:
    evaluable = [
        record
        for record in records
        if record.get("error") is None
        and isinstance(record.get("score"), (int, float))
        and isinstance(record.get("passed"), bool)
    ]
    scores = [float(record["score"]) for record in evaluable]
    ttfts = [record["ttft_ms"] for record in evaluable if isinstance(record.get("ttft_ms"), (int, float))]
    return {
        "runs": len(records),
        "evaluable": len(evaluable),
        "enough_data": len(evaluable) >= min_evaluable,
        "infra_failures": len(records) - len(evaluable),
        "pass_rate": round(sum(1 for record in evaluable if record["passed"]) / len(evaluable), 3)
        if evaluable
        else None,
        "score_mean": round(mean(scores), 3) if scores else None,
        "score_min": round(min(scores), 3) if scores else None,
        "ttft_mean_ms": round(mean(ttfts)) if ttfts else None,
    }


def compare_summaries(
    baseline: dict,
    candidate: dict,
    *,
    max_pass_rate_drop: float,
    max_score_drop: float,
) -> dict:
    """Compare one fixture's aggregate metrics and classify the outcome."""
    if not baseline.get("enough_data") or not candidate.get("enough_data"):
        return {"status": "infrastructure", "reason": "not enough evaluable runs"}

    pass_drop = baseline["pass_rate"] - candidate["pass_rate"]
    score_drop = baseline["score_mean"] - candidate["score_mean"]
    regressions = []
    if pass_drop > max_pass_rate_drop:
        regressions.append(
            f"pass rate dropped from {baseline['pass_rate']:.3f} to {candidate['pass_rate']:.3f}"
        )
    if score_drop > max_score_drop:
        regressions.append(
            f"mean score dropped from {baseline['score_mean']:.3f} to {candidate['score_mean']:.3f}"
        )
    return {
        "status": "regression" if regressions else "passed",
        "pass_rate_drop": round(pass_drop, 3),
        "score_drop": round(score_drop, 3),
        "reasons": regressions,
    }


def build_comparison(records: list[dict], manifest: dict) -> dict:
    min_evaluable = manifest.get("min_evaluable", 1)
    gates = manifest.get("gates", {})
    by_fixture: dict[str, dict[str, list[dict]]] = {}
    for record in records:
        by_fixture.setdefault(record["fixture"], {}).setdefault(record["phase"], []).append(record)

    fixtures = {}
    regressions = []
    infrastructure = []
    manifest_fixture_names = [fixture["name"] for fixture in manifest["fixtures"]]
    for name in sorted(manifest_fixture_names):
        phases = by_fixture.get(name, {})
        baseline = aggregate(phases.get("baseline", []), min_evaluable)
        candidate = aggregate(phases.get("candidate", []), min_evaluable)
        comparison = compare_summaries(
            baseline,
            candidate,
            max_pass_rate_drop=float(gates.get("max_pass_rate_drop", 0.0)),
            max_score_drop=float(gates.get("max_score_drop", 0.10)),
        )
        fixtures[name] = {"baseline": baseline, "candidate": candidate, "comparison": comparison}
        if comparison["status"] == "regression":
            regressions.append(name)
        elif comparison["status"] == "infrastructure":
            infrastructure.append(name)

    if regressions:
        status = "regression"
        exit_code = 1
    elif infrastructure:
        status = "infrastructure"
        exit_code = 2
    else:
        status = "passed"
        exit_code = 0
    return {
        "status": status,
        "exit_code": exit_code,
        "regressions": regressions,
        "infrastructure": infrastructure,
        "fixtures": fixtures,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True, help="Baseline Clawde binary")
    parser.add_argument("--candidate", type=Path, required=True, help="Candidate Clawde binary")
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--auth-file", type=Path, default=DEFAULT_AUTH)
    parser.add_argument("--output", type=Path, default=None, help="Campaign artifact directory")
    parser.add_argument("--timeout", type=float, default=300, help="Per-live-eval timeout in seconds")
    parser.add_argument("--offline-timeout", type=float, default=120, help="Offline test timeout")
    parser.add_argument("--model", default=None, help="Override the model for every fixture")
    parser.add_argument(
        "--permission-mode",
        choices=("default", "accept-edits", "bypass-permissions", "plan"),
        default="plan",
        help="Permission mode for fixture runs (default: plan; read-only evals can inspect files)",
    )
    parser.add_argument(
        "--baseline-cwd",
        type=Path,
        default=None,
        help="Baseline workspace passed to Clawde (default inferred from binary path)",
    )
    parser.add_argument(
        "--candidate-cwd",
        type=Path,
        default=None,
        help="Candidate workspace passed to Clawde (default inferred from binary path)",
    )
    parser.add_argument("--repeat", type=int, default=None, help="Override manifest repeats")
    parser.add_argument("--skip-offline-tests", action="store_true")
    args = parser.parse_args()

    try:
        manifest = load_manifest(args.manifest.resolve())
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    for label, binary in (("baseline", args.baseline), ("candidate", args.candidate)):
        if not binary.is_file():
            print(f"error: {label} binary does not exist: {binary}", file=sys.stderr)
            return 2
    if not args.auth_file.is_file():
        print(f"error: auth file does not exist: {args.auth_file}", file=sys.stderr)
        return 2
    requested_workdirs = {
        "baseline": (args.baseline_cwd or inferred_workdir(args.baseline)),
        "candidate": (args.candidate_cwd or inferred_workdir(args.candidate)),
    }
    for label, workdir in requested_workdirs.items():
        if not workdir.is_dir():
            print(f"error: {label} workspace does not exist: {workdir}", file=sys.stderr)
            return 2
    if args.repeat is not None and (args.repeat < 1):
        print("error: --repeat must be positive", file=sys.stderr)
        return 2

    timestamp = time.strftime("%Y%m%dT%H%M%S", time.gmtime())
    output_dir = (args.output or EVAL_DIR / "results" / "campaign" / timestamp).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    offline = None if args.skip_offline_tests else run_offline_tests(output_dir, args.offline_timeout)
    if offline and offline["exit_code"] != 0:
        report = {"status": "infrastructure", "exit_code": 2, "offline_tests": offline}
        (output_dir / "campaign.json").write_text(json.dumps(report, indent=2))
        print(f"campaign: offline tests failed; see {output_dir / 'offline-tests.txt'}", file=sys.stderr)
        return 2

    records = []
    repeats = args.repeat or manifest.get("repeats", 1)
    binaries = {
        "baseline": args.baseline.resolve(),
        "candidate": args.candidate.resolve(),
    }
    workdirs = {
        "baseline": (args.baseline_cwd or inferred_workdir(binaries["baseline"])).resolve(),
        "candidate": (args.candidate_cwd or inferred_workdir(binaries["candidate"])).resolve(),
    }
    for phase in ("baseline", "candidate"):
        binary = binaries[phase]
        for fixture in manifest["fixtures"]:
            fixture_repeats = args.repeat or fixture.get("repeats", repeats)
            for repetition in range(1, fixture_repeats + 1):
                record = run_fixture(
                    phase,
                    binary,
                    fixture,
                    repetition,
                    output_dir=output_dir,
                    auth_file=args.auth_file.resolve(),
                    timeout=args.timeout,
                    model=args.model,
                    cwd=workdirs[phase],
                    permission_mode=args.permission_mode,
                )
                records.append(record)
                print(
                    f"{phase:9} {fixture['name']:20} run={repetition} "
                    f"exit={record['exit_code']} score={record['score']} passed={record['passed']}"
                )

    comparison = build_comparison(records, manifest)
    report = {
        "schema_version": "clawde-eval.campaign.v1",
        "name": manifest.get("name", "campaign"),
        "baseline": str(args.baseline.resolve()),
        "candidate": str(args.candidate.resolve()),
        "baseline_cwd": str(workdirs["baseline"]),
        "candidate_cwd": str(workdirs["candidate"]),
        "permission_mode": args.permission_mode,
        "manifest": str(args.manifest.resolve()),
        "offline_tests": offline,
        "records": records,
        "comparison": comparison,
    }
    report_path = output_dir / "campaign.json"
    report_path.write_text(json.dumps(report, indent=2))
    print(f"campaign: {comparison['status']} — report: {report_path}")
    if comparison["regressions"]:
        print(f"quality regressions: {', '.join(comparison['regressions'])}", file=sys.stderr)
    if comparison["infrastructure"]:
        print(f"infrastructure failures: {', '.join(comparison['infrastructure'])}", file=sys.stderr)
    return comparison["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
