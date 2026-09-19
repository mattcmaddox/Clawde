//! Bounded, offline per-session digest for the `/history` session browser.
//!
//! A transcript's *tail* cannot describe what a session was: the opening
//! prompt sits at byte zero and the middle work is only visible in between.
//! The browser used to render `(untitled)` plus two synopsis rows that were
//! almost always empty, because the only per-session summaries were written
//! by exit-time writers that rarely ran.
//!
//! This module samples the file at fixed fractional offsets instead of
//! parsing it whole — six 16 KB windows cap the read cost at ~96 KB however
//! large the transcript is — and distils what the browser renders:
//!
//! * [`SessionDigest::title_hint`] — a deterministic phrase from the first
//!   user turn, so a session whose model title never landed still has a label.
//! * [`SessionDigest::opening`] — the most meaningful words of the first
//!   user message.
//! * [`SessionDigest::middle`] — words describing the middle work, excluding
//!   anything already shown in `opening`, so the row reads as a later phase.
//! * [`SessionDigest::flags`] — inferred ending state. Every flag is an
//!   inference from sampled evidence: no evidence yields no flag, never a
//!   guess.
//!
//! Everything here is deterministic and offline. `opening`, `middle` and
//! `title_hint` are pure functions of the transcript text, so they fill in
//! retroactively for sessions written long before this module existed.

use std::collections::HashMap;
use std::path::Path;

/// Bytes sampled per window. Six windows bound a digest's read cost at
/// ~96 KB regardless of transcript size.
const WINDOW_BYTES: u64 = 16 * 1024;

/// Fractional positions across the file that the sample windows are anchored
/// at. `0.0` is the opening prompt, `1.0` the tail (where the ending state
/// lives); the rest approximate the middle work.
const SAMPLE_POINTS: [f64; 6] = [0.0, 0.2, 0.4, 0.6, 0.8, 1.0];

/// Terms kept per row. The detail popup shows the full row; the list row
/// truncates to the modal width.
const MAX_TERMS: usize = 6;

/// Hard character cap per rendered row.
pub const MAX_ROW_CHARS: usize = 110;

/// Upper bound on the accumulated middle-window corpus, so a session with
/// enormous messages cannot turn a digest into a full read.
const MIDDLE_CORPUS_BYTES: usize = 12 * 1024;

/// How many words of the first prompt become the title hint.
const TITLE_HINT_WORDS: usize = 10;

/// Character cap for [`SessionDigest::title_hint`].
const TITLE_HINT_CHARS: usize = 60;

/// A single inferred fact about how a session ended.
///
/// Rendered as shorthand in the browser's status row. All variants are
/// inferences from the sampled windows — see [`digest_transcript`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestFlag {
    /// A compaction summary is present: the context was collapsed mid-session,
    /// so the transcript is not the full conversation.
    Compacted,
    /// The last file-mutating tool call is followed by a successful
    /// `git commit` shell call.
    Committed,
    /// File edits exist after the last `git commit` seen, or edits exist with
    /// no commit at all.
    Uncommitted,
    /// The last conversation entry in the file is a user turn — the session
    /// stopped before the model answered.
    Interrupted,
    /// An error surfaced near the end of the transcript (failed tool result or
    /// an API error block).
    Errored,
    /// No file-mutating tool call and no commit appeared in any sampled
    /// window: the session was research/analysis. Weakest of the inferences
    /// (a very large transcript is only sampled), so it is rendered last.
    NoEdits,
}

impl DigestFlag {
    /// Compact rendering for the status row. Symbols first so the row scans
    /// vertically even when the words are truncated away.
    pub fn shorthand(self) -> &'static str {
        match self {
            Self::Interrupted => "\u{2026} interrupted",
            Self::Errored => "\u{2717} error",
            Self::Compacted => "\u{2702} compacted",
            Self::Committed => "\u{2714} commit",
            Self::Uncommitted => "\u{25cf} uncommitted",
            Self::NoEdits => "\u{25e6} no edits",
        }
    }
}

/// Join flags into the one-line status row the browser and the detail popup
/// render. Kept free-standing so a caller holding only the flags (the TUI
/// stores them per session) produces the same text as a full digest.
pub fn status_line(flags: &[DigestFlag]) -> String {
    flags
        .iter()
        .map(|flag| flag.shorthand())
        .collect::<Vec<_>>()
        .join("  ")
}

/// Per-session digest rendered as the browser's rows 2-4.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDigest {
    title_hint: Option<String>,
    opening: String,
    middle: String,
    flags: Vec<DigestFlag>,
    /// Whether any user/assistant entry was seen. A transcript holding only
    /// metadata (a state-event written before the first turn) has no messages,
    /// no rows and nothing to resume — the browser drops those.
    saw_messages: bool,
}

impl SessionDigest {
    /// Deterministic title phrase from the first user turn, if the transcript
    /// carried a usable one. Never derived from harness meta prompts
    /// (`/command` expansions, `<system-reminder>` blocks).
    pub fn title_hint(&self) -> Option<&str> {
        self.title_hint.as_deref()
    }

    /// Most meaningful words of the first user message.
    pub fn opening(&self) -> &str {
        &self.opening
    }

    /// Words describing the middle work, excluding terms already in
    /// [`Self::opening`].
    pub fn middle(&self) -> &str {
        &self.middle
    }

    /// Inferred ending state, in rendering order.
    pub fn flags(&self) -> &[DigestFlag] {
        &self.flags
    }

    /// The status row: shorthand flags joined for one line. Empty when no
    /// signal was found.
    pub fn status(&self) -> String {
        status_line(&self.flags)
    }

    /// True when the transcript carried at least one user or assistant turn.
    /// `false` means nothing can be shown or resumed for this session.
    pub fn has_messages(&self) -> bool {
        self.saw_messages
    }

    /// True when nothing could be extracted (missing or empty transcript).
    pub fn is_empty(&self) -> bool {
        self.title_hint.is_none()
            && self.opening.is_empty()
            && self.middle.is_empty()
            && self.flags.is_empty()
    }
}

/// Compute a digest for `path`.
///
/// Never fails: an unreadable or empty transcript yields an empty digest and
/// the browser falls back to whatever metadata the tail read already found.
pub async fn digest_transcript(path: &Path) -> SessionDigest {
    let mut digest = SessionDigest::default();

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return digest,
    };
    let size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return digest,
    };
    if size == 0 {
        return digest;
    }

    let offsets = window_offsets(size);
    let last_window = offsets.len() - 1;

    // Order-sensitive accumulators. `op_seq` advances per tool call, so the
    // relative order of an edit and a commit survives the window gaps — and,
    // when both land in one assistant turn, the order of the blocks in it.
    let mut op_seq: u64 = 0;
    let mut opening_text: Option<String> = None;
    let mut last_user_prompt: Option<String> = None;
    let mut middle_corpus = String::new();
    let mut last_edit_op: u64 = 0;
    let mut last_commit_op: u64 = 0;
    let mut compacted = false;
    let mut errored = false;
    let mut last_participant_was_user = false;
    let mut saw_participant = false;

    for (index, offset) in offsets.iter().copied().enumerate() {
        let Some(text) = read_window(&mut file, offset, size).await else {
            continue;
        };
        let is_first = index == 0;
        let is_last = index == last_window;

        // A window that starts mid-file begins mid-line; the leading fragment
        // is not parseable JSON, so drop that one line. The final line of the
        // last window is the file's true end and is always intact.
        let mut lines = text.lines();
        if offset > 0 {
            lines.next();
        }

        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(entry) =
                serde_json::from_str::<crate::session_storage::TranscriptEntry>(trimmed)
            else {
                continue;
            };

            // System entries carry no conversation body, but they do carry
            // error blocks — the block scan below covers them too.
            let (message, is_participant, is_user) = match &entry {
                crate::session_storage::TranscriptEntry::User(message) => {
                    (Some(message), true, true)
                }
                crate::session_storage::TranscriptEntry::Assistant(message) => {
                    (Some(message), true, false)
                }
                crate::session_storage::TranscriptEntry::System(message) => {
                    (Some(message), false, false)
                }
                crate::session_storage::TranscriptEntry::Summary(_) => {
                    compacted = true;
                    (None, false, false)
                }
                _ => (None, false, false),
            };

            let Some(message) = message else { continue };

            if is_participant {
                saw_participant = true;
                last_participant_was_user = is_user;

                let body = collapse(&message.message.get_all_text());
                if !body.is_empty() && !is_harness_meta(&body) {
                    // The opening row is the first *user* turn in file order,
                    // wherever it lands: a first window full of harness meta
                    // (a slash-command expansion, a large injected reminder)
                    // must not blank it.
                    if is_user {
                        if opening_text.is_none() {
                            opening_text = Some(body.clone());
                        }
                        last_user_prompt = Some(body.clone());
                    }
                    if !is_first && !is_last && middle_corpus.len() < MIDDLE_CORPUS_BYTES {
                        // Middle windows only: the opening row owns the first
                        // message, the status row owns the tail.
                        middle_corpus.push_str(&body);
                        middle_corpus.push('\n');
                    }
                }
            }

            for block in blocks_of(&message.message) {
                match block {
                    crate::types::ContentBlock::ToolUse { name, input, .. } => {
                        op_seq += 1;
                        if crate::constants::is_file_mutator(name) {
                            last_edit_op = op_seq;
                        } else if (name == crate::constants::TOOL_NAME_BASH || name == "PowerShell")
                            && input
                                .get("command")
                                .and_then(|value| value.as_str())
                                .is_some_and(command_commits)
                        {
                            last_commit_op = op_seq;
                        }
                    }
                    // Errors only count near the end: an early failure the
                    // session recovered from is not how it ended.
                    crate::types::ContentBlock::ToolResult { is_error, .. }
                        if is_last && *is_error == Some(true) =>
                    {
                        errored = true;
                    }
                    crate::types::ContentBlock::SystemAPIError { .. } if is_last => {
                        errored = true;
                    }
                    _ => {}
                }
            }
        }
    }

    digest.saw_messages = saw_participant;

    if let Some(opening) = opening_text.as_deref() {
        digest.title_hint = title_hint_from_prompt(opening);
        let row = keywords(opening, &[], MAX_TERMS);
        // A prompt made of fragments ("2+2 is", "ok") yields no keywords at
        // all; fall back to the prompt's own opening words rather than leaving
        // the row blank.
        digest.opening = if row.is_empty() {
            digest.title_hint.clone().unwrap_or_default()
        } else {
            row
        };
    }
    let excluded = split_terms(&digest.opening);
    digest.middle = keywords(&middle_corpus, &excluded, MAX_TERMS);

    // A session short enough to fit one window has no middle windows. Fall
    // back to the assistant text from the first window, so the row describes
    // the work rather than sitting empty.
    if digest.middle.is_empty() {
        let mut first_window_assistant = String::new();
        if let Some(text) = read_window(&mut file, 0, size).await {
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(entry) =
                    serde_json::from_str::<crate::session_storage::TranscriptEntry>(trimmed)
                else {
                    continue;
                };
                if let crate::session_storage::TranscriptEntry::Assistant(message) = entry {
                    let body = collapse(&message.message.get_all_text());
                    if !body.is_empty() && !is_harness_meta(&body) {
                        first_window_assistant.push_str(&body);
                        first_window_assistant.push('\n');
                    }
                }
            }
        }
        digest.middle = keywords(&first_window_assistant, &excluded, MAX_TERMS);
    }

    // Last resort: name what the session was working on last, which beats a
    // blank row for a session whose middle windows held no prose at all.
    if digest.middle.is_empty() {
        if let Some(prompt) = last_user_prompt.as_deref() {
            digest.middle = keywords(prompt, &excluded, MAX_TERMS);
        }
    }
    // A single-turn session makes the fallback above repeat the opening row.
    // Row 3 exists to describe a *later* phase of the work, so a duplicate is
    // worse than an empty row (the browser renders a placeholder).
    if digest.middle.eq_ignore_ascii_case(&digest.opening) {
        digest.middle = String::new();
    }

    let mut flags = Vec::new();
    if saw_participant && last_participant_was_user {
        flags.push(DigestFlag::Interrupted);
    }
    if errored {
        flags.push(DigestFlag::Errored);
    }
    if compacted {
        flags.push(DigestFlag::Compacted);
    }
    if last_commit_op > 0 && last_commit_op > last_edit_op {
        flags.push(DigestFlag::Committed);
    } else if last_edit_op > 0 {
        flags.push(DigestFlag::Uncommitted);
    } else if saw_participant {
        flags.push(DigestFlag::NoEdits);
    }
    digest.flags = flags;

    digest
}

/// Sample offsets for a file of `size` bytes: rising, deduplicated (a small
/// file collapses to a single window) and always ending at the tail.
fn window_offsets(size: u64) -> Vec<u64> {
    if size <= WINDOW_BYTES {
        return vec![0];
    }
    let span = size - WINDOW_BYTES;
    let mut offsets: Vec<u64> = Vec::with_capacity(SAMPLE_POINTS.len());
    for point in SAMPLE_POINTS {
        let offset = (span as f64 * point).round() as u64;
        if offsets.last() != Some(&offset) {
            offsets.push(offset);
        }
    }
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

/// Read one `WINDOW_BYTES` window at `offset`, decoded lossily.
async fn read_window(file: &mut tokio::fs::File, offset: u64, size: u64) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let len = (size.saturating_sub(offset)).min(WINDOW_BYTES);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
        return None;
    }
    if file.read_exact(&mut buf).await.is_err() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Content blocks of a message, or an empty slice for plain-text messages.
fn blocks_of(message: &crate::types::Message) -> &[crate::types::ContentBlock] {
    match &message.content {
        crate::types::MessageContent::Blocks(blocks) => blocks,
        crate::types::MessageContent::Text(_) => &[],
    }
}

/// True when a shell command actually invokes `git commit`.
///
/// A substring test is wrong in both directions: `grep 'git commit' AGENTS.md`
/// and `git log --grep commit` are not commits, while `cd /repo && git commit
/// -m '…'` and `sudo git commit` are. Quoted segments are stripped first — a
/// commit message routinely mentions "git commit" — then each shell segment is
/// examined for a `git`/`commit` token pair.
fn command_commits(command: &str) -> bool {
    /// Read-only tools that take `git commit` as an argument, not a command.
    const NOT_A_SHELL: [&str; 8] = ["grep", "rg", "cat", "echo", "sed", "awk", "head", "tail"];

    strip_quoted(command)
        .split(['\n', ';', '|', '&'])
        .any(|segment| {
            let tokens: Vec<&str> = segment.split_whitespace().collect();
            if tokens.first().is_some_and(|t| NOT_A_SHELL.contains(t)) {
                return false;
            }
            tokens
                .windows(2)
                .any(|pair| pair[0] == "git" && pair[1] == "commit")
        })
}

/// Blank out single-quoted, double-quoted and backtick segments so their
/// contents cannot be mistaken for shell tokens.
fn strip_quoted(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut open: Option<char> = None;
    for ch in command.chars() {
        match open {
            Some(quote) if ch == quote => {
                open = None;
                out.push(' ');
            }
            Some(_) => {}
            None if matches!(ch, '\'' | '"' | '`') => {
                open = Some(ch);
                out.push(' ');
            }
            None => out.push(ch),
        }
    }
    out
}

/// Whitespace-collapse a message body into one line.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True for Clawde-internal meta prompts (slash-command expansions, injected
/// system reminders) that describe the harness rather than the user's goal.
fn is_harness_meta(text: &str) -> bool {
    text.starts_with('<') || text.starts_with('/') || text.starts_with('\u{2039}')
}

/// Deterministic title: the first few words of the opening prompt, cut on a
/// word boundary. Returns `None` for prompts that are pure harness noise or
/// too short to name anything.
///
/// Shared with the query crate's title fallback, so a session's label does not
/// depend on whether it was read from disk or still in memory.
pub fn title_hint_from_prompt(text: &str) -> Option<String> {
    if is_harness_meta(text.trim_start()) {
        return None;
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let phrase = words
        .iter()
        .take(TITLE_HINT_WORDS)
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = phrase.trim_end_matches(|c: char| {
        matches!(c, '.' | ',' | ':' | ';' | '!' | '?' | '-' | '\u{2014}')
    });
    let trimmed = trimmed.trim();
    if trimmed.chars().count() < 3 {
        return None;
    }
    Some(clamp_chars(trimmed, TITLE_HINT_CHARS))
}

/// Collapse to at most `max` characters on a word boundary, adding an
/// ellipsis when content was dropped.
fn clamp_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    if let Some(space) = cut.rfind(' ') {
        if space > max / 2 {
            cut.truncate(space);
        }
    }
    format!("{}\u{2026}", cut.trim_end())
}

/// Words that carry no session-specific meaning.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "these", "those", "from", "your", "you", "are",
    "not", "but", "can", "could", "should", "would", "will", "was", "were", "have", "has", "had",
    "its", "it's", "into", "out", "over", "just", "like", "need", "want", "use", "using", "used",
    "get", "got", "make", "made", "let", "lets", "also", "then", "than", "them", "they", "there",
    "here", "what", "when", "where", "which", "who", "how", "why", "all", "any", "some", "more",
    "most", "much", "very", "only", "own", "same", "such", "been", "being", "does", "did", "doing",
    "done", "about", "after", "before", "again", "because", "while", "during", "between",
    "through", "each", "other", "both", "few", "many", "now", "one", "two", "three", "way",
    "still", "even", "back", "see", "look", "sure", "okay", "yes", "please", "thanks", "things",
    "thing", "stuff", "really", "actually", "maybe", "first", "next", "last", "well", "good",
    "better", "best", "lots", "bit", "put", "take", "give", "know", "think", "say", "said", "run",
    "runs", "running",
];

/// Path/identifier fragments that appear in every repository and name nothing.
const GENERIC_TOKENS: &[&str] = &[
    "src", "crates", "target", "com", "org", "net", "http", "https", "www", "home", "usr", "bin",
    "tmp", "var", "etc", "dev", "null", "true", "false", "none", "some", "self", "fn", "impl",
    "let", "mut", "pub", "use", "mod", "return", "match", "struct", "enum", "test", "tests",
];

/// Token characters: `_ - .` stay inside a token so identifiers, file names and
/// flags survive (`session_digest.rs`, `cargo-test`, `src-rust`); everything
/// else separates.
fn is_token_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// Extract the highest-signal terms from `text`, excluding any term already
/// present in `exclude` (case-insensitive).
///
/// Scoring favours repetition (a token the session returned to names its real
/// subject) and identifier shape (`session_browser.rs`, `num_ctx`, `--resume`),
/// then length. Plain prose words rarely survive the cut.
fn keywords(text: &str, exclude: &[String], max_terms: usize) -> String {
    if text.is_empty() {
        return String::new();
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut display: HashMap<String, String> = HashMap::new();
    let mut first_seen: HashMap<String, usize> = HashMap::new();
    let mut order = 0usize;

    for raw in text.split(|c: char| !is_token_char(c)) {
        let token = raw.trim_matches(|c: char| matches!(c, '.' | '-' | '_'));
        let len = token.chars().count();
        // Too short to name anything; long enough to blow the row width.
        if !(3..=24).contains(&len) {
            continue;
        }
        if !token.chars().any(|c| c.is_alphabetic()) {
            continue;
        }
        // Commit hashes and other long hex runs name nothing about the work.
        let hex_body = token.trim_matches(|c: char| matches!(c, '.' | '-' | '_'));
        if hex_body.len() >= 7 && hex_body.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let lower = token.to_lowercase();
        if STOPWORDS.contains(&lower.as_str()) || GENERIC_TOKENS.contains(&lower.as_str()) {
            continue;
        }
        if counts.contains_key(&lower) {
            *counts.entry(lower.clone()).or_insert(0) += 1;
            continue;
        }
        counts.insert(lower.clone(), 1);
        display.insert(lower.clone(), token.to_string());
        first_seen.insert(lower, order);
        order += 1;
    }

    let mut ranked: Vec<(f32, usize, String)> = counts
        .iter()
        .map(|(lower, count)| {
            let shown = display.get(lower).cloned().unwrap_or_else(|| lower.clone());
            let mut score = 3.0 * count.saturating_sub(1) as f32;
            if shown.contains('_') || shown.contains('.') || shown.contains('-') {
                score += 2.0;
            }
            let length = shown.chars().count() as f32;
            score += (length.min(16.0) - 3.0) / 6.0;
            (
                score,
                first_seen.get(lower).copied().unwrap_or(usize::MAX),
                shown,
            )
        })
        .collect();

    // Highest score first; ties resolved by first appearance so the row is
    // stable across runs.
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));

    let excluded: Vec<String> = exclude.iter().map(|t| t.to_lowercase()).collect();
    let mut terms: Vec<String> = Vec::new();
    for (_, _, shown) in &ranked {
        if terms.len() >= max_terms {
            break;
        }
        if excluded.contains(&shown.to_lowercase()) {
            continue;
        }
        terms.push(shown.clone());
    }

    // If exclusion gutted the row (a short session whose middle repeats the
    // opening), fall back to the unfiltered ranking — a repeated term beats a
    // blank row.
    if terms.is_empty() {
        terms = ranked
            .iter()
            .take(max_terms)
            .map(|(_, _, shown)| shown.clone())
            .collect();
    }

    clamp_terms(&terms, MAX_ROW_CHARS)
}

/// Join terms for display, dropping any that would overflow `max_chars`.
fn clamp_terms(terms: &[String], max_chars: usize) -> String {
    let mut out = String::new();
    for term in terms {
        let candidate = if out.is_empty() {
            term.clone()
        } else {
            format!("{}{}{}", out, " \u{b7} ", term)
        };
        if candidate.chars().count() > max_chars {
            break;
        }
        out = candidate;
    }
    if out.is_empty() {
        return terms
            .first()
            .map(|t| t.chars().take(max_chars).collect())
            .unwrap_or_default();
    }
    out
}

/// Split a rendered row back into its terms (used to exclude row 2 from row 3).
fn split_terms(row: &str) -> Vec<String> {
    row.split(" \u{b7} ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_storage::{make_user_entry, TranscriptEntry};
    use crate::types::{ContentBlock, Message};
    use serde_json::json;
    use std::io::Write;

    /// Write a transcript JSONL from raw entry values and return its path.
    fn write_transcript(dir: &Path, name: &str, lines: &[serde_json::Value]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).expect("create transcript");
        for line in lines {
            writeln!(file, "{}", line).expect("write line");
        }
        file.flush().expect("flush");
        path
    }

    fn user_line(uuid: &str, text: &str) -> serde_json::Value {
        serde_json::to_value(make_user_entry(
            Message::user(text),
            uuid,
            None,
            "sess",
            "/tmp/project",
        ))
        .expect("serialize user entry")
    }

    fn assistant_line(uuid: &str, text: &str) -> serde_json::Value {
        let message = Message::assistant(text);
        serde_json::to_value(TranscriptEntry::Assistant(
            crate::session_storage::TranscriptMessage {
                uuid: Some(uuid.to_string()),
                parent_uuid: None,
                timestamp: "2026-09-18T00:00:00Z".to_string(),
                session_id: "sess".to_string(),
                cwd: "/tmp/project".to_string(),
                message,
                is_sidechain: false,
                user_type: "external".to_string(),
                version: "0.0.0".to_string(),
                git_branch: None,
                agent_role: None,
                managed_session_id: None,
                extra: Default::default(),
            },
        ))
        .expect("serialize assistant entry")
    }

    fn assistant_with_tools(uuid: &str, tools: &[(&str, serde_json::Value)]) -> serde_json::Value {
        let blocks: Vec<ContentBlock> = tools
            .iter()
            .map(|(name, input)| ContentBlock::ToolUse {
                id: format!("tu-{name}"),
                name: (*name).to_string(),
                input: input.clone(),
                thought_signature: None,
            })
            .collect();
        let message = Message::assistant_blocks(blocks);
        serde_json::to_value(TranscriptEntry::Assistant(
            crate::session_storage::TranscriptMessage {
                uuid: Some(uuid.to_string()),
                parent_uuid: None,
                timestamp: "2026-09-18T00:00:00Z".to_string(),
                session_id: "sess".to_string(),
                cwd: "/tmp/project".to_string(),
                message,
                is_sidechain: false,
                user_type: "external".to_string(),
                version: "0.0.0".to_string(),
                git_branch: None,
                agent_role: None,
                managed_session_id: None,
                extra: Default::default(),
            },
        ))
        .expect("serialize assistant entry")
    }

    #[tokio::test]
    async fn missing_file_yields_empty_digest() {
        let dir = tempfile::tempdir().expect("temp dir");
        let digest = digest_transcript(&dir.path().join("nope.jsonl")).await;
        assert!(digest.is_empty());
        assert!(digest.status().is_empty());
    }

    #[tokio::test]
    async fn opening_row_comes_from_first_user_message() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_transcript(
            dir.path(),
            "opening.jsonl",
            &[
                user_line(
                    "u1",
                    "the main /history popup just says (untitled). Make the session_browser rows useful.",
                ),
                assistant_line("a1", "I will look at the session_browser renderer."),
            ],
        );

        let digest = digest_transcript(&path).await;
        assert!(
            digest.opening().contains("session_browser"),
            "opening row should keep the identifier: {}",
            digest.opening()
        );
        assert!(
            digest.opening().contains("popup"),
            "opening row should keep prose nouns: {}",
            digest.opening()
        );
        assert!(
            digest.opening().chars().count() <= MAX_ROW_CHARS,
            "opening row must respect the row cap: {}",
            digest.opening().chars().count()
        );
    }

    #[tokio::test]
    async fn title_hint_skips_harness_meta_and_uses_the_first_real_prompt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_transcript(
            dir.path(),
            "title.jsonl",
            &[
                user_line("u1", "/history popup says untitled."),
                assistant_line("a1", "ok"),
                user_line("u2", "Fix the flaky auth test in session_browser.rs"),
            ],
        );

        let digest = digest_transcript(&path).await;
        assert_eq!(
            digest.title_hint(),
            Some("Fix the flaky auth test in session_browser.rs"),
            "the slash-command turn is harness meta; the next real prompt names the session"
        );
        assert!(
            !digest.opening().contains("history"),
            "harness meta must stay out of the keyword row: {}",
            digest.opening()
        );
    }

    #[tokio::test]
    async fn middle_row_excludes_terms_already_in_the_opening() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut lines = vec![user_line("u1", "session_browser renderer needs work")];
        for i in 0..40 {
            lines.push(assistant_line(
                &format!("a{i}"),
                "checkpoint_keeper validates the wire_format payload with cargo clippy",
            ));
        }
        let path = write_transcript(dir.path(), "middle.jsonl", &lines);

        let digest = digest_transcript(&path).await;
        assert!(
            digest.middle().contains("checkpoint_keeper"),
            "middle row should surface repeated identifiers: {}",
            digest.middle()
        );
        assert!(
            !digest.middle().to_lowercase().contains("session_browser"),
            "middle row must not repeat the opening row: {}",
            digest.middle()
        );
    }

    #[tokio::test]
    async fn status_flags_infer_commit_shapes() {
        let dir = tempfile::tempdir().expect("temp dir");

        let committed = write_transcript(
            dir.path(),
            "committed.jsonl",
            &[
                user_line("u1", "edit and commit the file"),
                assistant_with_tools(
                    "a1",
                    &[("Edit", json!({"file_path": "/tmp/a.rs", "old_string": "x"}))],
                ),
                assistant_with_tools(
                    "a2",
                    &[
                        ("Bash", json!({"command": "git commit -m 'fix'"})),
                        ("Bash", json!({"command": "cargo test"})),
                    ],
                ),
            ],
        );
        let flags = digest_transcript(&committed).await.flags().to_vec();
        assert!(flags.contains(&DigestFlag::Committed), "flags: {flags:?}");
        assert!(
            !flags.contains(&DigestFlag::Uncommitted),
            "flags: {flags:?}"
        );

        let dirty = write_transcript(
            dir.path(),
            "dirty.jsonl",
            &[
                user_line("u1", "edit without committing"),
                assistant_with_tools(
                    "a1",
                    &[
                        ("Bash", json!({"command": "git commit -m 'old'"})),
                        ("Write", json!({"file_path": "/tmp/b.rs"})),
                    ],
                ),
            ],
        );
        let flags = digest_transcript(&dirty).await.flags().to_vec();
        assert!(flags.contains(&DigestFlag::Uncommitted), "flags: {flags:?}");
        assert!(!flags.contains(&DigestFlag::Committed), "flags: {flags:?}");
    }

    #[tokio::test]
    async fn status_flags_infer_interrupted_and_errored() {
        let dir = tempfile::tempdir().expect("temp dir");

        let interrupted = write_transcript(
            dir.path(),
            "interrupted.jsonl",
            &[
                user_line("u1", "start"),
                assistant_line("a1", "working"),
                user_line("u2", "and now the session stops here"),
            ],
        );
        let digest = digest_transcript(&interrupted).await;
        assert!(
            digest.flags().contains(&DigestFlag::Interrupted),
            "flags: {:?}",
            digest.flags()
        );

        let errored = write_transcript(
            dir.path(),
            "errored.jsonl",
            &[
                user_line("u1", "start"),
                assistant_line("a1", "done"),
                serde_json::to_value(TranscriptEntry::System(
                    crate::session_storage::TranscriptMessage {
                        uuid: Some("s1".to_string()),
                        parent_uuid: None,
                        timestamp: "2026-09-18T00:00:00Z".to_string(),
                        session_id: "sess".to_string(),
                        cwd: "/tmp/project".to_string(),
                        message: {
                            let mut m = Message::assistant("");
                            m.content = crate::types::MessageContent::Blocks(vec![
                                ContentBlock::SystemAPIError {
                                    message: "overloaded".to_string(),
                                    retry_secs: None,
                                },
                            ]);
                            m
                        },
                        is_sidechain: false,
                        user_type: "external".to_string(),
                        version: "0.0.0".to_string(),
                        git_branch: None,
                        agent_role: None,
                        managed_session_id: None,
                        extra: Default::default(),
                    },
                ))
                .expect("serialize system entry"),
            ],
        );
        let digest = digest_transcript(&errored).await;
        assert!(
            digest.flags().contains(&DigestFlag::Errored),
            "flags: {:?}",
            digest.flags()
        );
    }

    #[test]
    fn window_offsets_cover_head_and_tail() {
        let offsets = window_offsets(100);
        assert_eq!(offsets, vec![0], "small files read as one window");

        let offsets = window_offsets(1024 * 1024);
        assert_eq!(offsets.first(), Some(&0));
        assert_eq!(
            offsets.last(),
            Some(&(1024 * 1024 - WINDOW_BYTES)),
            "the last window must end at EOF"
        );
        assert!(offsets.windows(2).all(|w| w[0] < w[1]), "strictly rising");
    }

    #[test]
    fn keywords_skip_commit_hashes() {
        let row = keywords(
            "committed as 797e8a9bc and then edited session_digest.rs",
            &[],
            MAX_TERMS,
        );
        assert!(!row.contains("797e8a9bc"), "row: {row}");
        assert!(row.contains("session_digest.rs"), "row: {row}");
    }

    #[test]
    fn keywords_prefer_identifiers_over_prose() {
        let row = keywords(
            "please make the thing work with session_browser and also render rows nicely",
            &[],
            MAX_TERMS,
        );
        assert!(row.contains("session_browser"), "row: {row}");
        assert!(!row.contains("please"), "row: {row}");
    }

    #[test]
    fn commit_detection_ignores_searched_and_quoted_text() {
        // Real commits.
        assert!(command_commits("git commit -m 'fix'"), "plain commit");
        assert!(
            command_commits("cd /repo && git commit -q -F - <<'EOF'\nmsg\nEOF"),
            "chained"
        );
        assert!(command_commits("sudo git commit --amend"), "prefixed");
        // Not commits.
        assert!(
            !command_commits("grep 'git commit' AGENTS.md"),
            "grep, quoted"
        );
        assert!(!command_commits("rg \"git commit\" -n"), "rg, quoted");
        assert!(!command_commits("git log --grep commit"), "log grep");
        assert!(!command_commits("cat docs/commits.md"), "unrelated");
        assert!(
            !command_commits("echo 'run git commit later'"),
            "prose in quotes"
        );
        assert!(!command_commits("git status"), "other subcommand");
    }

    #[test]
    fn commit_detection_handles_a_message_containing_the_words() {
        assert!(command_commits(
            "git commit -m 'docs: explain git commit usage'"
        ));
    }

    #[tokio::test]
    async fn opening_row_falls_back_when_the_prompt_has_no_keywords() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_transcript(
            dir.path(),
            "fragments.jsonl",
            &[
                user_line("u1", "2+2 is"),
                assistant_line("a1", "4"),
                user_line("u2", "what local storage memory do you keep for projects"),
            ],
        );
        let digest = digest_transcript(&path).await;
        assert!(
            !digest.opening().is_empty(),
            "a keyword-less prompt must not leave the opening row blank"
        );
        assert!(
            !digest.middle().is_empty(),
            "the middle row falls back to the last user prompt: {}",
            digest.middle()
        );
    }

    #[tokio::test]
    async fn middle_row_does_not_repeat_the_opening_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_transcript(
            dir.path(),
            "single.jsonl",
            &[user_line("u1", "probe with a distinctive_noun_here")],
        );
        let digest = digest_transcript(&path).await;
        assert!(!digest.opening().is_empty());
        assert_ne!(
            digest.middle(),
            digest.opening(),
            "row 3 must describe a later phase, not echo row 2"
        );
    }

    #[tokio::test]
    async fn metadata_only_transcript_reports_no_messages() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_transcript(
            dir.path(),
            "metadata.jsonl",
            &[json!({
                "type": "state-event",
                "sessionId": "sess",
                "timestamp": "2026-09-18T00:00:00Z",
                "event": { "kind": "tool_observed", "failed": false },
                "msgIndex": 1
            })],
        );
        let digest = digest_transcript(&path).await;
        assert!(
            !digest.has_messages(),
            "no user/assistant turns were written"
        );
        assert!(
            digest.is_empty(),
            "nothing can be rendered for this session"
        );
    }

    #[tokio::test]
    async fn status_reports_no_edits_for_a_research_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_transcript(
            dir.path(),
            "research.jsonl",
            &[
                user_line("u1", "how do the providers work"),
                assistant_with_tools("a1", &[("Read", json!({"file_path": "/tmp/a.rs"}))]),
            ],
        );
        let digest = digest_transcript(&path).await;
        assert_eq!(digest.flags(), &[DigestFlag::NoEdits]);
        assert_eq!(digest.status(), "\u{25e6} no edits");
    }

    #[test]
    fn status_shorthand_is_short_and_symbol_led() {
        for flag in [
            DigestFlag::Interrupted,
            DigestFlag::Errored,
            DigestFlag::Compacted,
            DigestFlag::Committed,
            DigestFlag::Uncommitted,
            DigestFlag::NoEdits,
        ] {
            let text = flag.shorthand();
            assert!(text.chars().count() <= 16, "{text}");
            assert!(
                !text.chars().next().expect("non-empty").is_alphanumeric(),
                "shorthand should be symbol-led: {text}"
            );
        }
    }
}
