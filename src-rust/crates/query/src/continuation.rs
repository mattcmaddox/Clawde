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
///
/// Deserialization now flows through the tolerant envelopes; this attribute is
/// retained as the public contract (only verdict/summary/findings may exist),
/// and the type is constructed exclusively via `From<SemanticVerifyEnvelope>`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticVerifyResponse {
    pub verdict: SemanticVerdict,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<String>,
}

/// Tolerant wire envelope accepted from a semantic verifier runner.
///
/// Some free models wrap their verdict in a Claude-style `{"message": …}`
/// assistant-message field, add a redundant `repeat` hint that the verdict
/// already expresses, or sprinkle arbitrary noise fields (e.g. a truncated
/// `f` or a `confidence` score) next to a perfectly valid verdict. The
/// envelope therefore IGNORES unknown fields while `verdict` and `summary`
/// remain strictly required and bounded — authority comes from the required
/// verdict enum, not from the absence of extra fields, so an ambiguous
/// response (no verdict, an invalid verdict value, an empty or over-long
/// summary) can never authorize continuation.
#[derive(Debug, serde::Deserialize)]
struct SemanticVerifyEnvelope {
    verdict: SemanticVerdict,
    summary: String,
    #[serde(default)]
    findings: Vec<String>,
    /// Tolerated and ignored; the field is never read after deserialization.
    #[serde(default)]
    #[allow(dead_code)]
    message: Option<serde_json::Value>,
    /// Tolerated and ignored; some models add a redundant `repeat` hint that
    /// the `verdict` field already expresses (`fixable`/`replan`). Never read.
    #[serde(default)]
    #[allow(dead_code)]
    repeat: Option<serde_json::Value>,
}

/// Recovery shape for a Claude-style `{"message": {…}}` envelope that nests
/// the verdict *inside* `message` instead of placing it at the top level.
///
/// The inner value is the tolerant envelope, so a nested verdict is held to
/// the exact same contract (required `verdict` + `summary`, bounded findings,
/// unknown fields ignored) as a top-level one.
#[derive(Debug, serde::Deserialize)]
struct SemanticVerifyNestedEnvelope {
    #[serde(default)]
    message: Option<SemanticVerifyEnvelope>,
}

impl From<SemanticVerifyEnvelope> for SemanticVerifyResponse {
    fn from(envelope: SemanticVerifyEnvelope) -> Self {
        Self {
            verdict: envelope.verdict,
            summary: envelope.summary,
            findings: envelope.findings,
        }
    }
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
    /// Reask hint for a retry attempt: the classified parse error from the
    /// previous response, fed back into the prompt so the model can correct
    /// its structured output. Parser-generated only, never raw model output;
    /// None on the first attempt.
    pub retry_hint: Option<String>,
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
    /// Bounded feedback from a previous fresh-fixer attempt. This is parser or
    /// application feedback only, never raw model output.
    pub feedback: Option<String>,
}

/// Result type returned by an injected fresh-executor fix runner.
pub type SemanticFixRunnerResult = Result<String, String>;

/// Async runner seam for a fresh-executor fixer (writer-verifier gap G5).
///
/// Unlike the verifier, this runner is expected to author a bounded patch in
/// a fresh executor session — never the same in-context trace the loop is
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
/// Maximum characters of a reask hint fed back into the retry prompt. Parser
/// errors are short in practice; this bounds the prompt even if a future error
/// embeds a larger snippet of the rejected response.
pub const SEMANTIC_VERIFY_MAX_RETRY_HINT_CHARS: usize = 300;
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

/// Convert an untrusted fixer error into a small, stable diagnostic before it
/// can be shown to another fresh executor. Raw provider/tool/model text must
/// never become retry prompt context because it may contain secrets or prompt
/// injection content.
fn classify_fix_feedback(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("scope")
        || lower.contains("working director")
        || lower.contains("changed file")
        || lower.contains("path")
    {
        "scope_rejected"
    } else if lower.contains("apply")
        || lower.contains("dry-run")
        || lower.contains("dry run")
        || lower.contains("without changing")
    {
        "patch_apply_failure"
    } else if lower.contains("patch")
        || lower.contains("hunk")
        || lower.contains("unified diff")
        || lower.contains("json")
        || lower.contains("header")
        || lower.contains("fenced")
    {
        "strict_parse_failure"
    } else {
        "runner_error"
    }
}

fn bounded_fix_feedback(error: &str) -> String {
    format!(
        "semantic fixer rejected the attempt: {}",
        classify_fix_feedback(error)
    )
}

/// Convert an untrusted semantic-runner failure into a stable note before it
/// can reach the parent loop or a subsequent model prompt. Provider errors may
/// contain credentials, URLs, or model-generated prompt injection content.
fn classify_semantic_runner_error(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("all free-mode upstreams exhausted")
        || lower.contains("free-mode upstreams exhausted")
    {
        "provider_chain_exhausted"
    } else if lower.contains("network_boundary_blocked")
        || (lower.contains("offline mode") && lower.contains("network-capable tools are disabled"))
    {
        "network_boundary_blocked"
    } else if lower.contains("provider_unavailable")
        || lower.contains("no api key for provider")
        || lower.contains("no credentials found")
        || lower.contains("provider unavailable")
        || lower.contains("provider not found")
    {
        "provider_unavailable"
    } else if lower.contains("rate limit") || lower.contains("rate-limit") || lower.contains("429")
    {
        "rate_limited"
    } else if lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authentication")
        || lower.contains("invalid key")
    {
        "authentication_error"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else if lower.contains("model not found") || lower.contains("modelnotfound") {
        "model_not_found"
    } else if lower.contains("invalid request") || lower.contains("invalidrequest") {
        "invalid_request"
    } else {
        "runner_error"
    }
}

/// Extract the first balanced JSON object from a string, respecting string
/// literals so braces inside quoted values do not terminate the scan early.
fn extract_json_object(input: &str) -> Option<&str> {
    let start = input.find('{')?;
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                    if depth == 0 {
                        return Some(&input[start..=offset]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Parse one of the two accepted verdict shapes from a raw JSON fragment: a
/// top-level verdict (with an optional ignored `message` field) or a verdict
/// nested inside a single `message` object. Both shapes reject unknown fields
/// and require a valid `verdict` + `summary`.
fn parse_verdict_shape(raw: &str) -> Result<SemanticVerifyResponse, String> {
    match serde_json::from_str::<SemanticVerifyEnvelope>(raw) {
        Ok(envelope) => Ok(SemanticVerifyResponse::from(envelope)),
        Err(top_level) => match serde_json::from_str::<SemanticVerifyNestedEnvelope>(raw) {
            // A nested `message` verdict is recovered; the tolerant inner
            // envelope is converted through the same contract as a top-level
            // one. When the inner is absent, the more accurate top-level error
            // is surfaced unchanged.
            Ok(nested) => nested
                .message
                .map(SemanticVerifyResponse::from)
                .ok_or_else(|| top_level.to_string()),
            Err(_) => Err(top_level.to_string()),
        },
    }
}

/// Parse a semantic verifier response. Responses must contain a bounded JSON
/// object with `verdict`, `summary`, and optional `findings`.
///
/// Free-tier models frequently wrap their JSON in markdown fences or prose,
/// return empty output, add a Claude-style `{"message": …}` field, or sprinkle
/// arbitrary noise fields next to a valid verdict. To avoid silently skipping
/// verification on those models, the parser first tries a strict whole-string
/// parse and then falls back to extracting the first balanced JSON object
/// (skipping fences and prose). The extracted object is parsed through a
/// tolerant envelope that ignores unknown fields (but still requires a valid
/// `verdict` enum value, a required non-empty bounded `summary`, and bounded
/// `findings`), then through a nested recovery that unwraps a verdict placed
/// *inside* a single `message` object. Because authority comes from the
/// required verdict enum, an ambiguous response can never authorize
/// continuation: anything that is not a clearly valid verdict object is
/// rejected.
pub fn parse_semantic_verify_response(response: &str) -> Result<SemanticVerifyResponse, String> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Err("semantic verifier returned an empty response".to_string());
    }
    if trimmed.len() > SEMANTIC_VERIFY_MAX_RESPONSE_BYTES {
        return Err(format!(
            "semantic verifier response exceeds the {}-byte limit",
            SEMANTIC_VERIFY_MAX_RESPONSE_BYTES
        ));
    }

    // Fast path: a bare JSON object with no surrounding prose or fences.
    let parsed = match parse_verdict_shape(trimmed) {
        Ok(response) => response,
        Err(fast_error) => {
            // Tolerant path: skip fences/prose and re-parse the first balanced
            // object. Any failure here remains fail-closed.
            let candidate = extract_json_object(trimmed).ok_or_else(|| {
                format!(
                    "semantic verifier returned malformed JSON (no JSON object found): {fast_error}"
                )
            })?;
            parse_verdict_shape(candidate)
                .map_err(|error| format!("semantic verifier returned malformed JSON: {error}"))?
        }
    };
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

/// True when a verifier response expresses no verdict at all — empty,
/// truncated, or prose-only (no complete JSON object), or a complete object
/// that omits the required `verdict` key. In every such shape the model never
/// produced a usable verdict, so exactly one retry is safe: the retry runs
/// through the same strict parser and can never authorize a verdict the first
/// attempt would not have. Structured failures where a verdict WAS expressed
/// (unknown verdict value, missing summary, unknown field) and over-limit
/// responses are not retried.
fn response_expresses_no_verdict(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.len() > SEMANTIC_VERIFY_MAX_RESPONSE_BYTES {
        return false;
    }
    let Some(candidate) = extract_json_object(trimmed) else {
        return true;
    };
    !serde_json::from_str::<serde_json::Value>(candidate)
        .map(|value| value.get("verdict").is_some())
        .unwrap_or(false)
}

/// Run the injected verifier once, retrying a single time when the response
/// expresses no verdict (empty/truncated/prose-only, or a complete object
/// missing the required `verdict` key) before declining. The retry runs
/// through the same strict parser, so the fail-closed contract is unchanged: a
/// retry can never authorize a verdict the first attempt would not have.
async fn run_verifier_with_no_verdict_retry(
    runner: SemanticVerifyRunner,
    request: SemanticVerifyRequest,
) -> Result<SemanticVerifyResponse, ContinuationDecision> {
    let raw = match runner(request.clone()).await {
        Ok(raw) => raw,
        Err(error) => {
            return Err(ContinuationDecision::Stop {
                note: Some(format!(
                    "Semantic verification could not run safely: {}",
                    classify_semantic_runner_error(&error)
                )),
            });
        }
    };
    match parse_semantic_verify_response(&raw) {
        Ok(response) => Ok(response),
        Err(first_error) if response_expresses_no_verdict(&raw) => {
            // Reask: feed the classified parse error back into the retry so the
            // model can correct its structured output. This is parser feedback
            // only — never raw model output — and the retry still runs through
            // the same strict parser, so fail-closed is unchanged.
            let mut retry_request = request;
            // Bound the hint: parser errors are short today, but a future error
            // could embed a larger snippet of the rejected response. The prompt
            // must stay bounded, so truncate defensively.
            retry_request.retry_hint = Some(
                first_error
                    .chars()
                    .take(SEMANTIC_VERIFY_MAX_RETRY_HINT_CHARS)
                    .collect(),
            );
            let retry_raw = match runner(retry_request).await {
                Ok(raw) => raw,
                Err(error) => {
                    return Err(ContinuationDecision::Stop {
                        note: Some(format!(
                            "Semantic verification could not run safely: {}",
                            classify_semantic_runner_error(&error)
                        )),
                    });
                }
            };
            parse_semantic_verify_response(&retry_raw).map_err(|error| ContinuationDecision::Stop {
                note: Some(format!("Semantic verification stopped: {error}")),
            })
        }
        Err(error) => Err(ContinuationDecision::Stop {
            note: Some(format!("Semantic verification stopped: {error}")),
        }),
    }
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
    max_fix_attempts: u32,
    last_report: std::sync::Mutex<Option<SemanticVerifyReport>>,
}

impl SemanticVerifyPolicy {
    pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

    pub fn new(
        runner: Option<SemanticVerifyRunner>,
        fix_runner: Option<SemanticFixRunner>,
    ) -> Self {
        Self::with_attempt_limits(
            runner,
            fix_runner,
            Self::DEFAULT_MAX_ATTEMPTS,
            Self::DEFAULT_MAX_ATTEMPTS,
        )
    }

    /// Build a policy with independently bounded semantic rounds and fixer retries.
    pub fn with_attempt_limits(
        runner: Option<SemanticVerifyRunner>,
        fix_runner: Option<SemanticFixRunner>,
        max_attempts: u32,
        max_fix_attempts: u32,
    ) -> Self {
        Self {
            runner,
            fix_runner,
            attempts: std::sync::atomic::AtomicU32::new(0),
            max_attempts: max_attempts.clamp(1, clawde_core::config::MAX_SEMANTIC_ATTEMPTS),
            max_fix_attempts: max_fix_attempts.clamp(1, clawde_core::config::MAX_SEMANTIC_ATTEMPTS),
            last_report: std::sync::Mutex::new(None),
        }
    }

    /// Maximum fix-and-reverify rounds before escalation.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Maximum fresh patch-author retries for one fixable verdict.
    pub fn max_fix_attempts(&self) -> u32 {
        self.max_fix_attempts
    }

    /// Reset the fix-and-reverify round budget for a new turn.
    ///
    /// The `attempts` counter bounds rounds within one verification session;
    /// a new turn starts a fresh session, so a prior exhaustion must not
    /// poison the next review.
    pub fn reset_attempts(&self) {
        self.attempts.store(0, std::sync::atomic::Ordering::Relaxed);
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
    /// G5: the fixer runs in a fresh executor session — it must never reuse
    /// the loop's in-context trace.
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
            retry_hint: None,
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
    /// Deferred gate policy: when true, a turn whose deterministic gate found
    /// no test/lint commands still runs the read-only semantic verifier as a
    /// bounded review signal. The verdict cannot authorize acceptance.
    semantic_only_when_no_lowlevel_tests: bool,
    /// When the no-checks review signal fires, route a `fixable` verdict to
    /// the bounded G5 fresh-executor fixer and re-verify. The deterministic
    /// `Stop` remains authoritative (fail-closed).
    semantic_fix_when_no_lowlevel_tests: bool,
    /// Reason the gate-open review signal declined this turn (skip / runner
    /// error / parse failure). Captured so the discarded decision stays
    /// observable as a status event; `None` while no decline is pending.
    semantic_note: std::sync::Mutex<Option<String>>,
}

impl SemanticAfterVerifyPolicy {
    pub fn new(
        verify_config: clawde_core::config::VerifyConfig,
        working_dir: &std::path::Path,
        runner: Option<SemanticVerifyRunner>,
        fix_runner: Option<SemanticFixRunner>,
    ) -> Self {
        let semantic_only_when_no_lowlevel_tests =
            verify_config.semantic_only_when_no_lowlevel_tests;
        let semantic_fix_when_no_lowlevel_tests = verify_config.semantic_fix_when_no_lowlevel_tests;
        Self {
            deterministic: crate::verify::VerifyPolicy::new(
                verify_config.clone(),
                working_dir.to_path_buf(),
            ),
            semantic: SemanticVerifyPolicy::with_attempt_limits(
                runner,
                fix_runner,
                verify_config.semantic_max_attempts,
                verify_config.semantic_fix_max_attempts,
            ),
            semantic_only_when_no_lowlevel_tests,
            semantic_fix_when_no_lowlevel_tests,
            semantic_note: std::sync::Mutex::new(None),
        }
    }

    /// True when a fresh-executor fixer is wired, i.e. the G5 fix loop is
    /// available rather than the legacy same-context `Continue`.
    pub fn has_fixer(&self) -> bool {
        self.semantic.has_fixer()
    }

    /// Run the semantic review + fresh-executor fix loop to a terminal decision.
    ///
    /// Shared by the deterministic-pass path and the actionable no-checks
    /// review-signal path. Each `fixable` verdict spawns a bounded
    /// fresh-executor patch author, then re-runs the deterministic gate and
    /// semantic review on the new state. Only a terminal decision (pass /
    /// replan / escalate / exhausted) is returned; the caller decides whether
    /// that decision can authorize continuation.
    async fn semantic_fix_loop<'a>(&'a self, ctx: &'a TurnEndContext<'a>) -> ContinuationDecision {
        loop {
            let decision = self.semantic.decide_async(ctx).await;
            match decision {
                // pass / replan / escalate / runner-error → terminal.
                ContinuationDecision::Stop { .. } => return decision,
                ContinuationDecision::Continue { message } => {
                    if !self.semantic.has_fixer() {
                        // No fresh-executor fixer configured: fall back to the
                        // legacy same-context Continue (the loop pushes the
                        // fix request into the existing trace).
                        return ContinuationDecision::Continue { message };
                    }
                    // Build the fresh-executor request from the verdict report
                    // (summary + findings) + scoped context. The semantic
                    // policy's own attempt counter bounds the number of
                    // fix-and-reverify rounds; the local counter below
                    // independently bounds retries within this one verdict.
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
                        diff: bound_semantic_diff(ctx.changed_diff.unwrap_or_default().to_string()),
                        task_id: ctx.spec.as_ref().map(|spec| spec.task_id.clone()),
                        spec: ctx.spec.clone(),
                        summary: report.summary.clone(),
                        findings: report.findings.clone(),
                        feedback: None,
                    };
                    let mut feedback = None;
                    let mut fix_attempts = 0;
                    loop {
                        fix_attempts += 1;
                        let mut attempt = fix_request.clone();
                        attempt.feedback = feedback.clone();
                        match self.semantic.run_fixer(attempt).await {
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
                                break;
                            }
                            Err(error) => {
                                feedback = Some(bounded_fix_feedback(&error));
                                if fix_attempts >= self.semantic.max_fix_attempts() {
                                    return ContinuationDecision::Stop {
                                        note: Some(format!(
                                            "Fresh-executor fixer exhausted after {} bounded attempts: {}",
                                            self.semantic.max_fix_attempts(),
                                            bounded_fix_feedback(&error)
                                        )),
                                    };
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn last_semantic_verdict(&self) -> Option<SemanticVerdict> {
        self.semantic.last_report().map(|report| report.verdict)
    }
}

/// Composite autonomous policy: run semantic acceptance first, then let the
/// existing goal guards decide whether another user/model turn is warranted.
///
/// This prevents goal autonomy from bypassing the semantic verifier while
/// keeping the goal store and its runaway/budget/no-progress guards as the
/// authority for continued work.
pub struct GoalSemanticVerifyPolicy {
    semantic: SemanticAfterVerifyPolicy,
    goal: GoalPolicy,
}

impl GoalSemanticVerifyPolicy {
    pub fn new(
        verify_config: clawde_core::config::VerifyConfig,
        working_dir: &std::path::Path,
        runner: Option<SemanticVerifyRunner>,
        fix_runner: Option<SemanticFixRunner>,
    ) -> Self {
        Self {
            semantic: SemanticAfterVerifyPolicy::new(
                verify_config,
                working_dir,
                runner,
                fix_runner,
            ),
            goal: GoalPolicy,
        }
    }
}

impl ContinuationPolicy for GoalSemanticVerifyPolicy {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        self.semantic.decide(ctx)
    }

    fn decide_async<'a>(
        &'a self,
        ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        Box::pin(async move {
            let semantic = self.semantic.decide_async(ctx).await;
            if semantic.is_continue() {
                // This is either a legacy same-context fix request (no fresh
                // fixer configured) or an internal continuation. Preserve it;
                // goal guards must not mask an unresolved semantic defect.
                return semantic;
            }
            match self.semantic.last_semantic_verdict() {
                Some(SemanticVerdict::Pass) => match self.goal.decide(ctx) {
                    // No active goal: preserve the semantic acceptance note
                    // rather than replacing it with a silent default stop.
                    ContinuationDecision::Stop { note: None } => semantic,
                    decision => decision,
                },
                _ => semantic,
            }
        })
    }

    fn verify_report(&self) -> Option<crate::verify::VerifyReport> {
        self.semantic.verify_report()
    }

    fn semantic_report(&self) -> Option<SemanticVerifyReport> {
        self.semantic.semantic_report()
    }

    fn semantic_note(&self) -> Option<String> {
        self.semantic.semantic_note()
    }

    fn will_run_checks(&self, ctx: &TurnEndContext<'_>) -> bool {
        self.semantic.will_run_checks(ctx)
    }

    fn review_only_async<'a>(
        &'a self,
        ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        self.semantic.review_only_async(ctx)
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
            // Clear any previous round before deterministic preflight. A prior
            // semantic pass must never authorize the goal after this round
            // exits early due to a failed or unavailable deterministic gate.
            self.semantic.last_report.lock().unwrap().take();
            *self.semantic_note.lock().unwrap() = None;
            self.semantic.reset_attempts();
            let deterministic = self.deterministic.decide(ctx);
            if deterministic.is_continue() {
                return deterministic;
            }
            let report = self.deterministic.verify_report();
            let deterministic_verdict = report.as_ref().map(|report| report.verdict);
            let no_checks_detected = matches!(
                deterministic_verdict,
                Some(crate::verify::VerifyVerdict::Escalate)
            ) && report
                .as_ref()
                .is_some_and(|report| report.results.is_empty() && !report.unavailable);
            if !matches!(
                deterministic_verdict,
                Some(crate::verify::VerifyVerdict::Pass)
            ) {
                // Deferred gate policy (`semantic_only_when_no_lowlevel_tests`):
                // when no deterministic test/lint commands were detected, the
                // read-only semantic verifier may still run as a bounded
                // review signal. Its verdict cannot authorize acceptance or
                // automatic completion — a model verdict is not an executable
                // grader — so the deterministic Stop is preserved and the
                // report is surfaced as evidence only (never a synthetic
                // pass). Other Escalate cases (checks failed, commands
                // missing, sandbox unavailable) stay fail-closed.
                if self.semantic_only_when_no_lowlevel_tests && no_checks_detected {
                    if self.semantic_fix_when_no_lowlevel_tests && self.semantic.has_fixer() {
                        // Actionable review signal: run the review + fresh-executor
                        // fix loop so a `fixable` verdict can change the on-disk
                        // result. Acceptance stays deterministic-only: the loop's
                        // terminal decision is discarded and the deterministic
                        // `Stop` below stays authoritative. A decline (skip /
                        // runner error / parse failure) is still captured.
                        let fixed = self.semantic_fix_loop(ctx).await;
                        if let ContinuationDecision::Stop { note: Some(note) } = fixed {
                            if self.semantic.last_report().is_none() {
                                *self.semantic_note.lock().unwrap() = Some(note);
                            }
                        }
                    } else {
                        // Read-only evidence: a single review whose verdict is
                        // surfaced as a report but never authorizes acceptance.
                        // A decline must stay observable instead of being
                        // silently discarded; a real verdict already surfaces
                        // through `semantic_report`.
                        let decision = self.semantic.decide_async(ctx).await;
                        if let ContinuationDecision::Stop { note: Some(note) } = decision {
                            if self.semantic.last_report().is_none() {
                                *self.semantic_note.lock().unwrap() = Some(note);
                            }
                        }
                    }
                }
                return deterministic;
            }

            // Deterministic gate passed → semantic review. G5: a `fixable`
            // verdict must NOT be replayed into the same in-context trace
            // (Trap 4). The fix-and-reverify loop spawns a fresh patch-author
            // executor per round and re-runs the deterministic gate + semantic
            // review on the new state; only a terminal decision is surfaced.
            self.semantic_fix_loop(ctx).await
        })
    }

    fn verify_report(&self) -> Option<crate::verify::VerifyReport> {
        self.deterministic.verify_report()
    }

    fn semantic_report(&self) -> Option<SemanticVerifyReport> {
        self.semantic.semantic_report()
    }

    fn semantic_note(&self) -> Option<String> {
        self.semantic_note.lock().unwrap().take()
    }

    fn will_run_checks(&self, ctx: &TurnEndContext<'_>) -> bool {
        self.deterministic.will_run_checks(ctx)
    }

    fn review_only_async<'a>(
        &'a self,
        ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        Box::pin(async move {
            // Review-only for a turn-capped run: clear stale state, run the
            // deterministic gate for evidence, then a single semantic review.
            // A `fixable` verdict never spawns the G5 fixer nor re-enters the
            // loop — its fix request is surfaced as a terminal note, and any
            // decline stays observable through `semantic_note`.
            self.semantic.last_report.lock().unwrap().take();
            *self.semantic_note.lock().unwrap() = None;
            self.semantic.reset_attempts();
            let deterministic = self.deterministic.decide(ctx);
            if deterministic.is_continue() {
                return deterministic;
            }
            let report = self.deterministic.verify_report();
            let deterministic_verdict = report.as_ref().map(|report| report.verdict);
            let no_checks_detected = matches!(
                deterministic_verdict,
                Some(crate::verify::VerifyVerdict::Escalate)
            ) && report
                .as_ref()
                .is_some_and(|report| report.results.is_empty() && !report.unavailable);
            let review_signal = matches!(
                deterministic_verdict,
                Some(crate::verify::VerifyVerdict::Pass)
            ) || (self.semantic_only_when_no_lowlevel_tests
                && no_checks_detected);
            if !review_signal {
                return deterministic;
            }
            let decision = self.semantic.review_only_async(ctx).await;
            // Surface the note when the review declined (no report) or when a
            // `fixable` verdict was converted to a terminal note (the G5 fixer
            // never runs on a capped turn, so the fix request must stay
            // observable). Pass/replan/escalate notes duplicate the report.
            let report = self.semantic.last_report();
            let keep_note = report.is_none()
                || matches!(
                    report.as_ref().map(|report| report.verdict),
                    Some(SemanticVerdict::Fixable)
                );
            if let ContinuationDecision::Stop { note: Some(note) } = &decision {
                if keep_note {
                    *self.semantic_note.lock().unwrap() = Some(note.clone());
                }
            }
            decision
        })
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
            // A report belongs only to the current verification attempt. Clear
            // stale state before validation so a later runner/parser failure
            // can never be mistaken for a previous pass by a composite policy.
            *self.last_report.lock().unwrap() = None;
            let request = match self.request_from_context(ctx) {
                Ok(request) => request,
                Err(decision) => return decision,
            };
            let runner = self
                .runner
                .as_ref()
                .expect("request_from_context checked runner presence")
                .clone();
            // Empty/truncated responses (a provider empty completion) get one
            // retry before declining; structured failures do not.
            let response = match run_verifier_with_no_verdict_retry(runner, request).await {
                Ok(response) => response,
                Err(decision) => return decision,
            };
            if response.verdict == SemanticVerdict::Pass {
                self.attempts.store(0, std::sync::atomic::Ordering::Relaxed);
            }
            self.response_decision(response)
        })
    }

    fn semantic_report(&self) -> Option<SemanticVerifyReport> {
        self.last_report.lock().unwrap().take()
    }

    fn review_only_async<'a>(
        &'a self,
        ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        Box::pin(async move {
            // Review-only: a `fixable` verdict must not re-enter the loop or
            // spawn the G5 fixer; surface the fix request as a terminal note.
            match self.decide_async(ctx).await {
                ContinuationDecision::Continue { message } => ContinuationDecision::Stop {
                    note: Some(message),
                },
                decision => decision,
            }
        })
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

    /// Bounded read-only review of the most recent writing turn, without
    /// continuing the loop. Used for the max-turns degradation turn: the
    /// deterministic gate and a single semantic review run and their reports
    /// surface through the same Verify / SemanticVerify / Status events, but
    /// the G5 fixer never runs and the result is always terminal. The default
    /// implementation delegates to [`Self::decide_async`]; the semantic
    /// policies override it to strip any continuation.
    fn review_only_async<'a>(
        &'a self,
        ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        Box::pin(async move { self.decide_async(ctx).await })
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

    /// Reason the read-only semantic verifier declined to produce a verdict
    /// for the most recent turn (skipped / runner error / parse failure),
    /// surfaced as a status event so a gate-open review signal that silently
    /// declined stays observable. Default: `None` — only the execute-and-
    /// verify policies override this.
    fn semantic_note(&self) -> Option<String> {
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

    fn review_only_async<'a>(
        &'a self,
        _ctx: &'a TurnEndContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ContinuationDecision> + Send + 'a>>
    {
        Box::pin(async move {
            // A turn-capped SpecMode run must not auto-open the spec review
            // dialog; the cap already ended the run.
            *self.last_spec_path.lock().unwrap() = None;
            ContinuationDecision::Stop { note: None }
        })
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
    /// Goal-driven autonomy with the deterministic + semantic verification
    /// pipeline enabled after each writing turn. Semantic acceptance is
    /// required before the goal guard may continue.
    GoalSemanticVerify(clawde_core::config::VerifyConfig),
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
            ContinuationMode::GoalSemanticVerify(verify_config) => {
                Box::new(GoalSemanticVerifyPolicy::new(
                    verify_config,
                    working_dir,
                    semantic_runner,
                    fix_runner,
                ))
            }
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

    #[tokio::test]
    async fn semantic_review_signal_runs_when_no_checks_detected_and_flag_enabled() {
        // A plain directory with no project manifest detects no test/lint
        // commands, reproducing the deferred `semantic_only_when_no_lowlevel_tests`
        // gate case (deterministic Escalate, empty results).
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            called_clone.store(true, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"reviewed"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        // The model verdict cannot authorize acceptance: the turn still stops
        // on the deterministic Escalate, never a synthetic pass or Continue.
        assert!(!decision.is_continue());
        assert!(called.load(std::sync::atomic::Ordering::Relaxed));
        // The read-only review signal is surfaced via the semantic report.
        let report = policy.semantic_report().expect("review signal surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Pass);
    }

    #[tokio::test]
    async fn semantic_verifier_retries_once_on_empty_response() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            let n = calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async move {
                if n == 0 {
                    Ok(String::new())
                } else {
                    Ok(r#"{"verdict":"pass","summary":"reviewed"}"#.to_string())
                }
            })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "empty response should trigger exactly one retry"
        );
        let report = policy.semantic_report().expect("review signal surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Pass);
    }

    #[tokio::test]
    async fn semantic_verifier_retries_on_truncated_response() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            let n = calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async move {
                if n == 0 {
                    Ok(r#"{"verdict":"pass","summary":"reviewed"#.to_string())
                } else {
                    Ok(r#"{"verdict":"pass","summary":"reviewed"}"#.to_string())
                }
            })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "truncated JSON should trigger exactly one retry"
        );
        let report = policy.semantic_report().expect("review signal surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Pass);
    }

    #[tokio::test]
    async fn semantic_verifier_declines_when_retry_is_also_empty() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(String::new()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert!(
            policy.semantic_report().is_none(),
            "a declined review leaves no report"
        );
    }

    #[tokio::test]
    async fn semantic_verifier_does_not_retry_a_structured_parse_error() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(r#"{"verdict":"maybe","summary":"reviewed"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a structured (non-empty) parse error must not be retried"
        );
        assert!(policy.semantic_report().is_none());
    }

    #[tokio::test]
    async fn semantic_verifier_reask_feeds_parse_error_to_retry() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let requests: std::sync::Arc<std::sync::Mutex<Vec<SemanticVerifyRequest>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = requests.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |request| {
            observed.lock().expect("request lock").push(request.clone());
            Box::pin(async move {
                if request.retry_hint.is_some() {
                    // Second attempt: the reask hint is present, and the model
                    // now returns a parseable verdict.
                    Ok(r#"{"verdict":"pass","summary":"reviewed"}"#.to_string())
                } else {
                    // First attempt: prose-only output (no JSON object).
                    Ok("The change looks correct.".to_string())
                }
            })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        let captured = requests.lock().expect("request lock");
        assert_eq!(
            captured.len(),
            2,
            "prose-only output should trigger one reask retry"
        );
        assert!(
            captured[0].retry_hint.is_none(),
            "the first attempt must never carry a reask hint"
        );
        let hint = captured[1]
            .retry_hint
            .as_deref()
            .expect("the retry must carry the classified parse error");
        assert!(
            hint.contains("malformed JSON") && hint.contains("no JSON object found"),
            "hint should name the exact parse failure, got: {hint}"
        );
        let report = policy.semantic_report().expect("review signal surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Pass);
    }

    #[tokio::test]
    async fn semantic_verifier_reask_recovers_truncated_json_after_hint() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let requests: std::sync::Arc<std::sync::Mutex<Vec<SemanticVerifyRequest>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = requests.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |request| {
            observed.lock().expect("request lock").push(request.clone());
            Box::pin(async move {
                if request.retry_hint.is_some() {
                    Ok(r#"{"verdict":"fixable","summary":"missing edge case","findings":["add boundary test"]}"#.to_string())
                } else {
                    // Truncated JSON (the v10 `expected , or }` decline shape):
                    // no complete object, so no verdict is expressed.
                    Ok(r#"{"verdict":"fixable","summary":"missing edge case""#.to_string())
                }
            })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        let captured = requests.lock().expect("request lock");
        assert_eq!(captured.len(), 2, "truncated JSON should trigger one reask");
        let hint = captured[1]
            .retry_hint
            .as_deref()
            .expect("the retry must carry the classified parse error");
        assert!(
            hint.contains("malformed JSON"),
            "hint should name the parse failure, got: {hint}"
        );
        let report = policy.semantic_report().expect("review signal surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Fixable);
    }

    #[tokio::test]
    async fn semantic_verifier_reask_declines_when_hinted_retry_also_fails() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok("still not JSON".to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "a reask still gets exactly one retry before declining"
        );
        assert!(
            policy.semantic_report().is_none(),
            "a declined review leaves no report even after the reask retry"
        );
    }

    #[test]
    fn response_expresses_no_verdict_classifies_shapes() {
        // No complete object: empty, whitespace, prose-only, truncated JSON.
        assert!(response_expresses_no_verdict(""));
        assert!(response_expresses_no_verdict("   "));
        assert!(response_expresses_no_verdict("just some prose"));
        assert!(response_expresses_no_verdict(
            r#"{"verdict":"pass","summary":"ok""#
        ));
        // Complete object that omits the required `verdict` key (the v8
        // `missing field 'verdict'` decline shape) → no verdict expressed.
        assert!(response_expresses_no_verdict(
            r#"{"summary":"missing verdict"}"#
        ));
        assert!(response_expresses_no_verdict(
            r#"{"summary":"x","findings":[]}"#
        ));
        // A verdict key present (even an invalid value) means a verdict WAS
        // expressed → not retryable; the strict parser decides.
        assert!(!response_expresses_no_verdict(
            r#"{"verdict":"pass","summary":"ok"}"#
        ));
        assert!(!response_expresses_no_verdict(
            r#"{"verdict":"bogus","summary":"ok"}"#
        ));
        assert!(!response_expresses_no_verdict(r#"{"verdict":"pass"}"#));
        // Over-limit responses are never retried.
        let big = "x".repeat(SEMANTIC_VERIFY_MAX_RESPONSE_BYTES + 1);
        assert!(!response_expresses_no_verdict(&big));
    }

    #[tokio::test]
    async fn semantic_verifier_retries_once_on_verdictless_object() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            let n = calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async move {
                if n == 0 {
                    Ok(r#"{"summary":"the change looks correct","findings":[]}"#.to_string())
                } else {
                    Ok(r#"{"verdict":"pass","summary":"reviewed"}"#.to_string())
                }
            })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "a verdict-less object should trigger exactly one retry"
        );
        let report = policy.semantic_report().expect("review signal surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Pass);
    }

    #[tokio::test]
    async fn semantic_review_only_fires_on_turn_capped_review() {
        // The max-turns degradation turn gets a bounded read-only review: the
        // semantic verifier runs once on the final writing state and the
        // result is always terminal (the loop stops regardless of verdict).
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"final review"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.review_only_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        let report = policy.semantic_report().expect("final review surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Pass);
        assert_eq!(report.summary, "final review");
    }

    #[tokio::test]
    async fn semantic_review_only_fixable_never_continues_nor_fixes() {
        // The load-bearing review-only guarantee for a turn-capped run: a
        // `fixable` verdict must not continue the loop, and the G5 fixer must
        // never run after the cap. The fix request surfaces as a terminal
        // note and the report stays observable.
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let fix_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fix_calls_clone = fix_calls.clone();
        let fix_runner: SemanticFixRunner = std::sync::Arc::new(move |_| {
            fix_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok("fixed".to_string()) })
        });
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async {
                Ok(r#"{"verdict":"fixable","summary":"needs work","findings":["x"]}"#.to_string())
            })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            semantic_fix_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy = SemanticAfterVerifyPolicy::new(
            verify_config,
            project.path(),
            Some(runner),
            Some(fix_runner),
        );
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.review_only_async(&context).await;
        // Never continues, never spawns the fixer.
        assert!(!decision.is_continue());
        assert_eq!(fix_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        // The fixable verdict stays observable as a report.
        let report = policy.semantic_report().expect("fixable report surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Fixable);
    }

    #[tokio::test]
    async fn semantic_review_only_respects_fail_closed_gate() {
        // With the gate-open flag off, a no-checks turn must NOT fire the
        // semantic review in review-only mode either (fail-closed unchanged).
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_clone = calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"should not run"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: false,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.review_only_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "fail-closed gate must not fire the review"
        );
        assert!(policy.semantic_report().is_none());
    }

    #[tokio::test]
    async fn semantic_review_signal_fixable_never_authorizes_continuation() {
        // The load-bearing fail-closed guarantee: a `fixable` verdict must not
        // continue the loop or invoke the fixer in the no-gate review-signal
        // path. Only the report surfaces.
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async {
                Ok(r#"{"verdict":"fixable","summary":"needs work","findings":["x"]}"#.to_string())
            })
        });
        let fixer_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fixer_calls_clone = fixer_calls.clone();
        let fixer: SemanticFixRunner = std::sync::Arc::new(move |_| {
            fixer_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok("must not run".to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
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
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert_eq!(
            fixer_calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the no-gate review signal must never invoke the fixer"
        );
        let report = policy.semantic_report().expect("fixable signal surfaced");
        assert_eq!(report.verdict, SemanticVerdict::Fixable);
    }

    #[tokio::test]
    async fn semantic_review_signal_fixable_runs_fixer_when_flag_enabled() {
        // With `semantic_fix_when_no_lowlevel_tests`, a `fixable` review-signal
        // verdict routes to the bounded fresh-executor fixer. Acceptance stays
        // deterministic-only: the turn still stops on the deterministic
        // Escalate even after the fixer runs and the re-review passes.
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        // First review returns `fixable`; after the fixer runs, the re-review
        // returns `pass` so the fix loop reaches a terminal decision.
        let review_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let review_calls_clone = review_calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            let n = review_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async move {
                if n == 0 {
                    Ok(
                        r#"{"verdict":"fixable","summary":"needs work","findings":["x"]}"#
                            .to_string(),
                    )
                } else {
                    Ok(r#"{"verdict":"pass","summary":"fixed"}"#.to_string())
                }
            })
        });
        let fixer_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fixer_calls_clone = fixer_calls.clone();
        let fixer: SemanticFixRunner = std::sync::Arc::new(move |_| {
            fixer_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok("patched".to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            semantic_fix_when_no_lowlevel_tests: true,
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
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(
            !decision.is_continue(),
            "the deterministic Stop stays authoritative after the actionable fix"
        );
        assert_eq!(
            fixer_calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a fixable review-signal verdict must route to the fixer exactly once"
        );
        assert_eq!(
            review_calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "the fix loop must re-verify after the fixer runs"
        );
    }

    #[tokio::test]
    async fn semantic_fix_loop_resets_attempt_budget_between_turns() {
        // A turn that exhausts the fix-and-reverify budget must not poison the
        // next turn: each turn is a fresh verification session. Without the
        // per-turn reset, the second turn's first `fixable` verdict would
        // immediately decline as "exhausted" and never reach the fixer.
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async {
                Ok(r#"{"verdict":"fixable","summary":"still wrong","findings":["x"]}"#.to_string())
            })
        });
        let fixer_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fixer_calls_clone = fixer_calls.clone();
        let fixer: SemanticFixRunner = std::sync::Arc::new(move |_| {
            fixer_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok("patched".to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            semantic_fix_when_no_lowlevel_tests: true,
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
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        // First turn: the fix loop re-verifies `fixable` every round until the
        // semantic attempt budget (default 3) is exhausted.
        let _ = policy.decide_async(&context).await;
        let first_fixer_calls = fixer_calls.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            first_fixer_calls, 3,
            "the first turn must exhaust three fix rounds"
        );
        // Second turn: the budget must be reset, so the fixer runs again
        // instead of immediately declining as "exhausted".
        let _ = policy.decide_async(&context).await;
        assert_eq!(
            fixer_calls.load(std::sync::atomic::Ordering::Relaxed),
            first_fixer_calls + 3,
            "the second turn must start a fresh fix budget"
        );
    }

    #[tokio::test]
    async fn semantic_review_signal_surfaces_decline_reason() {
        // A free model returning prose with no JSON object → parse failure.
        // The gate-open review signal is evidence-only, but the decline reason
        // must still be observable instead of silently discarded.
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async { Ok("sorry, I cannot produce JSON right now".to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert!(policy.semantic_report().is_none());
        let note = policy.semantic_note().expect("decline reason surfaced");
        assert!(note.contains("malformed JSON"), "note was: {note}");
    }

    #[tokio::test]
    async fn semantic_review_signal_surfaces_skip_reason() {
        // No scoped diff at all → request_from_context declines with a skip
        // note, which must also be surfaced.
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"must not run"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: None,
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        let note = policy.semantic_note().expect("skip reason surfaced");
        assert!(
            note.contains("no non-empty scoped diff"),
            "note was: {note}"
        );
    }

    #[tokio::test]
    async fn semantic_review_signal_clears_note_on_success() {
        // A successful verdict must not leave a stale decline note behind.
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"reviewed"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert!(policy.semantic_report().is_some());
        assert!(
            policy.semantic_note().is_none(),
            "a successful verdict must not leave a decline note"
        );
    }

    #[tokio::test]
    async fn semantic_review_signal_skipped_when_flag_disabled() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("notes.txt"), "plain\n").expect("write plain file");
        let changed = project.path().join("notes.txt");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = called.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            called_clone.store(true, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"must not run"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/notes.txt\n+++ b/notes.txt\n+reviewed\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(!decision.is_continue());
        assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
        assert!(policy.semantic_report().is_none());
    }

    #[tokio::test]
    async fn semantic_review_signal_still_closed_on_deterministic_failure() {
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
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"must not run"}"#.to_string()) })
        });
        let verify_config = clawde_core::config::VerifyConfig {
            semantic_only_when_no_lowlevel_tests: true,
            auto_lint: false,
            timeout_secs: 30,
            ..Default::default()
        };
        let policy =
            SemanticAfterVerifyPolicy::new(verify_config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+assert!(false);\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        // A failing executable check routes to the deterministic auto-fix path
        // (Continue), never the semantic verifier, even with the gate open.
        assert!(decision.is_continue());
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
    fn semantic_runner_feedback_is_classified_without_raw_error_text() {
        let error = "all free-mode upstreams exhausted: groq: unauthorized key=secret";
        assert_eq!(
            classify_semantic_runner_error(error),
            "provider_chain_exhausted"
        );
        assert!(!classify_semantic_runner_error(error).contains("secret"));
        assert_eq!(
            classify_semantic_runner_error("provider returned an unexpected response"),
            "runner_error"
        );
        assert_eq!(
            classify_semantic_runner_error(
                "Sub-agent error: No API key for provider 'free' (model 'free/auto')"
            ),
            "provider_unavailable"
        );
        assert_eq!(
            classify_semantic_runner_error("provider unavailable in nested agent"),
            "provider_unavailable"
        );
        assert_eq!(
            classify_semantic_runner_error(
                "Tool 'agent' is unavailable in Ollama offline mode: network-capable tools are disabled"
            ),
            "network_boundary_blocked"
        );
    }

    #[test]
    fn fixer_feedback_is_classified_without_raw_error_text() {
        let oversized = format!(
            "malformed unified diff {}",
            "secret-model-output".repeat(2_000)
        );
        let feedback = bounded_fix_feedback(&oversized);
        assert_eq!(
            feedback,
            "semantic fixer rejected the attempt: strict_parse_failure"
        );
        assert!(!feedback.contains("secret-model-output"));
        assert_eq!(
            bounded_fix_feedback("changed file escapes working directory"),
            "semantic fixer rejected the attempt: scope_rejected"
        );
        assert_eq!(
            bounded_fix_feedback("dry-run apply failed"),
            "semantic fixer rejected the attempt: patch_apply_failure"
        );
        assert_eq!(
            bounded_fix_feedback("provider returned an unexpected response"),
            "semantic fixer rejected the attempt: runner_error"
        );
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
        // Unknown fields are ignored, but authority comes from the required
        // verdict enum + summary — this object still parses.
        let noisy =
            parse_semantic_verify_response(r#"{"verdict":"pass","summary":"ok","extra":true}"#)
                .expect("unknown fields ignored");
        assert_eq!(noisy.verdict, SemanticVerdict::Pass);
        assert!(parse_semantic_verify_response(
            "```json\n{\"verdict\":\"pass\",\"summary\":\"ok\"}\n```"
        )
        .is_ok());
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

    #[test]
    fn semantic_response_parser_tolerates_fences_and_prose() {
        // Markdown-fenced JSON is accepted.
        let fenced = parse_semantic_verify_response(
            "```json\n{\"verdict\":\"pass\",\"summary\":\"looks good\",\"findings\":[]}\n```",
        )
        .expect("fenced JSON");
        assert_eq!(fenced.verdict, SemanticVerdict::Pass);

        // Prose wrapping a JSON object is accepted.
        let prose = parse_semantic_verify_response(
            "Here is my assessment:\n\n{\"verdict\":\"fixable\",\"summary\":\"handle the edge case\",\"findings\":[\"Add a guard\"]}\n\nHope that helps!",
        )
        .expect("prose-wrapped JSON");
        assert_eq!(prose.verdict, SemanticVerdict::Fixable);
        assert_eq!(prose.findings, vec!["Add a guard"]);

        // Braces and escaped quotes inside the summary string must not break
        // the balance scan.
        let braced = parse_semantic_verify_response(
            r#"prefix {"verdict":"pass","summary":"use a {map} with \"keys\"","findings":[]} suffix"#,
        )
        .expect("braces in summary");
        assert_eq!(braced.summary, "use a {map} with \"keys\"");

        // Truncated / unbalanced JSON still fails closed.
        assert!(parse_semantic_verify_response(r#"{"verdict":"pass","summary":"ok""#).is_err());

        // Empty output fails closed.
        assert!(parse_semantic_verify_response("").is_err());
        assert!(parse_semantic_verify_response("   ").is_err());

        // Prose with no JSON object at all still fails closed.
        assert!(parse_semantic_verify_response("just some prose, no object").is_err());

        // Unknown fields inside a fence are ignored alongside the valid
        // verdict (free models sprinkle noise keys into fenced output too).
        let noisy_fenced = parse_semantic_verify_response(
            "```json\n{\"verdict\":\"pass\",\"summary\":\"ok\",\"extra\":true}\n```",
        )
        .expect("fenced unknown fields ignored");
        assert_eq!(noisy_fenced.verdict, SemanticVerdict::Pass);
    }

    #[test]
    fn semantic_response_parser_tolerates_message_envelope() {
        // A Claude-style `{"message": …}` field alongside the verdict is
        // accepted and ignored.
        let with_message = parse_semantic_verify_response(
            r#"{"message":"looks good","verdict":"pass","summary":"all good","findings":[]}"#,
        )
        .expect("message envelope accepted");
        assert_eq!(with_message.verdict, SemanticVerdict::Pass);
        assert_eq!(with_message.summary, "all good");

        // A non-string `message` value is tolerated too (it is ignored).
        let nested = parse_semantic_verify_response(
            r#"{"message":{"role":"assistant"},"verdict":"pass","summary":"ok"}"#,
        )
        .expect("object message tolerated");
        assert_eq!(nested.verdict, SemanticVerdict::Pass);

        // A verdict nested *inside* a `message` object is recovered.
        let nested_verdict = parse_semantic_verify_response(
            r#"{"message":{"verdict":"pass","summary":"nested ok","findings":[]}}"#,
        )
        .expect("nested verdict recovered");
        assert_eq!(nested_verdict.verdict, SemanticVerdict::Pass);
        assert_eq!(nested_verdict.summary, "nested ok");

        // A `message` field alone (no verdict) still fails closed.
        assert!(
            parse_semantic_verify_response(r#"{"message":"The change looks correct"}"#).is_err()
        );

        // A `message` object that is not itself a valid verdict still fails
        // closed (it carries no verdict).
        assert!(parse_semantic_verify_response(
            r#"{"message":{"role":"assistant","content":"looks good"}}"#
        )
        .is_err());

        // Arbitrary unknown fields alongside a valid verdict are ignored.
        // Free models routinely add noise keys (a truncated `f`, confidence
        // scores, etc.); the verdict enum + required summary stay authoritative.
        let noisy = parse_semantic_verify_response(
            r#"{"message":"x","verdict":"pass","summary":"ok","extra":true,"f":"noise"}"#,
        )
        .expect("unknown fields ignored alongside a valid verdict");
        assert_eq!(noisy.verdict, SemanticVerdict::Pass);
        assert_eq!(noisy.summary, "ok");
    }

    #[test]
    fn semantic_response_parser_tolerates_repeat_field() {
        // Some free models add a redundant `repeat` hint next to the verdict;
        // it duplicates what `fixable`/`replan` already express, so it is
        // accepted and ignored.
        let with_repeat = parse_semantic_verify_response(
            r#"{"verdict":"fixable","summary":"redo it","findings":["x"],"repeat":true}"#,
        )
        .expect("repeat hint accepted");
        assert_eq!(with_repeat.verdict, SemanticVerdict::Fixable);
        assert_eq!(with_repeat.summary, "redo it");

        // A non-boolean `repeat` value is tolerated too (ignored).
        let repeat_string =
            parse_semantic_verify_response(r#"{"verdict":"pass","summary":"ok","repeat":"yes"}"#)
                .expect("string repeat tolerated");
        assert_eq!(repeat_string.verdict, SemanticVerdict::Pass);

        // `repeat` alone (no verdict) still fails closed.
        assert!(parse_semantic_verify_response(r#"{"repeat":true}"#).is_err());

        // Unknown fields alongside a valid verdict are ignored (the v12
        // `unknown field 'f'` decline shape).
        let noisy = parse_semantic_verify_response(
            r#"{"verdict":"pass","summary":"ok","repeat":true,"extra":1,"f":"x"}"#,
        )
        .expect("unknown fields ignored alongside `repeat`");
        assert_eq!(noisy.verdict, SemanticVerdict::Pass);
    }

    #[tokio::test]
    async fn semantic_runner_error_is_redacted_at_policy_boundary() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::create_dir_all(project.path().join("src")).expect("project src");
        let changed = project.path().join("src/lib.rs");
        std::fs::write(&changed, "fn changed() {}").expect("fixture");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async {
                Err("all free-mode upstreams exhausted: groq: unauthorized key=secret".to_string())
            })
        });
        let policy = SemanticVerifyPolicy::new(Some(runner), None);
        let context = TurnEndContext {
            session_id: "session-redaction",
            working_dir: project.path(),
            turn_made_writes: true,
            changed_files: Some(&patch),
            changed_diff: Some(
                "--- a/src/lib.rs\\n+++ b/src/lib.rs\\n@@ -1 +1 @@\\n+fn changed() {}",
            ),
            ..ctx()
        };

        let decision = policy.decide_async(&context).await;
        match decision {
            ContinuationDecision::Stop { note: Some(note) } => {
                assert!(note.contains("provider_chain_exhausted"));
                assert!(!note.contains("secret"));
                assert!(!note.contains("groq"));
            }
            other => panic!("expected redacted runner stop, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn semantic_runner_provider_unavailable_is_redacted_at_policy_boundary() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::create_dir_all(project.path().join("src")).expect("project src");
        let changed = project.path().join("src/lib.rs");
        std::fs::write(&changed, "fn changed() {}").expect("fixture");
        let patch = clawde_core::snapshot::Patch {
            hash: "tree".to_string(),
            files: vec![changed],
        };
        let runner: SemanticVerifyRunner = std::sync::Arc::new(|_| {
            Box::pin(async {
                Err(
                    "Sub-agent error: No API key for provider 'free' (model 'free/auto')"
                        .to_string(),
                )
            })
        });
        let policy = SemanticVerifyPolicy::new(Some(runner), None);
        let context = TurnEndContext {
            session_id: "session-provider-unavailable",
            working_dir: project.path(),
            turn_made_writes: true,
            changed_files: Some(&patch),
            changed_diff: Some(
                "--- a/src/lib.rs\\n+++ b/src/lib.rs\\n@@ -1 +1 @@\\n+fn changed() {}",
            ),
            ..ctx()
        };

        let decision = policy.decide_async(&context).await;
        match decision {
            ContinuationDecision::Stop { note: Some(note) } => {
                assert!(note.contains("provider_unavailable"));
                assert!(!note.contains("free/auto"));
                assert!(!note.contains("No API key"));
            }
            other => panic!("expected redacted provider stop, got {:?}", other),
        }
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
        let verifier_file = project.path().join("src/lib.rs");
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            let calls = verifier_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let observed_fix = calls > 0
                && std::fs::read_to_string(&verifier_file)
                    .expect("read fixture during re-verification")
                    .contains("fn edge() {}");
            Box::pin(async move {
                if calls == 0 {
                    Ok(
                        r#"{"verdict":"fixable","summary":"missing edge case","findings":["Add coverage"]}"#
                            .to_string(),
                    )
                } else if observed_fix {
                    Ok(r#"{"verdict":"pass","summary":"fixed"}"#.to_string())
                } else {
                    Ok(r#"{"verdict":"fixable","summary":"fix was not observed","findings":["Reapply the edge-case fix"]}"#.to_string())
                }
            })
        });
        let fixer_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fixer_calls_clone = fixer_calls.clone();
        let captured_fix_request = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_fix_clone = captured_fix_request.clone();
        let fixer: SemanticFixRunner = std::sync::Arc::new(move |request| {
            fixer_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *captured_fix_clone.lock().unwrap() = Some(request.clone());
            let file = request.working_dir.join("src/lib.rs");
            Box::pin(async move {
                let mut source = std::fs::read_to_string(&file).expect("read fixture for fix");
                source.push_str("\nfn edge() {}\n");
                std::fs::write(file, source).expect("write fixture fix");
                Ok("applied the edge-case fix".to_string())
            })
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
        assert!(
            std::fs::read_to_string(project.path().join("src/lib.rs"))
                .expect("read fixed fixture")
                .contains("fn edge() {}"),
            "fresh fixer must mutate the scoped fixture on disk"
        );
    }

    #[tokio::test]
    async fn goal_semantic_policy_verifies_before_goal_continues() {
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
        let verifier_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let verifier_calls_clone = verifier_calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            verifier_calls_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(r#"{"verdict":"pass","summary":"accepted"}"#.to_string()) })
        });
        let config = clawde_core::config::VerifyConfig {
            auto_lint: false,
            timeout_secs: 30,
            ..Default::default()
        };
        // No active default goal is required to prove ordering: a successful
        // semantic pass is terminal when GoalPolicy has no goal, rather than a
        // goal continuation silently bypassing semantic review.
        let policy = GoalSemanticVerifyPolicy::new(config, project.path(), Some(runner), None);
        let context = TurnEndContext {
            working_dir: project.path(),
            changed_files: Some(&patch),
            changed_diff: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n+fn changed() {}\n"),
            ..ctx()
        };
        let decision = policy.decide_async(&context).await;
        assert!(matches!(
            decision,
            ContinuationDecision::Stop { note: Some(note) } if note.contains("passed")
        ));
        assert_eq!(verifier_calls.load(std::sync::atomic::Ordering::Relaxed), 1);
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
            ContinuationDecision::Stop { note: Some(note) } if note.contains("fixer exhausted")
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

    #[tokio::test]
    async fn semantic_fixer_retries_with_feedback_within_each_fixable_round() {
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

        let verifier_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let verifier_calls_clone = verifier_calls.clone();
        let runner: SemanticVerifyRunner = std::sync::Arc::new(move |_| {
            let call = verifier_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Ok(r#"{"verdict":"fixable","summary":"needs a retry","findings":["apply the edge fix"]}"#.to_string())
                } else {
                    Ok(r#"{"verdict":"pass","summary":"fixed after retry"}"#.to_string())
                }
            })
        });

        let fixer_calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let fixer_calls_clone = fixer_calls.clone();
        let feedbacks = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
        let feedbacks_clone = feedbacks.clone();
        let fixer: SemanticFixRunner = std::sync::Arc::new(move |request| {
            fixer_calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            feedbacks_clone
                .lock()
                .expect("feedback lock")
                .push(request.feedback.clone());
            let file = request.working_dir.join("src/lib.rs");
            let attempt = fixer_calls_clone.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if attempt == 1 {
                    Err("strict_parse_failure: expected a complete unified diff".to_string())
                } else {
                    let mut source = std::fs::read_to_string(&file).expect("read fixture");
                    source.push_str("\nfn edge() {}\n");
                    std::fs::write(file, source).expect("write retry fix");
                    Ok("applied retry fix".to_string())
                }
            })
        });

        let verify_config = clawde_core::config::VerifyConfig {
            auto_lint: false,
            semantic_max_attempts: 1,
            semantic_fix_max_attempts: 2,
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
            fixer_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one fixable verdict gets its own two-attempt fixer budget"
        );
        let feedbacks = feedbacks.lock().expect("feedback lock");
        assert_eq!(feedbacks.len(), 2);
        assert!(feedbacks[0].is_none());
        assert_eq!(
            feedbacks[1].as_deref(),
            Some("semantic fixer rejected the attempt: strict_parse_failure")
        );
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
