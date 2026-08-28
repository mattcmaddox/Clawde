//! Message type renderers for the TUI.
//! Mirrors src/components/messages/ and src/components/Messages.tsx.
//!
//! Each message type has a dedicated render function. The top-level
//! `render_message()` dispatcher routes to the correct renderer based
//! on message content.

use std::collections::HashMap;

use crate::app::TurnMetadata;
use crate::kitty_image::render_image;
use crate::transcript_turn::reasoning_heading;
use clawde_core::types::{ContentBlock, Message, Role, ToolResultContent};
use clawde_core::{FollowupRank, RankedFollowup};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

mod markdown;
pub use markdown::render_markdown;

mod markdown_enhanced;
pub use markdown_enhanced::{
    detect_table, parse_inline_formatting, render_table, Table, TableAlignment,
};

/// Context passed to all renderers.
///
/// `tool_names` and `expanded_thinking` are borrowed rather than owned: the
/// transcript builder holds a single copy of each per render pass and lends it
/// to every message renderer, so the hot path no longer clones a `HashMap` and
/// a `HashSet` for every assistant message (see issue #222).
pub struct RenderContext<'a> {
    /// Current terminal width (for word-wrap decisions).
    pub width: u16,
    /// Whether syntax highlighting is enabled.
    pub highlight: bool,
    /// Whether to show thinking blocks.
    pub show_thinking: bool,
    /// Maps `tool_use_id` → `tool_name` so ToolResult blocks can dispatch to
    /// the correct specialized renderer (e.g. Bash output vs. generic result).
    pub tool_names: &'a HashMap<String, String>,
    /// Set of thinking block content hashes that are expanded per-block.
    pub expanded_thinking: &'a std::collections::HashSet<u64>,
    /// Index of the currently highlighted followup (for keyboard nav).
    pub followup_selected: Option<usize>,
}

/// Shared empty collections so `RenderContext::default()` can hand out
/// `'static` borrows without allocating.
static EMPTY_TOOL_NAMES: std::sync::LazyLock<HashMap<String, String>> =
    std::sync::LazyLock::new(HashMap::new);
static EMPTY_EXPANDED_THINKING: std::sync::LazyLock<std::collections::HashSet<u64>> =
    std::sync::LazyLock::new(std::collections::HashSet::new);

impl Default for RenderContext<'static> {
    fn default() -> Self {
        Self {
            width: 80,
            highlight: true,
            show_thinking: false,
            tool_names: &EMPTY_TOOL_NAMES,
            expanded_thinking: &EMPTY_EXPANDED_THINKING,
            followup_selected: None,
        }
    }
}

/// A styled line for rendering.
pub type StyledLine<'a> = Line<'a>;

const MAX_USER_PROMPT_DISPLAY_CHARS: usize = 10_000;
const TRUNCATE_USER_PROMPT_HEAD_CHARS: usize = 2_500;
const TRUNCATE_USER_PROMPT_TAIL_CHARS: usize = 2_500;

/// Claude orange: Rgb(215, 119, 87)
const CLAUDE_ORANGE: Color = Color::Rgb(233, 30, 99);
const TRANSCRIPT_USER_BG: Color = Color::Rgb(23, 23, 31);
const TRANSCRIPT_CHIP_BG: Color = Color::Rgb(31, 31, 41);
const TRANSCRIPT_TEXT: Color = Color::Rgb(236, 236, 241);
const TRANSCRIPT_MUTED: Color = Color::Rgb(139, 139, 153);
const TRANSCRIPT_SUBTLE: Color = Color::Rgb(112, 112, 126);

const TOOL_RESULT_MAX_LINES: usize = 30;

/// Accent color for goal-event blocks (warm amber/gold).
const GOAL_ACCENT: Color = Color::Rgb(255, 170, 50);
/// Body text color for goal-event objective display.
const GOAL_BODY: Color = Color::Rgb(215, 180, 110);
/// Muted color for goal continuation turn markers.
const GOAL_MUTED: Color = Color::Rgb(130, 115, 75);

/// Render a code block with optional language label. Uses basic styling
/// since full syntect integration is behind a feature flag.
pub fn render_code_block(lang: Option<&str>, code: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let label = lang.unwrap_or("code");
    // Language label in brackets at the top
    lines.push(Line::from(vec![Span::styled(
        format!("  [{lang_name}]", lang_name = label),
        Style::default()
            .fg(Color::Rgb(150, 150, 150))
            .add_modifier(Modifier::DIM),
    )]));
    lines.push(Line::from(vec![Span::styled(
        "  ┌─────────────────────────────────────────────────".to_string(),
        Style::default().fg(Color::Rgb(100, 100, 100)),
    )]));
    // `2` chars for the leading "  " indent; at least 10 chars of content
    let max_content = (width as usize).saturating_sub(4).max(10);
    for line in code.lines() {
        let display: String = if line.chars().count() > max_content {
            let truncated: String = line.chars().take(max_content.saturating_sub(1)).collect();
            format!("{truncated}\u{2026}")
        } else {
            line.to_string()
        };
        lines.push(Line::from(vec![
            Span::styled("  │ ", Style::default().fg(Color::Rgb(100, 100, 100))),
            Span::styled(display, Style::default().fg(Color::White)),
        ]));
    }
    lines.push(Line::from(vec![Span::styled(
        "  └─────────────────────────────────────────────────".to_string(),
        Style::default().fg(Color::Rgb(100, 100, 100)),
    )]));
    lines
}

/// Render an assistant text message body.
pub fn render_assistant_text(text: &str, ctx: &RenderContext) -> Vec<Line<'static>> {
    render_markdown(text, ctx.width.saturating_sub(3))
}

/// Render a user text message body.
fn render_user_text_with_ctx(text: &str, ctx: &RenderContext) -> Vec<Line<'static>> {
    let truncated = truncate_user_prompt_text(text);
    render_markdown(&truncated, ctx.width.saturating_sub(3))
}

/// Legacy public helper retained for snapshot tests.
pub fn render_user_text(text: &str) -> Vec<Line<'static>> {
    render_user_text_with_ctx(text, &RenderContext::default())
}

fn indent_line(
    mut line: Line<'static>,
    prefix: &str,
    prefix_style: Style,
    default_fg: Color,
) -> Line<'static> {
    for span in &mut line.spans {
        if span.style.fg.is_none() {
            span.style = span.style.fg(default_fg);
        }
    }

    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(prefix.to_string(), prefix_style));
    spans.extend(line.spans);
    Line::from(spans)
}

fn indent_lines(
    lines: Vec<Line<'static>>,
    prefix: &str,
    prefix_style: Style,
    default_fg: Color,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| indent_line(line, prefix, prefix_style, default_fg))
        .collect()
}

fn apply_block_style(mut line: Line<'static>, width: u16) -> Line<'static> {
    let bg = TRANSCRIPT_USER_BG;
    for span in &mut line.spans {
        if span.style.fg.is_none() {
            span.style = span.style.fg(TRANSCRIPT_TEXT);
        }
        span.style = span.style.bg(bg);
    }

    let mut spans = vec![
        Span::styled("▏", Style::default().fg(CLAUDE_ORANGE).bg(bg)),
        Span::styled(" ", Style::default().bg(bg)),
    ];
    spans.extend(line.spans);

    let used = spans.iter().map(|span| span.content.width()).sum::<usize>();
    if used < width as usize {
        spans.push(Span::styled(
            " ".repeat(width as usize - used),
            Style::default().bg(bg),
        ));
    }

    Line::from(spans)
}

fn empty_block_line(width: u16) -> Line<'static> {
    apply_block_style(Line::from(""), width)
}
fn render_attachment_chip(kind: &str, label: String) -> Line<'static> {
    render_attachment_chip_colored(kind, label, CLAUDE_ORANGE, Color::Black)
}

fn render_file_chip(label: String) -> Line<'static> {
    // Use a steel-blue badge with white text for file injections — distinct from
    // the orange img/doc chips and readable on dark terminal backgrounds.
    render_attachment_chip_colored("file", label, Color::Rgb(51, 102, 170), Color::White)
}

fn render_attachment_chip_colored(
    kind: &str,
    label: String,
    badge_bg: Color,
    badge_fg: Color,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {} ", kind),
            Style::default()
                .fg(badge_fg)
                .bg(badge_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", label),
            Style::default().fg(TRANSCRIPT_MUTED).bg(TRANSCRIPT_CHIP_BG),
        ),
    ])
}

fn user_metadata_line(_meta: Option<&TurnMetadata>) -> Option<Line<'static>> {
    // User prompt line has no metadata — mode/model/duration are shown on the
    // assistant footer instead (matching OpenCode's layout).
    None
}

pub fn render_transcript_assistant_meta(
    meta: Option<&TurnMetadata>,
    last_assistant: Option<&Message>,
    accent: Color,
) -> Option<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    // Interrupted status — mode, model, and duration are already displayed in
    // the status line above the prompt.
    if meta.is_some_and(|m| m.interrupted) {
        spans.push(Span::styled(
            "   \u{25a3} ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            "interrupted",
            Style::default().fg(TRANSCRIPT_MUTED),
        ));
    }

    // Turn observability badge for composite-provider turns: which upstream
    // actually served the answer and what it cost (free providers price at
    // $0.00, so the cost segment is omitted there). Persisted per message via
    // `Message::turn_meta` / `Message::cost`.
    if let Some(msg) = last_assistant {
        let upstream = msg.turn_meta.as_ref().and_then(|t| t.upstream_id.clone());
        if let Some(up) = upstream {
            let mut parts = vec![up];
            if let Some(cost) = msg.cost.as_ref().map(|c| c.cost_usd).filter(|c| *c > 0.0) {
                parts.push(format!("${cost:.4}"));
            }
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("   \u{2937} {}", parts.join(" \u{b7} ")),
                Style::default().fg(TRANSCRIPT_MUTED),
            ));
        }
    }

    if spans.is_empty() {
        return None;
    }
    Some(Line::from(spans))
}

pub fn render_transcript_live_text(text: &str, width: u16) -> Vec<Line<'static>> {
    indent_lines(
        render_markdown(text, width.saturating_sub(4)),
        "   ",
        Style::default(),
        TRANSCRIPT_TEXT,
    )
}

/// Segments of a potentially file-injected text block.
enum TextSegment {
    Plain(String),
    FileBlock(String), // path attribute value
}

/// Normalize `@token` references in user text when those files were already shown
/// as chips. Replaces `@long/absolute/path/file.rs` with just `@file.rs` so the
/// text stays readable ("Delete @file.rs" still makes sense) without showing the
/// full path noise.
fn normalize_at_tokens(text: &str, injected: &std::collections::HashSet<String>) -> String {
    let mut result = String::with_capacity(text.len());
    for word in text.split_inclusive(|c: char| c.is_whitespace()) {
        let trimmed = word.trim_end_matches(|c: char| c.is_whitespace());
        let trailing: &str = &word[trimmed.len()..];

        if trimmed.starts_with('@') && trimmed.len() > 1 {
            let mut path_part = trimmed[1..].to_string();
            // Strip trailing punctuation (same logic as parse_at_refs)
            while !path_part.is_empty()
                && path_part.ends_with(|c: char| c.is_ascii_punctuation())
                && !path_part.ends_with('/')
            {
                path_part.pop();
            }
            let punct_suffix = &trimmed[1 + path_part.len()..];

            let basename = std::path::Path::new(&path_part)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path_part.clone());

            let matches = injected.iter().any(|p| {
                p == &path_part
                    || std::path::Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().as_ref() == path_part.as_str())
                        .unwrap_or(false)
                    || p.ends_with(&format!("/{}", path_part))
            });

            if matches && basename != path_part {
                // Shorten to just the filename
                result.push('@');
                result.push_str(&basename);
                result.push_str(punct_suffix);
                result.push_str(trailing);
                continue;
            }
        }
        result.push_str(word);
    }
    result
}

/// Split text that may contain `<file path="...">...</file>` injection blocks
/// into alternating Plain and FileBlock segments.
fn extract_file_segments(text: &str) -> Vec<TextSegment> {
    let mut result = Vec::new();
    let mut remaining = text;
    const OPEN: &str = "<file path=\"";
    const CLOSE: &str = "</file>";

    while let Some(start) = remaining.find(OPEN) {
        if start > 0 {
            result.push(TextSegment::Plain(remaining[..start].to_string()));
        }
        let after = &remaining[start + OPEN.len()..];
        if let Some(path_end) = after.find('"') {
            let path = after[..path_end].to_string();
            let after_open_tag = &remaining[start..];
            if let Some(close_pos) = after_open_tag.find(CLOSE) {
                let consumed = start + close_pos + CLOSE.len();
                // skip one trailing newline if present
                let consumed = if remaining[consumed..].starts_with('\n') {
                    consumed + 1
                } else {
                    consumed
                };
                result.push(TextSegment::FileBlock(path));
                remaining = &remaining[consumed..];
            } else {
                result.push(TextSegment::Plain(remaining[start..].to_string()));
                remaining = "";
                break;
            }
        } else {
            result.push(TextSegment::Plain(remaining[start..].to_string()));
            remaining = "";
            break;
        }
    }

    if !remaining.is_empty() {
        result.push(TextSegment::Plain(remaining.to_string()));
    }
    result
}

pub fn render_transcript_user_message(
    msg: &Message,
    meta: Option<&TurnMetadata>,
    width: u16,
) -> Vec<Line<'static>> {
    // Goal-event messages injected by the /goal machinery render as a compact
    // event block, not as a user input bubble. The same applies to the user's
    // own `/goal <objective>` typing — replace it with the yellow GOAL ACTIVE
    // badge so the raw slash command doesn't sit next to the `[Goal started]`
    // event the machinery injects right after.
    if let Some(ContentBlock::Text { text }) = msg.content_blocks().into_iter().next() {
        if is_goal_event_message(&text) {
            return render_goal_event(&text, width);
        }
        if let Some(objective) = extract_goal_slash_objective(&text) {
            return render_goal_active_block(&objective);
        }
    }

    let inner_width = width.saturating_sub(4).max(10);
    let mut lines = Vec::new();
    let mut pending_text = String::new();

    // Collect the absolute paths of every injected file so we can strip the
    // corresponding @token references from the user's original text block.
    let injected_paths: std::collections::HashSet<String> = msg
        .content_blocks()
        .iter()
        .filter_map(|b| {
            if let ContentBlock::Text { text } = b {
                if text.contains("<file path=\"") {
                    Some(text)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .flat_map(|text| {
            extract_file_segments(text).into_iter().filter_map(|s| {
                if let TextSegment::FileBlock(p) = s {
                    Some(p)
                } else {
                    None
                }
            })
        })
        .collect();

    let flush_text = |buffer: &mut String, target: &mut Vec<Line<'static>>| {
        if buffer.is_empty() {
            return;
        }
        target.extend(render_user_text_with_ctx(
            buffer,
            &RenderContext {
                width: inner_width,
                ..RenderContext::default()
            },
        ));
        buffer.clear();
    };

    for block in msg.content_blocks() {
        match block {
            ContentBlock::Text { text } => {
                if text.contains("<file path=\"") {
                    flush_text(&mut pending_text, &mut lines);
                    for segment in extract_file_segments(&text) {
                        match segment {
                            TextSegment::FileBlock(path) => {
                                let label = std::path::Path::new(&path)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or(path);
                                lines.push(render_file_chip(label));
                            }
                            TextSegment::Plain(t) => {
                                if !t.trim().is_empty() {
                                    if !pending_text.is_empty() {
                                        pending_text.push('\n');
                                    }
                                    pending_text.push_str(&t);
                                }
                            }
                        }
                    }
                } else if !injected_paths.is_empty() {
                    // Shorten @long/path/file.rs → @file.rs since the chips already
                    // show the full path context.
                    let cleaned = normalize_at_tokens(&text, &injected_paths);
                    let trimmed = cleaned.trim();
                    if !trimmed.is_empty() {
                        if !pending_text.is_empty() {
                            pending_text.push('\n');
                        }
                        pending_text.push_str(trimmed);
                    }
                } else {
                    if !pending_text.is_empty() {
                        pending_text.push('\n');
                    }
                    pending_text.push_str(&text);
                }
            }
            ContentBlock::Image { source } => {
                flush_text(&mut pending_text, &mut lines);
                let label = source
                    .media_type
                    .or(source.url)
                    .unwrap_or_else(|| "pasted image".to_string());
                lines.push(render_attachment_chip("img", label));
            }
            ContentBlock::Document {
                title,
                context,
                source,
                ..
            } => {
                flush_text(&mut pending_text, &mut lines);
                let label = title
                    .or(context)
                    .or(source.url)
                    .or(source.media_type)
                    .unwrap_or_else(|| "attached document".to_string());
                lines.push(render_attachment_chip("doc", label));
            }
            ContentBlock::UserLocalCommandOutput { command, output } => {
                flush_text(&mut pending_text, &mut lines);
                lines.extend(render_user_local_command_output(&command, &output, 30));
            }
            ContentBlock::UserCommand { name, args } => {
                flush_text(&mut pending_text, &mut lines);
                lines.extend(render_user_command(&name, &args));
            }
            ContentBlock::UserMemoryInput { key, value } => {
                flush_text(&mut pending_text, &mut lines);
                lines.extend(render_user_memory_input(&key, &value));
            }
            ContentBlock::SystemAPIError {
                message,
                retry_secs,
            } => {
                flush_text(&mut pending_text, &mut lines);
                lines.extend(render_system_api_error(&message, retry_secs));
            }
            ContentBlock::CollapsedReadSearch {
                tool_name,
                paths,
                n_hidden,
            } => {
                flush_text(&mut pending_text, &mut lines);
                let path_refs: Vec<&str> = paths.iter().map(|path| path.as_str()).collect();
                lines.extend(render_collapsed_read_search(
                    &tool_name, &path_refs, n_hidden,
                ));
            }
            ContentBlock::TaskAssignment {
                id,
                subject,
                description,
            } => {
                flush_text(&mut pending_text, &mut lines);
                lines.extend(render_task_assignment(&id, &subject, &description));
            }
            ContentBlock::ToolUse { name, input, .. } => {
                flush_text(&mut pending_text, &mut lines);
                lines.extend(render_tool_use_inner(&name, &input));
            }
            ContentBlock::ToolResult {
                tool_use_id: _,
                content,
                is_error,
            } => {
                flush_text(&mut pending_text, &mut lines);
                let text = tool_result_text(&content);
                let rendered = if is_error.unwrap_or(false) {
                    render_tool_result_error(&text)
                } else {
                    render_tool_result_success(&text, false)
                };
                lines.extend(rendered);
            }
            ContentBlock::Thinking { thinking, .. } => {
                flush_text(&mut pending_text, &mut lines);
                lines.extend(render_transcript_reasoning_block(
                    &thinking,
                    false,
                    inner_width,
                ));
            }
            ContentBlock::RedactedThinking { .. } => {
                flush_text(&mut pending_text, &mut lines);
                lines.push(Line::from(vec![Span::styled(
                    "Thinking hidden".to_string(),
                    Style::default()
                        .fg(TRANSCRIPT_MUTED)
                        .add_modifier(Modifier::ITALIC),
                )]));
            }
        }
    }
    flush_text(&mut pending_text, &mut lines);

    if let Some(meta_line) = user_metadata_line(meta) {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(meta_line);
    }

    if lines.is_empty() {
        lines.push(Line::from(""));
    }

    let mut wrapped = Vec::with_capacity(lines.len() + 2);
    wrapped.push(empty_block_line(width));
    wrapped.extend(lines.into_iter().map(|line| apply_block_style(line, width)));
    wrapped.push(empty_block_line(width));
    wrapped
}

/// Stable hash of a thinking block's full text, used to track per-block
/// expansion. Shared by the transcript renderers and the Ctrl+O
/// expand/collapse-all toggle so both address blocks by the same key.
pub fn thinking_block_hash(thinking: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    thinking.hash(&mut h);
    h.finish()
}

/// Stable hash identifying a group of parallel tool calls. Derived from the
/// members' tool-use ids (unique per turn) so it is stable across renders and
/// distinct from thinking-block hashes.
pub fn grouped_tool_use_hash(ids: &[&str], names: &[&str]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    for (id, name) in ids.iter().zip(names) {
        id.hash(&mut h);
        name.hash(&mut h);
        h.write_u8(0);
    }
    h.finish()
}

/// Find maximal runs of >=2 consecutive `ToolUse` blocks in a message
/// (parallel tool calls emitted in one assistant turn).
///
/// Returns `(group_hash, start_block_index, run_len)` per run.
pub fn grouped_tool_use_runs(msg: &Message) -> Vec<(u64, usize, usize)> {
    let blocks = msg.content_blocks();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        if matches!(blocks[i], ContentBlock::ToolUse { .. }) {
            let mut j = i + 1;
            while j < blocks.len() && matches!(blocks[j], ContentBlock::ToolUse { .. }) {
                j += 1;
            }
            if j - i >= 2 {
                let ids: Vec<&str> = blocks[i..j]
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                        _ => None,
                    })
                    .collect();
                let names: Vec<&str> = blocks[i..j]
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                        _ => None,
                    })
                    .collect();
                runs.push((grouped_tool_use_hash(&ids, &names), i, j - i));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    runs
}

/// Stable hash for an LSP diagnostics block's full text. Namespaced with a
/// distinct prefix so it can't collide with thinking-block or grouped-tool
/// hashes stored in the same `expanded_thinking` set.
pub fn diagnostics_block_hash(text: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    "lsp-diag".hash(&mut h);
    text.hash(&mut h);
    h.finish()
}

/// Detect LSP diagnostics output as produced by `LspManager::format_diagnostics`
/// (`[ERROR] file:line:col - message` lines). Returns `(issue_count, file_count)`
/// when every non-empty line matches that shape, else `None`.
///
/// Content-based (not tool-name-gated) so the Ctrl+O toggle, which has no
/// access to `tool_names`, stays in lockstep with the renderer. The format is
/// distinctive enough that ordinary tool output can't false-positive: a Bash
/// result containing `[ERROR] file:1:2 - msg` mixed with other lines fails the
/// all-lines check and renders normally.
pub fn parse_lsp_diagnostics(text: &str) -> Option<(usize, usize)> {
    let mut issues = 0usize;
    let mut files: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rest = line.strip_prefix('[')?;
        let (severity, rest) = rest.split_once(']')?;
        if !matches!(severity, "ERROR" | "WARNING" | "INFO" | "HINT") {
            return None;
        }
        let rest = rest.trim_start();
        let (loc, _msg) = rest.split_once(" - ")?;
        let mut parts = loc.rsplitn(3, ':');
        let col = parts.next()?;
        let line_no = parts.next()?;
        let file = parts.next()?;
        if !col.chars().all(|c| c.is_ascii_digit()) || !line_no.chars().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        files.insert(file);
        issues += 1;
    }
    if issues == 0 {
        None
    } else {
        Some((issues, files.len()))
    }
}

/// Every collapsible-block hash in a message: thinking blocks, LSP diagnostics
/// summaries, and grouped parallel tool-use runs. Shared by the transcript
/// renderer and the Ctrl+O expand/collapse-all toggle so both address blocks
/// by the same key.
pub fn expandable_block_hashes(msg: &Message) -> Vec<u64> {
    let mut hashes = Vec::new();
    for block in msg.content_blocks() {
        match block {
            ContentBlock::Thinking { thinking, .. } => hashes.push(thinking_block_hash(&thinking)),
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                if !is_error.unwrap_or(false) {
                    let text = tool_result_text(&content);
                    if parse_lsp_diagnostics(&text).is_some() {
                        hashes.push(diagnostics_block_hash(&text));
                    }
                }
            }
            _ => {}
        }
    }
    hashes.extend(grouped_tool_use_runs(msg).into_iter().map(|(h, _, _)| h));
    hashes
}

pub fn render_transcript_reasoning_block(
    text: &str,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let heading = reasoning_heading(text).unwrap_or_else(|| "Thinking".to_string());
    let chevron = if expanded { "▼" } else { "▶" };
    let mut header_spans = vec![
        Span::styled(
            format!("  {} Thinking: ", chevron),
            Style::default()
                .fg(TRANSCRIPT_MUTED)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(
            heading,
            Style::default()
                .fg(TRANSCRIPT_SUBTLE)
                .add_modifier(Modifier::ITALIC),
        ),
    ];
    // Mirrors the spec's collapsed "(ctrl+o to expand)" affordance: the same
    // key (or clicking the header) expands the block.
    if !expanded {
        header_spans.push(Span::styled(
            " (ctrl+o to expand)",
            Style::default()
                .fg(TRANSCRIPT_SUBTLE)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    lines.push(Line::from(header_spans));

    if expanded {
        let rendered = render_markdown(text, width.saturating_sub(6));
        lines.extend(indent_lines(
            rendered,
            "    ",
            Style::default(),
            TRANSCRIPT_MUTED,
        ));
    }

    lines
}

/// Render the thinking content body (without header) for live streaming display.
pub fn render_thinking_live_content(text: &str, width: u16) -> Vec<Line<'static>> {
    let rendered = render_markdown(text, width.saturating_sub(6));
    indent_lines(rendered, "    ", Style::default(), TRANSCRIPT_MUTED)
}

/// Returns lines for each content block with an optional thinking hash tag.
/// The hash is `Some(hash)` only for the header line of a Thinking block,
/// enabling click-to-expand in the TUI.
pub fn render_transcript_assistant_message_tagged(
    msg: &Message,
    ctx: &RenderContext,
) -> Vec<(Line<'static>, Option<u64>)> {
    let mut out: Vec<(Line<'static>, Option<u64>)> = Vec::new();
    let mut pending_text = String::new();
    let mut ranked_followups = Vec::new();

    let mut flush_text =
        |buffer: &mut String, target: &mut Vec<(Line<'static>, Option<u64>)>, width: u16| {
            if buffer.is_empty() {
                return;
            }
            let parsed = clawde_core::parse_ranked_followups(buffer);
            if parsed.had_block {
                if !parsed.followups.is_empty() {
                    ranked_followups.extend(parsed.followups);
                }
                *buffer = parsed.visible_text;
            }
            if !buffer.is_empty() {
                for line in render_transcript_live_text(buffer, width) {
                    target.push((line, None));
                }
            }
            buffer.clear();
        };

    // Parallel tool calls (>=2 consecutive ToolUse blocks) collapse into one
    // expandable row; the precomputed runs tell us which blocks belong to one.
    let runs = grouped_tool_use_runs(msg);
    for (idx, block) in msg.content_blocks().into_iter().enumerate() {
        match block {
            ContentBlock::Text { text } => {
                if !pending_text.is_empty() {
                    pending_text.push('\n');
                }
                pending_text.push_str(&text);
            }
            ContentBlock::Thinking { thinking, .. } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                let thinking_hash = thinking_block_hash(&thinking);
                let expanded = ctx.show_thinking || ctx.expanded_thinking.contains(&thinking_hash);
                let block_lines = render_transcript_reasoning_block(&thinking, expanded, ctx.width);
                for (i, line) in block_lines.into_iter().enumerate() {
                    // Tag only the header line (index 0) with the hash so it's clickable.
                    out.push((line, if i == 0 { Some(thinking_hash) } else { None }));
                }
            }
            ContentBlock::RedactedThinking { .. } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                out.push((
                    Line::from(vec![Span::styled(
                        "  Thinking hidden".to_string(),
                        Style::default()
                            .fg(TRANSCRIPT_MUTED)
                            .add_modifier(Modifier::ITALIC),
                    )]),
                    None,
                ));
            }
            ContentBlock::ToolUse { name, input, .. } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                // Member of a parallel run: only the run start renders the
                // grouped header (click / Ctrl+O to expand); later members are
                // covered by it.
                if let Some(&(group_hash, start, len)) = runs.iter().find(|&&(_, s, _)| s == idx) {
                    if idx == start {
                        let group_blocks = msg.content_blocks();
                        let names: Vec<&str> = group_blocks[start..start + len]
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                                _ => None,
                            })
                            .collect();
                        let expanded =
                            ctx.show_thinking || ctx.expanded_thinking.contains(&group_hash);
                        if expanded {
                            // Show every tool's own header (with argument summary).
                            for b in &group_blocks[start..start + len] {
                                if let ContentBlock::ToolUse { name, input, .. } = b {
                                    for line in indent_lines(
                                        render_tool_use_inner(name, input),
                                        "   ",
                                        Style::default(),
                                        TRANSCRIPT_TEXT,
                                    ) {
                                        out.push((line, None));
                                    }
                                }
                            }
                        } else {
                            let mut lines = render_grouped_tool_use(&names, false);
                            let mut header = lines.remove(0);
                            // Mirrors the thinking-block affordance: same key
                            // (or clicking the header) expands the group.
                            header.spans.push(Span::styled(
                                " (ctrl+o to expand)".to_string(),
                                Style::default()
                                    .fg(TRANSCRIPT_SUBTLE)
                                    .add_modifier(Modifier::ITALIC),
                            ));
                            out.push((header, Some(group_hash)));
                        }
                    }
                } else {
                    for line in indent_lines(
                        render_tool_use_inner(&name, &input),
                        "   ",
                        Style::default(),
                        TRANSCRIPT_TEXT,
                    ) {
                        out.push((line, None));
                    }
                }
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                let text = tool_result_text(&content);
                // Autopilot deferrals/denials render as dimmed system
                // annotations rather than red errors (Phase 4C).
                if let Some(annotation) = autopilot_annotation(&text) {
                    for line in annotation {
                        out.push((line, None));
                    }
                    continue;
                }
                let tool_name = ctx.tool_names.get(&tool_use_id).map(|name| name.as_str());
                // LSP diagnostics collapse to a one-line summary (spec's
                // DiagnosticsDisplay): "Found N diagnostic issue(s) in M file(s)"
                // + (ctrl+o to expand). Expanded shows the per-file detail.
                let parsed_diag = if !is_error.unwrap_or(false) {
                    parse_lsp_diagnostics(&text)
                        .map(|(issues, files)| (diagnostics_block_hash(&text), issues, files))
                } else {
                    None
                };
                // If collapsed, swap the whole block for a one-line summary.
                // If expanded, keep the full detail but remember the hash so the
                // first rendered line stays click-to-collapse (same affordance
                // as thinking-block headers).
                let diagnostics_hash = match parsed_diag {
                    Some((hash, issues, files)) if !ctx.expanded_thinking.contains(&hash) => {
                        let summary = Line::from(vec![
                            Span::styled(
                                format!(
                                    "  ▶ Found {} diagnostic issue{} in {} file{}",
                                    issues,
                                    if issues == 1 { "" } else { "s" },
                                    files,
                                    if files == 1 { "" } else { "s" },
                                ),
                                Style::default().fg(TRANSCRIPT_SUBTLE),
                            ),
                            Span::styled(
                                " (ctrl+o to expand)".to_string(),
                                Style::default()
                                    .fg(TRANSCRIPT_SUBTLE)
                                    .add_modifier(Modifier::ITALIC),
                            ),
                        ]);
                        out.push((summary, Some(hash)));
                        continue;
                    }
                    Some((hash, _, _)) => Some(hash),
                    None => None,
                };
                let rendered = if is_error.unwrap_or(false) {
                    render_tool_result_error(&text)
                } else {
                    match tool_name {
                        Some("Bash") | Some("PowerShell") => {
                            render_bash_output_block(&text, TOOL_RESULT_MAX_LINES)
                        }
                        Some("Read") => render_file_read_result(&text),
                        Some("Edit") => render_file_op_result(false),
                        Some("Write") => render_file_op_result(true),
                        _ => render_tool_result_success(&text, false),
                    }
                };
                for (i, line) in indent_lines(rendered, "   ", Style::default(), TRANSCRIPT_TEXT)
                    .into_iter()
                    .enumerate()
                {
                    // Tag only the first line of an expanded diagnostics block
                    // (or any single-line result) so it stays clickable.
                    let tag = if i == 0 { diagnostics_hash } else { None };
                    out.push((line, tag));
                }
            }
            ContentBlock::Image { source } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                let label = render_image(&source).unwrap_or_else(|| {
                    source
                        .media_type
                        .or(source.url)
                        .unwrap_or_else(|| "assistant image".to_string())
                });
                for line in indent_lines(
                    vec![render_attachment_chip("img", label)],
                    "   ",
                    Style::default(),
                    TRANSCRIPT_TEXT,
                ) {
                    out.push((line, None));
                }
            }
            ContentBlock::Document {
                title,
                context,
                source,
                ..
            } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                let label = title
                    .or(context)
                    .or(source.url)
                    .or(source.media_type)
                    .unwrap_or_else(|| "attached document".to_string());
                for line in indent_lines(
                    vec![render_attachment_chip("doc", label)],
                    "   ",
                    Style::default(),
                    TRANSCRIPT_TEXT,
                ) {
                    out.push((line, None));
                }
            }
            ContentBlock::UserLocalCommandOutput { command, output } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                for line in indent_lines(
                    render_user_local_command_output(&command, &output, 30),
                    "   ",
                    Style::default(),
                    TRANSCRIPT_TEXT,
                ) {
                    out.push((line, None));
                }
            }
            ContentBlock::UserCommand { name, args } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                for line in indent_lines(
                    render_user_command(&name, &args),
                    "   ",
                    Style::default(),
                    TRANSCRIPT_TEXT,
                ) {
                    out.push((line, None));
                }
            }
            ContentBlock::UserMemoryInput { key, value } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                for line in indent_lines(
                    render_user_memory_input(&key, &value),
                    "   ",
                    Style::default(),
                    TRANSCRIPT_TEXT,
                ) {
                    out.push((line, None));
                }
            }
            ContentBlock::SystemAPIError {
                message,
                retry_secs,
            } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                for line in indent_lines(
                    render_system_api_error(&message, retry_secs),
                    "   ",
                    Style::default(),
                    TRANSCRIPT_TEXT,
                ) {
                    out.push((line, None));
                }
            }
            ContentBlock::CollapsedReadSearch {
                tool_name,
                paths,
                n_hidden,
            } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                let path_refs: Vec<&str> = paths.iter().map(|path| path.as_str()).collect();
                for line in indent_lines(
                    render_collapsed_read_search(&tool_name, &path_refs, n_hidden),
                    "   ",
                    Style::default(),
                    TRANSCRIPT_TEXT,
                ) {
                    out.push((line, None));
                }
            }
            ContentBlock::TaskAssignment {
                id,
                subject,
                description,
            } => {
                flush_text(&mut pending_text, &mut out, ctx.width);
                for line in indent_lines(
                    render_task_assignment(&id, &subject, &description),
                    "   ",
                    Style::default(),
                    TRANSCRIPT_TEXT,
                ) {
                    out.push((line, None));
                }
            }
        }
    }

    // Flush the final text buffer. Parsing also happens at earlier text/block
    // boundaries, so a metadata block can never leak merely because a tool or
    // thinking block followed it.
    flush_text(&mut pending_text, &mut out, ctx.width);
    if !ranked_followups.is_empty() {
        for line in render_ranked_followups(&ranked_followups, ctx.followup_selected) {
            out.push((line, None));
        }
    }
    out
}

fn title_case_word(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

pub fn extract_tool_summary(tool_name: &str, input: &serde_json::Value) -> String {
    fn str_field<'a>(input: &'a serde_json::Value, key: &str) -> &'a str {
        input.get(key).and_then(|v| v.as_str()).unwrap_or("")
    }
    fn truncate(s: &str, n: usize) -> String {
        let s = s.trim();
        let chars: Vec<char> = s.chars().collect();
        if chars.len() > n {
            format!("{}\u{2026}", chars[..n].iter().collect::<String>())
        } else {
            s.to_string()
        }
    }
    match tool_name.to_ascii_lowercase().as_str() {
        "bash" | "powershell" => {
            let cmd = str_field(input, "command");
            truncate(cmd.lines().next().unwrap_or(""), 60)
        }
        "read" => clawde_core::truncate::truncate_path(str_field(input, "file_path"), 60),
        "edit" => clawde_core::truncate::truncate_path(str_field(input, "file_path"), 60),
        "write" => clawde_core::truncate::truncate_path(str_field(input, "file_path"), 60),
        "glob" => truncate(str_field(input, "pattern"), 60),
        "grep" => truncate(str_field(input, "pattern"), 60),
        "webfetch" => truncate(str_field(input, "url"), 60),
        "websearch" => truncate(str_field(input, "query"), 60),
        "task" | "agent" => {
            let task = str_field(input, "task");
            let task = if task.is_empty() {
                str_field(input, "description")
            } else {
                task
            };
            truncate(task.lines().next().unwrap_or(""), 60)
        }
        _ => {
            // First string value from the input object
            if let Some(obj) = input.as_object() {
                for v in obj.values() {
                    if let Some(s) = v.as_str() {
                        return truncate(s, 60);
                    }
                }
            }
            String::new()
        }
    }
}

pub fn subagent_title(input: &serde_json::Value) -> String {
    let label = input
        .get("subagent_type")
        .and_then(|value| value.as_str())
        .map(title_case_word)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "General".to_string());
    format!("{label} agent")
}

/// Render a compact tool-use block that matches the newer transcript language.
pub fn render_tool_use(tool_name: &str, input_json: &str) -> Vec<Line<'static>> {
    let input: serde_json::Value =
        serde_json::from_str(input_json).unwrap_or(serde_json::Value::Null);
    render_tool_use_inner(tool_name, &input)
}

fn render_tool_use_inner(tool_name: &str, input: &serde_json::Value) -> Vec<Line<'static>> {
    let summary = extract_tool_summary(tool_name, input);
    let mut lines = Vec::new();
    let title = match tool_name.to_ascii_lowercase().as_str() {
        "bash" | "powershell" => "Running command",
        "read" => "Reading file",
        "write" => "Writing file",
        "edit" => "Editing file",
        "glob" | "list" => "Listing files",
        "grep" => "Searching code",
        "webfetch" => "Fetching page",
        "websearch" => "Searching web",
        "task" | "agent" => {
            return {
                let mut task_lines = Vec::new();
                task_lines.push(Line::from(vec![
                    Span::styled("  ~ ".to_string(), Style::default().fg(CLAUDE_ORANGE)),
                    Span::styled(
                        subagent_title(input),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                if !summary.is_empty() {
                    task_lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(summary, Style::default().fg(TRANSCRIPT_MUTED)),
                    ]));
                }
                task_lines
            }
        }
        _ => tool_name,
    };

    lines.push(Line::from(vec![
        Span::styled("  ~ ".to_string(), Style::default().fg(CLAUDE_ORANGE)),
        Span::styled(
            title.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if !summary.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(summary, Style::default().fg(TRANSCRIPT_MUTED)),
        ]));
    }

    if matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "bash" | "powershell"
    ) {
        let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        for (i, cmd_line) in command.lines().enumerate() {
            if i >= 2 {
                break;
            }
            let display: String = cmd_line.chars().take(160).collect();
            let display = if cmd_line.chars().count() > 160 {
                format!("{}\u{2026}", display)
            } else {
                display
            };
            lines.push(Line::from(vec![
                Span::styled(
                    "    $ ".to_string(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    display,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }

    lines
}

/// Render a file-read tool result: `Read N lines` summary.
fn render_file_read_result(output: &str) -> Vec<Line<'static>> {
    let n = output.lines().count();
    vec![Line::from(vec![Span::styled(
        format!("  Read {} line{}", n, if n == 1 { "" } else { "s" }),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )])]
}

/// Render a file-edit/write tool result: `Updated file` or `Created file`.
fn render_file_op_result(is_create: bool) -> Vec<Line<'static>> {
    let action = if is_create { "Created" } else { "Updated" };
    vec![Line::from(vec![Span::styled(
        format!("  {} file", action),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )])]
}

/// Render a tool result (success variant) — generic fallback.
pub fn render_tool_result_success(output: &str, truncated: bool) -> Vec<Line<'static>> {
    let total_lines = output.lines().count();
    // Use explicit Gray (brighter than terminal default DarkGray) so tool
    // output stays legible on themes where the default fg gets dimmed by
    // surrounding styles. Issue #149: tool result text contrast was too low.
    let body_style = Style::default().fg(Color::Gray);
    let mut lines: Vec<Line<'static>> = output
        .lines()
        .enumerate()
        .take_while(|(i, _)| *i < TOOL_RESULT_MAX_LINES)
        .map(|(_, l)| {
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(l.to_string(), body_style),
            ])
        })
        .collect();
    if total_lines > TOOL_RESULT_MAX_LINES {
        let remaining = total_lines - TOOL_RESULT_MAX_LINES;
        lines.push(Line::from(vec![Span::styled(
            format!("  ... {} more lines", remaining),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )]));
    }
    if truncated {
        lines.push(Line::from(vec![Span::styled(
            "  ... output truncated".to_string(),
            Style::default().fg(Color::DarkGray),
        )]));
    }
    lines
}

/// Render a tool result (error variant).
/// Render an autopilot deferral/denial tool result as a dimmed system
/// annotation instead of a red error block (Phase 4C). Returns `None` when the
/// text is not an autopilot marker, so ordinary errors keep their normal
/// rendering.
pub fn autopilot_annotation(text: &str) -> Option<Vec<Line<'static>>> {
    let trimmed = text.trim();
    let (tag, rest) = if let Some(rest) = trimmed
        .strip_prefix("Deferred for user review as ")
        .or_else(|| trimmed.strip_prefix("Question deferred for user review as "))
    {
        let id = rest.split('.').next().unwrap_or("?").trim();
        let suffix = rest.split_once('.').map(|(_, s)| s).unwrap_or("").trim();
        (format!("Autopilot deferred {id}"), suffix)
    } else if trimmed.contains("is classified as irreversible and cannot run under autopilot")
        || trimmed.contains("the autopilot review queue is full")
    {
        ("Autopilot denied".to_string(), trimmed)
    } else {
        return None;
    };

    let style = Style::default()
        .fg(TRANSCRIPT_MUTED)
        .add_modifier(Modifier::DIM);
    let mut lines = vec![Line::from(vec![Span::styled(format!("  [{tag}]"), style)])];
    if !rest.is_empty() {
        for line in rest.lines().take(3) {
            lines.push(Line::from(vec![Span::styled(format!("    {line}"), style)]));
        }
    }
    Some(lines)
}

pub fn render_tool_result_error(error: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // Use orange instead of red for color-blind accessibility
    let error_color = Color::Rgb(255, 140, 0); // Orange
    lines.push(Line::from(vec![Span::styled(
        "  Error",
        Style::default()
            .fg(error_color)
            .add_modifier(Modifier::BOLD),
    )]));
    for line in error.lines().take(10) {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(line.to_string(), Style::default().fg(error_color)),
        ]));
    }
    lines
}

/// Render a cancelled tool result.
pub fn render_tool_result_cancelled(tool_name: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        format!("  \u{2717} {} \u{2014} cancelled", tool_name),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    )])]
}

/// Render a rejected (interrupted) tool result with reason.
pub fn render_tool_result_rejected(tool_name: &str, reason: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            format!("  \u{2717} {} \u{2014} interrupted", tool_name),
            Style::default().fg(CLAUDE_ORANGE),
        )]),
        Line::from(vec![Span::styled(
            format!("    {}", reason),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )]),
    ]
}

/// Render an attachment message (skill listing, agent listing, MCP instructions, hook results, etc.)
pub fn render_attachment_message(
    kind_label: &str,
    content: &str,
    width: u16,
) -> Vec<Line<'static>> {
    // Reserve space for the "  [label] " prefix and a small margin.
    let prefix_len = kind_label.len() + 6; // "  [label] "
    let preview_max = (width as usize).saturating_sub(prefix_len).clamp(20, 120);
    let preview: String = content.chars().take(preview_max).collect();
    let preview = if content.chars().count() > preview_max {
        format!("{preview}\u{2026}")
    } else {
        preview
    };
    vec![Line::from(vec![
        Span::styled(
            format!("  [{kind_label}] "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(preview, Style::default().fg(Color::White)),
    ])]
}

/// Render an advisor status line.
pub fn render_advisor_message(is_loading: bool, model_name: Option<&str>) -> Vec<Line<'static>> {
    let model_suffix = model_name.map(|m| format!(" ({})", m)).unwrap_or_default();
    if is_loading {
        vec![Line::from(vec![Span::styled(
            format!("  \u{25cc} Advising\u{2026}{}", model_suffix),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        )])]
    } else {
        vec![Line::from(vec![Span::styled(
            format!("  \u{2713} Advisor reviewed{}", model_suffix),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )])]
    }
}

/// Render an agent notification line.
pub fn render_agent_notification(agent_name: &str, message: &str) -> Vec<Line<'static>> {
    render_agent_notification_with_severity(agent_name, message, "info")
}

/// Render an agent notification line with a severity level.
/// severity: "info" (cyan), "warn" (yellow), "error" (red).
pub fn render_agent_notification_with_severity(
    agent_name: &str,
    message: &str,
    severity: &str,
) -> Vec<Line<'static>> {
    let color = match severity {
        "warn" => Color::Yellow,
        "error" => Color::Red,
        _ => Color::Cyan,
    };
    vec![Line::from(vec![
        Span::styled(
            format!("  [{}] ", agent_name),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(message.to_string(), Style::default().fg(color)),
    ])]
}

/// Render a session shutdown message.
pub fn render_shutdown_message(reason: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            "\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )]),
        Line::from(vec![Span::styled(
            format!(
                "  \u{2014} Session ended: {} \u{2014}",
                reason
            ),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )]),
    ]
}

/// Render a bash command input line with a green `$ ` prefix.
pub fn render_bash_input_line(command: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(
            "  $ ".to_string(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            command.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ])]
}

/// Render bash output lines truncated to `max_lines` with an overflow indicator.
pub fn render_bash_output_block(output: &str, max_lines: usize) -> Vec<Line<'static>> {
    let total = output.lines().count();
    let mut lines: Vec<Line<'static>> = output
        .lines()
        .take(max_lines)
        .map(|l| {
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(l.to_string(), Style::default().fg(Color::Gray)),
            ])
        })
        .collect();
    if total > max_lines {
        let remaining = total - max_lines;
        lines.push(Line::from(vec![Span::styled(
            format!("  ... {} more lines", remaining),
            Style::default().fg(Color::DarkGray),
        )]));
    }
    lines
}

/// Render a plan with numbered steps.
pub fn render_plan_steps(steps: &[String]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        "  Plan:".to_string(),
        Style::default()
            .fg(CLAUDE_ORANGE)
            .add_modifier(Modifier::BOLD),
    )]));
    for (i, step) in steps.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}. ", i + 1), Style::default().fg(CLAUDE_ORANGE)),
            Span::styled(step.clone(), Style::default().fg(Color::White)),
        ]));
    }
    lines
}

/// Render a plan approval prompt.
pub fn render_plan_approval_prompt() -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(
            "  Approve this plan? ".to_string(),
            Style::default()
                .fg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "[y] yes  [n] no  [e] edit".to_string(),
            Style::default().fg(Color::White),
        ),
    ])]
}

/// Render a "compact boundary" separator.
pub fn render_compact_boundary() -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        "----------- context compacted -----------",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )])]
}

/// Render a summary message (post-compact).
pub fn render_summary_message(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        "Summary",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    for line in text.lines() {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
        ]));
    }
    lines
}

/// Render an unseen divider.
pub fn render_unseen_divider(count: usize) -> Vec<Line<'static>> {
    vec![Line::from(vec![Span::styled(
        format!(
            "---- {} new message{} ----",
            count,
            if count == 1 { "" } else { "s" }
        ),
        Style::default().fg(Color::Yellow),
    )])]
}

/// Render a system message (dimmed, italic).
pub fn render_system_message(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| {
            Line::from(vec![Span::styled(
                line.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )])
        })
        .collect()
}

/// Render a thinking block (collapsible - show header only when collapsed).
pub fn render_thinking_block(text: &str, expanded: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let heading = reasoning_heading(text).unwrap_or_else(|| "Thinking".to_string());
    let mut header_spans = vec![
        Span::styled(
            "Thinking: ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(
            heading,
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        ),
    ];
    // Mirrors the spec's collapsed "(ctrl+o to expand)" affordance: the same
    // key (or clicking the header) expands the block.
    if !expanded {
        header_spans.push(Span::styled(
            " (ctrl+o to expand)",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    lines.push(Line::from(header_spans));
    if expanded {
        for line in text.lines() {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(line.to_string(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    lines
}

/// Render a rate-limit warning banner.
pub fn render_rate_limit_banner(retry_after_secs: u64) -> Vec<Line<'static>> {
    render_rate_limit_with_hint(retry_after_secs, false)
}

/// Render a rate-limit warning banner with optional upgrade hint.
pub fn render_rate_limit_with_hint(
    retry_after_secs: u64,
    show_upgrade_hint: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![Span::styled(
            "Rate limit exceeded",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![Span::styled(
            format!("  Retrying in {}s...", retry_after_secs),
            Style::default().fg(Color::Yellow),
        )]),
    ];
    if show_upgrade_hint {
        lines.push(Line::from(vec![Span::styled(
            "  \u{2192} claude.ai/upgrade for higher limits",
            Style::default().fg(Color::DarkGray),
        )]));
    }
    lines
}

/// Render a hook progress line (grey spinner + command).
pub fn render_hook_progress(command: &str, last_line: Option<&str>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("... ", Style::default().fg(Color::DarkGray)),
        Span::styled(command.to_string(), Style::default().fg(Color::DarkGray)),
    ]));
    if let Some(line) = last_line {
        lines.push(Line::from(vec![Span::styled(
            format!("  {}", line),
            Style::default().fg(Color::DarkGray),
        )]));
    }
    lines
}

fn truncate_user_prompt_text(text: &str) -> String {
    if text.len() <= MAX_USER_PROMPT_DISPLAY_CHARS {
        return text.to_string();
    }

    // The *_CHARS constants count characters, not bytes. Slice by chars so a
    // multibyte codepoint straddling the cut never panics (#221).
    let head: String = text.chars().take(TRUNCATE_USER_PROMPT_HEAD_CHARS).collect();
    let tail: String = {
        let total_chars = text.chars().count();
        let skip = total_chars.saturating_sub(TRUNCATE_USER_PROMPT_TAIL_CHARS);
        text.chars().skip(skip).collect()
    };
    let hidden_lines = text
        .chars()
        .take(TRUNCATE_USER_PROMPT_HEAD_CHARS)
        .filter(|c| *c == '\n')
        .count()
        .saturating_sub(tail.chars().filter(|c| *c == '\n').count());

    format!("{head}\n… +{hidden_lines} lines …\n{tail}")
}

fn prefix_message_lines(
    mut rendered: Vec<Line<'static>>,
    role: &Role,
    width: u16,
) -> Vec<Line<'static>> {
    if rendered.is_empty() {
        return rendered;
    }

    let (prefix, prefix_style, body_style) = match role {
        Role::User => (
            "› ",
            Style::default()
                .fg(Color::Rgb(233, 30, 99))
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::White),
        ),
        Role::Assistant => ("", Style::default(), Style::default().fg(Color::White)),
    };

    if !prefix.is_empty() {
        if let Some(first) = rendered.first_mut() {
            let mut spans = Vec::with_capacity(first.spans.len() + 1);
            spans.push(Span::styled(prefix.to_string(), prefix_style));
            spans.extend(first.spans.clone());
            first.spans = spans;
        }
    }

    if *role == Role::User {
        let background = Color::Rgb(52, 52, 52);
        for line in &mut rendered {
            let mut line_width = 0usize;
            for span in &mut line.spans {
                line_width += span.content.width();
                if span.style.fg.is_none() {
                    span.style = body_style;
                }
                span.style = span.style.bg(background);
            }
            let pad = (width as usize).saturating_sub(line_width.min(width as usize));
            if pad > 0 {
                line.spans.push(Span::styled(
                    " ".repeat(pad),
                    Style::default().bg(background),
                ));
            }
        }
    }

    rendered
}

fn flush_text(lines: &mut Vec<Line<'static>>, role: &Role, text: &mut String, ctx: &RenderContext) {
    if text.is_empty() {
        return;
    }

    let rendered = match role {
        Role::User => prefix_message_lines(render_markdown(text, ctx.width), role, ctx.width),
        Role::Assistant => prefix_message_lines(render_assistant_text(text, ctx), role, ctx.width),
    };
    lines.extend(rendered);
    text.clear();
}

fn tool_result_text(content: &ToolResultContent) -> String {
    match content {
        ToolResultContent::Text(text) => text.clone(),
        ToolResultContent::Blocks(blocks) => {
            let joined = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                    ContentBlock::RedactedThinking { .. } => Some("[redacted thinking]"),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if joined.is_empty() {
                "[structured tool result]".to_string()
            } else {
                joined
            }
        }
    }
}

fn render_attachment_line(kind: &str, label: String) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled(
            format!("  {} ", kind),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(label, Style::default().fg(Color::DarkGray)),
    ])]
}

pub fn render_message(msg: &Message, ctx: &RenderContext) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut pending_text = String::new();

    for block in msg.content_blocks() {
        match block {
            ContentBlock::Text { text } => {
                if !pending_text.is_empty() {
                    pending_text.push('\n');
                }
                pending_text.push_str(&text);
            }
            ContentBlock::Thinking { thinking, .. } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                // Compute a stable hash of the thinking content for per-block expansion tracking
                let thinking_hash = thinking_block_hash(&thinking);
                let expanded = ctx.show_thinking || ctx.expanded_thinking.contains(&thinking_hash);
                lines.extend(prefix_message_lines(
                    render_thinking_block(&thinking, expanded),
                    &msg.role,
                    ctx.width,
                ));
            }
            ContentBlock::RedactedThinking { .. } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                lines.extend(prefix_message_lines(
                    vec![Line::from(vec![Span::styled(
                        "Thinking redacted",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )])],
                    &msg.role,
                    ctx.width,
                ));
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                let rendered = render_tool_use_inner(&name, &input);
                // Silence unused-variable warning on id — kept for symmetry with ToolResult lookup.
                let _ = &id;
                lines.extend(prefix_message_lines(rendered, &msg.role, ctx.width));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                let text = tool_result_text(&content);
                let tool_name = ctx.tool_names.get(&tool_use_id).map(|s| s.as_str());
                let rendered = if is_error.unwrap_or(false) {
                    render_tool_result_error(&text)
                } else {
                    match tool_name {
                        Some("Bash") | Some("PowerShell") => {
                            render_bash_output_block(&text, TOOL_RESULT_MAX_LINES)
                        }
                        Some("Read") => render_file_read_result(&text),
                        Some("Edit") => render_file_op_result(false),
                        Some("Write") => render_file_op_result(true),
                        _ => render_tool_result_success(&text, false),
                    }
                };
                lines.extend(prefix_message_lines(rendered, &msg.role, ctx.width));
            }
            ContentBlock::Image { source } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                // Attempt Kitty graphics protocol rendering.  When the
                // terminal supports it and the source carries inline base64
                // data, `render_image` emits the APC escape sequence directly
                // to stdout and returns `None` — nothing more to do for this
                // block.  Otherwise it returns a human-readable fallback
                // string that we display as a normal styled line.
                if let Some(label) = render_image(&source) {
                    lines.extend(prefix_message_lines(
                        render_attachment_line("Image", label),
                        &msg.role,
                        ctx.width,
                    ));
                }
            }
            ContentBlock::Document {
                title,
                context,
                source,
                ..
            } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                let label = title
                    .or(context)
                    .or(source.url)
                    .or(source.media_type)
                    .unwrap_or_else(|| "attached document".to_string());
                lines.extend(prefix_message_lines(
                    render_attachment_line("Document", label),
                    &msg.role,
                    ctx.width,
                ));
            }
            ContentBlock::UserLocalCommandOutput { command, output } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                lines.extend(render_user_local_command_output(&command, &output, 30));
            }
            ContentBlock::UserCommand { name, args } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                lines.extend(render_user_command(&name, &args));
            }
            ContentBlock::UserMemoryInput { key, value } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                lines.extend(render_user_memory_input(&key, &value));
            }
            ContentBlock::SystemAPIError {
                message,
                retry_secs,
            } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                lines.extend(render_system_api_error(&message, retry_secs));
            }
            ContentBlock::CollapsedReadSearch {
                tool_name,
                paths,
                n_hidden,
            } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                let path_refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
                lines.extend(render_collapsed_read_search(
                    &tool_name, &path_refs, n_hidden,
                ));
            }
            ContentBlock::TaskAssignment {
                id,
                subject,
                description,
            } => {
                flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
                lines.extend(render_task_assignment(&id, &subject, &description));
            }
        }
    }

    flush_text(&mut lines, &msg.role, &mut pending_text, ctx);
    lines.push(Line::from(""));
    lines
}

/// Render a system API error block (red-bordered, first 5 lines with \[expand] hint,
/// optional retry countdown).
pub fn render_system_api_error(msg: &str, retry_secs: Option<u32>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        "\u{250c}\u{2500} API Error ",
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )]));
    let all_lines: Vec<&str> = msg.lines().collect();
    let total = all_lines.len();
    for line in all_lines.iter().take(5) {
        lines.push(Line::from(vec![
            Span::styled("\u{2502} ", Style::default().fg(Color::Red)),
            Span::styled(line.to_string(), Style::default().fg(Color::White)),
        ]));
    }
    if total > 5 {
        lines.push(Line::from(vec![Span::styled(
            format!("\u{2502} ... {} more lines [expand]", total - 5),
            Style::default().fg(Color::DarkGray),
        )]));
    }
    lines.push(Line::from(vec![Span::styled(
        "\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
        Style::default().fg(Color::Red),
    )]));
    if let Some(n) = retry_secs {
        lines.push(Line::from(vec![Span::styled(
            format!("  \u{21bb} Retrying in {}s...", n),
            Style::default().fg(Color::Yellow),
        )]));
    }
    lines
}

/// Render a user command invocation (skill invocation display).
/// Shows: `▸ ` in cyan bold + command name in cyan bold + " " + args in white.
///
/// Special case: `/goal <objective>` is replaced with a yellow `GOAL ACTIVE /
/// Objective: <obj>` badge so the raw slash command doesn't sit next to the
/// `[Goal started]` event the machinery injects right after it. Subcommands
/// (`/goal status`, `pause`, `resume`, `clear`, `complete`) keep the normal
/// rendering.
pub fn render_user_command(name: &str, args: &str) -> Vec<Line<'static>> {
    if name == "goal" {
        if let Some(objective) = extract_goal_objective_from_args(args) {
            return render_goal_active_block(&objective);
        }
    }
    vec![Line::from(vec![
        Span::styled(
            "\u{25b8} ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ".to_string(), Style::default()),
        Span::styled(args.to_string(), Style::default().fg(Color::White)),
    ])]
}

/// Recognizes a raw `/goal <objective>` user message. Returns the objective
/// string when the first line is `/goal …` with actual objective text;
/// returns `None` for subcommand forms, no-args, or anything that isn't a
/// `/goal` slash command (including the case where the user pastes a
/// multi-line message with `/goal …` somewhere in the middle).
fn extract_goal_slash_objective(text: &str) -> Option<String> {
    let first_line = text.lines().next()?;
    let rest = first_line
        .trim_start()
        .strip_prefix("/goal")?
        .strip_prefix(|c: char| c.is_whitespace())
        .unwrap_or("");
    let objective = extract_goal_objective_from_args(rest)?;
    // Reject bare `/goal` (no following body) — strip_prefix above returned
    // empty `rest`, which extract_goal_objective_from_args already handles.
    if text.lines().count() > 1 {
        // If the user typed more than just `/goal …`, fold the rest of the
        // message into the objective so nothing is silently dropped.
        let trailing: String = text.lines().skip(1).collect::<Vec<_>>().join("\n");
        let trailing = trailing.trim();
        if !trailing.is_empty() {
            return Some(format!("{}\n{}", objective, trailing));
        }
    }
    Some(objective)
}

/// Pulls the objective text out of the `args` portion of a `/goal …` slash
/// command. Returns `None` for empty args or for the subcommand forms
/// (`status`, `pause`, `resume`, `clear`, `complete`).
fn extract_goal_objective_from_args(args: &str) -> Option<String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip an optional `--tokens <budget>` prefix so the objective shown
    // doesn't include the budget flag.
    let rest = if let Some(after_flag) = trimmed.strip_prefix("--tokens") {
        let after_flag = after_flag.trim_start();
        after_flag
            .split_once(char::is_whitespace)
            .map(|x| x.1)
            .unwrap_or("")
            .trim()
    } else {
        trimmed
    };
    if rest.is_empty() {
        return None;
    }
    let first = rest
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        first.as_str(),
        "status" | "pause" | "resume" | "clear" | "complete"
    ) {
        return None;
    }
    Some(rest.to_string())
}

/// Render the yellow `GOAL ACTIVE / Objective: …` badge that replaces the
/// `/goal <objective>` user-input line in the transcript.
fn render_goal_active_block(objective: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            "  GOAL ACTIVE".to_string(),
            Style::default()
                .fg(GOAL_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(vec![
            Span::styled(
                "  Objective: ".to_string(),
                Style::default()
                    .fg(GOAL_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(objective.to_string(), Style::default().fg(GOAL_BODY)),
        ]),
    ]
}

/// Render a user memory input line.
/// Shows: `# {key}: {value}` in cyan, with an optional `  Got it.` line in dark gray italic.
pub fn render_user_memory_input(key: &str, value: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            format!("# {}: {}", key, value),
            Style::default().fg(Color::Cyan),
        )]),
        Line::from(vec![Span::styled(
            "  Got it.",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]),
    ]
}

/// Render a user local command output block.
/// Header: `  !{command}` in dark gray bold, body up to max_lines in gray,
/// overflow indicator: `  ... N more lines` in dark gray.
pub fn render_user_local_command_output(
    command: &str,
    output: &str,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        format!("  !{}", command),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )]));
    let total = output.lines().count();
    for line in output.lines().take(max_lines) {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(line.to_string(), Style::default().fg(Color::Gray)),
        ]));
    }
    if total > max_lines {
        lines.push(Line::from(vec![Span::styled(
            format!("  ... {} more lines", total - max_lines),
            Style::default().fg(Color::DarkGray),
        )]));
    }
    lines
}

/// Render a resource update notification line.
/// Shows: `↻ ` in cyan + `{server}: ` in dark gray bold + `{uri}` in white + ` · {reason}` in dark gray.
pub fn render_resource_update(server: &str, uri: &str, reason: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("\u{21bb} ", Style::default().fg(Color::Cyan)),
        Span::styled(
            format!("{}: ", server),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(uri.to_string(), Style::default().fg(Color::White)),
        Span::styled(
            format!(" \u{00b7} {}", reason),
            Style::default().fg(Color::DarkGray),
        ),
    ])]
}

/// Render ranked post-response followups.
///
/// The rank remains visible so the user can distinguish required verification
/// from optional ideas. Undesired items are intentionally dimmed rather than
/// hidden, matching the feature specification.
///
/// When `selected_idx` is `Some(i)`, the i-th followup is rendered with a
/// highlight background and a `→` prefix so the user can see which followup
/// will be inserted on Enter.
pub fn render_ranked_followups(
    followups: &[RankedFollowup],
    selected_idx: Option<usize>,
) -> Vec<Line<'static>> {
    render_ranked_followups_wrapped(followups, selected_idx, u16::MAX)
        .into_iter()
        .map(|(_, line)| line)
        .collect()
}

/// Render followups as visual rows and retain the logical item index for every
/// row. Long suggestions are wrapped rather than silently treated as one-row
/// items, so mouse hit-testing can map any wrapped row to the same suggestion.
/// `usize::MAX` identifies the section header.
pub fn render_ranked_followups_wrapped(
    followups: &[RankedFollowup],
    selected_idx: Option<usize>,
    width: u16,
) -> Vec<(usize, Line<'static>)> {
    let highlight_bg = Color::Rgb(40, 44, 52);
    let width = usize::from(width).max(1);
    let mut lines = vec![(
        usize::MAX,
        Line::from(Span::styled(
            "  Suggested followups",
            Style::default()
                .fg(TRANSCRIPT_MUTED)
                .add_modifier(Modifier::BOLD),
        )),
    )];
    for (idx, followup) in followups.iter().enumerate() {
        let is_selected = selected_idx == Some(idx);
        let (rank_color, modifier) = match followup.rank {
            FollowupRank::HighlyRecommended => (Color::Green, Modifier::BOLD),
            FollowupRank::Recommended => (Color::Cyan, Modifier::BOLD),
            FollowupRank::Optional => (Color::Yellow, Modifier::empty()),
            FollowupRank::ForCompletion => (Color::Blue, Modifier::empty()),
            FollowupRank::Unimportant => (Color::DarkGray, Modifier::DIM),
            FollowupRank::Undesired => (Color::DarkGray, Modifier::DIM | Modifier::ITALIC),
        };
        let reason = if followup.reason.trim().is_empty() {
            String::new()
        } else {
            format!(" — {}", followup.reason.trim())
        };
        let prefix = if is_selected { "  → " } else { "  • " };
        let bg = if is_selected {
            highlight_bg
        } else {
            Color::Reset
        };
        let text_modifier = if is_selected {
            modifier | Modifier::BOLD
        } else {
            modifier
        };
        let full_text = format!(
            "{}{}: {}{}",
            prefix,
            followup.rank.label(),
            followup.text.trim(),
            reason
        );
        let mut remaining = full_text.as_str();
        let continuation_prefix = "     ";
        let continuation_width = UnicodeWidthStr::width(continuation_prefix);
        let mut first_row = true;
        while !remaining.is_empty() {
            // Continuation rows must also fit the indent prefix, otherwise the
            // tail of the segment gets clipped at the terminal edge and text
            // is silently lost.
            let row_width = if first_row {
                width
            } else {
                width.saturating_sub(continuation_width)
            };
            let mut split_at = remaining.len();
            let mut used = 0usize;
            for (byte_idx, ch) in remaining.char_indices() {
                let ch_width =
                    UnicodeWidthStr::width(&remaining[byte_idx..byte_idx + ch.len_utf8()]);
                if used + ch_width > row_width {
                    split_at = byte_idx;
                    break;
                }
                used += ch_width;
            }
            if split_at == 0 {
                let Some((byte_idx, ch)) = remaining.char_indices().next() else {
                    break;
                };
                split_at = byte_idx + ch.len_utf8();
            }
            let segment = &remaining[..split_at];
            let display = if first_row {
                segment.to_string()
            } else {
                format!("{}{}", continuation_prefix, segment)
            };
            let segment_style = if first_row {
                Style::default()
                    .fg(rank_color)
                    .bg(bg)
                    .add_modifier(text_modifier)
            } else {
                Style::default().fg(TRANSCRIPT_TEXT).bg(bg)
            };
            lines.push((idx, Line::from(Span::styled(display, segment_style))));
            remaining = &remaining[split_at..];
            first_row = false;
        }
    }
    lines
}

/// Render a collapsed read/search tool use summary.
/// Shows: `▸ ` in yellow + `{tool_name} ` in yellow bold + first few paths comma-joined,
/// followed by `(+ {n_hidden} more)` in dark gray if n_hidden > 0.
pub fn render_collapsed_read_search(
    tool_name: &str,
    paths: &[&str],
    n_hidden: usize,
) -> Vec<Line<'static>> {
    let paths_str = paths.join(", ");
    let mut spans = vec![
        Span::styled("\u{25b8} ", Style::default().fg(Color::Yellow)),
        Span::styled(
            format!("{} ", tool_name),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(paths_str, Style::default().fg(Color::White)),
    ];
    if n_hidden > 0 {
        spans.push(Span::styled(
            format!(" (+ {} more)", n_hidden),
            Style::default().fg(Color::DarkGray),
        ));
    }
    vec![Line::from(spans)]
}

/// Render a transcript task assignment row using the same structured title/subtitle language.
pub fn render_task_assignment(id: &str, subject: &str, desc: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let title = if subject.trim().is_empty() {
        "Assigned task"
    } else {
        subject.trim()
    };
    lines.push(Line::from(vec![
        Span::styled("  ~ ", Style::default().fg(CLAUDE_ORANGE)),
        Span::styled(
            title.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · task #{}", id),
            Style::default().fg(TRANSCRIPT_MUTED),
        ),
    ]));
    for line in desc.lines().take(5) {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(line.to_string(), Style::default().fg(TRANSCRIPT_MUTED)),
        ]));
    }
    lines
}

/// Render a grouped tool use summary.
/// Collapsed: `▸ {n} tool calls` in yellow with first few names comma-joined.
/// Expanded: same header + each tool on its own line with `  • ` prefix.
pub fn render_grouped_tool_use(names: &[&str], expanded: bool) -> Vec<Line<'static>> {
    let n = names.len();
    let preview = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let header = Line::from(vec![
        Span::styled(
            "\u{25b8} ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} tool call{}", n, if n == 1 { "" } else { "s" }),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", preview),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    if !expanded {
        return vec![header];
    }
    let mut lines = vec![header];
    for name in names {
        lines.push(Line::from(vec![
            Span::styled("  \u{2022} ", Style::default().fg(Color::Yellow)),
            Span::styled(name.to_string(), Style::default().fg(Color::White)),
        ]));
    }
    lines
}

// ---------------------------------------------------------------------------
// Goal event rendering
// ---------------------------------------------------------------------------

/// Returns `true` when a user message was injected by the goal machinery
/// (i.e. it should NOT render as a regular user message bubble).
pub fn is_goal_event_message(text: &str) -> bool {
    text.starts_with("[Goal started]")
        || text.starts_with("[Goal continuation \u{2014}")  // em dash
        || text.starts_with("[Goal continuation -") // fallback
}

/// Extract the turn number from a "[Goal continuation — turn N]" header.
fn extract_goal_turn(text: &str) -> Option<u32> {
    // Find the first [...] bracket, search inside for "turn <N>"
    let open = text.find('[')?;
    let close = text.find(']')?;
    if close <= open {
        return None;
    }
    let segment = &text[open..close];
    let tag = "turn ";
    let idx = segment.rfind(tag)? + tag.len();
    segment[idx..].trim().parse().ok()
}

/// Render a goal-event message block.
///
/// `[Goal started]` renders as nothing — the user's typed `/goal …` line
/// already produces the canonical GOAL ACTIVE block via
/// `render_goal_active_block`, so showing the kickoff event too would
/// duplicate it.
/// `[Goal continuation — turn N]` shows a compact inline turn marker.
pub fn render_goal_event(text: &str, _width: u16) -> Vec<Line<'static>> {
    if text.starts_with("[Goal continuation —") {
        let turn = extract_goal_turn(text).unwrap_or(0);
        return vec![Line::from(vec![
            Span::styled(
                "  \u{21ba} ".to_string(), // ↺
                Style::default().fg(GOAL_MUTED),
            ),
            Span::styled(
                format!("goal \u{00b7} turn {}", turn), // ·
                Style::default()
                    .fg(GOAL_MUTED)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])];
    }

    // [Goal started] — hidden.
    Vec::new()
}

#[cfg(test)]
mod tests {

    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>()
    }

    #[test]
    fn wrapped_followup_rows_keep_the_same_logical_index() {
        let followups = vec![RankedFollowup {
            text: "This is a deliberately long followup that must wrap".into(),
            rank: FollowupRank::Recommended,
            reason: "because the terminal is narrow".into(),
        }];
        let rows = render_ranked_followups_wrapped(&followups, Some(0), 24);
        assert_eq!(rows.first().map(|(index, _)| *index), Some(usize::MAX));
        let item_rows: Vec<_> = rows
            .iter()
            .filter(|(index, _)| *index != usize::MAX)
            .collect();
        assert!(item_rows.len() > 1);
        assert!(item_rows.iter().all(|(index, _)| *index == 0));
    }

    #[test]
    fn wrapped_followup_rows_never_exceed_the_terminal_width() {
        let followups = vec![RankedFollowup {
            text: "a deliberately long followup ".repeat(10),
            rank: FollowupRank::Recommended,
            reason: "with a long reason that must also wrap".into(),
        }];
        let width = 24usize;
        let rows = render_ranked_followups_wrapped(&followups, Some(0), width as u16);
        for (index, line) in &rows {
            let row_width = UnicodeWidthStr::width(line_text(line).as_str());
            assert!(
                row_width <= width,
                "row {index:?} is {row_width} wide (limit {width}): {:?}",
                line_text(line)
            );
        }
    }

    #[test]
    fn autopilot_annotation_renders_deferral_and_denial() {
        let deferred = autopilot_annotation(
            "Deferred for user review as AP-001. Continue with safe work; do not retry this \
             exact action until the user reviews it.",
        )
        .expect("deferral marker renders");
        let text = deferred
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[Autopilot deferred AP-001]"), "{text}");

        let question = autopilot_annotation(
            "Question deferred for user review as AP-002. Continue with safe work; the user \
             will answer it when they return.",
        )
        .expect("question marker renders");
        let text = question
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[Autopilot deferred AP-002]"), "{text}");

        let denied = autopilot_annotation(
            "Denied: 'Bash' is classified as irreversible and cannot run under autopilot.",
        )
        .expect("denial marker renders");
        let text = denied.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("[Autopilot denied]"), "{text}");

        let queue_full = autopilot_annotation(
            "Denied: the autopilot review queue is full (64 items). Continue with safe work.",
        )
        .expect("queue-full marker renders");
        let text = queue_full
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[Autopilot denied]"), "{text}");

        // Ordinary tool errors must NOT be intercepted.
        assert!(autopilot_annotation("command exited with status 1").is_none());
        assert!(autopilot_annotation("").is_none());
    }

    #[test]
    fn assistant_meta_renders_upstream_and_cost_badge() {
        let mut msg = Message::assistant("hello");
        msg.turn_meta = Some(clawde_core::types::TurnMeta {
            upstream_id: Some("groq".to_string()),
            started_at: Some("2026-08-19T00:00:00.000Z".to_string()),
            completed_at: Some("2026-08-19T00:00:05.000Z".to_string()),
        });
        msg.cost = Some(clawde_core::types::MessageCost {
            cost_usd: 0.0123,
            ..Default::default()
        });

        let line = render_transcript_assistant_meta(None, Some(&msg), Color::White)
            .expect("badge renders for a turn with upstream attribution");
        let text = line_text(&line);
        assert!(text.contains("groq"), "badge names the upstream: {text}");
        assert!(text.contains("$0.0123"), "badge shows the cost: {text}");

        // A message without attribution renders nothing (no stray meta line).
        let plain = Message::assistant("hello");
        assert!(render_transcript_assistant_meta(None, Some(&plain), Color::White).is_none());
    }

    #[test]
    fn assistant_meta_omits_cost_for_free_provider_zero_pricing() {
        let mut msg = Message::assistant("hello");
        msg.turn_meta = Some(clawde_core::types::TurnMeta {
            upstream_id: Some("poolside".to_string()),
            ..Default::default()
        });
        msg.cost = Some(clawde_core::types::MessageCost {
            cost_usd: 0.0,
            ..Default::default()
        });

        let line = render_transcript_assistant_meta(None, Some(&msg), Color::White)
            .expect("badge renders with upstream even at zero cost");
        let text = line_text(&line);
        assert!(text.contains("poolside"));
        assert!(
            !text.contains("$"),
            "zero-cost free turn shows no price: {text}"
        );
    }

    #[test]
    fn render_message_uses_message_families_for_assistant_blocks() {
        let msg = Message::assistant_blocks(vec![
            ContentBlock::Thinking {
                thinking: "reasoning".to_string(),
                signature: "sig".to_string(),
            },
            ContentBlock::Text {
                text: "hello".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({ "path": "README.md" }),
                thought_signature: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: ToolResultContent::Text("file contents".to_string()),
                is_error: Some(false),
            },
        ]);
        let ctx = RenderContext {
            width: 80,
            highlight: true,
            show_thinking: false,
            ..Default::default()
        };

        let rendered = render_message(&msg, &ctx)
            .into_iter()
            .map(|line| line_text(&line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!rendered.contains("◆"));
        assert!(rendered.contains("Thinking"));
        assert!(rendered.contains("read_file"));
        // ToolResult now shows output directly (no "Result" header)
        assert!(rendered.contains("file contents"));
        assert!(rendered.contains("hello"));
    }

    #[test]
    fn render_message_renders_interleaved_thinking_and_text_in_order() {
        // Stream-order assembly can produce text / thinking / text / thinking
        // within one turn; every thinking block must render its own heading
        // and text blocks must stay in their emitted positions.
        let msg = Message::assistant_blocks(vec![
            ContentBlock::Text {
                text: "first answer".to_string(),
            },
            ContentBlock::Thinking {
                thinking: "first reasoning".to_string(),
                signature: "s1".to_string(),
            },
            ContentBlock::Text {
                text: "second answer".to_string(),
            },
            ContentBlock::Thinking {
                thinking: "second reasoning".to_string(),
                signature: "s2".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({ "path": "README.md" }),
                thought_signature: None,
            },
        ]);
        let ctx = RenderContext {
            width: 80,
            highlight: true,
            show_thinking: true,
            ..Default::default()
        };

        let rendered = render_message(&msg, &ctx)
            .into_iter()
            .map(|line| line_text(&line))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(rendered.matches("Thinking").count(), 2);
        assert!(rendered.contains("first reasoning"));
        assert!(rendered.contains("second reasoning"));
        let first = rendered.find("first answer").expect("first text");
        let second = rendered.find("second answer").expect("second text");
        let think_one = rendered.find("first reasoning").expect("first thinking");
        let think_two = rendered.find("second reasoning").expect("second thinking");
        assert!(
            first < think_one && think_one < second && second < think_two,
            "interleaved order must be preserved, got:\n{rendered}"
        );
        assert!(rendered.contains("read_file"));
    }

    #[test]
    fn render_message_renders_user_text_in_brief_prompt_style() {
        let msg = Message::user("hello from user");
        let ctx = RenderContext::default();

        let rendered = render_message(&msg, &ctx)
            .into_iter()
            .map(|line| line_text(&line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("hello from user"));
        assert!(!rendered.contains("You"));
    }

    #[test]
    fn render_user_text_truncates_large_prompts() {
        let msg = Message::user(format!("{}\nquestion", "a".repeat(12_000)));
        let ctx = RenderContext::default();

        let rendered = render_message(&msg, &ctx)
            .into_iter()
            .map(|line| line_text(&line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("question"));
        assert!(rendered.contains(&"a".repeat(40)));
    }

    #[test]
    fn test_render_tool_result_cancelled() {
        let result = render_tool_result_cancelled("Bash");
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("Bash"));
        assert!(text.contains("cancelled"));
    }

    #[test]
    fn test_render_tool_result_rejected() {
        let result = render_tool_result_rejected("Edit", "user pressed ctrl-c");
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("Edit"));
        assert!(text.contains("interrupted"));
        let reason = line_text(&result[1]);
        assert!(reason.contains("user pressed ctrl-c"));
    }

    #[test]
    fn test_render_attachment_message() {
        let result =
            render_attachment_message("skill_listing", "5 tools available: Bash, Read", 80);
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("skill_listing"));
        assert!(text.contains("5 tools"));
    }

    #[test]
    fn test_render_attachment_message_truncates_long_content() {
        let long = "x".repeat(200);
        let result = render_attachment_message("kind", &long, 80);
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(
            text.contains('\u{2026}') || text.len() < long.len(),
            "expected truncation"
        );
    }

    #[test]
    fn test_render_advisor_message_loading() {
        let result = render_advisor_message(true, Some("claude-3"));
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("Advising"));
        assert!(text.contains("claude-3"));
    }

    #[test]
    fn test_render_advisor_message_done() {
        let result = render_advisor_message(false, None);
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("Advisor reviewed"));
    }

    #[test]
    fn test_render_agent_notification() {
        let result = render_agent_notification("Planner", "Starting task analysis...");
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("Planner"));
        assert!(text.contains("Starting task analysis"));
    }

    #[test]
    fn test_render_shutdown_message() {
        let result = render_shutdown_message("max turns reached");
        assert!(!result.is_empty());
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("Session ended"));
        assert!(combined.contains("max turns reached"));
    }

    #[test]
    fn test_render_bash_input_line() {
        let result = render_bash_input_line("ls -la");
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("$"));
        assert!(text.contains("ls -la"));
    }

    #[test]
    fn test_render_bash_output_block() {
        let output = (0..50)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = render_bash_output_block(&output, 10);
        assert!(!result.is_empty());
        // 10 content lines + 1 overflow indicator
        assert_eq!(result.len(), 11);
        let last = line_text(result.last().unwrap());
        assert!(last.contains("more lines"));
    }

    #[test]
    fn test_render_bash_output_block_no_overflow() {
        let output = "line 1\nline 2\nline 3";
        let result = render_bash_output_block(output, 10);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_render_plan_steps() {
        let steps = vec!["First step".to_string(), "Second step".to_string()];
        let result = render_plan_steps(&steps);
        assert!(!result.is_empty());
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("Plan:"));
        assert!(combined.contains("1."));
        assert!(combined.contains("First step"));
        assert!(combined.contains("2."));
        assert!(combined.contains("Second step"));
    }

    #[test]
    fn test_render_plan_approval_prompt() {
        let result = render_plan_approval_prompt();
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("Approve this plan?"));
        assert!(text.contains("[y]"));
        assert!(text.contains("[n]"));
        assert!(text.contains("[e]"));
    }

    #[test]
    fn test_render_tool_result_success_uses_30_lines() {
        let output = (0..50)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = render_tool_result_success(&output, false);
        // 30 content lines + 1 overflow indicator = 31 (no separate header line)
        assert_eq!(result.len(), 31);
        let overflow_text = line_text(result.last().unwrap());
        assert!(overflow_text.contains("more lines"));
        assert!(!overflow_text.contains("ctrl+o"));
    }

    #[test]
    fn bash_tool_use_shows_running_command_title_and_command() {
        let msg = Message::assistant_blocks(vec![ContentBlock::ToolUse {
            id: "tu-1".to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({"command": "ls -la"}),
            thought_signature: None,
        }]);
        let rendered = render_message(&msg, &RenderContext::default())
            .into_iter()
            .map(|l| line_text(&l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("ls -la"),
            "command should appear in output"
        );
        assert!(
            rendered.contains("Running command"),
            "updated tool title should appear"
        );
        assert!(
            !rendered.contains("ctrl+o"),
            "legacy expansion hint should be removed"
        );
    }

    #[test]
    fn non_bash_tool_use_shows_reading_file_title_with_summary() {
        let msg = Message::assistant_blocks(vec![ContentBlock::ToolUse {
            id: "tu-2".to_string(),
            name: "Read".to_string(),
            input: serde_json::json!({"file_path": "/tmp/foo.txt"}),
            thought_signature: None,
        }]);
        let rendered = render_message(&msg, &RenderContext::default())
            .into_iter()
            .map(|l| line_text(&l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("Reading file"),
            "tool title should appear"
        );
        assert!(
            rendered.contains("foo.txt"),
            "file path summary should appear"
        );
        assert!(
            !rendered.contains("ctrl+o"),
            "legacy expansion hint should be removed"
        );
    }

    #[test]
    fn task_tool_use_shows_subagent_title_and_description() {
        let msg = Message::assistant_blocks(vec![ContentBlock::ToolUse {
            id: "tu-3".to_string(),
            name: "Task".to_string(),
            input: serde_json::json!({
                "subagent_type": "explore",
                "description": "Trace the auth flow"
            }),
            thought_signature: None,
        }]);
        let rendered = render_message(&msg, &RenderContext::default())
            .into_iter()
            .map(|l| line_text(&l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Explore agent"));
        assert!(rendered.contains("Trace the auth flow"));
    }

    #[test]
    fn bash_tool_result_renders_as_bash_output_with_tool_names_context() {
        let mut tool_names = HashMap::new();
        tool_names.insert("tu-bash-1".to_string(), "Bash".to_string());
        let ctx = RenderContext {
            tool_names: &tool_names,
            ..Default::default()
        };

        let msg = Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "tu-bash-1".to_string(),
            content: ToolResultContent::Text("hello world\nline2".to_string()),
            is_error: Some(false),
        }]);
        let rendered = render_message(&msg, &ctx)
            .into_iter()
            .map(|l| line_text(&l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("hello world"), "output should appear");
        // bash_output_block does NOT prefix with "Result" (that's render_tool_result_success)
        assert!(
            !rendered.contains("Result"),
            "bash output should NOT show generic 'Result' header"
        );
    }

    #[test]
    fn non_bash_tool_result_shows_content() {
        let msg = Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "tu-read-1".to_string(),
            content: ToolResultContent::Text("file content here".to_string()),
            is_error: Some(false),
        }]);
        // No tool_names → falls back to render_tool_result_success (no separate header)
        let rendered = render_message(&msg, &RenderContext::default())
            .into_iter()
            .map(|l| line_text(&l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("file content here"),
            "content should appear"
        );
    }

    // ── New function tests ────────────────────────────────────────────────────

    #[test]
    fn tagged_renderer_strips_followups_before_a_following_tool() {
        let msg = Message::assistant_blocks(vec![
            ContentBlock::Text {
                text: "Done.\n<clawde_followups>[{\"text\":\"Run tests\",\"rank\":\"recommended\",\"reason\":\"Verify\"}]</clawde_followups>".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({"file_path":"src/lib.rs"}),
                thought_signature: None,
            },
        ]);
        let tagged = render_transcript_assistant_message_tagged(&msg, &RenderContext::default());
        let visible = tagged
            .iter()
            .map(|(line, _)| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("Done."));
        assert!(visible.contains("Read"));
        assert!(!visible.contains("clawde_followups"));
        // The action is intentionally rendered as a visible ranked followup;
        // only the transport metadata must be absent.
        assert!(visible.contains("Run tests"));
        assert!(visible.contains("Recommended"));
    }

    #[test]
    fn tagged_renderer_strips_malformed_closed_followup_block() {
        let msg = Message::assistant_blocks(vec![ContentBlock::Text {
            text: "Answer <clawde_followups>{bad}</clawde_followups>".to_string(),
        }]);
        let tagged = render_transcript_assistant_message_tagged(&msg, &RenderContext::default());
        let visible = tagged
            .iter()
            .map(|(line, _)| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("Answer"));
        assert!(!visible.contains("clawde_followups"));
    }

    #[test]
    fn tagged_renderer_keeps_unclosed_followup_text() {
        let msg = Message::assistant_blocks(vec![ContentBlock::Text {
            text: "Answer <clawde_followups>".to_string(),
        }]);
        let tagged = render_transcript_assistant_message_tagged(&msg, &RenderContext::default());
        let visible = tagged
            .iter()
            .map(|(line, _)| line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("clawde_followups"));
    }

    #[test]
    fn test_render_system_api_error_short_message() {
        let result = render_system_api_error("Connection refused", None);
        assert!(!result.is_empty());
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("API Error"));
        assert!(combined.contains("Connection refused"));
        // No retry line
        assert!(!combined.contains("Retrying"));
    }

    #[test]
    fn test_render_system_api_error_with_retry() {
        let result = render_system_api_error("Timeout", Some(30));
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("API Error"));
        assert!(combined.contains("Timeout"));
        assert!(combined.contains("Retrying in 30s"));
    }

    #[test]
    fn test_render_system_api_error_long_message_shows_expand_hint() {
        let msg = (0..10)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = render_system_api_error(&msg, None);
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            combined.contains("[expand]"),
            "should show [expand] hint when more than 5 lines"
        );
        assert!(combined.contains("5 more lines"));
    }

    #[test]
    fn test_render_user_command() {
        let result = render_user_command("doctor", "--verbose");
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains('\u{25b8}'), "should have ▸ prefix");
        assert!(text.contains("doctor"));
        assert!(text.contains("--verbose"));
    }

    #[test]
    fn goal_objective_renders_goal_active_block_not_user_command() {
        let result = render_user_command("goal", "Migrate to React");
        let header = line_text(&result[0]);
        let body = line_text(&result[1]);
        assert!(header.contains("GOAL ACTIVE"));
        assert!(
            !header.contains('\u{25b8}'),
            "should not show ▸ user-command prefix"
        );
        assert!(body.contains("Objective:"));
        assert!(body.contains("Migrate to React"));
    }

    #[test]
    fn goal_subcommands_render_as_normal_user_command() {
        for sub in ["status", "pause", "resume", "clear", "complete"] {
            let result = render_user_command("goal", sub);
            let text = line_text(&result[0]);
            assert!(
                text.contains('\u{25b8}'),
                "/goal {sub} should keep ▸ prefix"
            );
            assert!(text.contains(sub));
        }
    }

    #[test]
    fn goal_with_tokens_flag_strips_flag_from_objective() {
        let result = render_user_command("goal", "--tokens 250K Migrate to React");
        let body = line_text(&result[1]);
        assert!(body.contains("Migrate to React"));
        assert!(
            !body.contains("--tokens"),
            "flag should not appear in displayed objective"
        );
        assert!(!body.contains("250K"));
    }

    #[test]
    fn extract_goal_objective_returns_none_for_subcommands_and_empty() {
        assert!(extract_goal_objective_from_args("").is_none());
        assert!(extract_goal_objective_from_args("   ").is_none());
        assert!(extract_goal_objective_from_args("status").is_none());
        assert!(extract_goal_objective_from_args("pause now").is_none()); // first token is subcommand
        assert_eq!(
            extract_goal_objective_from_args("Migrate to React").as_deref(),
            Some("Migrate to React"),
        );
    }

    #[test]
    fn extract_goal_slash_objective_handles_typed_user_message() {
        assert_eq!(
            extract_goal_slash_objective("/goal build GPT 6 make no mistakes").as_deref(),
            Some("build GPT 6 make no mistakes"),
        );
        assert_eq!(
            extract_goal_slash_objective("/goal --tokens 250K Migrate to React").as_deref(),
            Some("Migrate to React"),
        );
        // Subcommands fall through.
        assert!(extract_goal_slash_objective("/goal status").is_none());
        assert!(extract_goal_slash_objective("/goal").is_none());
        // Not a /goal message.
        assert!(extract_goal_slash_objective("just a normal message").is_none());
        assert!(extract_goal_slash_objective("/goalbuild").is_none());
    }

    #[test]
    fn extract_goal_slash_objective_folds_trailing_lines_into_objective() {
        let text = "/goal Migrate to React\nwith strict typing\nand tests passing";
        let extracted = extract_goal_slash_objective(text).unwrap();
        assert!(extracted.starts_with("Migrate to React"));
        assert!(extracted.contains("strict typing"));
        assert!(extracted.contains("tests passing"));
    }

    #[test]
    fn test_render_user_memory_input() {
        let result = render_user_memory_input("project", "Clawde");
        assert_eq!(result.len(), 2);
        let first = line_text(&result[0]);
        assert!(first.contains("# project: Clawde"));
        let second = line_text(&result[1]);
        assert!(second.contains("Got it."));
    }

    #[test]
    fn test_render_user_local_command_output_with_overflow() {
        let output = (0..20)
            .map(|i| format!("out {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = render_user_local_command_output("ls", &output, 5);
        // 1 header + 5 body + 1 overflow = 7
        assert_eq!(result.len(), 7);
        let header = line_text(&result[0]);
        assert!(header.contains("!ls"));
        let overflow = line_text(result.last().unwrap());
        assert!(overflow.contains("15 more lines"));
    }

    #[test]
    fn test_render_user_local_command_output_no_overflow() {
        let output = "line1\nline2";
        let result = render_user_local_command_output("echo", output, 10);
        // 1 header + 2 body = 3
        assert_eq!(result.len(), 3);
        let header = line_text(&result[0]);
        assert!(header.contains("!echo"));
    }

    #[test]
    fn test_render_resource_update() {
        let result = render_resource_update("mcp-server", "file:///tmp/foo.txt", "modified");
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains('\u{21bb}'), "should have ↻ prefix");
        assert!(text.contains("mcp-server"));
        assert!(text.contains("file:///tmp/foo.txt"));
        assert!(text.contains("modified"));
    }

    #[test]
    fn test_render_collapsed_read_search_no_hidden() {
        let paths = vec!["src/lib.rs", "src/main.rs"];
        let result = render_collapsed_read_search("Read", &paths, 0);
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains('\u{25b8}'), "should have ▸ prefix");
        assert!(text.contains("Read"));
        assert!(text.contains("src/lib.rs"));
        assert!(
            !text.contains("more"),
            "should not show 'more' when n_hidden is 0"
        );
    }

    #[test]
    fn test_render_collapsed_read_search_with_hidden() {
        let paths = vec!["a.rs", "b.rs"];
        let result = render_collapsed_read_search("Glob", &paths, 3);
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("(+ 3 more)"));
    }

    #[test]
    fn test_render_task_assignment() {
        let result = render_task_assignment(
            "42",
            "Implement feature X",
            "Add the new widget system\nWith multi-line support",
        );
        assert!(!result.is_empty());
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("Implement feature X"));
        assert!(combined.contains("task #42"));
        assert!(combined.contains("Add the new widget system"));
    }

    #[test]
    fn test_render_task_assignment_truncates_desc_at_5_lines() {
        let desc = (0..10)
            .map(|i| format!("desc line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let result = render_task_assignment("1", "Subject", &desc);
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        // Only first 5 desc lines should appear
        assert!(combined.contains("desc line 4"));
        assert!(
            !combined.contains("desc line 5"),
            "should truncate desc at 5 lines"
        );
    }

    #[test]
    fn test_render_grouped_tool_use_collapsed() {
        let names = vec!["Bash", "Read", "Write", "Glob"];
        let result = render_grouped_tool_use(&names, false);
        assert_eq!(result.len(), 1, "collapsed should be a single header line");
        let text = line_text(&result[0]);
        assert!(text.contains("4 tool calls"));
        assert!(text.contains("Bash"));
    }

    #[test]
    fn test_render_grouped_tool_use_expanded() {
        let names = vec!["Bash", "Read"];
        let result = render_grouped_tool_use(&names, true);
        // 1 header + 2 tool lines
        assert_eq!(result.len(), 3);
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("2 tool calls"));
        assert!(combined.contains("Bash"));
        assert!(combined.contains("Read"));
        assert!(
            combined.contains('\u{2022}'),
            "expanded lines should have • prefix"
        );
    }

    #[test]
    fn test_grouped_tool_use_runs_detects_parallel_runs() {
        // Three consecutive ToolUse blocks form one run; a Text block between
        // tool calls breaks the run so the trailing Grep stays ungrouped.
        let msg = Message::assistant_blocks(vec![
            ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({ "command": "ls" }),
                thought_signature: None,
            },
            ContentBlock::ToolUse {
                id: "tu-2".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({ "file_path": "a.rs" }),
                thought_signature: None,
            },
            ContentBlock::Text {
                text: "moving on".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tu-3".to_string(),
                name: "Grep".to_string(),
                input: serde_json::json!({ "pattern": "foo" }),
                thought_signature: None,
            },
        ]);
        let runs = grouped_tool_use_runs(&msg);
        assert_eq!(runs.len(), 1, "only the 2-block run is grouped");
        let (hash, start, len) = runs[0];
        assert_eq!((start, len), (0, 2));

        // The hash is stable across calls and appears in the expandable set.
        assert!(expandable_block_hashes(&msg).contains(&hash));
    }

    #[test]
    fn test_transcript_collapses_parallel_tool_uses() {
        let msg = Message::assistant_blocks(vec![
            ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({ "command": "cargo test" }),
                thought_signature: None,
            },
            ContentBlock::ToolUse {
                id: "tu-2".to_string(),
                name: "Grep".to_string(),
                input: serde_json::json!({ "pattern": "fn main" }),
                thought_signature: None,
            },
        ]);
        let tagged = render_transcript_assistant_message_tagged(&msg, &RenderContext::default());
        let text: Vec<String> = tagged.iter().map(|(l, _)| line_text(l)).collect();
        let combined = text.join("\n");
        assert!(
            combined.contains("2 tool calls"),
            "collapsed group header expected, got: {combined}"
        );
        assert!(combined.contains("ctrl+o to expand"));
        assert!(
            !combined.contains("cargo test"),
            "collapsed group must hide per-tool summaries"
        );

        // Exactly one line is tagged with the group hash (the header).
        let (header, tag) = tagged[0].clone();
        assert!(
            tag.is_some(),
            "group header must be clickable, got: {}",
            line_text(&header)
        );
        assert!(
            tagged.iter().skip(1).all(|(_, t)| t.is_none()),
            "only the group header carries the expand hash"
        );
    }

    #[test]
    fn test_transcript_group_expands_to_individual_headers() {
        let msg = Message::assistant_blocks(vec![
            ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({ "command": "cargo test" }),
                thought_signature: None,
            },
            ContentBlock::ToolUse {
                id: "tu-2".to_string(),
                name: "Grep".to_string(),
                input: serde_json::json!({ "pattern": "fn main" }),
                thought_signature: None,
            },
        ]);
        let (hash, _, _) = grouped_tool_use_runs(&msg)[0];
        let mut expanded = std::collections::HashSet::new();
        expanded.insert(hash);
        let ctx = RenderContext {
            expanded_thinking: &expanded,
            ..RenderContext::default()
        };
        let tagged = render_transcript_assistant_message_tagged(&msg, &ctx);
        let combined: String = tagged
            .iter()
            .map(|(l, _)| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            combined.contains("cargo test"),
            "expanded group must show per-tool summaries, got: {combined}"
        );
        assert!(
            !combined.contains("ctrl+o to expand"),
            "expanded group hides the hint"
        );
    }

    #[test]
    fn test_render_rate_limit_with_hint_false() {
        let result = render_rate_limit_with_hint(60, false);
        assert_eq!(result.len(), 2, "without hint should have 2 lines");
        let combined = result
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("Rate limit exceeded"));
        assert!(combined.contains("Retrying in 60s"));
        assert!(!combined.contains("upgrade"));
    }

    #[test]
    fn test_render_rate_limit_with_hint_true() {
        let result = render_rate_limit_with_hint(60, true);
        assert_eq!(result.len(), 3, "with hint should have 3 lines");
        let last = line_text(result.last().unwrap());
        assert!(last.contains("claude.ai/upgrade"));
    }

    #[test]
    fn test_render_rate_limit_banner_is_wrapper() {
        // render_rate_limit_banner must produce identical output to render_rate_limit_with_hint(n, false)
        let banner = render_rate_limit_banner(45);
        let hint_false = render_rate_limit_with_hint(45, false);
        let banner_text: Vec<_> = banner.iter().map(|l| line_text(l)).collect();
        let hint_text: Vec<_> = hint_false.iter().map(|l| line_text(l)).collect();
        assert_eq!(banner_text, hint_text);
    }

    #[test]
    fn test_render_agent_notification_with_severity_info() {
        let result = render_agent_notification_with_severity("Scout", "All clear", "info");
        let text = line_text(&result[0]);
        assert!(text.contains("Scout"));
        assert!(text.contains("All clear"));
    }

    #[test]
    fn test_render_agent_notification_with_severity_warn() {
        let result = render_agent_notification_with_severity("Scout", "Low memory", "warn");
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("Scout"));
        assert!(text.contains("Low memory"));
    }

    #[test]
    fn test_render_agent_notification_with_severity_error() {
        let result = render_agent_notification_with_severity("Scout", "Crash detected", "error");
        assert!(!result.is_empty());
        let text = line_text(&result[0]);
        assert!(text.contains("Scout"));
        assert!(text.contains("Crash detected"));
    }

    #[test]
    fn test_render_agent_notification_defaults_to_info() {
        // render_agent_notification delegates to severity "info"
        let a = render_agent_notification("Bot", "hello");
        let b = render_agent_notification_with_severity("Bot", "hello", "info");
        let a_text: Vec<_> = a.iter().map(|l| line_text(l)).collect();
        let b_text: Vec<_> = b.iter().map(|l| line_text(l)).collect();
        assert_eq!(a_text, b_text);
    }

    #[test]
    fn truncate_user_prompt_text_handles_multibyte_over_limit() {
        // >10K chars of a 3-byte codepoint. Pre-fix, the *_CHARS constants were
        // used as BYTE offsets, slicing mid-codepoint (2500 % 3 == 1) (#221).
        let text = "\u{2705}".repeat(11_000); // ✅ (3 bytes), 11K chars > 10K limit
        let out = truncate_user_prompt_text(&text);
        assert!(out.starts_with('\u{2705}'));
        assert!(out.contains("lines"));
        assert!(out.chars().count() < text.chars().count());

        // Mixed multibyte content around both cut points must also be safe.
        let mixed = "😀é✅ん".repeat(3_000);
        let _ = truncate_user_prompt_text(&mixed); // no panic == pass
    }

    // ---- LSP diagnostics summary -------------------------------------------

    #[test]
    fn test_parse_lsp_diagnostics_counts_issues_and_files() {
        let out = parse_lsp_diagnostics(
            "[ERROR] /src/main.rs:12:5 - missing semicolon (rustc) [E0308]\n\
             [WARNING] /src/main.rs:40:1 - unused variable (rustc) [W0001]\n\
             [ERROR] /src/lib.rs:3:9 - type mismatch (rustc) [E0277]",
        );
        assert_eq!(out, Some((3, 2)));
    }

    #[test]
    fn test_parse_lsp_diagnostics_rejects_non_diagnostics() {
        // Ordinary tool output must NOT be treated as diagnostics — mixed
        // content fails the all-lines check and returns None.
        assert_eq!(
            parse_lsp_diagnostics("  Build failed.\n  make: *** [all] Error 2"),
            None
        );
        assert_eq!(
            parse_lsp_diagnostics("[ERROR] no location format here"),
            None
        );
        assert_eq!(parse_lsp_diagnostics("No diagnostics for 'x'."), None);
        assert_eq!(parse_lsp_diagnostics(""), None);
    }

    #[test]
    fn test_transcript_collapses_lsp_diagnostics() {
        let msg = Message::assistant_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "tu-1".to_string(),
            content: ToolResultContent::Text(
                "[ERROR] /src/main.rs:12:5 - missing semicolon\n\
                 [WARNING] /src/lib.rs:3:9 - unused import"
                    .to_string(),
            ),
            is_error: Some(false),
        }]);
        let mut ctx = RenderContext::default();
        let expanded = std::collections::HashSet::new();
        ctx.expanded_thinking = &expanded;
        let tagged = render_transcript_assistant_message_tagged(&msg, &ctx);
        let text: Vec<String> = tagged.iter().map(|(l, _)| line_text(l)).collect();
        let combined = text.join("\n");
        assert!(
            combined.contains("Found 2 diagnostic issues in 2 files"),
            "collapsed summary expected, got: {combined}"
        );
        assert!(combined.contains("ctrl+o to expand"));
        assert!(
            !combined.contains("missing semicolon"),
            "collapsed summary must hide per-diagnostic detail"
        );

        // The summary line is tagged with the hash → clickable + Ctrl+O toggle.
        let hash = diagnostics_block_hash(
            "[ERROR] /src/main.rs:12:5 - missing semicolon\n\
             [WARNING] /src/lib.rs:3:9 - unused import",
        );
        assert_eq!(tagged[0].1, Some(hash));
        assert!(expandable_block_hashes(&msg).contains(&hash));

        // Expanded shows the full detail instead of the summary.
        {
            let mut ctx = RenderContext::default();
            let expanded2 = std::collections::HashSet::from([hash]);
            ctx.expanded_thinking = &expanded2;
            let tagged = render_transcript_assistant_message_tagged(&msg, &ctx);
            let text: Vec<String> = tagged.iter().map(|(l, _)| line_text(l)).collect();
            let combined = text.join("\n");
            assert!(
                combined.contains("missing semicolon"),
                "expanded must show full diagnostics, got: {combined}"
            );
            assert!(!combined.contains("Found 2 diagnostic issues"));
        }
    }
}
