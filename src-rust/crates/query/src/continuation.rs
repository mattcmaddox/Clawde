// continuation.rs — In-loop continuation policy for `run_query_loop`.
//
// At the end of every turn that finishes with `end_turn` (no tool calls),
// `run_query_loop` consults a `ContinuationPolicy` to decide whether to keep
// going — and if so, with what follow-up user message — instead of always
// returning after one turn.
//
// This mirrors pi's agent-loop callbacks (`shouldStopAfterTurn`,
// `getFollowUpMessages`, `prepareNextTurn`) and its `agentLoopContinue`
// primitive for "keep going without a new user message". The decision now
// lives INSIDE the loop, so autonomous continuation (e.g. `/goal`) no longer
// requires the CLI REPL to re-dispatch a fresh turn after the loop returns.
//
// The default policy is `StopPolicy`: stop after one turn, exactly reproducing
// the historical non-goal behaviour. Goal-driven continuation is provided by
// `GoalPolicy` (see the goal-policy section below), which reuses the existing
// `goal_loop` guards (runaway cap, soft token budget, continuation message).

/// Inputs available to a continuation policy after a turn completes with
/// `end_turn` (no tool calls were requested).
pub struct TurnEndContext<'a> {
    /// Session identifier — used to look up any active goal for this session.
    pub session_id: &'a str,
    /// Cumulative token count for the whole session (soft-budget accounting).
    pub total_tokens_used: u64,
    /// Wall-clock seconds this turn took (goal time accounting).
    pub turn_elapsed_secs: u64,
    /// Working directory of the run — the project root verification commands
    /// are detected against and executed in.
    pub working_dir: &'a std::path::Path,
    /// Whether the turn's tool round included a file-writing tool. Used by the
    /// verify policy to skip verification for pure read/search turns, and by
    /// the goal no-progress guard (a writing turn is always progress).
    pub turn_made_writes: bool,
    /// Model output tokens generated this turn. Feeds the goal no-progress
    /// guard: several consecutive turns with negligible output and no writes
    /// pause the goal as stalled.
    pub turn_output_tokens: u64,
    /// Optional changed-file patch for the completed turn. This contains the
    /// tree hash and changed paths, not unified diff text. Borrowed from the
    /// assistant message while the policy evaluates it; absent when snapshot
    /// tracking is disabled or unchanged.
    pub changed_files: Option<&'a clawde_core::snapshot::Patch>,
    /// Bounded unified diff for the completed turn. This is separate from the
    /// patch metadata so callers can omit expensive diff text when semantic
    /// verification is not selected.
    pub changed_diff: Option<&'a str>,
    /// The latest parsed structured spec for the working directory, when one
    /// exists. Owned so a future async verifier can safely retain it across an
    /// await without borrowing the filesystem or query-loop state.
    pub spec: Option<clawde_core::spec::Spec>,
}

/// Find the spec that belongs to an accepted task in the active session.
///
/// Specs are loaded from the latest parseable artifact, then matched against
/// both identity fields so a stale artifact from another session cannot become
/// semantic-verifier input merely because it reuses a task ID.
pub(crate) fn matching_spec(
    working_dir: &std::path::Path,
    task_id: &str,
    session_id: &str,
) -> Option<clawde_core::spec::Spec> {
    clawde_core::spec::Spec::latest_in(working_dir)
        .map(|(_, spec)| spec)
        .filter(|spec| spec.task_id == task_id && spec.session_id.as_deref() == Some(session_id))
}

fn path_is_within_working_dir(path: &std::path::Path, working_dir: &std::path::Path) -> bool {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }

    let Ok(root) = working_dir.canonicalize() else {
        return false;
    };
    if std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return false;
    }
    let candidate = if path.exists() {
        path.canonicalize().ok()
    } else {
        path.parent()
            .and_then(|parent| parent.canonicalize().ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
    };
    candidate.is_some_and(|candidate| candidate.starts_with(root))
}

/// The verdict returned by a semantic verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SemanticVerdict {
    /// The verifier found no relevant semantic problem.
    Pass,
    /// The verifier found a problem the current bounded loop may be able to fix.
    ///
    /// `fail` remains a wire-level alias for compatibility with the first
    /// scaffolding version; new prompts and callers use the canonical name.
    #[serde(alias = "fail")]
    Fixable,
    /// The verifier found a problem requiring a fresh plan or execution route.
    /// The current loop stops rather than replaying the same writer context.
    Replan,
    /// The verifier could not establish correctness safely.
    Escalate,
}

impl SemanticVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fixable => "fixable",
            Self::Replan => "replan",
            Self::Escalate => "escalate",
        }
    }
}

/// Strict, machine-readable output accepted from a semantic verifier runner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticVerifyResponse {
    pub verdict: SemanticVerdict,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<String>,
}

/// Machine-visible semantic-verification outcome emitted to clients.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SemanticVerifyReport {
    pub verdict: SemanticVerdict,
    pub summary: String,
    pub findings: Vec<String>,
}

/// Owned request passed to an injected semantic verifier runner.
///
/// The request deliberately contains only the current session's scoped change
/// list and matching spec metadata. It does not contain a live `ToolContext`,
/// credentials, or a writable tool set.
#[derive(Debug, Clone)]
pub struct SemanticVerifyRequest {
    pub session_id: String,
    pub working_dir: std::path::PathBuf,
    pub changed_files: Vec<std::path::PathBuf>,
    pub tree_hash: String,
    /// Bounded unified diff for the current turn, delimited as untrusted input.
    pub diff: String,
    pub task_id: Option<String>,
    pub spec: Option<clawde_core::spec::Spec>,
    /// Explicit allowlist for a future read-only verifier agent.
    pub read_only_tools: Vec<String>,
}

/// Owned request handed to a fresh-executor fix runner (writer-verifier gap
/// G5). Carries the verifier's verdict context plus the scoped spec/diff so
/// the executor can act without re-deriving the task from the session trace.
#[derive(Debug, Clone)]
pub struct SemanticFixRequest {
    pub session_id: String,
    pub working_dir: std::path::PathBuf,
    pub changed_files: Vec<std::path::PathBuf>,
    pub tree_hash: String,
    /// Bounded unified diff for the turn under review.
    pub diff: String,
    pub task_id: Option<String>,
    pub spec: Option<clawde_core::spec::Spec>,
    /// Verifier summary of the fixable defect.
    pub summary: String,
    /// Verifier findings, one concrete defect each.
    pub findings: Vec<String>,
}

/// Result type returned by an injected fresh-executor fix runner.
pub type SemanticFixRunnerResult = Result<String, String>;

/// Async runner seam for a fresh-executor fixer (writer-verifier gap G5).
///
/// Unlike the verifier, this runner is expected to EDIT files (write tools)
/// in a fresh sub-agent session — never the same in-context trace the loop is
/// replaying. Injected by the caller so the policy stays provider-neutral.
pub type SemanticFixRunner = std::sync::Arc<
    dyn Fn(
            SemanticFixRequest,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = SemanticFixRunnerResult> + Send>>
        + Send
        + Sync,
>;

/// Result type returned by an injected semantic verifier runner.
pub type SemanticVerifyRunnerResult = Result<String, String>;

/// Async runner seam for a semantic verifier.
///
/// The runner is intentionally injected by the caller. This keeps the policy
/// provider-neutral and lets tests use a deterministic response without making
/// a live model call. A future AgentTool adapter must construct a fresh
/// read-only context and use only `read_only_tools`.
pub type SemanticVerifyRunner = std::sync::Arc<
    dyn Fn(
            SemanticVerifyRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = SemanticVerifyRunnerResult> + Send>,
        > + Send
        + Sync,
>;

/// Maximum raw response size accepted from an untrusted semantic verifier.
/// This bounds parsing work before serde allocates response fields.
pub const SEMANTIC_VERIFY_MAX_RESPONSE_BYTES: usize = 32 * 1024;
/// Maximum summary size carried into a continuation note.
pub const SEMANTIC_VERIFY_MAX_SUMMARY_CHARS: usize = 4_000;
/// Maximum number of findings carried into a continuation note.
pub const SEMANTIC_VERIFY_MAX_FINDINGS: usize = 16;
/// Maximum size of one finding carried into a continuation note.
pub const SEMANTIC_VERIFY_MAX_FINDING_CHARS: usize = 1_000;
/// Maximum unified-diff characters passed to the semantic verifier.
pub const SEMANTIC_VERIFY_MAX_DIFF_CHARS: usize = 64 * 1024;

fn bound_semantic_diff(diff: String) -> String {
    if diff.chars().count() <= SEMANTIC_VERIFY_MAX_DIFF_CHARS {
        return diff;
    }
    let end = diff
        .char_indices()
        .nth(SEMANTIC_VERIFY_MAX_DIFF_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(diff.len());
    format!("{}\n[diff truncated]", &diff[..end])
}

/// Parse a semantic verifier response. Responses must be a bounded JSON object
/// with exactly `verdict`, `summary`, and optional `findings`; prose or fenced
/// JSON is rejected so an ambiguous model response can never authorize
/// continuation.
pub fn parse_semantic_verify_response(response: &str) -> Result<SemanticVerifyResponse, String> {
    let trimmed = response.trim();
    if trimmed.len() > SEMANTIC_VERIFY_MAX_RESPONSE_BYTES {
        return Err(format!(
            "semantic verifier response exceeds the {}-byte limit",
            SEMANTIC_VERIFY_MAX_RESPONSE_BYTES
        ));
    }

    let parsed: SemanticVerifyResponse = serde_json::from_str(trimmed)
        .map_err(|error| format!("semantic verifier returned malformed JSON: {error}"))?;
    if parsed.summary.trim().is_empty() {
        return Err("semantic verifier returned an empty summary".to_string());
    }
    if parsed.summary.chars().count() > SEMANTIC_VERIFY_MAX_SUMMARY_CHARS {
        return Err(format!(
            "semantic verifier summary exceeds the {}-character limit",
            SEMANTIC_VERIFY_MAX_SUMMARY_CHARS
        ));
    }
    if parsed.findings.len() > SEMANTIC_VERIFY_MAX_FINDINGS {
        return Err(format!(
            "semantic verifier returned more than {} findings",
            SEMANTIC_VERIFY_MAX_FINDINGS
        ));
    }
    if parsed
        .findings
        .iter()
        .any(|finding| finding.chars().count() > SEMANTIC_VERIFY_MAX_FINDING_CHARS)
    {
        return Err(format!(
            "a semantic verifier finding exceeds the {}-character limit",
            SEMANTIC_VERIFY_MAX_FINDING_CHARS
        ));
    }
    Ok(parsed)
}

/// Read-only tools a future semantic verifier may receive.
///
/// This is an explicit allowlist rather than a permission-level filter: a tool
/// accidentally marked `None` cannot become available to the verifier merely by
/// changing its metadata.
pub fn semantic_read_only_tool_names() -> Vec<String> {
    [
        clawde_core::constants::TOOL_NAME_FILE_READ,
        clawde_core::constants::TOOL_NAME_GLOB,
        clawde_core::constants::TOOL_NAME_GREP,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Tools a fresh-executor fixer receives: the read-only verifier set plus the
/// file-mutating tools needed to apply a fix. No shell/network tools — the
/// executor repairs files, it does not run commands.
pub fn semantic_fixer_tool_names() -> Vec<String> {
    let mut names = semantic_read_only_tool_names();
    names.extend(
        [
            clawde_core::constants::TOOL_NAME_FILE_EDIT,
            clawde_core::constants::TOOL_NAME_FILE_WRITE,
            clawde_core::constants::TOOL_NAME_BATCH_EDIT,
            clawde_core::constants::TOOL_NAME_APPLY_PATCH,
            clawde_core::constants::TOOL_NAME_NOTEBOOK_EDIT,
        ]
        .into_iter()
        .map(str::to_string),
    );
    names
}

/// Decision returned by a continuation policy at the end of a completed turn.
#[derive(Debug, Clone)]
pub enum ContinuationDecision {
    /// Inject `message` as the next user turn and keep the loop running.
    Continue { message: String },
    /// Stop the loop. `note`, when present, is surfaced to the user as a
    /// status line (e.g. the goal's paused / budget-limited message).
    Stop { note: Option<String> },
}

impl ContinuationDecision {
    /// Whether this decision keeps the loop running.
    pub fn is_continue(&self) -> bool {
        matches!(self, ContinuationDecision::Continue { .. })
    }
}

/// Opt-in read-only semantic verification policy.
///
/// This policy is deliberately conservative: it only runs after a writing
/// turn with a non-empty scoped patch, requires an injected runner, and stops
/// on runner errors or malformed responses. A `fixable` verdict can continue
/// with bounded, explicit feedback; `pass`, `replan`, and `escalate` stop.
pub struct SemanticVerifyPolicy {
    runner: Option<SemanticVerifyRunner>,
    fix_runner: Option<SemanticFixRunner>,
    attempts: std::sync::atomic::AtomicU32,
    max_attempts: u32,
    last_report: std::sync::Mutex<Option<SemanticVerifyReport>>,
}

impl SemanticVerifyPolicy {
    pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

    pub fn new(
        runner: Option<SemanticVerifyRunner>,
        fix_runner: Option<SemanticFixRunner>,
    ) -> Self {
        Self::with_max_attempts(runner, fix_runner, Self::DEFAULT_MAX_ATTEMPTS)
    }

    /// Build a policy with a caller-configured, bounded fix/reverify limit.
    pub fn with_max_attempts(
        runner: Option<SemanticVerifyRunner>,
        fix_runner: Option<SemanticFixRunner>,
        max_attempts: u32,
    ) -> Self {
        Self {
            runner,
            fix_runner,
            attempts: std::sync::atomic::AtomicU32::new(0),
            max_attempts: max_attempts.clamp(1, clawde_core::config::MAX_SEMANTIC_ATTEMPTS),
            last_report: std::sync::Mutex::new(None),
        }
    }

    /// Maximum fix-and-reverify rounds before escalation.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Clone of the latest verdict report (peek; does not consume).
    pub fn last_report(&self) -> Option<SemanticVerifyReport> {
        self.last_report.lock().unwrap().clone()
    }

    /// Whether a fresh-executor fixer is configured for this policy.
    pub fn has_fixer(&self) -> bool {
        self.fix_runner.is_some()
    }

    /// Invoke the fresh-executor fixer with the given request.
    ///
    /// G5: the fixer runs in a fresh sub-agent session with write tools — it
    /// must never reuse the loop's in-context trace.
    async fn run_fixer(&self, request: SemanticFixRequest) -> Result<String, String> {
        let fixer = self
            .fix_runner
            .as_ref()
            .ok_or_else(|| "no fresh-executor fixer configured".to_string())?;
        fixer(request).await
    }

    fn request_from_context(
        &self,
        ctx: &TurnEndContext<'_>,
    ) -> Result<SemanticVerifyRequest, ContinuationDecision> {
        if !ctx.turn_made_writes {
            return Err(ContinuationDecision::Stop { note: None });
        }
        let Some(patch) = ctx.changed_files else {
            return Err(ContinuationDecision::Stop {
                note: Some(
                    "Semantic verification skipped: no scoped changed-file patch was available."
                        .to_string(),
                ),
            });
        };
        if patch.files.is_empty() {
            return Err(ContinuationDecision::Stop { note: None });
        }
        if ctx.changed_diff.is_none_or(|diff| diff.trim().is_empty()) {
            return Err(ContinuationDecision::Stop {
                note: Some(
                    "Semantic verification skipped: no non-empty scoped diff was available."
                        .to_string(),
                ),
            });
        }
        // Patch paths are untrusted model context. Require canonical paths inside
        // this run's working directory before they can reach a verifier prompt.
        // Deleted files are checked through their existing canonical parent.
        if patch
            .files
            .iter()
            .any(|path| !path_is_within_working_dir(path, ctx.working_dir))
        {
            return Err(ContinuationDecision::Stop {
                note: Some(
                    "Semantic verification stopped: changed-file scope escaped the working directory."
                        .to_string(),
                ),
            });
        }
        if self.runner.is_none() {
            return Err(ContinuationDecision::Stop {
                note: Some(
                    "Semantic verification is enabled, but no read-only verifier runner is configured; no model call was made."
                        .to_string(),
                ),
            });
        }

        Ok(SemanticVerifyRequest {
            session_id: ctx.session_id.to_string(),
            working_dir: ctx.working_dir.to_path_buf(),
            changed_files: patch.files.clone(),
            tree_hash: patch.hash.clone(),
            diff: bound_semantic_diff(ctx.changed_diff.unwrap_or_default().to_string()),
            task_id: ctx.spec.as_ref().map(|spec| spec.task_id.clone()),
            spec: ctx.spec.clone(),
            read_only_tools: semantic_read_only_tool_names(),
        })
    }

    fn response_decision(&self, response: SemanticVerifyResponse) -> ContinuationDecision {
        *self.last_report.lock().unwrap() = Some(SemanticVerifyReport {
            verdict: response.verdict,
            summary: response.summary.clone(),
            findings: response.findings.clone(),
        });
        let findings = if response.findings.is_empty() {
            String::new()
        } else {
            format!("\n\nFindings:\n- {}", response.findings.join("\n- "))
        };
        match response.verdict {
            SemanticVerdict::Pass => ContinuationDecision::Stop {
                note: Some(format!(
                    "Semantic verification passed: {}",
                    response.summary
                )),
            },
            SemanticVerdict::Fixable => {
                let attempt = self
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                if attempt > self.max_attempts {
                    return ContinuationDecision::Stop {
                        note: Some(format!(
                            "Semantic verification exhausted ({max} attempts): {}{}",
                            response.summary,
                            findings,
                            max = self.max_attempts
                        )),
                    };
                }
                ContinuationDecision::Continue {
                    message: format!(
                        "Semantic verification found fixable issues (attempt {attempt}/{max}): {}{}\n\nFix the reported issues, then finish the turn for another verification round.",
                        response.summary,
                        findings,
                        max = self.max_attempts
                    ),
                }
            }

            SemanticVerdict::Replan => ContinuationDecision::Stop {
                note: Some(format!(
                    "Semantic verification requests a replan: {}{}",
                    response.summary, findings
                )),
            },
            SemanticVerdict::Escalate => ContinuationDecision::Stop {
                note: Some(format!(
                    "Semantic verification requires review: {}{}",
                    response.summary, findings
                )),
            },
        }
    }
}

/// Composite policy that keeps deterministic verification authoritative and
/// invokes semantic review only after a passing deterministic round.
pub struct SemanticAfterVerifyPolicy {
    deterministic: crate::verify::VerifyPolicy,
    semantic: SemanticVerifyPolicy,
}

impl SemanticAfterVerifyPolicy {
    pub fn new(
        verify_config: clawde_core::config::VerifyConfig,
        working_dir: &std::path::Path,
        runner: Option<SemanticVerifyRunner>,
        fix_runner: Option<SemanticFixRunner>,
    ) -> Self {
        Self {
            deterministic: crate::verify::VerifyPolicy::new(
                verify_config.clone(),
                working_dir.to_path_buf(),
            ),
            semantic: SemanticVerifyPolicy::with_max_attempts(
                runner,
                fix_runner,
                verify_config.semantic_max_attempts,
            ),
        }
    }

    /// True when a fresh-executor fixer is wired, i.e. the G5 fix loop is
    /// available rather than the legacy same-context `Continue`.
    pub fn has_fixer(&self) -> bool {
        self.semantic.has_fixer()
    }
}

impl ContinuationPolicy for SemanticAfterVerifyPolicy {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        self.deterministic.decide(ctx)
    }

    fn decide_async<'a>(
        &'a self,
        ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        Box::pin(async move {
            let deterministic = self.deterministic.decide(ctx);
            if deterministic.is_continue() {
                return deterministic;
            }
            let deterministic_verdict = self
                .deterministic
                .verify_report()
                .map(|report| report.verdict);
            if !matches!(
                deterministic_verdict,
                Some(crate::verify::VerifyVerdict::Pass)
            ) {
                return deterministic;
            }

            // Deterministic gate passed → semantic review. G5: a `fixable`
            // verdict must NOT be replayed into the same in-context trace
            // (Trap 4). When a fresh-executor fixer is configured, run the
            // fix-and-reverify loop entirely inside this policy: each round
            // spawns a fresh write-tools executor with the verdict, then
            // re-runs the deterministic gate + semantic review on the new
            // state. Only a terminal decision (pass / escalate / replan /
            // exhausted) is surfaced to the loop.
            let mut round: u32 = 0;
            loop {
                let decision = self.semantic.decide_async(ctx).await;
                match decision {
                    // pass / replan / escalate / runner-error → terminal.
                    ContinuationDecision::Stop { .. } => return decision,
                    ContinuationDecision::Continue { message } => {
                        if !self.semantic.has_fixer() {
                            // No fresh-executor fixer configured: fall back to
                            // the legacy same-context Continue (documented
                            // degraded mode; the loop pushes the fix request
                            // into the existing trace).
                            return ContinuationDecision::Continue { message };
                        }
                        round += 1;
                        if round >= self.semantic.max_attempts() {
                            return ContinuationDecision::Stop {
                                note: Some(format!(
                                    "Semantic verification exhausted after {round} fresh-executor \
                                     fix rounds; the reported issues remain unresolved: {message}"
                                )),
                            };
                        }
                        // Build the fresh-executor request from the verdict
                        // report (summary + findings) + scoped context.
                        let Some(report) = self.semantic.last_report() else {
                            return ContinuationDecision::Stop {
                                note: Some(
                                    "Semantic verification lost its verdict before the fixer \
                                     could act; stopping."
                                        .to_string(),
                                ),
                            };
                        };
                        let Some(patch) = ctx.changed_files else {
                            return ContinuationDecision::Stop {
                                note: Some(
                                    "Semantic verification lost the changed-file scope; stopping."
                                        .to_string(),
                                ),
                            };
                        };
                        let fix_request = SemanticFixRequest {
                            session_id: ctx.session_id.to_string(),
                            working_dir: ctx.working_dir.to_path_buf(),
                            changed_files: patch.files.clone(),
                            tree_hash: patch.hash.clone(),
                            diff: bound_semantic_diff(
                                ctx.changed_diff.unwrap_or_default().to_string(),
                            ),
                            task_id: ctx.spec.as_ref().map(|spec| spec.task_id.clone()),
                            spec: ctx.spec.clone(),
                            summary: report.summary.clone(),
                            findings: report.findings.clone(),
                        };
                        match self.semantic.run_fixer(fix_request).await {
                            Ok(summary) => {
                                // Fresh executor applied a fix on disk. Re-run
                                // the deterministic gate on the new state; if
                                // it regressed, stop — never silently accept.
                                let after = self.deterministic.decide(ctx);
                                if after.is_continue() {
                                    return ContinuationDecision::Stop {
                                        note: Some(format!(
                                            "Fresh-executor fix did not pass the deterministic \
                                             gate: {summary}"
                                        )),
                                    };
                                }
                                // Gate green → next round re-reviews semantically.
                            }
                            Err(error) => {
                                return ContinuationDecision::Stop {
                                    note: Some(format!("Fresh-executor fixer failed: {error}")),
                                };
                            }
                        }
                    }
                }
            }
        })
    }

    fn verify_report(&self) -> Option<crate::verify::VerifyReport> {
        self.deterministic.verify_report()
    }

    fn semantic_report(&self) -> Option<SemanticVerifyReport> {
        self.semantic.semantic_report()
    }

    fn will_run_checks(&self, ctx: &TurnEndContext<'_>) -> bool {
        self.deterministic.will_run_checks(ctx)
    }
}

impl ContinuationPolicy for SemanticVerifyPolicy {
    fn decide(&self, _ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        // A synchronous call cannot safely invoke the async runner. The query
        // loop uses `decide_async`; direct callers get a conservative stop.
        ContinuationDecision::Stop {
            note: Some("Semantic verification requires the async policy hook.".to_string()),
        }
    }

    fn decide_async<'a>(
        &'a self,
        ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        Box::pin(async move {
            let request = match self.request_from_context(ctx) {
                Ok(request) => request,
                Err(decision) => return decision,
            };
            let runner = self
                .runner
                .as_ref()
                .expect("request_from_context checked runner presence")
                .clone();
            let raw = match runner(request).await {
                Ok(raw) => raw,
                Err(error) => {
                    return ContinuationDecision::Stop {
                        note: Some(format!(
                            "Semantic verification could not run safely: {error}"
                        )),
                    }
                }
            };
            match parse_semantic_verify_response(&raw) {
                Ok(response) => {
                    if response.verdict == SemanticVerdict::Pass {
                        self.attempts.store(0, std::sync::atomic::Ordering::Relaxed);
                    }
                    self.response_decision(response)
                }
                Err(error) => ContinuationDecision::Stop {
                    note: Some(format!("Semantic verification stopped: {error}")),
                },
            }
        })
    }

    fn semantic_report(&self) -> Option<SemanticVerifyReport> {
        self.last_report.lock().unwrap().take()
    }
}

/// A policy the runner consults at the end of each completed `end_turn` turn.
///
/// Implementations must be cheap and side-effect-aware: `decide` is called at
/// most once per turn, from the async loop, but must never hold a lock across
/// an `.await` (it is fully synchronous by design).
pub trait ContinuationPolicy: Send + Sync {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision;

    /// Async decision hook for policies that need to await external work, such
    /// as a future read-only semantic verifier. The default implementation
    /// preserves the synchronous policy path exactly, so existing policies do
    /// not need async state or a second implementation yet.
    fn decide_async<'a>(
        &'a self,
        ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        Box::pin(async move { self.decide(ctx) })
    }

    /// Structured report of the most recent verification round, when this
    /// policy is the execute-and-verify policy and a round actually ran.
    /// Default: `None` — only `VerifyPolicy` overrides this. The query loop
    /// forwards the report as `QueryEvent::Verify` so the TUI can render the
    /// boxed per-check indicator.
    fn verify_report(&self) -> Option<crate::verify::VerifyReport> {
        None
    }

    /// Structured result of the most recent semantic verifier round.
    fn semantic_report(&self) -> Option<SemanticVerifyReport> {
        None
    }

    /// Whether the end-of-turn checks will actually spawn. Default: `false`
    /// — only the execute-and-verify policy overrides this. The query loop
    /// calls it BEFORE the potentially slow checks run so the TUI can show a
    /// `verifying…` indicator instead of a silent wait.
    fn will_run_checks(&self, _ctx: &TurnEndContext<'_>) -> bool {
        false
    }

    /// Path to a spec generated by the turn that just ended, when this
    /// policy stopped the loop specifically so the spec can be reviewed.
    /// Default: `None` — only the spec-mode policy overrides this. The query
    /// loop forwards the path as `QueryEvent::SpecForReview` so the TUI can
    /// auto-open the Accept/Edit/Reject dialog for the generated spec (§10.2).
    fn spec_for_review(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// Default policy: always stop after the turn completes.
///
/// This is the historical, non-goal behaviour — a normal turn that ends with
/// `end_turn` returns immediately instead of continuing.
#[derive(Debug, Clone, Copy, Default)]
pub struct StopPolicy;

impl ContinuationPolicy for StopPolicy {
    fn decide(&self, _ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        ContinuationDecision::Stop { note: None }
    }
}

/// Goal-driven continuation policy (the `/goal` feature).
///
/// Reuses the existing `goal_loop` guards verbatim — the runaway turn cap, the
/// soft token budget, and the per-turn continuation message. While the session
/// has an active goal and its guards allow, the loop continues with the goal
/// continuation message injected as the next user turn; otherwise it stops and
/// surfaces the same paused / budget-limited / runaway note as before.
///
/// This policy relocates only WHERE the decision is made (in-loop, per turn),
/// not the guards themselves: it delegates to
/// [`crate::goal_loop::check_and_continue_goal`], which opens the default goal
/// store and applies the identical logic the CLI post-loop path used to run.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoalPolicy;

impl ContinuationPolicy for GoalPolicy {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        use crate::goal_loop::GoalContinuation;
        match crate::goal_loop::check_and_continue_goal(
            ctx.session_id,
            ctx.total_tokens_used,
            ctx.turn_elapsed_secs,
            ctx.turn_output_tokens,
            ctx.turn_made_writes,
        ) {
            GoalContinuation::Continue { message } => ContinuationDecision::Continue { message },
            // Paused / budget / runaway / complete: stop, surfacing the same
            // user-facing note the CLI used to print.
            GoalContinuation::Stop { reason } => ContinuationDecision::Stop {
                note: reason.user_message(),
            },
            // No goal set for this session: behave exactly like `StopPolicy`.
            GoalContinuation::NoGoal => ContinuationDecision::Stop { note: None },
        }
    }
}

/// Spec-driven development continuation policy (audit spec Phase 4, §10).
///
/// After a turn that wrote files, checks whether a structured spec was
/// produced (`specs/<title>.json` in the working dir). If one exists, the
/// loop stops and surfaces the spec for the user to review (Accept / Edit /
/// Reject — the TUI review dialog or `/spec-review <file>`); the agent must
/// not implement against an unreviewed spec (§10.2). The chosen spec's path
/// is stashed so the query loop can auto-open the review dialog via
/// [`ContinuationPolicy::spec_for_review`]; the stash is cleared at the
/// start of every `decide`, so it only ever holds the current turn's spec.
/// If the turn produced no spec, behaves like `StopPolicy`.
#[derive(Debug, Default)]
pub struct SpecModePolicy {
    last_spec_path: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl SpecModePolicy {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ContinuationPolicy for SpecModePolicy {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        // Reset first so `spec_for_review` never returns a stale path from a
        // previous turn once this decision completes.
        *self.last_spec_path.lock().unwrap() = None;
        if ctx.turn_made_writes {
            if let Some((path, spec)) = clawde_core::spec::Spec::latest_in(ctx.working_dir) {
                *self.last_spec_path.lock().unwrap() = Some(path.clone());
                return ContinuationDecision::Stop {
                    note: Some(format!(
                        "Spec generated: \"{}\" ({}). Review it before implementing — \
                         run /spec-review {}",
                        spec.title,
                        path.display(),
                        path.display()
                    )),
                };
            }
        }
        ContinuationDecision::Stop { note: None }
    }

    fn spec_for_review(&self) -> Option<std::path::PathBuf> {
        self.last_spec_path.lock().unwrap().clone()
    }
}

/// Selects which continuation policy `run_query_loop` uses for a run.
///
/// Stored on `QueryConfig` so callers opt in per invocation. Subagents,
/// headless runs, and every non-goal interactive turn use `Default`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ContinuationMode {
    /// Stop after the turn completes (default, non-goal behaviour).
    #[default]
    Default,
    /// Goal-driven autonomous continuation (the `/goal` feature).
    Goal,
    /// Execute-and-verify continuation (audit spec Phase 1): after a turn
    /// that wrote files, run the project's tests/lints and feed failures back
    /// to the model for auto-fix, up to `max_retries`.
    Verify(clawde_core::config::VerifyConfig),
    /// Spec-driven development (audit spec Phase 4, §10): after a turn that
    /// produced a spec, stop so the user can review it before implementation.
    SpecMode,
    /// Deterministic verification followed by opt-in read-only semantic
    /// verification. Deterministic checks are authoritative; semantic review
    /// runs only after they genuinely pass.
    SemanticVerify(clawde_core::config::VerifyConfig),
}

impl ContinuationMode {
    /// Build the concrete policy for this mode.
    ///
    /// `working_dir` is the project root verification commands are detected
    /// against and executed in. Semantic mode uses the same deterministic
    /// verification configuration before its read-only semantic review.
    pub fn policy(self, working_dir: &std::path::Path) -> Box<dyn ContinuationPolicy> {
        self.policy_with_runner(working_dir, None)
    }

    /// Build a policy with the optional injected semantic-verifier runner.
    pub fn policy_with_runner(
        self,
        working_dir: &std::path::Path,
        semantic_runner: Option<SemanticVerifyRunner>,
    ) -> Box<dyn ContinuationPolicy> {
        self.policy_with_fixer(working_dir, semantic_runner, None)
    }

    /// Build a policy with both the semantic verifier runner and the
    /// fresh-executor fixer injected (G5). When `fix_runner` is `None`, a
    /// `fixable` verdict degrades to the legacy same-context `Continue`.
    pub fn policy_with_fixer(
        self,
        working_dir: &std::path::Path,
        semantic_runner: Option<SemanticVerifyRunner>,
        fix_runner: Option<SemanticFixRunner>,
    ) -> Box<dyn ContinuationPolicy> {
        match self {
            ContinuationMode::Default => Box::new(StopPolicy),
            ContinuationMode::Goal => Box::new(GoalPolicy),
            ContinuationMode::Verify(config) => Box::new(crate::verify::VerifyPolicy::new(
                config,
                working_dir.to_path_buf(),
            )),
            ContinuationMode::SpecMode => Box::new(SpecModePolicy::new()),
            ContinuationMode::SemanticVerify(verify_config) => {
                Box::new(SemanticAfterVerifyPolicy::new(
                    verify_config,
                    working_dir,
                    semantic_runner,
                    fix_runner,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TurnEndContext<'static> {
        TurnEndContext {
            session_id: "sess",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
            working_dir: std::path::Path::new("."),
            turn_made_writes: true,
            turn_output_tokens: 0,
            changed_files: None,
            changed_diff: None,
            spec: None,
        }
    }

    #[test]
    fn stop_policy_always_stops() {
        let decision = StopPolicy.decide(&ctx());
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => assert!(note.is_none()),
            _ => panic!("StopPolicy must stop with no note"),
        }
    }

    #[test]
    fn default_mode_resolves_to_stop() {
        let policy = ContinuationMode::default().policy(std::path::Path::new("."));
        assert!(!policy.decide(&ctx()).is_continue());
    }

    #[tokio::test]
    async fn async_hook_defaults_to_sync_decision() {
        let policy = StopPolicy;
        let decision = policy.decide_async(&ctx()).await;
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => assert!(note.is_none()),
            ContinuationDecision::Continue { .. } => panic!("StopPolicy must stop"),
        }
    }

    #[test]
    fn matching_spec_requires_task_and_session_identity() {
        let dir = std::env::temp_dir().join(format!("clawde-matching-spec-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).expect("create specs dir");
        let spec = clawde_core::spec::Spec {
            task_id: "task-1".to_string(),
            session_id: Some("session-a".to_string()),
            title: "Scoped spec".to_string(),
            ..Default::default()
        };
        spec.write_to(&dir.join("specs/task.json"))
            .expect("write spec");

        assert!(matching_spec(&dir, "task-1", "session-a").is_some());
        assert!(matching_spec(&dir, "task-1", "session-b").is_none());
        assert!(matching_spec(&dir, "task-2", "session-a").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turn_context_can_carry_verifier_inputs() {
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![std::path::PathBuf::from("src/lib.rs")],
        };
        let spec = clawde_core::spec::Spec {
            title: "Example".to_string(),
            ..Default::default()
        };
        let context = TurnEndContext {
            changed_files: Some(&patch),
            changed_diff: None,
            spec: Some(spec),
            ..ctx()
        };
        assert_eq!(context.changed_files.expect("changed files").files.len(), 1);
        assert_eq!(context.spec.expect("spec").title, "Example");
    }

    fn cargo_available() -> bool {
        std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn write_cargo_semantic_fixture(dir: &std::path::Path, body: &str) {
        std::fs::create_dir_all(dir.join("src")).expect("fixture src");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"semantic-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("fixture manifest");
        std::fs::write(dir.join("src/lib.rs"), body).expect("fixture source");
    }

    #[tokio::test]
    async fn semantic_mode_keeps_deterministic_gate_authoritative() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            called_clone.store(true, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"should not run"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            enabled: true,
            auto_test: false,
            auto_lint: false,
            ..Default::default()
        };
        let policy = SemanticAfterVerifyPolicy::new(
            verify_config,
            std::path::Path::new("."),
            Some(runner),
            None,
        );
        let decision = policy.decide_async(&ctx()).await;
        assert!(!decision.is_continue());
        assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn semantic_mode_suppresses_runner_when_deterministic_checks_fail() {
        if !cargo_available() {
            return;
        }
        let project = tempfile::tempdir().expect("temporary project");
        write_cargo_semantic_fixture(project.path(), "#[test]\nfn fails() { assert!(false); }\n");
        let changed = project.path().join("src/lib.rs");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            called_clone.store(true, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"unexpected"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            auto_lint: false,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+assert!(false);"),
            ..ctx()
        };
        let decision = policy.decide(&context);
        assert!(matches!(decision, ContinuationDecision::Continue { .. }));
        assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn semantic_mode_suppresses_verifier_and_fixer_when_gate_fails() {
        if !cargo_available() {
            return;
        }
        let project = tempfile::tempdir().expect("temporary project");
        write_cargo_semantic_fixture(project.path(), "#[test]\nfn fails() { assert!(false); }\n");
        let changed = project.path().join("src/lib.rs");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let verifier_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let verifier_calls_clone = verifier_calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            verifier_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async {
                Ok(r#"{\"verdict\":\"pass\",\"summary\":\"must not run\"}"#.to_string())
            })
        });
        let fixer_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fixer_calls_clone = fixer_calls.clone();
        let fixer: SemanticFixRunner = std::sync::Arc::new(move |_| {
            fixer_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok("must not run".to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            auto_lint: false,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy = SemanticAfterVerifyPolicy::new(
            verify_config,
            project.path(),
            Some(runner),
            Some(fixer),
        );
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+assert!(false);"),
            ..ctx()
        };

        let decision = policy.decide_async(&context).await;
        assert!(matches!(decision, ContinuationDecision::Continue { .. }));
        assert_eq!(
            verifier_calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a failed deterministic gate must suppress semantic review"
        );
        assert_eq!(
            fixer_calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a failed deterministic gate must suppress the fresh fixer too"
        );
    }

    #[tokio::test]
    async fn semantic_mode_runs_after_deterministic_pass() {
        if !cargo_available() {
            return;
        }
        let project = tempfile::tempdir().expect("temporary project");
        write_cargo_semantic_fixture(
            project.path(),
            "#[cfg(test)]\nmod tests { #[test] fn passes() {} }\n",
        );
        let changed = project.path().join("src/lib.rs");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            called_clone.store(true, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async {
                Ok(r#"{"verdict":"replan","summary":"criteria need review"}"#.to_string())
            })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            auto_lint: false,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+mod tests;"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(matches!(
            decision,
            ContinuationDecision::Stop { note: Some(note) } if note.contains("replan")
        ));
        assert!(called.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            policy.semantic_report().expect("semantic report").verdict,
            SemanticVerdict::Replan
        );
    }

    #[test]
    fn verify_mode_resolves_to_verify_policy() {
        let cfg = clawde_core::config::VerifyConfig {
            enabled: false,
            ..Default::default()
        };
        let policy = ContinuationMode::Verify(cfg).policy(std::path::Path::new("."));
        // A disabled verify config stops silently, even on a writing turn.
        let decision = policy.decide(&ctx());
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => assert!(note.is_none()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn spec_mode_stops_with_review_note_when_spec_written() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-mode-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).expect("create specs dir");
        let spec = clawde_core::spec::Spec {
            title: "Rate-Limiting Middleware".to_string(),
            ..Default::default()
        };
        spec.write_to(&dir.join("specs/rate-limiting.json"))
            .expect("write spec");

        let policy = SpecModePolicy::new();
        let decision = policy.decide(&TurnEndContext {
            session_id: "sess",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
            working_dir: &dir,
            turn_made_writes: true,
            turn_output_tokens: 0,
            changed_files: None,
            changed_diff: None,
            spec: None,
        });
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => {
                let note = note.expect("review note present");
                assert!(note.contains("Spec generated"));
                assert!(note.contains("Rate-Limiting Middleware"));
                assert!(note.contains("/spec-review"));
            }
            _ => unreachable!(),
        }
        // The generated spec's path is surfaced for auto-open.
        let review_path = policy
            .spec_for_review()
            .expect("spec path stashed for review");
        assert!(review_path.ends_with("rate-limiting.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spec_for_review_clears_after_non_spec_turn() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-mode-clr-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).expect("create specs dir");
        let policy = SpecModePolicy::new();
        let spec = clawde_core::spec::Spec {
            title: "Rate-Limiting Middleware".to_string(),
            ..Default::default()
        };
        spec.write_to(&dir.join("specs/rate-limiting.json"))
            .expect("write spec");

        // First turn produces the spec → path stashed.
        policy.decide(&TurnEndContext {
            session_id: "sess",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
            working_dir: &dir,
            turn_made_writes: true,
            turn_output_tokens: 0,
            changed_files: None,
            changed_diff: None,
            spec: None,
        });
        assert!(policy.spec_for_review().is_some());
        // A later non-spec turn must not keep reporting the old path.
        policy.decide(&TurnEndContext {
            session_id: "sess",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
            working_dir: &dir,
            turn_made_writes: false,
            turn_output_tokens: 0,
            changed_files: None,
            changed_diff: None,
            spec: None,
        });
        assert!(policy.spec_for_review().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_diff_is_bounded() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("file.txt"), "changed").expect("fixture");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![project.path().join("file.txt")],
        };
        let diff = "x".repeat(SEMANTIC_VERIFY_MAX_DIFF_CHARS + 100);
        let policy = SemanticVerifyPolicy::new(
            Some(std::sync::Arc::new(|_| {
                Box::pin(async { Ok(r#"{"verdict":"pass","summary":"ok"}"#.to_string()) })
            })),
            None,
        );
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some(&diff),
            ..ctx()
        };
        let request = policy
            .request_from_context(&context)
            .expect("scoped request");
        assert!(request.diff.len() < diff.len());
        assert!(request.diff.ends_with("[diff truncated]"));
    }

    #[cfg(unix)]
    #[test]
    fn semantic_path_scope_rejects_parent_and_symlink_escape() {
        let root =
            std::env::temp_dir().join(format!("clawde-semantic-scope-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).expect("create scope root");
        std::fs::write(root.join("src/lib.rs"), "ok").expect("write fixture");
        assert!(path_is_within_working_dir(&root.join("src/lib.rs"), &root));
        let dangling = root.join("src/dangling");
        std::os::unix::fs::symlink(root.join("outside"), &dangling).expect("symlink fixture");
        assert!(!path_is_within_working_dir(&dangling, &root));
        assert!(!path_is_within_working_dir(
            &root.join("src/../secret"),
            &root
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn semantic_response_parser_is_strict() {
        let parsed = parse_semantic_verify_response(
            r#"{"verdict":"fail","summary":"missing case","findings":["Add a test"]}"#,
        )
        .expect("valid semantic response");
        assert_eq!(parsed.verdict, SemanticVerdict::Fixable);
        assert_eq!(parsed.findings, vec!["Add a test"]);

        // The initial scaffolding emitted `fail`; retain a migration alias.
        let legacy = parse_semantic_verify_response(r#"{"verdict":"fail","summary":"legacy"}"#)
            .expect("legacy verdict alias");
        assert_eq!(legacy.verdict, SemanticVerdict::Fixable);

        assert!(parse_semantic_verify_response("not json").is_err());
        assert!(parse_semantic_verify_response(
            r#"{"verdict":"pass","summary":"ok","extra":true}"#
        )
        .is_err());
        assert!(parse_semantic_verify_response(
            "```json\n{\"verdict\":\"pass\",\"summary\":\"ok\"}\n```"
        )
        .is_err());
        assert!(parse_semantic_verify_response(r#"{"verdict":"pass","summary":""}"#).is_err());
        assert!(parse_semantic_verify_response(&format!(
            r#"{{"verdict":"pass","summary":"{}"}}"#,
            "x".repeat(SEMANTIC_VERIFY_MAX_SUMMARY_CHARS + 1)
        ))
        .is_err());
        assert!(parse_semantic_verify_response(
            r#"{"verdict":"replan","summary":"new approach needed"}"#
        )
        .is_ok());
    }

    #[tokio::test]
    async fn semantic_policy_scopes_request_and_decides_without_live_model() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::create_dir_all(project.path().join("src")).expect("project src");
        std::fs::write(project.path().join("src/lib.rs"), "fn changed() {}").expect("fixture");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree-1".to_string(),
            files: vec![project.path().join("src/lib.rs")],
        };
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_clone = captured.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |request| {
            *captured_clone.lock().unwrap() = Some(request);
            Box::pin(async {
                Ok(
                    r#"{"verdict":"fixable","summary":"behavior gap","findings":["Add coverage"]}"#
                        .to_string(),
                )
            })
        });
        let policy = SemanticVerifyPolicy::new(Some(runner), None);
        let context = TurnEndContext {
            session_id: "session-1",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
            working_dir: project.path(),
            turn_made_writes: true,
            turn_output_tokens: 0,
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+fn changed() {}\n"),
            spec: None,
        };
        let decision = policy.decide_async(&context).await;
        assert!(decision.is_continue());
        let request = captured
            .lock()
            .unwrap()
            .take()
            .expect("runner captured request");
        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.tree_hash, "tree-1");
        assert_eq!(request.changed_files, patch.files);
        assert!(request.diff.contains("fn changed"));
        assert_eq!(request.read_only_tools, semantic_read_only_tool_names());
        assert!(!request.read_only_tools.iter().any(|name| name == "Write"));
    }

    #[tokio::test]
    async fn semantic_policy_rejects_out_of_scope_patch() {
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![std::path::PathBuf::from("/outside/secret.txt")],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"ok"}"#.to_string()) })
        });
        let policy = SemanticVerifyPolicy::new(Some(runner), None);
        let context = TurnEndContext {
            working_dir: std::path::Path::new("/project"),
            changed_files: Some(&patch),
            changed_diff: Some("diff"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(matches!(
            decision,
            ContinuationDecision::Stop { note: Some(_) }
        ));
    }

    #[tokio::test]
    async fn semantic_fixer_routes_fixable_to_fresh_executor_and_reverifies() {
        if !cargo_available() {
            return;
        }
        let project = tempfile::tempdir().expect("temporary project");
        write_cargo_semantic_fixture(
            project.path(),
            "#[cfg(test)]\nmod tests { #[test] fn passes() {} }\n",
        );
        let changed = project.path().join("src/lib.rs");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };

        // Round 1: fixable. Round 2 (after the fixer): pass.
        let verifier_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let verifier_calls_clone = verifier_calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            let calls = verifier_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async move {
                if calls == 0 {
                    Ok(
                        r#"{"verdict":"fixable","summary":"missing edge case","findings":["Add coverage"]}"#
                            .to_string(),
                    )
                } else {
                    Ok(r#"{"verdict":"pass","summary":"fixed"}"#.to_string())
                }
            })
        });
        let fixer_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fixer_calls_clone = fixer_calls.clone();
        let captured_fix_request = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_fix_clone = captured_fix_request.clone();
        let fixer: SemanticFixRunner = std::sync::Arc::new(move |request| {
            fixer_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *captured_fix_clone.lock().unwrap() = Some(request);
            Box::pin(async { Ok("applied the edge-case fix".to_string()) })
        });

        let verify_config = clawde_core::config::VerifyConfig {
            auto_lint: false,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy = SemanticAfterVerifyPolicy::new(
            verify_config,
            project.path(),
            Some(runner),
            Some(fixer),
        );
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+fn edge() {}\n"),
            ..ctx()
        };

        let decision = policy.decide_async(&context).await;
        assert!(matches!(
            decision,
            ContinuationDecision::Stop { note: Some(note) } if note.contains("passed")
        ));
        assert_eq!(
            fixer_calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "fresh executor must run exactly once before the re-review passes"
        );
        // The fix request carried the verdict context, not a same-context push.
        let fix_request = captured_fix_request
            .lock()
            .unwrap()
            .take()
            .expect("fixer captured request");
        assert!(fix_request.summary.contains("missing edge case"));
        assert_eq!(fix_request.findings, vec!["Add coverage"]);
        assert_eq!(fix_request.working_dir, project.path());
        assert!(fix_request.diff.contains("fn edge"));
    }

    #[tokio::test]
    async fn semantic_fixer_without_fixer_degrades_to_same_context_continue() {
        if !cargo_available() {
            return;
        }
        let project = tempfile::tempdir().expect("temporary project");
        write_cargo_semantic_fixture(
            project.path(),
            "#[cfg(test)]\nmod tests { #[test] fn passes() {} }\n",
        );
        let changed = project.path().join("src/lib.rs");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async {
                Ok(r#"{"verdict":"fixable","summary":"needs work","findings":["x"]}"#.to_string())
            })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            auto_lint: false,
            timeout_secs: 30,
            ..Default::default()
        };
        // Deterministic gate passes (fixture tests are green) → the semantic
        // tier runs. No fixer configured → the legacy same-context Continue is
        // the documented degraded mode.
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+fn changed() {}\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(decision.is_continue());
    }

    #[tokio::test]
    async fn semantic_fixer_error_stops_instead_of_same_context_retry() {
        if !cargo_available() {
            return;
        }
        let project = tempfile::tempdir().expect("temporary project");
        write_cargo_semantic_fixture(
            project.path(),
            "#[cfg(test)]\nmod tests { #[test] fn passes() {} }\n",
        );
        let changed = project.path().join("src/lib.rs");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async {
                Ok(r#"{"verdict":"fixable","summary":"broken","findings":["fix it"]}"#.to_string())
            })
        });
        let fixer: SemanticFixRunner = std::sync::Arc::new(|_| {
            Box::pin(async { Err("fixer could not apply the patch".to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            auto_lint: false,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy = SemanticAfterVerifyPolicy::new(
            verify_config,
            project.path(),
            Some(runner),
            Some(fixer),
        );
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+fn changed() {}\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(matches!(
            decision,
            ContinuationDecision::Stop { note: Some(note) } if note.contains("fixer failed")
        ));
    }

    #[tokio::test]
    async fn semantic_policy_stops_on_missing_runner_or_read_only_turn() {
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![std::path::PathBuf::from("src/lib.rs")],
        };
        let policy = SemanticVerifyPolicy::new(None, None);
        let context = TurnEndContext {
            turn_made_writes: true,
            changed_files: Some(&patch),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert!(matches!(
            decision,
            ContinuationDecision::Stop { note: Some(_) }
        ));

        let mut read_only = context;
        read_only.turn_made_writes = false;
        let decision = policy.decide_async(&read_only).await;
        assert!(!decision.is_continue());
        assert!(matches!(
            decision,
            ContinuationDecision::Stop { note: None }
        ));
    }

    #[test]
    fn spec_mode_stops_silently_without_spec() {
        let dir =
            std::env::temp_dir().join(format!("clawde-spec-mode-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let policy = SpecModePolicy::new();
        let decision = policy.decide(&TurnEndContext {
            session_id: "sess",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
            working_dir: &dir,
            turn_made_writes: true,
            turn_output_tokens: 0,
            changed_files: None,
            changed_diff: None,
            spec: None,
        });
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => assert!(note.is_none()),
            _ => unreachable!(),
        }
        // No spec was produced, so nothing is surfaced for review.
        assert!(policy.spec_for_review().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
