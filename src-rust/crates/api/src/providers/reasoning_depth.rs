//! Adaptive reasoning depth — decide when a turn deserves more reasoning than
//! the user's own effort setting.
//!
//! The rule set here is deliberately small and rule-based (no second model
//! call): a turn is classified from five signal groups the harness already
//! has, and the resulting floor can only *raise* the effective effort, only up
//! to [`AUTO_RAISE_CEILING`], and only when the output budget can carry the
//! extra thinking.
//!
//! Explicit user intent always wins. `/effort`, `--effort`, `defaultEffort`,
//! `ing`, and the `ultracode` keyword are resolved before this module is
//! consulted, and the query loop skips the classifier entirely when the user
//! resolved a level for themselves — the raise fills in the unconfigured case
//! rather than second-guessing a deliberate choice. An explicit `none` is never
//! raised here either, and neither is any level at or above the target, so
//! [`decide`] can only ever move the effort up.
//!
//! Two callers must agree on the shape of this:
//!
//! - the query loop, which raises the effort for the turn, and
//! - the TUI, which shows the user that a turn was raised and why.
//!
//! It is pure and deterministic over its inputs so both can call it freely.

use clawde_core::effort::EffortLevel;

use super::free::{classify_text, TaskType};

/// The highest rung an automatic raise may reach.
///
/// `Ultracode` stays user-intent gated (keyword or CLI): it carries the
/// delegation workflow, and escalating to it silently is the wrong default
/// (spec §9 Q2). `XHigh` is the ceiling for "several strong signals concur".
pub const AUTO_RAISE_CEILING: EffortLevel = EffortLevel::XHigh;

/// A task statement at least this long counts as broad enough to warrant more
/// reasoning on its own.
pub const LONG_PROMPT_CHARS: usize = 900;

/// Distinct file paths in the task statement that make the work multi-file.
pub const MULTI_FILE_THRESHOLD: usize = 2;

/// Output tokens that must remain for the answer *after* the raised level's own
/// thinking budget.
///
/// Several providers consume thinking from `max_tokens` (DeepSeek, Poolside,
/// Ollama), so a raise on a small-cap upstream converts reasoning into an empty
/// reply — spec §2.3's `reasoning_tokens: 501, completion_tokens: 0`. A raise
/// is skipped when it would breach this reserve; `free`'s `max_tokens_cap`
/// values (8_192 on Poolside and friends) are exactly the case this guards.
pub const MIN_OUTPUT_RESERVE_TOKENS: u32 = 2_048;

/// Why a turn's effort was raised above the user's own setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseReason {
    /// Intent classification: reasoning, planning, verification, or code work.
    TaskKind,
    /// The request itself is broad — long, or naming many files/concepts.
    RequestComplexity,
    /// The task spans more than one file.
    MultiFileCode,
    /// An earlier attempt failed (tool error / failed verification).
    RetryAfterFailure,
}

impl RaiseReason {
    /// Short label for the TUI (the footer shows this next to the level).
    pub fn label(self) -> &'static str {
        match self {
            Self::TaskKind => "task type",
            Self::RequestComplexity => "request size",
            Self::MultiFileCode => "multi-file",
            Self::RetryAfterFailure => "retry after failure",
        }
    }
}

/// What the current turn looks like, from signals the harness already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthSignals {
    /// Intent classification of the task statement (reuses the routing
    /// classifier — there is deliberately only one).
    pub task: TaskType,
    /// Length of the task statement.
    pub prompt_chars: usize,
    /// Distinct file paths named in the task statement.
    pub files_mentioned: usize,
    /// Whether a previous attempt in this task failed (tool error or failed
    /// verification). Supplied by the caller: it depends on history, not text.
    pub retry_after_failure: bool,
}

impl Default for DepthSignals {
    fn default() -> Self {
        Self {
            task: TaskType::Search,
            prompt_chars: 0,
            files_mentioned: 0,
            retry_after_failure: false,
        }
    }
}

impl DepthSignals {
    /// Signals derivable from the task statement alone.
    pub fn from_prompt(prompt: &str) -> Self {
        Self {
            task: if prompt.trim().is_empty() {
                TaskType::Search
            } else {
                classify_text(prompt)
            },
            prompt_chars: prompt.chars().count(),
            files_mentioned: count_file_mentions(prompt),
            retry_after_failure: false,
        }
    }

    /// Whether the work spans more than one file.
    pub fn multi_file(&self) -> bool {
        self.files_mentioned >= MULTI_FILE_THRESHOLD
    }

    /// Whether the request is broad on size alone.
    pub fn long_prompt(&self) -> bool {
        self.prompt_chars >= LONG_PROMPT_CHARS
    }

    /// Whether the task kind is one that always justifies thinking harder.
    fn task_wants_depth(&self) -> bool {
        matches!(
            self.task,
            TaskType::Reasoning
                | TaskType::Planning
                | TaskType::Verification
                | TaskType::CodeEdit
                | TaskType::CodeGeneration
        )
    }

    /// Whether the task kind is design work, which is a strong signal on its own
    /// (architecture/design is one of the documented `XHigh` cases).
    fn is_design_work(&self) -> bool {
        matches!(self.task, TaskType::Planning | TaskType::Reasoning)
    }

    /// Whether this is a non-trivial task — the definition shared by the §6.1
    /// reasoning-guidance ceremony and the §6.4 Plan Mode interview gate.
    ///
    /// Read-only lookups (`Search`) and mechanical one-liners (`SimpleEdit`) are
    /// trivial: they are done, not interrogated. Everything else — design, code
    /// work, debugging/architecture, and verification runs — is non-trivial, and
    /// so is any request broad enough to be long even when its wording reads
    /// mechanical. This is a narrower bar than [`Self::reason`]: a raise is
    /// cheap, while asking the user a question costs their attention.
    pub fn non_trivial(&self) -> bool {
        match self.task {
            TaskType::Search | TaskType::SimpleEdit => self.long_prompt(),
            TaskType::Planning
            | TaskType::Reasoning
            | TaskType::CodeGeneration
            | TaskType::CodeEdit
            | TaskType::Verification => true,
        }
    }

    /// The strongest reason this turn deserves a raise, if any.
    fn reason(&self) -> Option<RaiseReason> {
        if self.retry_after_failure {
            return Some(RaiseReason::RetryAfterFailure);
        }
        if self.multi_file() {
            return Some(RaiseReason::MultiFileCode);
        }
        if self.long_prompt() {
            return Some(RaiseReason::RequestComplexity);
        }
        if self.task_wants_depth() {
            return Some(RaiseReason::TaskKind);
        }
        None
    }

    /// The level this turn should reach for, before any comparison with the
    /// user's own setting. `None` means "no raise for this turn".
    fn target_level(&self) -> Option<EffortLevel> {
        self.reason()?;
        // Strong signals reach the ceiling; ordinary code/reasoning work gets
        // the middle rung.
        let strong = self.retry_after_failure || self.multi_file() || self.is_design_work();
        Some(if strong {
            AUTO_RAISE_CEILING
        } else {
            EffortLevel::High
        })
    }
}

/// The effort to use for this turn, plus why it exceeds the user's setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthDecision {
    /// The effective level for the turn.
    pub level: Option<EffortLevel>,
    /// `Some` only when `level` was raised above the user's own setting.
    pub reason: Option<RaiseReason>,
}

/// Resolve the effective effort for a turn.
///
/// `user_level` is the *resolved* user intent (session `/effort`, CLI
/// `--effort`, `settings.json`, or the provider/model default); `None` means
/// nothing was configured. `max_tokens` is the request's output budget.
///
/// Invariants: an explicit `none` is returned unchanged (the user asked for
/// direct answers); a raise never lowers the level; the raise is capped at
/// [`AUTO_RAISE_CEILING`]; and a raise is skipped when it would leave less than
/// [`MIN_OUTPUT_RESERVE_TOKENS`] of output budget after the level's thinking.
///
/// `max_tokens` is the budget the request asks for. On the `free` composite that
/// is the *requested* budget, not the per-upstream `max_tokens_cap` of whichever
/// upstream ends up serving the turn — the chain picks that after this point.
/// The §6.6 routing hint (`FreeProvider::attempt_plan_inner`) orders a raised
/// `Route::Auto` turn toward upstreams whose cap can hold the level's thinking
/// budget, and the free chain still clamps the request's `max_tokens` and the
/// Google numeric budget to the serving upstream's cap.
pub fn decide(
    user_level: Option<EffortLevel>,
    signals: &DepthSignals,
    max_tokens: u32,
) -> DepthDecision {
    let base = user_level.unwrap_or(EffortLevel::Medium);
    if user_level == Some(EffortLevel::None) {
        return DepthDecision {
            level: user_level,
            reason: None,
        };
    }
    let Some(target) = signals.target_level() else {
        return DepthDecision {
            level: Some(base),
            reason: None,
        };
    };
    let reason = signals.reason();

    // Try the target, then one rung down, so a level whose thinking budget does
    // not fit a small output cap does not forfeit the raise entirely.
    let candidates = [Some(target), Some(EffortLevel::High)];
    for candidate in candidates.into_iter().flatten() {
        if candidate <= base {
            // Never lower, and never report a "raise" that changes nothing.
            break;
        }
        let thinking = candidate.thinking_budget_tokens().unwrap_or(0);
        if max_tokens < thinking.saturating_add(MIN_OUTPUT_RESERVE_TOKENS) {
            continue;
        }
        return DepthDecision {
            level: Some(candidate),
            reason,
        };
    }

    DepthDecision {
        level: Some(base),
        reason: None,
    }
}

/// Distinct file paths named in `text`.
///
/// A token counts as a file when it ends in a source-file extension.
/// Directory-only tokens, slash commands, URLs, and slashes in prose are not
/// multi-file evidence. Quotes and trailing punctuation are stripped first;
/// matching is case-insensitive.
pub fn count_file_mentions(text: &str) -> usize {
    const CODE_EXTENSIONS: &[&str] = &[
        "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "rb", "c", "cc", "cpp",
        "h", "hpp", "cs", "kt", "swift", "php", "sh", "bash", "toml", "json", "yaml", "yml", "md",
        "sql", "css", "html", "lock",
    ];

    let mut seen: Vec<String> = Vec::new();
    for raw in text.split_whitespace() {
        let token = raw
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '`' | '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';' | ':' | '*' | '<' | '>'
                )
            })
            .trim_end_matches('.')
            .trim_start_matches("./");
        if token.len() < 2 || raw.contains("://") {
            continue;
        }
        let has_code_extension = token
            .rsplit_once('.')
            .is_some_and(|(_, ext)| CODE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()));
        // Slashes in prose (and/or, CI/CD) are not file paths. Directory-only
        // tokens do not establish multi-file scope either.
        if !has_code_extension {
            continue;
        }
        let normalized = token.to_ascii_lowercase();
        if !seen.contains(&normalized) {
            seen.push(normalized);
        }
    }
    seen.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(task: TaskType, chars: usize, files: usize, retry: bool) -> DepthSignals {
        DepthSignals {
            task,
            prompt_chars: chars,
            files_mentioned: files,
            retry_after_failure: retry,
        }
    }

    #[test]
    fn count_file_mentions_ignores_prose_and_dedupes() {
        assert_eq!(count_file_mentions("explain the thing"), 0);
        assert_eq!(count_file_mentions("fix `src/a.rs` please"), 1);
        // Same path twice, different quoting → one file.
        assert_eq!(
            count_file_mentions("`src/a.rs` and src/a.rs, plus src/b.py"),
            2
        );
        // URLs and extensions in prose are not paths.
        assert_eq!(count_file_mentions("see https://example.com/a.rs"), 0);
        assert_eq!(count_file_mentions("the plan is clear"), 0);
        assert_eq!(count_file_mentions("CI/CD and/or /effort /plan src/"), 0);
        assert_eq!(count_file_mentions("a.rs b.rs"), 2);
    }

    #[test]
    fn absent_task_text_neither_raises_nor_interviews() {
        let signals = DepthSignals::from_prompt("   ");
        assert!(!signals.non_trivial());
        assert_eq!(decide(None, &signals, 32_768).reason, None);
        assert!(!DepthSignals::default().non_trivial());
    }

    #[test]
    fn simple_work_is_not_raised() {
        // Search / trivial edits / a short read-only question stay at the
        // user's level — tool availability is not itself a reason to escalate.
        for task in [TaskType::Search, TaskType::SimpleEdit] {
            let decision = decide(
                Some(EffortLevel::Medium),
                &signals(task, 40, 0, false),
                8_192,
            );
            assert_eq!(decision.level, Some(EffortLevel::Medium), "{task:?}");
            assert_eq!(decision.reason, None);
        }
    }

    #[test]
    fn code_work_raises_to_high_and_design_to_the_ceiling() {
        let code = decide(
            Some(EffortLevel::Medium),
            &signals(TaskType::CodeEdit, 80, 1, false),
            32_768,
        );
        assert_eq!(code.level, Some(EffortLevel::High));
        assert_eq!(code.reason, Some(RaiseReason::TaskKind));

        let design = decide(
            Some(EffortLevel::Medium),
            &signals(TaskType::Planning, 80, 1, false),
            32_768,
        );
        assert_eq!(design.level, Some(AUTO_RAISE_CEILING));
        assert_eq!(design.reason, Some(RaiseReason::TaskKind));
    }

    #[test]
    fn strong_signals_report_the_dominant_reason() {
        let multi = decide(
            Some(EffortLevel::Medium),
            &signals(TaskType::CodeEdit, 80, 3, false),
            32_768,
        );
        assert_eq!(multi.reason, Some(RaiseReason::MultiFileCode));
        assert_eq!(multi.level, Some(AUTO_RAISE_CEILING));

        // A retry outranks the others: it is the signal that says the approach
        // so far did not work.
        let retry = decide(
            Some(EffortLevel::Medium),
            &signals(TaskType::CodeEdit, 80, 3, true),
            32_768,
        );
        assert_eq!(retry.reason, Some(RaiseReason::RetryAfterFailure));
        assert_eq!(retry.level, Some(AUTO_RAISE_CEILING));

        // A long prompt is the weakest of the signals: it raises, but to the
        // middle rung rather than the ceiling.
        let long = decide(
            Some(EffortLevel::Medium),
            &signals(TaskType::CodeGeneration, LONG_PROMPT_CHARS + 1, 0, false),
            32_768,
        );
        assert_eq!(long.reason, Some(RaiseReason::RequestComplexity));
        assert_eq!(long.level, Some(EffortLevel::High));
    }

    #[test]
    fn raise_never_lowers_and_never_escalates_to_ultracode() {
        // The user is already above every candidate.
        let already_top = decide(
            Some(EffortLevel::Max),
            &signals(TaskType::Planning, LONG_PROMPT_CHARS, 3, true),
            32_768,
        );
        assert_eq!(already_top.level, Some(EffortLevel::Max));
        assert_eq!(already_top.reason, None, "no raise happened");

        // Ultracode is reachable only through the keyword / CLI, never here.
        let raised = decide(
            Some(EffortLevel::Medium),
            &signals(TaskType::Planning, LONG_PROMPT_CHARS, 3, true),
            32_768,
        );
        assert_eq!(raised.level, Some(EffortLevel::XHigh));
        assert!(raised.level.unwrap() < EffortLevel::Ultracode);
    }

    #[test]
    fn explicit_off_is_never_raised() {
        let decision = decide(
            Some(EffortLevel::None),
            &signals(TaskType::Planning, LONG_PROMPT_CHARS, 4, true),
            32_768,
        );
        assert_eq!(decision.level, Some(EffortLevel::None));
        assert_eq!(decision.reason, None);
    }

    #[test]
    fn raise_steps_down_when_the_output_budget_is_small() {
        let strong = signals(TaskType::Planning, 80, 3, false);

        // A 16_384-token cap cannot carry XHigh's 16_000-token thinking plus a
        // reserve, but it does carry High (10_000) — the raise lands there
        // rather than being forfeited.
        let mid = decide(Some(EffortLevel::Medium), &strong, 16_384);
        assert_eq!(mid.level, Some(EffortLevel::High));
        assert_eq!(mid.reason, Some(RaiseReason::MultiFileCode));

        // An 8_192 cap (Poolside's, spec §2.3's `reasoning_tokens: 501,
        // completion_tokens: 0`) carries neither rung: raising here would
        // convert the raise into an empty reply, so no raise happens at all.
        let small = decide(Some(EffortLevel::Medium), &strong, 8_192);
        assert_eq!(small.level, Some(EffortLevel::Medium));
        assert_eq!(small.reason, None);

        // Neither does 4_096.
        let tiny = decide(Some(EffortLevel::Medium), &strong, 4_096);
        assert_eq!(tiny.level, Some(EffortLevel::Medium));
        assert_eq!(tiny.reason, None);
    }

    #[test]
    fn unset_user_level_uses_the_medium_default_as_the_floor() {
        let decision = decide(None, &signals(TaskType::CodeEdit, 40, 0, false), 32_768);
        assert_eq!(decision.level, Some(EffortLevel::High));

        // ...and a no-raise turn still reports a usable level.
        let quiet = decide(None, &signals(TaskType::Search, 40, 0, false), 32_768);
        assert_eq!(quiet.level, Some(EffortLevel::Medium));
        assert_eq!(quiet.reason, None);
    }

    #[test]
    fn non_trivial_excludes_lookups_and_mechanical_edits() {
        // A find/grep lookup and a rename/typo fix are the spec's trivial
        // cases, however long the wording of the request is not.
        for task in [TaskType::Search, TaskType::SimpleEdit] {
            assert!(!signals(task, 120, 0, false).non_trivial(), "{task:?}");
            assert!(!signals(task, 120, 2, false).non_trivial(), "{task:?}");
        }
        // ...but a request broad enough to be long is not mechanical.
        assert!(signals(TaskType::Search, LONG_PROMPT_CHARS, 0, false).non_trivial());
        assert!(signals(TaskType::SimpleEdit, LONG_PROMPT_CHARS, 0, false).non_trivial());
    }

    #[test]
    fn non_trivial_covers_design_code_and_debugging_work() {
        for task in [
            TaskType::Planning,
            TaskType::CodeGeneration,
            TaskType::CodeEdit,
            TaskType::Verification,
        ] {
            assert!(signals(task, 40, 0, false).non_trivial(), "{task:?}");
        }
        // Debugging/architecture questions classify as `Reasoning` and are
        // non-trivial by the spec's own list.
        assert!(signals(TaskType::Reasoning, 40, 0, false).non_trivial());
        // The non-trivial bar is narrower than the raise bar: a short search is
        // raised (it is a raise-worthy *effort* call) but not non-trivial.
        let short_search = signals(TaskType::Search, 40, 3, false);
        assert!(short_search.reason().is_some());
        assert!(!short_search.non_trivial());
    }

    #[test]
    fn from_prompt_derives_task_size_and_files() {
        let signals = DepthSignals::from_prompt("Refactor `src/a.rs` and `src/b.rs` for clarity");
        assert_eq!(signals.task, TaskType::CodeEdit);
        assert_eq!(signals.files_mentioned, 2);
        assert!(signals.multi_file());
        assert!(!signals.retry_after_failure);
    }
}
