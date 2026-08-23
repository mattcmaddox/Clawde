# CI Workflow Testing

## Purpose

Prevent wasting time on inefficient CI testing patterns. This skill captures
lessons learned from debugging the release workflow — where polling loops and
unnecessary waits burned 45+ minutes of context.

---

## THE RULE: Test Locally, Then Verify Remotely

**Never** jump straight to pushing and waiting for CI. Always test locally first.

### Local Testing Checklist

Before pushing any workflow change:

```bash
# 1. Test the build locally (catches packaging bugs in seconds)
cd src-rust/
cargo build --release --package clawde
bash scripts/build.sh package

# 2. Verify archives exist and are valid
ls -la dist/*.tar.gz dist/*.zip dist/SHA256SUMS

# 3. Test npm publish separately (isolates npm concerns from build)
cd npm/
npm publish --dry-run  # verifies package.json, files, etc.

# 4. Validate YAML syntax
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"
```

**Time saved:** 15-30 minutes (catches bugs that would fail in CI after build time)

---

## NEVER: Polling Loops

### What NOT to do:

```bash
# BAD: Polling in a loop
for i in {1..10}; do
    sleep 300  # 5 minutes!
    gh api repos/owner/repo/actions/runs | jq '.workflow_runs[0].status'
done

# BAD: Long sleeps
sleep 600  # 10 minutes of doing nothing
gh api repos/owner/repo/actions/runs
```

**Why it's bad:**
- Burns time doing nothing useful
- Each poll tells you almost nothing
- Context window fills with useless status checks
- You could have caught the bug locally

### What to do instead:

```bash
# GOOD: Real-time streaming
gh run watch <run-id>

# GOOD: Immediate error feedback after failure
gh run view <run-id> --log-failed

# GOOD: Quick status check (no sleep needed)
gh api repos/owner/repo/actions/runs/latest | jq '.workflow_runs[0].status'
```

---

## NEVER: Long Waits Without Purpose

### What NOT to do:

```bash
# BAD: Waiting "just in case"
sleep 600  # "Rust builds take a while"
# Then poll anyway

# BAD: Waiting for convergence
sleep 300
gh api ...
sleep 300
gh api ...
```

**Why it's bad:**
- If you need to wait, use `gh run watch` (streams live)
- If you don't need to wait, test locally first
- Long sleeps = context wasted on nothing

### What to do instead:

1. **Trigger the workflow:**
   ```bash
   gh workflow run release.yml
   ```

2. **Watch it live:**
   ```bash
   gh run watch $(gh api repos/owner/repo/actions/runs/latest -q '.workflow_runs[0].id')
   ```

3. **If it fails, get the error immediately:**
   ```bash
   gh run view <run-id> --log-failed
   ```

**Total time:** 2-3 minutes of active debugging, not 45 minutes of sleeping

---

## Test Isolation: Separate Concerns

**Don't test everything at once.** Isolate what you're debugging.

### Example: npm publish failing

**Bad approach:** Push full workflow, wait 15 min for builds, fail at npm step

**Good approach:**
```bash
# Test npm publish locally (no builds needed)
cd npm/
echo "//registry.npmjs.org/:_authToken=$NPM_TOKEN" > .npmrc
npm publish --dry-run

# If dry-run passes, the issue is in CI, not the package
# If dry-run fails, fix locally (no CI wait)
```

### Example: build failing on specific platform

**Bad approach:** Push, wait for all 3 builds, see one fail

**Good approach:**
```bash
# Test the failing platform locally
bash scripts/build.sh build-one linux-x86_64

# If it passes locally, the issue is CI environment
# If it fails locally, fix it now
```

---

## Decision Tree: Testing CI Changes

```
Is the change in the workflow YAML itself?
├─ YES → Validate YAML syntax locally first
│        Then: gh workflow run <name> + gh run watch
│
├─ Is the change in a build script?
│  ├─ YES → Test the script locally
│  │        Then: push + gh run watch
│  │
│  └─ Is the change in npm/package files?
│     ├─ YES → Test npm publish --dry-run locally
│     │        Then: push + gh run watch
│     │
│     └─ NO → Push + gh run watch (skip local testing)
```

---

## Quick Reference: gh Commands

| Command | Use Case | Time |
|---------|----------|------|
| `gh run watch <id>` | Stream live status | Real-time |
| `gh run view <id> --log-failed` | Get error after failure | 2 sec |
| `gh api repos/.../actions/runs/latest` | Quick status check | 1 sec |
| `gh workflow run <name>` | Trigger manually | 1 sec |
| `gh run cancel <id>` | Stop a running workflow | 1 sec |

**Never use:** `sleep N; gh api ...` in a loop

---

## Anti-Patterns to Avoid

1. **The Polling Loop**
   - Symptom: `for i in {1..10}; do sleep N; gh api ...; done`
   - Fix: `gh run watch` instead

2. **The Long Wait**
   - Symptom: `sleep 300` or `sleep 600` before checking
   - Fix: Test locally first, or use `gh run watch`

3. **The Kitchen Sink Test**
   - Symptom: Push everything, wait 15 min, fail at step 5
   - Fix: Isolate concerns, test each piece separately

4. **The Guess-and-Check**
   - Symptom: Push, wait, fail, guess, push again, wait again
   - Fix: Read error logs (`gh run view --log-failed`) before guessing

---

## Time Budget

| Activity | Max Time | Method |
|----------|----------|--------|
| Local build test | 2 min | `cargo build` + `build.sh package` |
| npm dry-run | 30 sec | `npm publish --dry-run` |
| YAML validation | 5 sec | `python3 -c "import yaml; ..."` |
| CI workflow trigger | 1 sec | `gh workflow run` |
| CI status check | 2 sec | `gh run view --log-failed` |
| **Total before push** | **< 3 min** | **All local** |

**If you're spending more than 3 minutes testing before pushing, you're doing it wrong.**

---

## Post-Debugging Checklist

After fixing a CI issue, verify you didn't waste time:

- [ ] Did I test locally first? (If no → add to local checklist)
- [ ] Did I use `gh run watch` instead of polling? (If no → note the pattern)
- [ ] Did I wait less than 5 minutes total? (If no → what was the bottleneck?)
- [ ] Could I have caught this locally? (If yes → update this skill)

**Update this skill** if you discover a new anti-pattern or a better testing method.
