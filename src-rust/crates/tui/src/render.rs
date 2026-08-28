// render.rs â€” All ratatui rendering logic.

use std::cell::RefCell;

use crate::agents_view::render_agents_menu;
use crate::app::{
    App, ContextMenuKind, FollowupRowTarget, FollowupSource, SystemAnnotation, SystemMessageStyle,
    ToolStatus,
};
use crate::ask_user_dialog::render_ask_user_dialog;
use crate::bypass_permissions_dialog::render_bypass_permissions_dialog;
use crate::context_viz::{key_ring_rows_from_registry, render_context_viz};
use crate::custom_provider_dialog::render_custom_provider_dialog;
use crate::desktop_upsell_startup::render_desktop_upsell_startup;
use crate::device_auth_dialog::render_device_auth_dialog;
use crate::dialog_select::render_dialog_select;
use crate::dialogs::{render_mcp_approval_dialog, render_permission_dialog};
use crate::diff_viewer::render_diff_dialog;
use crate::elicitation_dialog::render_elicitation_dialog;
use crate::export_dialog::render_export_dialog;
use crate::feedback_survey::render_feedback_survey;
use crate::figures;
use crate::file_injection_dialog::render_file_injection_dialog;
use crate::hooks_config_menu::render_hooks_config_menu;
use crate::import_config_dialog::render_import_config_dialog;
use crate::invalid_config_dialog::render_invalid_config_dialog;
use crate::key_input_dialog::render_key_input_dialog;
use crate::mcp_view::render_mcp_view;
use crate::memory_file_selector::render_memory_file_selector;
use crate::memory_update_notification::render_memory_update_notification;
use crate::messages::{
    render_thinking_live_content, render_transcript_assistant_message_tagged,
    render_transcript_assistant_meta, render_transcript_live_text, render_transcript_user_message,
    RenderContext,
};
use crate::mode_panel::render_mode_panel;
use crate::model_picker::render_model_picker;
use crate::notifications::{render_notification_banner, Notification, NotificationKind};
use crate::ollama_config_dialog::render_ollama_config_dialog;
use crate::onboarding_dialog::render_onboarding_dialog;
use crate::overage_upsell::render_overage_upsell;
use crate::overlays::{
    render_global_search, render_help_overlay, render_history_search_overlay,
    render_keybindings_overlay, render_rewind_flow, CLAWDE_ACCENT,
};
use crate::plugin_views::render_plugin_hints;
use crate::prompt_input::{input_height, render_prompt_input, InputMode, TypeaheadSource, VimMode};
use crate::rustail::rustail_lines;
use crate::rustail_editor::render_rustail_editor;
use crate::session_branching::render_session_branching;
use crate::session_browser::render_session_browser;
use crate::settings_screen::render_settings_screen;
use crate::stats_dialog::render_stats_dialog;
use crate::tasks_overlay::render_tasks_overlay;
use crate::theme_creator::render_theme_creator;
use crate::theme_screen::render_theme_screen;
use crate::transcript_turn::{build_transcript_turns, TranscriptTurn};
use crate::virtual_list::{VirtualItem, VirtualList};
use crate::voice_mode_notice::render_voice_mode_notice;
use clawde_core::constants::APP_VERSION;
use clawde_core::format_utils::format_duration_ms;
use clawde_core::types::Role;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

// Default spinner: classic braille dot rotation — universally supported,
// always renders cleanly with solid color. Shows during normal processing.
const SPINNER: &[char] = &[
    '\u{280b}', // ⠋
    '\u{2819}', // ⠙
    '\u{2839}', // ⠹
    '\u{2838}', // ⠸
    '\u{283c}', // ⠼
    '\u{2834}', // ⠴
    '\u{2826}', // ⠦
    '\u{2827}', // ⠧
    '\u{2807}', // ⠇
    '\u{280f}', // ⠏
];

// Attention-grabbing snowflake spinner, used when a modal dialog needs
// the user's response (permission requests, AskUser questions, etc.).
// Deliberately more eye-catching than the subtle braille dots.
const SPINNER_SNOWFLAKE: &[char] = &[
    '\u{00b7}', '\u{2722}', '\u{273b}', '\u{273d}', '\u{273d}', '\u{273b}', '\u{2722}', '\u{00b7}',
];
const CLAUDE_ORANGE: Color = Color::Rgb(233, 30, 99);
const WELCOME_BOX_HEIGHT: u16 = 12;
const STATUS_THINKING: &str = "thinking";
const STATUS_THINKING_ELLIPSIS: &str = "thinking\u{2026}";

/// Returns the colour to use for the streaming spinner: pink normally,
/// brightening to a hot red when no stream data has arrived for over 3 seconds.
fn spinner_color(app: &App) -> Color {
    if let Some(start) = app.stall_start {
        if start.elapsed() > std::time::Duration::from_secs(3) {
            return Color::Rgb(255, 70, 70);
        }
    }
    CLAUDE_ORANGE
}

fn is_modal_open(app: &App) -> bool {
    app.any_modal_open()
}

// -----------------------------------------------------------------------
// Error modal rendering
// -----------------------------------------------------------------------

/// Render an error modal dialog with wrapped content.
fn render_error_modal(
    frame: &mut Frame,
    area: Rect,
    notification: &Notification,
    _scroll_offset: usize,
    footer_area: Rect,
    is_welcome_screen: bool,
) {
    // When the footer anchor is inside the welcome box (y < WELCOME_BOX_HEIGHT), or explicitly on
    // the welcome screen, center the modal so it doesn't awkwardly overlap the welcome box.
    let anchored_in_welcome_box = footer_area.width > 0 && footer_area.y < WELCOME_BOX_HEIGHT;
    let modal_area = if is_welcome_screen || anchored_in_welcome_box {
        let modal_width = (area.width * 2 / 3).max(40).min(area.width);
        let modal_height = (area.height / 3).max(8).min(area.height.saturating_sub(2));
        Rect {
            x: area.x + (area.width.saturating_sub(modal_width)) / 2,
            y: area.y + (area.height.saturating_sub(modal_height)) / 2,
            width: modal_width,
            height: modal_height,
        }
    } else if footer_area.width > 0 {
        let desired_height = (area.height / 3)
            .max(8)
            .min(area.height.saturating_sub(footer_area.y));
        Rect {
            x: footer_area.x,
            y: footer_area.y,
            width: footer_area.width,
            height: desired_height,
        }
    } else {
        let modal_width = area.width / 2;
        let modal_height = area.height.saturating_sub(4);
        Rect {
            x: area.x + modal_width,
            y: area.y,
            width: modal_width,
            height: modal_height,
        }
    };

    frame.render_widget(Clear, modal_area);

    let modal_block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .style(Style::default().fg(Color::Red));
    frame.render_widget(modal_block, modal_area);

    let header_bg_area = Rect {
        x: modal_area.x + 1,
        y: modal_area.y + 1,
        width: modal_area.width.saturating_sub(2),
        height: 1,
    };
    let header_style = Style::default().bg(Color::Rgb(60, 15, 15)).fg(Color::Red);
    let header_para =
        Paragraph::new("  ⚠ Error  ").style(header_style.add_modifier(Modifier::BOLD));
    frame.render_widget(header_para, header_bg_area);

    let sep_area = Rect {
        x: modal_area.x + 1,
        y: modal_area.y + 2,
        width: modal_area.width.saturating_sub(2),
        height: 1,
    };
    let sep_line = Paragraph::new(Line::from(Span::styled(
        "─".repeat(sep_area.width as usize),
        Style::default().fg(Color::Rgb(80, 20, 20)),
    )));
    frame.render_widget(sep_line, sep_area);

    // Chrome: border(1) + header(1) + sep(1) + blank(1) + border(1) = 5 rows
    let body_start_y = modal_area.y + 4;
    let body_height = modal_area.height.saturating_sub(5).max(1);
    let body_area = Rect {
        x: modal_area.x + 2,
        y: body_start_y,
        width: modal_area.width.saturating_sub(4),
        height: body_height,
    };

    let body_para = Paragraph::new(notification.message.as_str())
        .style(Style::default().fg(Color::Rgb(220, 220, 220)))
        .wrap(Wrap { trim: true });
    frame.render_widget(body_para, body_area);
}

// -----------------------------------------------------------------------
// Text truncation helpers
// -----------------------------------------------------------------------

/// Short relative timestamp for the welcome screen's recent-activity list:
/// "just now", "5m ago", "2h ago", "3d ago". Clock skew (mtime in the future)
/// degrades gracefully to "just now".
fn short_relative_time(mtime: std::time::SystemTime) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(mtime)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    short_relative_secs(secs)
}

/// Whether a recent-session entry is old enough that a relative timestamp
/// ("2h ago") is less useful than an absolute date/time ("Aug 3 14:22").
/// Sessions older than 24 h get the absolute form.
fn recent_activity_is_stale(mtime: std::time::SystemTime) -> bool {
    match std::time::SystemTime::now().duration_since(mtime) {
        Ok(d) => d.as_secs() >= 86_400,
        Err(_) => false, // clock skew → treat as fresh
    }
}

/// Formatter split out from [`short_relative_time`] so it can be unit-tested
/// without depending on the wall clock.
fn short_relative_secs(secs: u64) -> String {
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Build the body lines for the welcome box's "Recent activity" section.
///
/// Renders up to five recent sessions as `<label> <relative-time>` (the label
/// truncated to fit `width`), or a single dimmed "No recent activity" line when
/// there are none. Split out from [`render_welcome_box`] so it can be unit
/// tested from controlled state without the surrounding layout.
/// Build the compact project-memory status line for the welcome screen
/// (audit spec §15.3): `⚡ Mnemosyne: N files · updated <age>`, or `None`
/// when the project has no memory files yet.
///
/// Takes the resolved memory dir (not a project path) so tests can point it at
/// a temp dir without touching env vars. Day-granular freshness reuses the
/// memdir age helper ("today" / "yesterday" / "N days ago").
fn project_memory_line(mem_dir: &std::path::Path) -> Option<Line<'static>> {
    let files = clawde_core::memdir::scan_memory_dir(mem_dir);
    let index_path = mem_dir.join(clawde_core::memdir::MEMORY_ENTRYPOINT);
    let index_present = index_path.is_file();
    if files.is_empty() && !index_present {
        return None;
    }
    let mut newest = files.iter().map(|f| f.modified_secs).max().unwrap_or(0);
    if index_present {
        if let Ok(meta) = std::fs::metadata(&index_path) {
            if let Ok(mtime) = meta.modified() {
                let secs = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                newest = newest.max(secs);
            }
        }
    }
    let count = files.len() + usize::from(index_present);
    let mut spans = vec![
        Span::styled("⚡ ", Style::default().fg(Color::Yellow)),
        Span::styled(
            "Mnemosyne",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                ": {} files · {}",
                count,
                clawde_core::memdir::memory_age(newest)
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    // Pending adjudications: surface unconfirmed memory conflicts at a glance
    // so the user knows they exist without running /memory status. Counts
    // adjudicable *pairs* (shared with the injected block and `/memory
    // status`), so resolved / dangling / self / superseded-claimant entries
    // never inflate the number.
    let pending = clawde_core::memdir::pending_conflict_count(mem_dir);
    if pending > 0 {
        spans.push(Span::styled(
            format!(" · {} Lethesyne", pending),
            Style::default().fg(Color::Yellow),
        ));
    }
    Some(Line::from(spans))
}

fn recent_activity_lines(
    recent: &[crate::app::RecentSession],
    width: usize,
    hovered_idx: Option<usize>,
) -> Vec<Line<'static>> {
    if recent.is_empty() {
        return vec![Line::from(Span::styled(
            "No recent activity",
            Style::default().fg(Color::DarkGray),
        ))];
    }

    recent
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, s)| {
            let is_hovered = hovered_idx == Some(i);
            // Fresh sessions get a relative time ("2h ago"); stale ones get an
            // absolute date/time ("Aug 3 14:22") so the list stays meaningful
            // across days.
            let when = if recent_activity_is_stale(s.mtime) {
                clawde_core::format_utils::format_short_absolute_time(s.mtime)
            } else {
                short_relative_time(s.mtime)
            };
            // Reserve room for the trailing " <time>" so the label truncates
            // instead of wrapping onto a second line.
            let label_w = width.saturating_sub(when.chars().count() + 1);
            let label = truncate_end(&s.label, label_w.max(1));
            let label_style = if is_hovered {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default().fg(Color::Gray)
            };
            let time_style = if is_hovered {
                Style::default()
                    .fg(Color::Rgb(180, 180, 180))
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(vec![
                Span::styled(label, label_style),
                Span::raw(" "),
                Span::styled(when, time_style),
            ])
        })
        .collect()
}

fn truncate_end(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "\u{2026}".to_string();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthStr::width(ch.encode_utf8(&mut [0; 4]));
        if width + ch_width >= max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push('\u{2026}');
    out
}

fn truncate_middle(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return truncate_end(text, max_width);
    }
    let keep_each_side = (max_width.saturating_sub(1)) / 2;
    let left: String = text.chars().take(keep_each_side).collect();
    let right: String = text
        .chars()
        .rev()
        .take(keep_each_side)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{left}\u{2026}{right}")
}

fn truncate_text(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let next = format!("{out}{ch}");
        if next.width() > max_width {
            if max_width > 1 && out.width() < max_width {
                out.push('\u{2026}');
            }
            break;
        }
        out.push(ch);
    }
    out
}

/// Total display width of a span list, used to check whether right-side
/// footer/prompt spans fit within the available columns.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

// -----------------------------------------------------------------------
// Startup notice helpers
// -----------------------------------------------------------------------

fn startup_notice_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let max_width = width.saturating_sub(10) as usize;

    if let Some(summary) = app.away_summary.as_deref() {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", crate::figures::REFERENCE_MARK),
                Style::default()
                    .fg(CLAUDE_ORANGE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_end(summary, max_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    match &app.bridge_state {
        crate::bridge_state::BridgeConnectionState::Connected { peer_count, .. } => {
            let label = if *peer_count > 0 {
                format!(
                    "Remote session active \u{00b7} {} peer{}",
                    peer_count,
                    if *peer_count == 1 { "" } else { "s" }
                )
            } else {
                "Remote session active".to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(CLAUDE_ORANGE)),
                Span::styled(label, Style::default().fg(Color::DarkGray)),
            ]));
        }
        crate::bridge_state::BridgeConnectionState::Reconnecting { attempt } => {
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("Reconnecting remote session (attempt #{attempt})"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        crate::bridge_state::BridgeConnectionState::Failed { reason } => {
            lines.push(Line::from(vec![
                Span::styled(" remote ", Style::default().fg(Color::Red)),
                Span::styled(
                    truncate_end(reason, max_width),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        _ => {}
    }

    if let Some(url) = app.remote_session_url.as_deref() {
        lines.push(Line::from(vec![
            Span::styled(" link ", Style::default().fg(CLAUDE_ORANGE)),
            Span::styled(
                truncate_end(url, max_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    // Additional directories (from --add-dir)
    for dir in &app.config.additional_dirs {
        lines.push(Line::from(vec![
            Span::styled(" +dir ", Style::default().fg(Color::Cyan)),
            Span::styled(
                truncate_end(&dir.display().to_string(), max_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    lines
}

fn render_startup_notices(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let lines = startup_notice_lines(app, area.width);
    if lines.is_empty() {
        return;
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[derive(Clone)]
struct RenderedLineItem {
    line: Line<'static>,
    search_text: String,
    is_header: bool,
    message_index: Option<usize>,
    /// If this line is the clickable header of a thinking block, its hash.
    thinking_hash: Option<u64>,
    /// If this line is a ranked followup, its index in the current followups list.
    #[allow(dead_code)]
    followup_target: Option<FollowupRowTarget>,
}

impl VirtualItem for RenderedLineItem {
    fn measure_height(&self, _width: u16) -> u16 {
        1
    }

    fn render(&self, area: Rect, buf: &mut Buffer, _selected: bool) {
        Paragraph::new(vec![self.line.clone()]).render(area, buf);
    }

    fn search_text(&self) -> String {
        self.search_text.clone()
    }

    fn is_section_header(&self) -> bool {
        self.is_header
    }
}

fn flatten_line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.to_string())
        .collect::<Vec<_>>()
        .join("")
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MessageLinesCacheKey {
    width: u16,
    transcript_version: u64,
    messages_ptr: usize,
    messages_len: usize,
    annotations_ptr: usize,
    annotations_len: usize,
    thinking_expanded_len: usize,
    // Followup state changes (mode toggle, clear) without touching the
    // transcript, but the appended followup rows are part of the cached lines.
    // Key on identity + mode so stale lists never render from the cache.
    followup_history_mode: bool,
    current_followups_ptr: usize,
    current_followups_len: usize,
    persisted_followups_ptr: usize,
    persisted_followups_len: usize,
    // Arrow-key navigation changes the highlighted followup without touching
    // the transcript or the list identity; without this the cache would serve
    // stale lines and the selection highlight would never appear.
    followup_selected: Option<usize>,
}

#[derive(Clone)]
struct MessageLinesCache {
    key: MessageLinesCacheKey,
    lines: Vec<RenderedLineItem>,
}

/// Cache key for the *committed prefix* served during streaming: all messages
/// before the live (actively-streaming) turn.
///
/// Deliberately keyed by message/annotation identity — NOT by
/// `transcript_version`, which bumps on every streaming token and would churn
/// the entry away each frame (issue #222). During streaming the committed
/// messages do not change, so `messages_ptr`/`messages_len` stay stable and the
/// prefix is a cache hit every frame; when the committed set changes (a turn
/// completes, session switch/fork/revert/compaction) the pointer, length, or
/// `prefix_len` shifts and the entry is rebuilt. `prefix_len` is the number of
/// committed messages that precede the live turn, so growing the transcript by
/// one turn re-keys the prefix cleanly.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CompletedMsgCacheKey {
    width: u16,
    prefix_len: usize,
    messages_ptr: usize,
    messages_len: usize,
    annotations_ptr: usize,
    annotations_len: usize,
    thinking_expanded_len: usize,
}

#[derive(Clone)]
struct CompletedMsgCache {
    key: CompletedMsgCacheKey,
    lines: Vec<RenderedLineItem>,
}

thread_local! {
    static MESSAGE_LINES_CACHE: RefCell<Option<MessageLinesCache>> = const { RefCell::new(None) };
    /// Stores rendered lines for the committed prefix (all messages before the
    /// live turn); valid and reused across streaming deltas.
    static COMPLETED_MSG_CACHE: RefCell<Option<CompletedMsgCache>> = const { RefCell::new(None) };
}

// Instrumentation so tests can prove the committed prefix is served from cache
// (a hit) rather than rebuilt on every streaming frame. Compiled out of release
// builds.
#[cfg(test)]
thread_local! {
    static PREFIX_CACHE_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static PREFIX_CACHE_MISSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_prefix_cache_hit() {
    PREFIX_CACHE_HITS.with(|c| c.set(c.get() + 1));
}
#[cfg(test)]
fn record_prefix_cache_miss() {
    PREFIX_CACHE_MISSES.with(|c| c.set(c.get() + 1));
}
#[cfg(not(test))]
#[inline(always)]
fn record_prefix_cache_hit() {}
#[cfg(not(test))]
#[inline(always)]
fn record_prefix_cache_miss() {}

/// Test-only: `(hits, misses)` for the committed-prefix cache.
#[cfg(test)]
fn prefix_cache_counts() -> (u64, u64) {
    (
        PREFIX_CACHE_HITS.with(|c| c.get()),
        PREFIX_CACHE_MISSES.with(|c| c.get()),
    )
}

/// Test-only: reset the render caches and counters so a test starts clean and
/// is not affected by cache state left over from a previous render on this
/// thread.
#[cfg(test)]
fn reset_render_caches() {
    MESSAGE_LINES_CACHE.with(|c| *c.borrow_mut() = None);
    COMPLETED_MSG_CACHE.with(|c| *c.borrow_mut() = None);
    PREFIX_CACHE_HITS.with(|c| c.set(0));
    PREFIX_CACHE_MISSES.with(|c| c.set(0));
}

// -----------------------------------------------------------------------
// Top-level layout
// -----------------------------------------------------------------------

/// Render the entire application into the current frame.
pub fn render_app(frame: &mut Frame, app: &App) {
    // Sync the thread-local palette so all CLAWDE_* constants resolve to
    // the active theme without threading `app` through every sub-function.
    // While the theme creator's editor is open, the whole UI (creator modal
    // included) is themed by the work-in-progress palette so colour
    // assignments preview live against the main TUI.
    let live_palette = app.theme_creator.editor_palette().unwrap_or(app.palette);
    crate::theme_colors::CURRENT_PALETTE.with(|p| *p.borrow_mut() = live_palette);

    let size = frame.area();
    app.last_selectable_area.set(size);

    // Fill the entire frame with the theme's panel background so the terminal's
    // default color (blue on Windows) doesn't bleed through cells not covered by widgets.
    let p = crate::theme_colors::current_palette();
    frame.render_widget(
        Block::default().style(Style::default().bg(p.panel_bg).fg(p.text_light)),
        size,
    );

    let prompt_focused = app.permission_request.is_none() && !app.history_search_overlay.visible;
    // Suggestions popup tracks whether the prompt accepts input, not whether
    // it is the focused widget. Text entry is allowed during streaming so the
    // user can queue the next message, so the typeahead popup must follow
    // that same affordance.
    let suggestions_visible =
        app.permission_request.is_none() && !app.history_search_overlay.visible;
    let status_visible = should_render_status_row(app);
    // One blank separator row above the status/input area when status is active,
    // matching the visual breathing room in the TS layout.
    let separator_height: u16 = if status_visible { 1 } else { 0 };
    let status_height: u16 = if status_visible {
        if app.is_streaming {
            // The spinner row is always a short single line.
            1
        } else if let Some(text) = app.status_message.as_deref() {
            // Measure how many terminal rows the message needs so that long
            // error strings (e.g. "Error: overloaded_error (529): …") wrap
            // instead of overflowing the input area.  Cap at 3 lines.
            let usable_width = size.width.max(1) as usize;
            let rows = text.split('\n').fold(0usize, |acc, line| {
                acc + line.chars().count().div_ceil(usable_width)
            });
            rows.clamp(1, 3) as u16
        } else {
            1
        }
    } else {
        0
    };
    let suggestions_height = if suggestions_visible && !app.prompt_input.suggestions.is_empty() {
        app.prompt_input.suggestions.len().min(5) as u16
    } else {
        0
    };
    // The prompt body width is the terminal width minus the prompt prefix
    // ("> ") and the right-margin padding used inside `render_prompt_input`.
    // Keep this in sync with prefix_width=2 + right_pad=2 there.
    let prompt_text_width = size.width.saturating_sub(4);
    // While the `/effort` selector is open it DOCKS into the prompt area, fully
    // replacing the prompt box, so the row budget follows the docked panel height
    // (clamped by the layout below) instead of the prompt's own line count.
    let prompt_height = if app.effort_picker.visible {
        crate::effort_picker::DOCK_HEIGHT
    } else {
        input_height(&app.prompt_input, prompt_text_width) + 1 // +1 for model/mode status line
    };
    // CWD row below the prompt input — 1 row when enabled, 0 otherwise.
    let cwd_row_height: u16 =
        if app.settings_screen.show_cwd && app.has_credentials && app.current_dir.is_some() {
            1
        } else {
            0
        };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(separator_height),
            Constraint::Length(status_height),
            Constraint::Length(prompt_height),
            Constraint::Length(cwd_row_height),
            Constraint::Length(suggestions_height),
            Constraint::Length(1),
        ])
        .split(size);

    render_messages(frame, app, chunks[0]);
    // chunks[1] is the blank separator — intentionally left empty
    if status_height > 0 {
        render_status_row(frame, app, chunks[2]);
    }
    // Compute the current-thinking inspector once per frame and thread it
    // to both `render_input` (warning marker) and `render_context_viz`
    // (thinking section) — avoids double `inspect_thinking()` + JSON-map
    // construction at 60 fps.
    let current_insp = app.current_inspector();

    // The `/effort` selector replaces the prompt box while open: render it into
    // the input area (full width) and SKIP the prompt input. The prompt returns
    // when the picker closes on confirm/cancel.
    if app.effort_picker.visible {
        let inspector = app.effort_picker_inspector();
        crate::effort_picker::render_effort_picker(
            frame,
            &app.effort_picker,
            chunks[3],
            app.frame_count,
            inspector.as_ref(),
        );
    } else {
        render_input(frame, app, chunks[3], prompt_focused, current_insp.as_ref());
    }
    app.last_input_area.set(chunks[3]);
    // CWD row — rendered below the prompt input, left-aligned and dimmed.
    if cwd_row_height > 0 {
        if let Some(ref dir) = app.current_dir {
            let home = dirs::home_dir()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
                .filter(|s| !s.is_empty());
            let display_dir = match home {
                Some(h) if dir.starts_with(&h) => dir.replacen(&h, "~", 1),
                _ => dir.clone(),
            };
            let cwd_area = Rect {
                x: chunks[4].x + 2, // indent to align with prompt text
                y: chunks[4].y,
                width: chunks[4].width.saturating_sub(2),
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    display_dir,
                    Style::default().fg(Color::DarkGray),
                ))),
                cwd_area,
            );
        }
    }
    if suggestions_height > 0 {
        render_prompt_suggestions(frame, app, chunks[5]);
    }
    render_footer(frame, app, chunks[6]);

    // Free-model dropdown (Alt+J/K) — anchored just above the prompt input.
    if app.free_model_popup.visible {
        let inspector = app.free_model_popup_inspector();
        crate::free_model_popup::render_free_model_popup(
            frame,
            &app.free_model_popup,
            chunks[3],
            inspector.as_ref(),
        );
    }

    // Overlays (rendered on top in Z-order)

    // Permission dialog (highest priority)
    if let Some(ref pr) = app.permission_request {
        render_permission_dialog(frame, pr, size);
    }

    // Rewind flow (takes over screen)
    if app.rewind_flow.visible {
        render_rewind_flow(frame, &app.rewind_flow, size);
    }

    // Tasks overlay (Ctrl+T)
    if app.tasks_overlay.visible {
        render_tasks_overlay(frame, &app.tasks_overlay, size);
    }

    // Keybinding cheat-sheet overlay (Ctrl+/)
    if app.keybindings_overlay.visible {
        render_keybindings_overlay(
            frame,
            &app.keybindings_overlay,
            size,
            app.frame_count,
            app.accent_color,
        );
    }

    // New help overlay
    if app.help_overlay.visible {
        render_help_overlay(frame, &app.help_overlay, size);
    } else if app.show_help {
        // Legacy fallback â€” render the simple help overlay
        render_simple_help_overlay(frame, size);
    }

    // History search overlay
    if app.history_search_overlay.visible {
        render_history_search_overlay(
            frame,
            &app.history_search_overlay,
            &app.prompt_input.history,
            size,
        );
    } else if let Some(ref hs) = app.history_search {
        // Legacy history search rendering
        render_legacy_history_search(frame, hs, app, size);
    }

    // Settings screen (highest-priority full-screen overlay)
    if app.settings_screen.visible {
        render_settings_screen(frame, &app.settings_screen, size);
    }

    // Theme picker overlay
    if app.theme_screen.visible {
        render_theme_screen(frame, &app.theme_screen, size);
    }
    // Mode picker overlay
    if app.mode_panel.visible {
        render_mode_panel(frame, &app.mode_panel, size);
    }
    // Theme creator overlay
    if app.theme_creator.visible {
        render_theme_creator(frame, &app.theme_creator, size);
    }

    // Rustail mascot editor overlay
    if app.rustail_editor.visible {
        render_rustail_editor(frame, &app.rustail_editor, size);
    }

    if app.stats_dialog.visible {
        render_stats_dialog(&app.stats_dialog, size, frame.buffer_mut());
    }

    if app.mcp_view.visible {
        render_mcp_view(&app.mcp_view, size, frame.buffer_mut());
    }

    if app.agents_menu.visible {
        render_agents_menu(&app.agents_menu, size, frame.buffer_mut());
    }

    if app.diff_viewer.visible {
        let mut state = app.diff_viewer.clone();
        render_diff_dialog(&mut state, size, frame.buffer_mut());
    }

    if app.paste_viewer.visible {
        crate::paste_viewer::render_paste_viewer_buf(&app.paste_viewer, size, frame.buffer_mut());
    }

    if app.global_search.visible {
        render_global_search(&app.global_search, size, frame.buffer_mut());
    }

    if app.feedback_survey.visible {
        render_feedback_survey(&app.feedback_survey, size, frame.buffer_mut());
    }

    if app.memory_file_selector.visible {
        render_memory_file_selector(&app.memory_file_selector, size, frame.buffer_mut());
    }

    if app.hooks_config_menu.visible {
        render_hooks_config_menu(&app.hooks_config_menu, size, frame.buffer_mut());
    }

    // Overage credit upsell banner
    if app.overage_upsell.visible {
        let banner_h = app.overage_upsell.height();
        if size.height > banner_h + 4 {
            let banner_area = Rect {
                x: size.x,
                y: size.y,
                width: size.width,
                height: banner_h,
            };
            render_overage_upsell(&app.overage_upsell, banner_area, frame.buffer_mut());
        }
    }

    // Voice mode availability notice
    if app.voice_mode_notice.visible {
        let notice_h = app.voice_mode_notice.height();
        if size.height > notice_h + 4 {
            let notice_area = Rect {
                x: size.x,
                y: size.y,
                width: size.width,
                height: notice_h,
            };
            render_voice_mode_notice(&app.voice_mode_notice, notice_area, frame.buffer_mut());
        }
    }

    // Memory update notification banner (bottom of message area)
    if app.memory_update_notification.visible {
        let notif_h = app.memory_update_notification.height();
        if size.height > notif_h + 4 {
            // Place at the bottom of the screen, just above the prompt bar area
            let notif_y = size.y + size.height.saturating_sub(notif_h + 4);
            let notif_area = Rect {
                x: size.x,
                y: notif_y,
                width: size.width,
                height: notif_h,
            };
            render_memory_update_notification(
                &app.memory_update_notification,
                notif_area,
                frame.buffer_mut(),
            );
        }
    }

    // Desktop upsell startup modal
    if app.desktop_upsell.visible {
        render_desktop_upsell_startup(&app.desktop_upsell, size, frame.buffer_mut());
    }

    // Import-config preview dialog
    if app.import_config_dialog.visible {
        render_import_config_dialog(frame, &app.import_config_dialog, size);
    }

    // Invalid config/settings dialog (shown when settings.json or AGENTS.md is malformed)
    if app.invalid_config_dialog.visible {
        render_invalid_config_dialog(frame, &app.invalid_config_dialog, size);
    }

    // Bypass-permissions confirmation dialog (topmost — rendered last so it sits above all)
    if app.bypass_permissions_dialog.visible {
        render_bypass_permissions_dialog(frame, &app.bypass_permissions_dialog, size);
    }

    // File injection warning dialog (shown when oversized/binary files detected)
    if app.file_injection_dialog.visible {
        render_file_injection_dialog(frame, &app.file_injection_dialog, size);
    }

    // AskUserQuestion dialog — renders above bypass-permissions so the model's
    // question is never obscured by the startup confirmation prompt.
    if app.ask_user_dialog.visible {
        render_ask_user_dialog(&app.ask_user_dialog, size, frame.buffer_mut());
    }

    // First-launch onboarding dialog (shown after bypass dialog, below elicitation)
    if app.onboarding_dialog.visible {
        render_onboarding_dialog(frame, &app.onboarding_dialog, size);
    }

    // The `/effort` selector is NOT an overlay — it docks into the prompt input
    // area (see the input dispatch above), replacing the prompt box while open.

    // Import-config source picker
    if app.import_config_picker.visible {
        render_dialog_select(frame, &app.import_config_picker, size);
    }

    // Connect-a-provider dialog (/connect command)
    if app.connect_dialog.visible {
        render_dialog_select(frame, &app.connect_dialog, size);
    }

    // API key input dialog (opened from /connect for key-based providers)
    if app.key_input_dialog.visible {
        render_key_input_dialog(
            frame,
            &app.key_input_dialog,
            app.prompt_input.vim_enabled,
            size,
        );
    }

    // Custom provider URL + API key dialog.
    if app.custom_provider_dialog.visible {
        render_custom_provider_dialog(
            frame,
            &app.custom_provider_dialog,
            app.prompt_input.vim_enabled,
            size,
        );
    }

    // Ollama config dialog (host URL + model picker).
    if app.ollama_config_dialog.visible {
        render_ollama_config_dialog(
            frame,
            &app.ollama_config_dialog,
            app.prompt_input.vim_enabled,
            size,
        );
    }

    // "Free" composite-provider setup dialog (Zen + OpenRouter).
    if app.free_mode_dialog.visible {
        crate::free_mode_dialog::render_free_mode_dialog(
            frame,
            &app.free_mode_dialog,
            app.prompt_input.vim_enabled,
            size,
        );
    }

    // Smart-router comparison dialog (/compare).
    if app.compare_dialog.visible {
        crate::compare_dialog::render_compare_dialog(frame, &app.compare_dialog, size);
    }

    // Task-routing pinning dialog (/routing edit — audit spec §8.6).
    if app.routing_dialog.visible {
        crate::routing_dialog::render_routing_dialog(
            frame,
            &app.routing_dialog,
            app.prompt_input.vim_enabled,
            size,
        );
    }

    // Spec review dialog (/spec-review — audit spec §10 Accept/Edit/Reject).
    if app.spec_review.visible {
        crate::spec_review::render_spec_review(
            frame,
            &app.spec_review,
            app.prompt_input.vim_enabled,
            size,
        );
    }

    // Device code / browser auth dialog (GitHub Copilot, Anthropic OAuth)
    if app.device_auth_dialog.visible {
        render_device_auth_dialog(frame, &app.device_auth_dialog, size);
    }

    // Ctrl+K command palette
    if app.command_palette.visible {
        render_dialog_select(frame, &app.command_palette, size);
    }

    // MCP elicitation dialog (highest priority modal — rendered last to sit on top)
    if app.elicitation.visible {
        render_elicitation_dialog(
            &app.elicitation,
            size,
            app.prompt_input.vim_enabled,
            frame.buffer_mut(),
        );
    }

    // Model picker overlay
    if app.model_picker.visible {
        let inspector = app.model_picker_inspector();
        render_model_picker(
            &app.model_picker,
            size,
            frame.buffer_mut(),
            inspector.as_ref(),
        );
    }

    // Session browser overlay
    if app.session_browser.visible {
        render_session_browser(&app.session_browser, size, frame.buffer_mut());
    }

    // Session branching overlay
    if app.session_branching.visible {
        render_session_branching(&app.session_branching, size, frame.buffer_mut());
    }

    // Export format picker dialog
    if app.export_dialog.visible {
        render_export_dialog(frame, &app.export_dialog, size);
    }

    // Context visualization overlay
    if app.context_viz.visible {
        // Prefer the live app registry (refreshed after key / routing changes)
        // so the key-health table reflects the current chain; fall back to the
        // startup callback when no registry is wired (e.g. tests).
        let mut key_rows = app
            .provider_registry
            .as_ref()
            .map(|reg| key_ring_rows_from_registry(reg.as_ref()))
            .or_else(|| app.key_ring_data_fn.as_ref().map(|f| f()))
            .unwrap_or_default();
        // Merge per-provider HTTP rate limit data into the table rows.
        for row in &mut key_rows {
            if let Some(&(tokens, requests)) = app
                .provider_http_rates
                .get(&row.provider_name.to_lowercase())
            {
                row.tokens_pct = Some(tokens);
                row.requests_pct = Some(requests);
            }
        }
        // Per-upstream key-health and cooldown state for the free provider
        // (drives the status dots and cooldown annotations in the table).
        let free_health = app
            .provider_registry
            .as_ref()
            .map(|r| r.upstream_key_health_summaries())
            .unwrap_or_default()
            .into_iter()
            .find(|(pid, _)| pid == "free")
            .map(|(_, entries)| entries)
            .unwrap_or_default();
        let free_cooldowns = app
            .provider_registry
            .as_ref()
            .map(|r| r.upstream_cooldown_summaries())
            .unwrap_or_default()
            .into_iter()
            .find(|(pid, _)| pid == "free")
            .map(|(_, entries)| entries)
            .unwrap_or_default();

        render_context_viz(
            frame,
            &app.context_viz,
            size,
            app.context_used_tokens,
            app.context_window_size,
            key_rows,
            app.cost_usd,
            app.messages.len(),
            app.messages
                .iter()
                .filter(|m| m.role == clawde_core::types::Role::User)
                .count(),
            app.messages
                .iter()
                .filter(|m| m.role == clawde_core::types::Role::Assistant)
                .count(),
            app.messages
                .iter()
                .flat_map(|m| m.get_tool_use_blocks())
                .count(),
            app.free_model_defaults.clone(),
            free_health,
            free_cooldowns,
            app.model_picker.task_sort,
            current_insp.as_ref(),
        );
    }

    // MCP approval dialog
    if app.mcp_approval.visible {
        render_mcp_approval_dialog(&app.mcp_approval, size, frame.buffer_mut());
    }

    // Always show error modals on top of everything (highest priority)
    if let Some(notif) = app.notifications.current() {
        if notif.kind == NotificationKind::Error {
            let is_welcome_screen = app.messages.is_empty()
                && app.streaming_text.is_empty()
                && app.streaming_thinking.is_empty()
                && app.tool_use_blocks.is_empty();
            render_error_modal(
                frame,
                size,
                notif,
                app.error_modal_scroll_offset,
                app.footer_right_column_area.get(),
                is_welcome_screen,
            );
            return; // Don't render other overlays/notifications when error modal is showing
        }
    }

    let modal_active = is_modal_open(app);

    // Render non-error notifications as toast banners (unless another modal is open)
    if !modal_active && app.notifications.current().is_some() {
        render_notification_banner(frame, &app.notifications, size);
    }

    // ---- Text selection highlight (topmost post-pass) ---------------------
    apply_selection_highlight(frame, app);
    cache_selectable_row_text(frame, app);
    render_context_menu(frame, app);
    // Topmost: hover tooltip for the free-model task-sort badge.
    render_task_badge_tooltip(frame, app);
}

/// Snapshot the rendered text of every row inside the selectable area into
/// `app.last_row_text` so that subsequent double/triple-clicks can locate
/// word and paragraph boundaries (issue #149 follow-up).
fn cache_selectable_row_text(frame: &mut Frame, app: &App) {
    let selectable_area = app.last_selectable_area.get();
    if selectable_area.width == 0 || selectable_area.height == 0 {
        app.last_row_text.borrow_mut().clear();
        return;
    }
    let buf = frame.buffer_mut();
    let max_row = selectable_area
        .y
        .saturating_add(selectable_area.height)
        .saturating_sub(1);
    let max_col = selectable_area
        .x
        .saturating_add(selectable_area.width)
        .saturating_sub(1);
    let mut cache = app.last_row_text.borrow_mut();
    cache.clear();
    for row in selectable_area.y..=max_row {
        let mut s = String::new();
        for col in selectable_area.x..=max_col {
            if let Some(cell) = buf.cell_mut((col, row)) {
                let sym = cell.symbol();
                if sym.is_empty() || sym == "\0" {
                    s.push(' ');
                } else {
                    s.push_str(sym);
                }
            }
        }
        cache.insert(row, s);
    }
}

/// Post-render pass: invert colours on selected cells and extract the
/// selection text into `app.selection_text`.
fn apply_selection_highlight(frame: &mut Frame, app: &App) {
    let (anchor, focus) = match (app.selection_anchor, app.selection_focus) {
        (Some(a), Some(f)) => (a, f),
        _ => return,
    };
    if anchor == focus {
        return;
    }

    let selectable_area = app.last_selectable_area.get();
    if selectable_area.width == 0 || selectable_area.height == 0 {
        return;
    }

    // Validate selection is within selectable bounds
    if anchor.0 < selectable_area.x
        || anchor.0 >= selectable_area.x.saturating_add(selectable_area.width)
        || anchor.1 < selectable_area.y
        || anchor.1 >= selectable_area.y.saturating_add(selectable_area.height)
    {
        return;
    }

    let max_row = selectable_area
        .y
        .saturating_add(selectable_area.height)
        .saturating_sub(1);
    let max_col = selectable_area
        .x
        .saturating_add(selectable_area.width)
        .saturating_sub(1);

    // Clamp anchor and focus to selectable bounds
    let anchor = (
        anchor.0.clamp(selectable_area.x, max_col),
        anchor.1.clamp(selectable_area.y, max_row),
    );
    let focus = (
        focus.0.clamp(selectable_area.x, max_col),
        focus.1.clamp(selectable_area.y, max_row),
    );

    // Normalise so start ≤ end (row-major order).
    let (start, end) = if (anchor.1, anchor.0) <= (focus.1, focus.0) {
        (anchor, focus)
    } else {
        (focus, anchor)
    };

    let buf = frame.buffer_mut();
    let mut text = String::new();
    let last_row = end.1.min(max_row);
    for row in start.1..=last_row {
        let col_from = if row == start.1 {
            start.0
        } else {
            selectable_area.x
        };
        let col_to = if row == end.1 { end.0 } else { max_col };
        for col in col_from..=col_to {
            if let Some(cell) = buf.cell_mut((col, row)) {
                let sym = cell.symbol().to_owned();
                text.push_str(if sym.is_empty() || sym == "\0" {
                    " "
                } else {
                    &sym
                });
                // Highlight: white background, black foreground
                let new_style = Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(200, 200, 200));
                cell.set_style(new_style);
            }
        }
        if row < last_row {
            // Trim trailing spaces from line before newline
            while text.ends_with(' ') {
                text.pop();
            }
            text.push('\n');
        }
    }
    while text.ends_with(|c: char| c.is_whitespace()) {
        text.pop();
    }
    *app.selection_text.borrow_mut() = text;
}

/// Render a right-click context menu at the specified position.
fn render_context_menu(frame: &mut Frame, app: &App) {
    if let Some(menu) = app.context_menu_state {
        let selection_present = !app.selection_text.borrow().trim().is_empty();
        let items: Vec<(&str, bool)> = match menu.kind {
            ContextMenuKind::Message { message_index } => vec![
                ("Copy", app.messages.get(message_index).is_some()),
                ("Fork new chat", app.messages.get(message_index).is_some()),
            ],
            ContextMenuKind::Selection => vec![("Copy", selection_present)],
        };

        let menu_height = (items.len() as u16).saturating_add(2);
        let menu_width = items
            .iter()
            .map(|(label, _)| label.len())
            .max()
            .unwrap_or(4)
            .saturating_add(4) as u16;

        // Clamp menu position to screen bounds
        let screen = frame.area();
        let menu_x = menu.x.min(screen.width.saturating_sub(menu_width + 1));
        let menu_y = menu.y.min(screen.height.saturating_sub(menu_height + 1));

        let menu_area = Rect {
            x: menu_x,
            y: menu_y,
            width: menu_width,
            height: menu_height,
        };

        // Draw menu background with border
        let menu_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().fg(Color::White).bg(Color::Rgb(24, 24, 30)))
            .border_style(Style::default().fg(crate::theme_colors::current_palette().accent));
        menu_block.render(menu_area, frame.buffer_mut());

        // Render menu items
        let inner = Rect {
            x: menu_area.x + 1,
            y: menu_area.y + 1,
            width: menu_area.width.saturating_sub(2),
            height: menu_area.height.saturating_sub(2),
        };

        for (idx, (label, enabled)) in items.iter().enumerate() {
            if idx >= inner.height as usize {
                break;
            }

            let y = inner.y + idx as u16;
            let is_selected = idx == menu.selected_index;

            let fg_color = if *enabled {
                if is_selected {
                    Color::Black
                } else {
                    Color::White
                }
            } else {
                Color::DarkGray
            };

            let bg_color = if is_selected {
                if *enabled {
                    crate::theme_colors::current_palette().accent
                } else {
                    Color::Rgb(24, 24, 30)
                }
            } else {
                Color::Rgb(24, 24, 30)
            };

            let style = Style::default().fg(fg_color).bg(bg_color);
            let padded_label = format!(
                " {:<width$} ",
                label,
                width = menu_width.saturating_sub(2) as usize
            );

            if let Some(cell) = frame.buffer_mut().cell_mut((inner.x, y)) {
                cell.set_symbol(&padded_label[0..1.min(padded_label.len())]);
                cell.set_style(style);
            }

            for (col_offset, ch) in padded_label.chars().enumerate() {
                if col_offset >= inner.width as usize {
                    break;
                }
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut((inner.x + col_offset as u16, y))
                {
                    cell.set_symbol(&ch.to_string());
                    cell.set_style(style);
                }
            }
        }
    }
}

// -----------------------------------------------------------------------
// Messages pane
// -----------------------------------------------------------------------

fn render_messages(frame: &mut Frame, app: &App, area: Rect) {
    // Reserve space at the top for plugin hint banners
    let hint_height = if app.plugin_hints.iter().any(|h| h.is_visible()) {
        3u16
    } else {
        0
    };

    let (hint_area, content_area) = if hint_height > 0 && area.height > hint_height + 2 {
        let splits = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(hint_height), Constraint::Min(1)])
            .split(area);
        (Some(splits[0]), splits[1])
    } else {
        (None, area)
    };

    // Render plugin hint banner if there is one
    if let Some(ha) = hint_area {
        render_plugin_hints(frame, &app.plugin_hints, ha);
    }

    // The rich two-column welcome box is a full welcome SCREEN, shown only while
    // the transcript is empty. Once a conversation starts it is NOT kept as a
    // fixed header (which permanently ate ~9 rows, issue #310); instead a compact
    // banner is prepended to the transcript (see `welcome_banner_lines`) so it
    // scrolls away with the content and the conversation reclaims the space.
    let transcript_empty = app.messages.is_empty()
        && app.streaming_text.is_empty()
        && app.streaming_thinking.is_empty()
        && app.tool_use_blocks.is_empty();

    if transcript_empty {
        app.last_msg_area.set(Rect::default());
        app.message_row_map.borrow_mut().clear();
        app.thinking_row_map.borrow_mut().clear();
        render_welcome_box(frame, app, content_area);
        // Startup notices (remote session, +dir, away summary) sit just below
        // the welcome box on the empty screen.
        let notice_lines = startup_notice_lines(app, content_area.width);
        if !notice_lines.is_empty() && content_area.height > WELCOME_BOX_HEIGHT {
            let notices_area = Rect {
                x: content_area.x,
                y: content_area.y + WELCOME_BOX_HEIGHT,
                width: content_area.width,
                height: content_area.height - WELCOME_BOX_HEIGHT,
            };
            render_startup_notices(frame, app, notices_area);
        }
        return;
    }

    // Active conversation: the whole content area is the (scrollable) transcript.
    // The welcome box is no longer on screen, so clear the anchor rect used to
    // position error modals against its right column.
    app.footer_right_column_area.set(Rect::default());
    let msg_area = content_area;

    // Store the actual message pane bounds for mouse event handling (text selection, scrolling).
    app.last_msg_area.set(msg_area);

    let lines = render_message_items(app, msg_area.width);

    // Highlight search matches in transcript when global search is active
    let lines = if app.global_search.visible && !app.global_search.query.is_empty() {
        let query_lc = app.global_search.query.to_lowercase();
        lines
            .into_iter()
            .map(|mut item| {
                if item.search_text.to_lowercase().contains(query_lc.as_str()) {
                    // Re-render the line with yellow highlight on matching spans
                    let highlighted_spans: Vec<Span<'static>> = item
                        .line
                        .spans
                        .into_iter()
                        .map(|span| {
                            if span.content.to_lowercase().contains(query_lc.as_str()) {
                                Span::styled(
                                    span.content,
                                    span.style.bg(Color::Rgb(60, 50, 0)).fg(Color::Yellow),
                                )
                            } else {
                                span
                            }
                        })
                        .collect();
                    item.line = ratatui::text::Line::from(highlighted_spans);
                }
                item
            })
            .collect()
    } else {
        lines
    };

    // Append persisted followups when in history mode (Alt+F toggle). Current
    // followups are appended by `build_all_items` / `render_streaming_items`;
    // history rows are appended here so every render path shows exactly one
    // followup list.
    let mut items = lines;
    if app.followup_history_mode {
        append_history_followup_items(app, &mut items, msg_area.width);
    }
    let lines = items;

    // Compute total virtual height and apply scroll clamping.
    // When auto_scroll is on we always show the tail; otherwise we respect
    // the user's scroll_offset.
    let content_height = lines.len() as u16;
    let visible_height = msg_area.height; // no borders, full height available
    let max_scroll = content_height.saturating_sub(visible_height) as usize;
    // Publish the max meaningful scroll offset so the next scroll event can
    // clamp `scroll_offset` against it (the content height is only known here,
    // at render time). Prevents unbounded inflation when scrolling past the top
    // (#223).
    app.last_max_scroll.set(max_scroll);
    // scroll_offset counts lines above the bottom (0 = at bottom).
    // ratatui scroll() takes an absolute top-row index, so convert:
    //   top_row = max_scroll - scroll_offset  (clamped to [0, max_scroll])
    let scroll = if app.auto_scroll {
        max_scroll
    } else {
        max_scroll.saturating_sub(app.scroll_offset)
    };

    let mut visible_rows: std::collections::HashMap<u16, usize> = std::collections::HashMap::new();
    let mut thinking_rows: std::collections::HashMap<u16, u64> = std::collections::HashMap::new();
    for (idx, item) in lines
        .iter()
        .enumerate()
        .skip(scroll)
        .take(msg_area.height as usize)
    {
        let screen_row = msg_area
            .y
            .saturating_add((idx.saturating_sub(scroll)) as u16);
        if let Some(message_index) = item.message_index {
            visible_rows.insert(screen_row, message_index);
        }
        if let Some(hash) = item.thinking_hash {
            thinking_rows.insert(screen_row, hash);
        }
    }
    *app.message_row_map.borrow_mut() = visible_rows;
    *app.thinking_row_map.borrow_mut() = thinking_rows;

    // Track visible followup rows for click-to-insert using explicit targets
    // attached during rendering; this remains correct when lists are mixed.
    let mut followup_rows: std::collections::HashMap<u16, FollowupRowTarget> =
        std::collections::HashMap::new();
    let active_followup_count = if app.followup_history_mode {
        app.persisted_followups.len()
    } else {
        app.current_followups.len()
    };
    if active_followup_count > 0 {
        for (idx, item) in lines
            .iter()
            .enumerate()
            .skip(scroll)
            .take(msg_area.height as usize)
        {
            let screen_row = msg_area
                .y
                .saturating_add((idx.saturating_sub(scroll)) as u16);
            if let Some(target) = item.followup_target {
                if target.index < active_followup_count {
                    followup_rows.insert(screen_row, target);
                }
            }
        }
    }
    *app.followup_row_map.borrow_mut() = followup_rows;

    // No border — messages render directly into the area.
    let mut list = VirtualList::new();
    list.viewport_height = msg_area.height;
    list.sticky_bottom = app.auto_scroll;
    list.set_items(lines);
    list.scroll_offset = scroll as u16;

    // Track scroll offset for selection validation
    app.last_render_scroll_offset.set(scroll as u16);

    list.render(msg_area, frame.buffer_mut());

    // Scrollbar: thin vertical strip flush with the right edge — no arrow
    // caps, no visible track, muted thumb color. Mirrors Windows Terminal /
    // most modern terminal scrollbars rather than ratatui's chunky default.
    if content_height > visible_height {
        use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

        // ratatui 0.29's Scrollbar maps `position` over `content_length - 1`,
        // not over a 0..=max_scroll range. Passing `content_height` directly
        // makes the thumb top out at `content / (content + viewport)` of the
        // track when fully scrolled — i.e. it never reaches the bottom.
        // Fix: tell ratatui the content length is the number of distinct
        // scroll positions (`max_scroll + 1`), keeping `viewport_content_length`
        // for the proportional thumb size.
        let content_len = max_scroll + 1;
        let mut scrollbar_state = ScrollbarState::new(content_len)
            .position(scroll.min(max_scroll))
            .viewport_content_length(visible_height as usize);

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("\u{2590}") // ▐ right half block — thin vertical strip
            .thumb_style(Style::default().fg(Color::Rgb(110, 110, 130)));

        frame.render_stateful_widget(scrollbar, msg_area, &mut scrollbar_state);
    }

    // “↓ N new messages” jump-to-bottom pill when the user is scrolled up
    if app.new_messages_while_scrolled > 0 && msg_area.height > 4 && msg_area.width > 20 {
        let indicator = format!(
            " \u{2193} {} new message{} ",
            app.new_messages_while_scrolled,
            if app.new_messages_while_scrolled == 1 {
                ""
            } else {
                "s"
            }
        );
        let ind_len = indicator.len() as u16;
        let ind_x = msg_area
            .x
            .saturating_add(msg_area.width.saturating_sub(ind_len + 2));
        let ind_y = msg_area.y + msg_area.height.saturating_sub(1);
        let ind_area = Rect {
            x: ind_x,
            y: ind_y,
            width: ind_len.min(msg_area.width.saturating_sub(2)),
            height: 1,
        };
        let ind_line = Line::from(vec![Span::styled(
            indicator,
            Style::default()
                .fg(Color::Black)
                .bg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(Paragraph::new(vec![ind_line]), ind_area);
        // Record the pill's horizontal span so a click on it can jump to bottom.
        app.last_jump_bottom_area
            .set(Some((ind_y, ind_x, ind_x.saturating_add(ind_area.width))));
    } else {
        app.last_jump_bottom_area.set(None);
    }
}

fn push_rendered_items(
    items: &mut Vec<RenderedLineItem>,
    lines: Vec<Line<'static>>,
    message_index: Option<usize>,
    mark_first_header: bool,
) {
    for (index, line) in lines.into_iter().enumerate() {
        items.push(RenderedLineItem {
            search_text: flatten_line_text(&line),
            is_header: mark_first_header && index == 0,
            message_index,
            thinking_hash: None,
            followup_target: None,
            line,
        });
    }
}

/// Push tagged lines from `render_transcript_assistant_message_tagged`.
/// Lines with `Some(hash)` become clickable thinking headers.
fn push_rendered_items_tagged(
    items: &mut Vec<RenderedLineItem>,
    tagged: Vec<(Line<'static>, Option<u64>)>,
    message_index: Option<usize>,
) {
    for (line, thinking_hash) in tagged {
        items.push(RenderedLineItem {
            search_text: flatten_line_text(&line),
            is_header: false,
            message_index,
            thinking_hash,
            followup_target: None,
            line,
        });
    }
}

fn push_blank_item(items: &mut Vec<RenderedLineItem>) {
    push_rendered_items(items, vec![Line::from("")], None, false);
}

fn render_live_thinking_lines(
    turn: &TranscriptTurn<'_>,
    frame_count: u64,
    width: u16,
) -> Vec<Line<'static>> {
    let mut header_spans = vec![Span::raw("  ▼ ")];
    header_spans.extend(shimmer_spans("Thinking", frame_count));
    if let Some(heading) = turn.reasoning_heading() {
        header_spans.push(Span::styled(
            format!(": {}", heading),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    let mut lines = vec![Line::from(header_spans)];
    if let Some(text) = turn.live_thinking {
        lines.extend(render_thinking_live_content(text, width));
    }
    lines
}

fn append_turn_items(
    items: &mut Vec<RenderedLineItem>,
    turn: &TranscriptTurn<'_>,
    ctx: &RenderContext,
    frame_count: u64,
    accent: Color,
) {
    let width = ctx.width;
    push_rendered_items(
        items,
        render_transcript_user_message(turn.user_message, turn.metadata, width),
        Some(turn.user_index),
        true,
    );

    enum SectionContent {
        Plain(Vec<Line<'static>>),
        Tagged(Vec<(Line<'static>, Option<u64>)>),
    }

    let mut sections: Vec<(SectionContent, Option<usize>)> = Vec::new();
    for (message_index, message) in &turn.assistant_messages {
        let tagged = render_transcript_assistant_message_tagged(message, ctx);
        if !tagged.is_empty() {
            sections.push((SectionContent::Tagged(tagged), Some(*message_index)));
        }
    }

    for block in &turn.tool_blocks {
        let mut lines = Vec::new();
        render_tool_block_lines(&mut lines, block, frame_count);
        if !lines.is_empty() {
            sections.push((
                SectionContent::Plain(lines),
                Some(turn.primary_message_index()),
            ));
        }
    }

    if turn.active && turn.live_thinking.is_some() {
        sections.push((
            SectionContent::Plain(render_live_thinking_lines(turn, frame_count, width)),
            Some(turn.primary_message_index()),
        ));
    }

    // Show a "Thinking" shimmer when the turn is active but no text or
    // thinking content has arrived yet — gives visual feedback that the
    // model is working (especially for providers without thinking support).
    if turn.active
        && turn.live_text.is_none()
        && turn.live_thinking.is_none()
        && turn
            .tool_blocks
            .iter()
            .all(|b| b.status != ToolStatus::Running)
    {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(shimmer_spans("Thinking", frame_count));
        sections.push((
            SectionContent::Plain(vec![Line::from(spans)]),
            Some(turn.primary_message_index()),
        ));
    }

    if let Some(text) = turn.live_text {
        let lines = render_transcript_live_text(text, width);
        if !lines.is_empty() {
            sections.push((
                SectionContent::Plain(lines),
                Some(turn.primary_message_index()),
            ));
        }
    }

    if !turn.active {
        if let Some(meta_line) = render_transcript_assistant_meta(
            turn.metadata,
            turn.assistant_messages.last().map(|(_, m)| *m),
            accent,
        ) {
            if turn.has_visible_assistant_content() {
                sections.push((
                    SectionContent::Plain(vec![meta_line]),
                    Some(turn.primary_message_index()),
                ));
            }
        }
    }

    if !sections.is_empty() {
        push_blank_item(items);
        let total_sections = sections.len();
        for (index, (content, message_index)) in sections.into_iter().enumerate() {
            match content {
                SectionContent::Plain(lines) => {
                    push_rendered_items(items, lines, message_index, false)
                }
                SectionContent::Tagged(tagged) => {
                    push_rendered_items_tagged(items, tagged, message_index)
                }
            }
            if index + 1 < total_sections {
                push_blank_item(items);
            }
        }
    }

    push_blank_item(items);
}

/// Append rendered items for the transcript messages in `[start, end)` to
/// `items`, mirroring the single linear pass used by the full transcript build.
///
/// System annotations are emitted at the top of each landed index exactly as
/// the full pass does; `emit_end_annotations` additionally flushes the
/// annotations anchored at `end` (used when `end` is the true message count so
/// trailing annotations are not lost).
///
/// Splitting the pass at a turn boundary is byte-identical to building the whole
/// range in one shot: `range(0, k, false)` followed by `range(k, total, true)`
/// produces exactly the same items as `range(0, total, true)` whenever `k` is an
/// index the linear pass lands on (i.e. a turn's user index). This is what lets
/// the streaming path serve the committed prefix from cache and rebuild only the
/// live tail without any risk of ghosting.
#[allow(clippy::too_many_arguments)]
fn build_message_items_range(
    app: &App,
    width: u16,
    ctx: &RenderContext,
    turn_map: &std::collections::HashMap<usize, &TranscriptTurn<'_>>,
    start: usize,
    end: usize,
    emit_end_annotations: bool,
    items: &mut Vec<RenderedLineItem>,
) {
    let mut index = start;
    while index < end {
        for ann in app
            .system_annotations
            .iter()
            .filter(|ann| ann.after_index == index)
        {
            let mut lines = Vec::new();
            render_system_annotation_lines(&mut lines, ann, width as usize);
            // Record where the verify box lands in the rendered line list so a
            // click on the footer badge can scroll it into view later.
            if ann.style == SystemMessageStyle::Verify {
                app.last_verify_box_line.set(Some(items.len()));
            }
            push_rendered_items(items, lines, None, false);
        }

        let message = &app.messages[index];
        if message.role == Role::User {
            if let Some(&turn) = turn_map.get(&index) {
                append_turn_items(items, turn, ctx, app.frame_count, app.accent_color);
                index = turn.end_message_index + 1;
                continue;
            }
        }

        let tagged = render_transcript_assistant_message_tagged(message, ctx);
        push_rendered_items_tagged(items, tagged, Some(index));
        push_blank_item(items);
        index += 1;
    }

    if emit_end_annotations {
        for ann in app
            .system_annotations
            .iter()
            .filter(|ann| ann.after_index == end)
        {
            let mut lines = Vec::new();
            render_system_annotation_lines(&mut lines, ann, width as usize);
            push_rendered_items(items, lines, None, false);
        }
    }
}

/// Build the full transcript item list from scratch (no caching). Used for the
/// non-streaming path, the streaming fallback, and as the correctness reference
/// in tests.
fn build_all_items(app: &App, width: u16) -> Vec<RenderedLineItem> {
    // Build `tool_names` and the render context ONCE per rebuild and lend them
    // to every message renderer (issue #222).
    let tool_names = build_tool_names(&app.messages);
    let ctx = RenderContext {
        width,
        highlight: true,
        show_thinking: false,
        tool_names: &tool_names,
        expanded_thinking: &app.thinking_expanded,
        followup_selected: app.followup_selected,
    };
    let turns = build_transcript_turns(app);
    let mut turn_map = std::collections::HashMap::new();
    for turn in &turns {
        turn_map.insert(turn.user_index, turn);
    }

    let total = app.messages.len();
    let mut items = Vec::new();
    // Prepend the compact welcome banner as ordinary scrollable content so it
    // scrolls away with the conversation instead of sitting in a fixed header
    // (issue #310).
    push_rendered_items(&mut items, welcome_banner_lines(app, width), None, false);
    build_message_items_range(app, width, &ctx, &turn_map, 0, total, true, &mut items);
    append_current_followup_items(app, &mut items, width);

    if total == 0 && !app.tool_use_blocks.is_empty() {
        for block in &app.tool_use_blocks {
            let mut lines = Vec::new();
            render_tool_block_lines(&mut lines, block, app.frame_count);
            push_rendered_items(&mut items, lines, None, false);
            push_blank_item(&mut items);
        }
    }

    items
}

fn render_message_items(app: &App, width: u16) -> Vec<RenderedLineItem> {
    let streaming =
        app.is_streaming || !app.streaming_text.is_empty() || !app.streaming_thinking.is_empty();
    let has_running_tool_blocks = app
        .tool_use_blocks
        .iter()
        .any(|block| block.status == ToolStatus::Running);
    let cacheable = !streaming && !has_running_tool_blocks;

    if !cacheable {
        // Live content is on screen. Instead of re-rendering the whole backlog
        // every frame (the O(messages^2) hot path from issue #222), serve the
        // committed prefix from cache and rebuild only the live tail.
        return render_streaming_items(app, width);
    }

    // Fast path: nothing live — use the full-result cache (ptr-stable check).
    let full_key = MessageLinesCacheKey {
        width,
        transcript_version: app.transcript_version.get(),
        messages_ptr: app.messages.as_ptr() as usize,
        messages_len: app.messages.len(),
        annotations_ptr: app.system_annotations.as_ptr() as usize,
        annotations_len: app.system_annotations.len(),
        thinking_expanded_len: app.thinking_expanded.len(),
        followup_history_mode: app.followup_history_mode,
        current_followups_ptr: app.current_followups.as_ptr() as usize,
        current_followups_len: app.current_followups.len(),
        persisted_followups_ptr: app
            .persisted_followups
            .front()
            .map_or(0, |followup| followup as *const _ as usize),
        persisted_followups_len: app.persisted_followups.len(),
        followup_selected: app.followup_selected,
    };
    if let Some(lines) = MESSAGE_LINES_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|c| c.key == full_key)
            .map(|c| c.lines.clone())
    }) {
        return lines;
    }

    let items = build_all_items(app, width);
    MESSAGE_LINES_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(MessageLinesCache {
            key: full_key,
            lines: items.clone(),
        });
    });
    items
}

/// Render the transcript while there is live content on screen.
///
/// The only part of the transcript that changes between streaming frames is the
/// last turn (its live text/thinking and any running tool blocks). Every earlier
/// turn is already committed and byte-identical to a full rebuild, so we serve
/// that committed prefix from `COMPLETED_MSG_CACHE` and rebuild only the live
/// tail. Because `build_message_items_range` splits the exact same linear pass
/// at a turn boundary, `prefix ++ tail` is identical to `build_all_items` — no
/// ghosting, no missing content.
fn append_current_followup_items(app: &App, items: &mut Vec<RenderedLineItem>, width: u16) {
    if app.followup_history_mode || app.current_followups.is_empty() {
        return;
    }
    for (index, line) in crate::messages::render_ranked_followups_wrapped(
        &app.current_followups,
        app.followup_selected,
        width,
    ) {
        items.push(RenderedLineItem {
            search_text: flatten_line_text(&line),
            is_header: false,
            message_index: None,
            thinking_hash: None,
            followup_target: (index != usize::MAX).then_some(FollowupRowTarget {
                source: FollowupSource::Current,
                index,
            }),
            line,
        });
    }
}

fn append_history_followup_items(app: &App, items: &mut Vec<RenderedLineItem>, width: u16) {
    if !app.followup_history_mode || app.persisted_followups.is_empty() {
        return;
    }
    let followups: Vec<_> = app.persisted_followups.iter().cloned().collect();
    for (index, line) in
        crate::messages::render_ranked_followups_wrapped(&followups, app.followup_selected, width)
    {
        items.push(RenderedLineItem {
            search_text: flatten_line_text(&line),
            is_header: false,
            message_index: None,
            thinking_hash: None,
            followup_target: (index != usize::MAX).then_some(FollowupRowTarget {
                source: FollowupSource::History,
                index,
            }),
            line,
        });
    }
}

fn render_streaming_items(app: &App, width: u16) -> Vec<RenderedLineItem> {
    let tool_names = build_tool_names(&app.messages);
    let ctx = RenderContext {
        width,
        highlight: true,
        show_thinking: false,
        tool_names: &tool_names,
        expanded_thinking: &app.thinking_expanded,
        followup_selected: app.followup_selected,
    };
    let turns = build_transcript_turns(app);

    // The live tail is the last turn; its user index is the prefix boundary.
    // Without a turn (e.g. tool-blocks-only welcome state) there is no stable
    // prefix to reuse, so fall back to a full rebuild.
    let split_idx = match turns.last() {
        Some(last) => last.user_index,
        None => return build_all_items(app, width),
    };

    let mut turn_map = std::collections::HashMap::new();
    for turn in &turns {
        turn_map.insert(turn.user_index, turn);
    }

    let total = app.messages.len();
    let prefix_key = CompletedMsgCacheKey {
        width,
        prefix_len: split_idx,
        messages_ptr: app.messages.as_ptr() as usize,
        messages_len: total,
        annotations_ptr: app.system_annotations.as_ptr() as usize,
        annotations_len: app.system_annotations.len(),
        thinking_expanded_len: app.thinking_expanded.len(),
    };

    // Committed prefix: messages before the live turn. Stable across streaming
    // deltas, so keyed by identity (not `transcript_version`) and served from
    // cache every frame after the first. The cached prefix does NOT include the
    // welcome banner, so the entry stays byte-identical to the non-streaming
    // build's committed range.
    let prefix = if let Some(lines) = COMPLETED_MSG_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|c| c.key == prefix_key)
            .map(|c| c.lines.clone())
    }) {
        record_prefix_cache_hit();
        lines
    } else {
        record_prefix_cache_miss();
        let mut prefix = Vec::new();
        build_message_items_range(
            app,
            width,
            &ctx,
            &turn_map,
            0,
            split_idx,
            false,
            &mut prefix,
        );
        COMPLETED_MSG_CACHE.with(|cache| {
            *cache.borrow_mut() = Some(CompletedMsgCache {
                key: prefix_key,
                lines: prefix.clone(),
            });
        });
        prefix
    };

    // The welcome banner leads the transcript and scrolls away with content
    // (issue #310). Prepended here, outside the committed-prefix cache, so
    // banner ++ prefix ++ tail stays byte-identical to `build_all_items`.
    let mut items = Vec::new();
    push_rendered_items(&mut items, welcome_banner_lines(app, width), None, false);
    items.extend(prefix);

    // Live tail: the actively-streaming turn, rebuilt fresh every frame.
    build_message_items_range(
        app, width, &ctx, &turn_map, split_idx, total, true, &mut items,
    );
    // History rows are appended once by `render_messages` after this function
    // returns; appending them here too would double-render the list while
    // streaming in history mode.
    if !app.followup_history_mode {
        append_current_followup_items(app, &mut items, width);
    }
    items
}

// â”€â”€ Welcome / startup screen â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Render the two-column orange round-bordered welcome box (matches TS LogoV2).
fn render_welcome_box(frame: &mut Frame, app: &App, area: Rect) {
    // --- Box dimensions ---
    // The box should be at most the full area width, and a fixed height.
    let box_width = area.width;
    let box_height: u16 = WELCOME_BOX_HEIGHT;
    if area.height < box_height || box_width < 35 {
        // Too small: fall back to a single line
        let line = Line::from(vec![
            Span::styled(
                "Clawde ",
                Style::default()
                    .fg(CLAUDE_ORANGE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{}", APP_VERSION),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(vec![line]), area);
        return;
    }
    let box_area = Rect {
        x: area.x,
        y: area.y,
        width: box_width,
        height: box_height,
    };

    // Outer border with title "Clawde vX.Y"
    let accent = app.accent_color;
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Line::from(vec![
            Span::styled(
                " \u{1F43E} Clawde ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{} ", APP_VERSION),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    frame.render_widget(outer_block, box_area);

    // Inner area (inside the border)
    let inner = Rect {
        x: box_area.x + 1,
        y: box_area.y + 1,
        width: box_area.width.saturating_sub(2),
        height: box_area.height.saturating_sub(2),
    };

    // Split inner into left | divider(1) | right
    // Left width: at least 29 (mascot art is 29 wide), at most 32
    let left_w = (inner.width / 2)
        .clamp(29, 32)
        .min(inner.width.saturating_sub(3));
    let right_w = inner.width.saturating_sub(left_w + 1);
    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_w),
            Constraint::Length(1),
            Constraint::Length(right_w),
        ])
        .split(inner);

    // Store the right column area for error modal positioning
    app.footer_right_column_area.set(h_chunks[2]);

    // Draw vertical divider in accent color
    let divider_lines: Vec<Line> = (0..inner.height)
        .map(|_| Line::from(Span::styled("\u{2502}", Style::default().fg(accent))))
        .collect();
    frame.render_widget(Paragraph::new(divider_lines), h_chunks[1]);

    // --- Left column ---
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty());
    let welcome_msg = if let Some(ref name) = username {
        format!("Welcome back {}!", name)
    } else {
        "Welcome back!".to_string()
    };
    let rustail = rustail_lines(&app.rustail_current_pose);
    let mut left_lines: Vec<Line> = Vec::new();
    left_lines.push(Line::from(Span::styled(
        welcome_msg,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    // Blank separator is intentionally removed — the greeting sits flush above
    // the mascot, giving all 10 rows to the animation.  (The right column is
    // unaffected since both panes are independent Paragraph widgets.)
    // Center mascot in left column
    let mascot_indent = left_w.saturating_sub(29) / 2;
    let pad = " ".repeat(mascot_indent as usize);
    for cl in &rustail {
        let mut spans = vec![Span::raw(pad.clone())];
        spans.extend(cl.spans.iter().cloned());
        left_lines.push(Line::from(spans));
    }
    left_lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(
            "All borders Are porous to cats",
            Style::default().fg(accent),
        ),
    ]));
    // No wrapping needed: all mascot rows (29 wide + 1 pad) fit within
    // left_w (=29-32), and the welcome message is shorter still. Using plain
    // Paragraph (no Wrap) avoids any potential WordWrapper quirks with
    // Unicode block characters in the mascot frames.
    frame.render_widget(Paragraph::new(left_lines), h_chunks[0]);

    // --- Right column ---
    let tip_text = clawde_core::tips::select_tip(0)
        .map(|t| t.content.to_string())
        .unwrap_or_else(|| "Edit AGENTS.md to add instructions for Clawde".to_string());

    let mut right_lines: Vec<Line> = Vec::new();
    right_lines.push(Line::from(Span::styled(
        "Tips for getting started",
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )));
    // Word-wrap the tip text into the right column width
    let right_w_usize = right_w.saturating_sub(1) as usize;
    for chunk in tip_text
        .chars()
        .collect::<Vec<_>>()
        .chunks(right_w_usize.max(1))
    {
        right_lines.push(Line::from(chunk.iter().collect::<String>()));
    }
    right_lines.push(Line::from(""));

    // Free mode health badge — shows key count when free mode has keys
    let free_filled = app.free_mode_dialog.filled_count();
    if free_filled > 0 {
        right_lines.push(Line::from(vec![
            Span::styled(
                " Free mode ",
                Style::default()
                    .fg(Color::Rgb(120, 210, 150))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{} key{}",
                    free_filled,
                    if free_filled == 1 { "" } else { "s" }
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    // Mnemosyne status (audit spec §15.3): one compact line when the project
    // has memory files, so freshness is visible at a glance. Only rendered on
    // the welcome screen, so the small dir scan is not a per-frame cost during
    // active conversation.
    if let Some(mem_line) = app
        .config
        .project_dir
        .as_deref()
        .map(clawde_core::memdir::auto_memory_path)
        .and_then(|dir| project_memory_line(&dir))
    {
        right_lines.push(Line::from(""));
        right_lines.push(mem_line);
    }

    // Record the absolute screen row where "Recent activity" starts, so the
    // mouse handler can compute which session row was clicked without guessing
    // the tip-text height.  The right column is rendered as a single Paragraph
    // starting at h_chunks[2].y; the "Recent activity" header is at the row
    // count of right_lines (before we push it).
    let recent_header_row = h_chunks[2].y + right_lines.len() as u16;

    right_lines.push(Line::from(Span::styled(
        "Recent activity",
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )));
    right_lines.extend(recent_activity_lines(
        &app.recent_sessions,
        right_w_usize,
        app.recent_activity_hovered_idx.get(),
    ));

    app.recent_activity_start_row.set(recent_header_row + 1);

    frame.render_widget(
        Paragraph::new(right_lines).wrap(Wrap { trim: false }),
        h_chunks[2],
    );
}

/// Build the compact welcome banner shown at the very top of the transcript.
///
/// Unlike the full two-column welcome box (which is a whole welcome *screen*
/// rendered only while the transcript is empty), this banner is prepended to the
/// message list as ordinary scrollable content, so it scrolls away with the
/// conversation instead of occupying a permanent fixed header (issue #310). It
/// carries the greeting the box led with plus a getting-started hint and any
/// startup notices, in just a few rows.
///
/// Deliberately free of disk/IO or per-frame state (no `select_tip`, which reads
/// the tip history from disk) so it is cheap to rebuild every streaming frame and
/// byte-identical between the full and cached-prefix render paths.
fn welcome_banner_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let accent = app.accent_color;

    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty());
    let greeting = match username {
        Some(ref name) => format!("Welcome back, {}!", name),
        None => "Welcome to Clawde".to_string(),
    };

    // Too narrow for a bordered box: fall back to a single title line + notices.
    if width < 24 {
        let mut lines = vec![Line::from(vec![
            Span::styled(
                "Clawde ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{}", APP_VERSION),
                Style::default().fg(Color::DarkGray),
            ),
        ])];
        lines.extend(startup_notice_lines(app, width));
        lines.push(Line::from(""));
        return lines;
    }

    let box_w = width as usize;
    let inner_w = box_w.saturating_sub(4); // "│ " + content + " │"

    // Top border with an embedded title: ╭─ 🐾Clawde vX.Y ─…─╮
    // Span widths: "╭─"=2, " 🐾Clawde "=10, "v{ver} "=ver+2, dashes=fill, "╮"=1.
    let used = 2 + 10 + (APP_VERSION.len() + 2) + 1;
    let fill = box_w.saturating_sub(used);
    let top = Line::from(vec![
        Span::styled("\u{256d}\u{2500}", Style::default().fg(accent)),
        Span::styled(
            " \u{1F43E} Clawde ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("v{} ", APP_VERSION),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{}\u{256e}", "\u{2500}".repeat(fill)),
            Style::default().fg(accent),
        ),
    ]);

    let content_row = |text: String, style: Style| -> Line<'static> {
        let text = truncate_end(&text, inner_w);
        let pad = inner_w.saturating_sub(UnicodeWidthStr::width(text.as_str()));
        Line::from(vec![
            Span::styled("\u{2502} ", Style::default().fg(accent)),
            Span::styled(text, style),
            Span::raw(" ".repeat(pad)),
            Span::styled(" \u{2502}", Style::default().fg(accent)),
        ])
    };

    let bottom = Line::from(Span::styled(
        format!(
            "\u{2570}{}\u{256f}",
            "\u{2500}".repeat(box_w.saturating_sub(2))
        ),
        Style::default().fg(accent),
    ));

    let mut lines = vec![
        top,
        content_row(
            greeting,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        content_row(
            "/help for commands  \u{00b7}  ? for shortcuts".to_string(),
            Style::default().fg(Color::Gray),
        ),
        bottom,
    ];
    // Show up to 3 recent sessions in the compact banner (only when the welcome
    // screen is not showing — the full box already has the full list).
    if !app.recent_sessions.is_empty() && !app.messages.is_empty() {
        let recents = recent_activity_lines(&app.recent_sessions, inner_w, None);
        if !recents.is_empty() {
            lines.push(Line::from(Span::styled(
                " Recent activity",
                Style::default()
                    .fg(app.accent_color)
                    .add_modifier(Modifier::BOLD),
            )));
            for rline in recents.into_iter().take(3) {
                lines.push(rline);
            }
        }
    }
    lines.extend(startup_notice_lines(app, width));
    lines.push(Line::from(""));
    lines
}

// â”€â”€ Per-message rendering â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Build a tool_use_id → tool_name lookup from all messages in the transcript.
/// This allows ToolResult blocks to dispatch to tool-specific renderers.
fn build_tool_names(
    messages: &[clawde_core::types::Message],
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for msg in messages {
        for block in msg.content_blocks() {
            if let clawde_core::types::ContentBlock::ToolUse { id, name, .. } = block {
                map.insert(id.clone(), name.clone());
            }
        }
    }
    map
}

// â”€â”€ System annotation (compact boundary, info notices) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Execute-and-verify round indicator (audit spec Phase 1 §15.1): a boxed
/// block with one line per check (✓/✗/△ + right-aligned PASS/FAIL/SKIP) and a
/// summary headline coloured by outcome.
fn render_verify_block(
    lines: &mut Vec<Line<'static>>,
    report: &clawde_query::VerifyReport,
    width: usize,
) {
    let border = Style::default().fg(Color::DarkGray);
    // Cap the box width so very wide terminals don't produce a giant rule;
    // also leave a small left margin so the box reads as an annotation.
    let box_w = width.saturating_sub(2).clamp(12, 96);
    let area = box_w.saturating_sub(6); // content between "│ " and " │"

    // ┌─ Verify · git worktree ──...──┐
    let title = if report.unavailable {
        format!(" Verify · {} · unavailable ", report.sandbox.label())
    } else {
        format!(" Verify · {} ", report.sandbox.label())
    };
    let title_fill = box_w.saturating_sub(5 + title.chars().count());
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("┌─{}{}┐", title, "─".repeat(title_fill)), border),
    ]));

    // One row per check: ✓/✗/△ + label + right-aligned status. The status
    // carries the check's wall-clock duration when it actually ran, so a slow
    // test is visible at a glance: `PASS (42s)`.
    for r in &report.results {
        let (icon, status, color) = if r.skipped {
            ("△", "SKIP", Color::Yellow)
        } else if r.ok {
            ("✓", "PASS", Color::Green)
        } else if r.timed_out {
            ("✗", "TIMEOUT", Color::Red)
        } else {
            ("✗", "FAIL", Color::Red)
        };
        // A skipped check never ran, so it must not show a duration even if a
        // stray value were attached upstream.
        let timing = if r.skipped {
            String::new()
        } else {
            r.elapsed_secs
                .map(|s| format!(" ({s}s)"))
                .unwrap_or_default()
        };
        let status_disp = format!(" {status}{timing}");
        let label_max = area.saturating_sub(2 + status_disp.chars().count() + 1);
        let label = truncate_end(&r.label, label_max.max(4));
        let content = format!("{icon} {label}");
        let pad = area.saturating_sub(content.chars().count() + status_disp.chars().count());
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("│ ", border),
            Span::styled(content, Style::default().fg(color)),
            Span::raw(" ".repeat(pad)),
            Span::styled(
                status_disp,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │", border),
        ]));
    }

    // Summary headline row: the policy's headline text, coloured by outcome.
    let headline = report.headline.clone();
    let hcolor = verify_headline_color(report);
    let pad = area.saturating_sub(headline.chars().count());
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("│ ", border),
        Span::styled(
            headline,
            Style::default().fg(hcolor).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(" │", border),
    ]));

    // └─────...──┘
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("└{}┘", "─".repeat(box_w.saturating_sub(4))), border),
    ]));
}

/// Summary colour for a verify round's headline.
///
/// The headline TEXT comes from [`VerifyPolicy`](clawde_query::VerifyPolicy) —
/// the single source of truth for what happened ("All checks passed",
/// "Auto-fix attempt 1/3", "Verification could not run — commands missing",
/// ...). Only the colour is derived here, from the per-check results.
fn verify_headline_color(report: &clawde_query::VerifyReport) -> Color {
    if report.unavailable {
        return Color::Yellow;
    }
    let any_failure = report.results.iter().any(|r| !r.ok && !r.skipped);
    let all_skipped = !report.results.is_empty() && report.results.iter().all(|r| r.skipped);
    if report.results.is_empty() {
        Color::DarkGray
    } else if any_failure && report.attempt > report.max_retries {
        // Auto-fix retries exhausted — still failing.
        Color::Red
    } else if any_failure {
        // Mid-loop auto-fix round — fixing.
        Color::Yellow
    } else if all_skipped {
        // Every command failed to start — an environment gap, not a pass.
        Color::Yellow
    } else {
        Color::Green
    }
}

/// Footer badge for the most recent verify round: icon + colour + optional
/// attempt counter. Green ✓ when everything passed, red ✗ when any check
/// failed, neutral △ when nothing ran (no checks configured / skipped).
/// The attempt counter is shown only for mid-loop auto-fix rounds so a
/// `✓ verify` or `✗ verify` stays compact.
fn verify_footer_badge(report: &clawde_query::VerifyReport) -> (String, Color) {
    let (icon, color) = if report.unavailable {
        ("!", Color::Yellow)
    } else if report.results.is_empty() {
        ("△", Color::DarkGray)
    } else if report.results.iter().all(|r| r.ok || r.skipped) {
        ("✓", Color::Green)
    } else {
        ("✗", Color::Red)
    };
    // Show the attempt counter only for mid-loop auto-fix rounds; the final
    // round (passed, or auto-fix exhausted) stays a compact ✓/✗.
    let attempt = if report.attempt > 1 && report.attempt <= report.max_retries {
        format!(" ({}/{})", report.attempt, report.max_retries)
    } else {
        String::new()
    };
    (format!("{icon} verify{attempt}"), color)
}

fn render_system_annotation_lines(
    lines: &mut Vec<Line<'static>>,
    ann: &SystemAnnotation,
    width: usize,
) {
    // Compact boundary: show âœ» prefix with dimmed text
    if ann.style == SystemMessageStyle::Compact {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", figures::TEARDROP_ASTERISK),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                ann.text.clone(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
        lines.push(Line::from(""));
        return;
    }

    // Execute-and-verify round indicator (audit spec Phase 1 §15.1): a boxed
    // block with one line per check plus a summary headline.
    if ann.style == SystemMessageStyle::Verify {
        if let Some(report) = &ann.verify {
            render_verify_block(lines, report, width);
            lines.push(Line::from(""));
            return;
        }
        // No structured data (shouldn't happen) — fall through to the plain rule.
    }

    let (text_color, border_color) = match ann.style {
        SystemMessageStyle::Info => (Color::DarkGray, Color::DarkGray),
        SystemMessageStyle::Warning => (Color::Yellow, Color::Yellow),
        SystemMessageStyle::Compact => unreachable!(),
        // Defensive: a Verify annotation without structured data degrades to
        // the plain centered rule (push_verify_annotation always sets it).
        SystemMessageStyle::Verify => (Color::DarkGray, Color::DarkGray),
    };

    // Centred, padded rule: "â”€â”€â”€ text â”€â”€â”€"
    let text = ann.text.as_str();
    let inner_width = width.saturating_sub(4);
    let text_len = text.len();
    let dashes = inner_width.saturating_sub(text_len + 2);
    let left = dashes / 2;
    let right = dashes - left;

    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}", "\u{2500}".repeat(left)),
            Style::default().fg(border_color),
        ),
        Span::styled(
            format!("\u{2500} {} \u{2500}", text),
            Style::default().fg(text_color).add_modifier(Modifier::DIM),
        ),
        Span::styled("\u{2500}".repeat(right), Style::default().fg(border_color)),
    ]));
    lines.push(Line::from(""));
}

// â”€â”€ Tool use block â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Per-tool marker shown at the head of a tool block (the marker conveys the
/// tool, the line then shows the primary argument). Falls back to the generic
/// `~` for unmapped tools.
///
/// These are deliberately ASCII: many terminals render "pretty" Unicode glyphs
/// (arrows, ✱, ☰, …) two cells wide while ratatui's layout counts them as one,
/// which both breaks header alignment and desyncs the scroll redraw. ASCII is
/// guaranteed one cell everywhere, and the shell-flavoured choices read well in
/// context (`<` read, `>` write, `*` glob, `/` grep).
fn tool_icon(normalized: &str) -> &'static str {
    match normalized {
        "bash" | "powershell" => "$",
        "read" => "<",
        "write" | "apply_patch" | "edit" => ">",
        "glob" | "list" => "*",
        "grep" | "codesearch" => "/",
        "webfetch" => "@",
        "websearch" => "?",
        "todowrite" | "todo_write" | "todo" => ":",
        "task" | "agent" => "+",
        _ => "~",
    }
}

/// Replace a leading home-directory prefix with `~` for compact display
/// (mirrors pi's `shortenPath`). Works on Windows too via `dirs::home_dir`.
fn shorten_home_path(s: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        let home = home.trim_end_matches(['/', '\\']);
        if !home.is_empty() && s.starts_with(home) {
            let rest = &s[home.len()..];
            return format!("~{}", rest);
        }
    }
    s.to_string()
}

/// Running-state verb shown (with shimmer) while a tool is in flight.
fn tool_running_label(normalized: &str, fallback: &str) -> String {
    match normalized {
        "bash" | "powershell" => "Running command",
        "read" => "Reading file",
        "write" | "apply_patch" => "Writing file",
        "edit" => "Editing file",
        "glob" | "list" => "Listing files",
        "grep" | "codesearch" => "Searching code",
        "webfetch" => "Fetching page",
        "websearch" => "Searching web",
        "todowrite" | "todo_write" | "todo" => "Updating todos",
        _ => fallback,
    }
    .to_string()
}

fn render_tool_block_lines(
    lines: &mut Vec<Line<'static>>,
    block: &crate::app::ToolUseBlock,
    frame_count: u64,
) {
    let input_val: serde_json::Value =
        serde_json::from_str(&block.input_json).unwrap_or(serde_json::Value::Null);
    let normalized = block.name.to_ascii_lowercase();
    let running = block.status == ToolStatus::Running;
    let accent = if block.status == ToolStatus::Error {
        Color::Rgb(255, 140, 0)
    } else {
        CLAUDE_ORANGE
    };
    let icon = tool_icon(&normalized);

    // TodoWrite renders as a real checklist rather than a generic tool block.
    if matches!(normalized.as_str(), "todowrite" | "todo_write" | "todo")
        && render_todo_block(lines, &input_val, icon, accent, running, frame_count)
    {
        return;
    }

    // Primary argument shown on the header line (icon + arg), opencode-style.
    let mut summary = crate::messages::extract_tool_summary(&block.name, &input_val);
    let running_label = if normalized == "task" || normalized == "agent" {
        if let Some(description) = input_val
            .get("description")
            .and_then(|value| value.as_str())
        {
            summary = description.to_string();
        }
        crate::messages::subagent_title(&input_val)
    } else {
        tool_running_label(&normalized, &block.name)
    };

    // Shorten home paths in path-bearing summaries.
    if matches!(
        normalized.as_str(),
        "read" | "edit" | "write" | "apply_patch" | "glob" | "list"
    ) {
        summary = shorten_home_path(&summary);
    }

    let mut header_spans = vec![Span::styled(
        format!("   {} ", icon),
        Style::default().fg(accent),
    )];
    if running {
        header_spans.extend(shimmer_spans(&running_label, frame_count));
    } else {
        // Show the primary argument; fall back to the tool name when there is none.
        let primary = if summary.is_empty() {
            block.name.clone()
        } else {
            summary
        };
        header_spans.push(Span::styled(
            primary,
            Style::default()
                .fg(if block.status == ToolStatus::Error {
                    accent
                } else {
                    Color::White
                })
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(header_spans));

    // Output preview (done/error state) — home paths shortened, dimmed.
    if let Some(ref preview) = block.output_preview {
        let preview_style = match block.status {
            ToolStatus::Error => Style::default().fg(Color::Rgb(255, 140, 0)),
            _ => Style::default().fg(Color::DarkGray),
        };
        for line_text in preview.lines() {
            if line_text.starts_with('\u{2026}') {
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(
                        line_text.to_string(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(shorten_home_path(line_text), preview_style),
                ]));
            }
        }
    }
}

/// Render a TodoWrite call as a checklist. Returns `false` (so the caller can
/// fall back to the generic block) when the input carries no `todos` array.
fn render_todo_block(
    lines: &mut Vec<Line<'static>>,
    input_val: &serde_json::Value,
    icon: &str,
    accent: Color,
    running: bool,
    frame_count: u64,
) -> bool {
    let Some(todos) = input_val.get("todos").and_then(|v| v.as_array()) else {
        return false;
    };
    if todos.is_empty() {
        return false;
    }

    fn status_of(t: &serde_json::Value) -> &str {
        t.get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending")
    }
    let done = todos.iter().filter(|t| status_of(t) == "completed").count();
    let total = todos.len();

    // Header: ☰ Todos   <done>/<total>
    let mut header = vec![Span::styled(
        format!("   {} ", icon),
        Style::default().fg(accent),
    )];
    if running {
        header.extend(shimmer_spans("Updating todos", frame_count));
    } else {
        header.push(Span::styled(
            "Todos".to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        header.push(Span::styled(
            format!("  {}/{} done", done, total),
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(header));

    // Checklist items: ✓ done (green/dim) · • in-progress (orange) · ○ pending.
    const MAX_ITEMS: usize = 12;
    for t in todos.iter().take(MAX_ITEMS) {
        let content = t
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            continue;
        }
        // ASCII checkboxes (markdown-style) so alignment holds on every
        // terminal: [x] done, [>] in-progress, [ ] pending.
        let (glyph, glyph_color, text_style) = match status_of(t) {
            "completed" => (
                "[x]",
                Color::Rgb(120, 200, 120),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
            "in_progress" => (
                "[>]",
                accent,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            _ => (
                "[ ]",
                Color::Rgb(150, 150, 150),
                Style::default().fg(Color::Rgb(170, 170, 170)),
            ),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("     {} ", glyph), Style::default().fg(glyph_color)),
            Span::styled(content.to_string(), text_style),
        ]));
    }
    if total > MAX_ITEMS {
        lines.push(Line::from(vec![
            Span::raw("     "),
            Span::styled(
                format!("... {} more", total - MAX_ITEMS),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
    }
    true
}

// -----------------------------------------------------------------------
// Input pane
// -----------------------------------------------------------------------

/// Compute the on-screen rect of the span at `idx` inside a right-aligned
/// `Line` rendered into `area`. Used to hit-test the free-model task-sort
/// badge for the hover tooltip.
fn right_aligned_span_rect(spans: &[Span<'static>], idx: usize, area: Rect) -> Option<Rect> {
    let span = spans.get(idx)?;
    let line_w: u16 = spans.iter().map(|s| s.width() as u16).sum();
    // A right-aligned Paragraph clips the LEFT side of an over-long line, so
    // the computed badge position would not match what is visible. Bail out
    // instead of advertising a rect the user can't actually hover.
    if line_w > area.width {
        return None;
    }
    let offset: u16 = spans[..idx].iter().map(|s| s.width() as u16).sum();
    let start_x = area.x + area.width.saturating_sub(line_w);
    Some(Rect {
        x: start_x + offset,
        y: area.y,
        width: span.width() as u16,
        height: 1,
    })
}

/// Content lines for the task-sort hover tooltip: a usage hint, the valid
/// `/task <name>` values colour-coded per task (active bolded), and the
/// alt+t / number-key affordances.
fn task_tooltip_lines(active: crate::model_picker::FreeTask) -> Vec<Line<'static>> {
    let p = crate::theme_colors::current_palette();
    let tasks = crate::model_picker::FreeTask::ALL;

    let mut content: Vec<Line<'static>> = Vec::new();
    content.push(Line::from(vec![
        Span::styled(
            "/task <name>",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  sets the sort", Style::default().fg(p.hint)),
    ]));

    for row in [&tasks[..3], &tasks[3..6], &tasks[6..]] {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, t) in row.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            }
            let mut style = Style::default().fg(t.color());
            if *t == active {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(t.label(), style));
        }
        content.push(Line::from(spans));
    }

    content.push(Line::from(vec![
        Span::styled(
            "alt+t",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" cycles · ", Style::default().fg(p.hint)),
        Span::styled(
            "1-7",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" jumps in /models", Style::default().fg(p.hint)),
    ]));
    content
}

/// Popup tooltip for the free-model task-sort badge in the status line.
///
/// When the mouse hovers over the badge (recorded each frame as
/// `task_badge_rect`, cursor position tracked in `handle_mouse_event`),
/// show the valid `/task <name>` values plus the alt+t / number-key
/// affordances. Mirrors the context-menu popup styling.
fn render_task_badge_tooltip(frame: &mut Frame, app: &App) {
    let badge_rect = app.task_badge_rect.get();
    if badge_rect.width == 0 || badge_rect.height == 0 {
        return;
    }
    let Some((mx, my)) = app.last_mouse_pos.get() else {
        return;
    };
    if my != badge_rect.y
        || mx < badge_rect.x
        || mx >= badge_rect.x.saturating_add(badge_rect.width)
    {
        return;
    }
    if is_modal_open(app) {
        return;
    }

    let p = crate::theme_colors::current_palette();
    let content = task_tooltip_lines(app.model_picker.task_sort);
    let content_w = content.iter().map(|l| l.width()).max().unwrap_or(20) as u16;
    let tip_w = content_w.saturating_add(4);
    let tip_h = content.len() as u16 + 2;

    let screen = frame.area();
    let tip_x = badge_rect.x.min(screen.width.saturating_sub(tip_w));
    let tip_y = badge_rect.y.saturating_sub(tip_h + 1); // 1-row gap above the badge
    let tip_area = Rect {
        x: tip_x,
        y: tip_y,
        width: tip_w,
        height: tip_h,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(Color::White).bg(Color::Rgb(24, 24, 30)))
        .border_style(Style::default().fg(p.accent));
    block.render(tip_area, frame.buffer_mut());

    let inner = Rect {
        x: tip_area.x + 1,
        y: tip_area.y + 1,
        width: tip_area.width.saturating_sub(2),
        height: tip_area.height.saturating_sub(2),
    };
    frame.render_widget(
        Paragraph::new(content).style(Style::default().bg(Color::Rgb(24, 24, 30))),
        inner,
    );
}

fn render_input(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    focused: bool,
    current_inspector: Option<&clawde_api::providers::effort_shaping::ThinkingInspection>,
) {
    // Any stale task-badge rect from a previous frame must not keep the
    // hover tooltip alive once the badge is gone (streaming, tiny terminal,
    // non-free provider, …).
    app.task_badge_rect.set(Rect::default());
    // Split: 1-row model/mode status line + remaining rows for the prompt input.
    let (status_area, input_area) = if area.height > 2 {
        let splits = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(area);
        (Some(splits[0]), splits[1])
    } else {
        // Not enough room for the extra line — skip the status row.
        (None, area)
    };

    // Render model + agent mode status line above the prompt.
    if let Some(status_area) = status_area {
        let agent_mode = match app.agent_mode.as_deref() {
            Some(m) => m,
            None if app.plan_mode => "plan",
            _ => "build",
        };

        let pink = app.accent_color;
        // Dim secondary text (provider, badges, shortcut hints) and the bold
        // model name are themeable slots (hint / model_name), not hardcoded.
        let p = crate::theme_colors::current_palette();
        let dim = p.hint;
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Min(1)])
            .split(status_area);

        let left_line = if app.has_credentials {
            let (mut provider, mut model_short) =
                if let Some((provider, model)) = app.model_name.split_once('/') {
                    (provider.to_string(), model.to_string())
                } else {
                    ("local".to_string(), app.model_name.clone())
                };
            // For the free composite provider, cycle through upstreams or
            // show the abstract "auto" label based on free_upstream_index.
            // 0 = auto, 1..N = upstream from free_model_defaults.
            if provider == "free" && !app.free_model_defaults.is_empty() {
                let idx = app.free_upstream_index;
                let upstream_count = app.free_model_defaults.len();
                let safe_idx = if idx == 0 || idx > upstream_count {
                    None
                } else {
                    Some(idx - 1)
                };
                if let Some(ui) = safe_idx {
                    if let Some((_upstream_id, upstream, upstream_model)) =
                        app.free_model_defaults.get(ui)
                    {
                        provider = upstream.clone();
                        model_short = upstream_model.clone();
                    }
                }
                // idx == 0: keep the abstract "auto" / "free" labels
            }
            let mut spans = vec![
                Span::styled(
                    format!(" {} ", agent_mode.to_uppercase()),
                    Style::default()
                        .fg(Color::Black)
                        .bg(pink)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    model_short,
                    Style::default()
                        .fg(p.model_name)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            spans.push(Span::styled(
                format!(" · {}", provider),
                Style::default().fg(dim),
            ));
            if let Some(ref badge) = app.agent_type_badge {
                spans.push(Span::styled(
                    format!(" · {}", badge),
                    Style::default().fg(dim),
                ));
            }
            // Effort level indicator (thinking level) — shown for all providers.
            let effort_color = p.effort;
            spans.push(Span::styled(
                format!(
                    " · {} {}",
                    app.effort_level.symbol(),
                    app.effort_level.label()
                ),
                Style::default().fg(effort_color),
            ));
            // Persistent thinking-inspector warning marker: when the current
            // model+effort has a clamp / ignored-param / ladder quirk, surface
            // it subtly here (the popup/pickers carry the detail).
            if current_inspector.is_some_and(|insp| !insp.warnings.is_empty()) {
                spans.push(Span::styled(
                    " · ⚠",
                    Style::default()
                        .fg(CLAWDE_ACCENT)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        } else {
            Line::from(vec![
                Span::styled(
                    " /connect ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(pink)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" connect a provider", Style::default().fg(dim)),
            ])
        };

        // Context-sensitive hints in the right slot. These are suppressed
        // once the prompt has text so they don't compete with typing, and
        // during streaming when the input is readonly.
        // Index of the free-model task-sort badge within the final right-hint
        // span list, set when the badge is drawn this frame.
        let mut task_badge_span_idx: Option<usize> = None;
        let mut right_hint = if app.prompt_input.has_expandable_paste_ref() {
            // A [Pasted text #N ...] placeholder is in the buffer — tell the
            // user how to view the full pasted body before submitting.
            Line::from(vec![Span::styled(
                "click to view paste · alt+e expands",
                Style::default().fg(dim),
            )])
        } else if app.has_credentials && app.prompt_input.text.is_empty() && !app.is_streaming {
            // Build a set of state badges + a shortcut hint.
            // State badges show persistent config info (goal, effort, modes).
            let mut badge_spans: Vec<Span<'static>> = Vec::new();

            // Effort level symbol + label (when not default Medium)
            if app.effort_level != clawde_core::effort::EffortLevel::Medium {
                let effort_color = p.effort;
                badge_spans.push(Span::styled(
                    format!("{} {}", app.effort_level.symbol(), app.effort_level.label()),
                    Style::default().fg(effort_color),
                ));
            }
            // Active free-model task sort (when not the default "all") — e.g.
            // "coding" — so the user sees /models is pre-sorted by task. Only
            // for the free composite provider: the sort is inert elsewhere.
            let mut task_badge_idx: Option<usize> = None;
            if app.config.selected_provider_id() == "free"
                && app.model_picker.task_sort != crate::model_picker::FreeTask::All
            {
                let task = app.model_picker.task_sort;
                badge_spans.push(Span::styled(
                    task.label(),
                    Style::default()
                        .fg(task.color())
                        .add_modifier(Modifier::BOLD),
                ));
                task_badge_idx = Some(badge_spans.len() - 1);
            }
            // Routing strategy badge (only for free provider).
            if app.config.selected_provider_id() == "free" {
                if let Some(ref registry) = app.provider_registry {
                    let active_pid = app.config.selected_provider_id();
                    if let Some(provider) =
                        registry.get(&clawde_core::provider_id::ProviderId::new(active_pid))
                    {
                        if let Some(strategy_name) = provider.routing_strategy_name() {
                            badge_spans
                                .push(Span::styled(strategy_name, Style::default().fg(p.routing)));
                        }
                    }
                }
            }

            // Vim navigation hint (Shift+K/J/H/L) when in Normal mode.
            if app.prompt_input.vim_enabled && app.prompt_input.vim_mode == VimMode::Normal {
                badge_spans.push(Span::styled("K↑J↓H↑L↓", Style::default().fg(p.vim_hint)));
            }

            // Active mode preset name (when not default).
            if let Some(ref mode_name) = app.config.mode {
                if mode_name != "default" {
                    badge_spans.push(Span::styled(
                        mode_name.clone(),
                        Style::default().fg(p.effort).add_modifier(Modifier::BOLD),
                    ));
                }
            }

            // Shortcut hint: pick the most relevant actionable hint.
            let shortcut = if app.voice_recorder.is_some() {
                "Alt+V speak"
            } else if app.config.selected_provider_id() == "free"
                && app.free_model_defaults.len() > 1
            {
                "Alt+J/K models"
            } else {
                "? shortcuts · Ctrl+/ keys"
            };

            // Join badges with · separator, then append shortcut. Remember
            // where the task badge lands in the span list so the hover
            // tooltip can compute its on-screen rect via
            // `right_aligned_span_rect`.
            let mut spans: Vec<Span> = Vec::new();
            let mut first = true;
            for (i, s) in badge_spans.iter().enumerate() {
                if !first {
                    spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
                }
                if Some(i) == task_badge_idx {
                    task_badge_span_idx = Some(spans.len());
                }
                spans.push(s.clone());
                first = false;
            }
            if !first {
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            }
            spans.push(Span::styled(shortcut, Style::default().fg(dim)));

            Line::from(spans)
        } else {
            Line::from(Vec::<Span>::new())
        };

        // Truncate the right hint to fit within the right column so a
        // right-aligned Paragraph never wraps/clips mid-content.
        {
            let right_w = chunks[1].width.saturating_sub(1) as usize;
            let mut used = 0usize;
            let mut last_fit = 0usize;
            for (i, s) in right_hint.spans.iter().enumerate() {
                let sw = UnicodeWidthStr::width(s.content.as_ref());
                if used + sw > right_w {
                    break;
                }
                used += sw;
                last_fit = i + 1;
            }
            right_hint.spans.truncate(last_fit);
        }
        // NOTE: left_line is NOT truncated here. Ratatui wraps long lines
        // naturally; truncating would silently drop important context like
        // the CWD. On very narrow terminals the wrapped tail is clipped by
        // the 1-row status area, which is acceptable.

        let left_padded = Rect {
            x: chunks[0].x + 1,
            y: chunks[0].y,
            width: chunks[0].width.saturating_sub(1),
            height: chunks[0].height,
        };
        let right_padded = Rect {
            x: chunks[1].x,
            y: chunks[1].y,
            width: chunks[1].width.saturating_sub(1),
            height: chunks[1].height,
        };

        // Record the task badge's on-screen rect (right-aligned line inside
        // right_padded) so the hover tooltip can hit-test it next frame.
        // Computed before `right_hint` is moved into the render call below.
        if let Some(idx) = task_badge_span_idx {
            if let Some(rect) = right_aligned_span_rect(&right_hint.spans, idx, right_padded) {
                app.task_badge_rect.set(rect);
            } else {
                app.task_badge_rect.set(Rect::default());
            }
        } else {
            app.task_badge_rect.set(Rect::default());
        }

        frame.render_widget(Paragraph::new(vec![left_line]), left_padded);
        frame.render_widget(
            Paragraph::new(vec![right_hint]).alignment(Alignment::Right),
            right_padded,
        );
    }

    render_prompt_input(
        &app.prompt_input,
        input_area,
        frame.buffer_mut(),
        focused,
        if app.is_streaming {
            InputMode::Readonly
        } else if app.plan_mode {
            InputMode::Plan
        } else {
            InputMode::Default
        },
        app.accent_color,
        app.settings_screen.cursor_blink_enabled,
    );
}

fn should_render_status_row(app: &App) -> bool {
    let interesting_stream_status = app
        .status_message
        .as_deref()
        .map(|status| {
            let trimmed = status.trim();
            !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case(STATUS_THINKING)
                && !trimmed.eq_ignore_ascii_case(STATUS_THINKING_ELLIPSIS)
        })
        .unwrap_or(false);

    // Check if any provider has exhausted API keys — keep the status row
    // visible so the user sees the key exhaustion indicator even while idle.
    let has_exhausted_keys = app.provider_registry.as_ref().is_some_and(|reg| {
        reg.key_ring_summaries()
            .iter()
            .any(|(_, active, total, _)| *active < *total)
    });

    // Also keep the row visible while any upstream is in empty-completion
    // cooldown (spec §6.3) so the user sees which upstreams are cooled down
    // for repeated empty completions.
    let has_empty_cooldowns = app.provider_registry.as_ref().is_some_and(|reg| {
        reg.empty_cooldown_summaries()
            .iter()
            .any(|(_, entries)| entries.iter().any(|(_, _, retry)| retry.is_some()))
    });

    // Note: a completed turn's "Worked for Xs" summary (`last_turn_elapsed`) is
    // intentionally NOT a reason to keep the status row on — it stays set until
    // the next submit, so gating on it pinned the idle spinner glyph on screen
    // permanently after the first turn. The row now shows only while actually
    // active (voice, streaming, or an idle status message). Free-mode routing
    // and key health are shown in the prompt config row (Row 2) instead.
    app.voice_recording
        || app.is_verifying
        || app.is_compacting
        || (!app.is_streaming && app.status_message.is_some())
        || (app.is_streaming && interesting_stream_status)
        || has_exhausted_keys
        || has_empty_cooldowns
        || app.fast_mode
        || app.active_goal_badge.is_some()
}

fn render_status_row(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    let mut spans = if app.voice_recording {
        vec![Span::styled(
            format!(
                "{} Recording... press Alt+V to transcribe",
                figures::black_circle()
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]
    } else if app.is_streaming {
        // Pick a label: use the status message if it has real content,
        // otherwise show a default "Thinking" shimmer so the user always
        // sees that the model is working.
        let raw_label = app
            .status_message
            .as_deref()
            .filter(|s| {
                let t = s.trim();
                !t.is_empty()
                    && !t.eq_ignore_ascii_case(STATUS_THINKING)
                    && !t.eq_ignore_ascii_case(STATUS_THINKING_ELLIPSIS)
            })
            .or(app.spinner_verb.as_deref())
            .unwrap_or("Thinking");

        let needs_attention = app.permission_request.is_some()
            || app.ask_user_dialog.visible
            || app.mcp_approval.visible
            || app.elicitation.visible;

        // Pick the spinner set: braille during normal streaming,
        // snowflake during attention dialogs (more eye-catching).
        let spinner_set = if needs_attention {
            SPINNER_SNOWFLAKE
        } else {
            SPINNER
        };
        // Braille slows down to every 3 frames so the rotation is visible;
        // snowflake stays at frame-by-frame speed to grab attention.
        let rate = if needs_attention { 1 } else { 3 };
        let spinner = spinner_set[((app.frame_count as usize) / rate) % spinner_set.len()];

        let mut s = vec![Span::styled(
            spinner.to_string(),
            Style::default()
                .fg(spinner_color(app))
                .add_modifier(Modifier::BOLD),
        )];
        let label = format!("{}…", raw_label.trim_end_matches('…'));

        s.push(Span::raw(" "));
        s.extend(shimmer_spans(&label, app.frame_count));
        s
    } else if app.is_verifying {
        // A verify round is running its checks — spinning indicator so the
        // potentially 30-180s wait never looks like a hang.
        let spinner = SPINNER[(app.frame_count as usize) % SPINNER.len()];
        vec![Span::styled(
            format!("{spinner} verifying…"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]
    } else if app.is_compacting {
        // A background /compact request is in flight — spinning indicator so
        // the model call (up to COMPACT_API_TIMEOUT) never looks like a hang.
        // Esc cancels it.
        let spinner = SPINNER[(app.frame_count as usize) % SPINNER.len()];
        vec![Span::styled(
            format!("{spinner} compacting…"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]
    } else if let (Some(verb), Some(elapsed)) =
        (app.last_turn_verb, app.last_turn_elapsed.as_deref())
    {
        // "✽ Worked for 2m 5s" — mirrors TS TeammateSpinnerLine idle state
        vec![Span::styled(
            format!("{} {} for {}", figures::TEARDROP_ASTERISK, verb, elapsed),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )]
    } else if let Some(status) = app.status_message.as_deref() {
        vec![Span::styled(
            status.to_string(),
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        Vec::new()
    };

    // Append key-ring status when any keys are exhausted. Only shown
    // when the active provider is "free" — standalone providers surface
    // their exhaustion through their own error paths, and free-catalog
    // upstream status is noise on non-free providers (e.g. ollama).
    if let Some(ref registry) = app.provider_registry {
        // The effective provider — free mode is the default, so a fresh config
        // (provider unset) must still be treated as free here, otherwise the
        // exhausted-key indicator would never render.
        let active_provider = app.config.selected_provider_id();
        let summaries = registry.key_ring_summaries();
        let has_exhausted = summaries
            .iter()
            .any(|(_, active, total, _)| *active < *total);
        if has_exhausted && !spans.is_empty() && active_provider == "free" {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            for (provider, active, total, retry_secs) in &summaries {
                if *active < *total {
                    let color = if *active == 0 {
                        Color::Red
                    } else {
                        Color::Yellow
                    };
                    let retry_label =
                        retry_secs.map(|s| format!("retry in {}", format_duration_ms(s * 1000)));
                    let mut label = match retry_label {
                        Some(r) => format!("{}:{}/{} ({})", provider, active, total, r),
                        None => format!("{}:{}/{}", provider, active, total),
                    };
                    // Fallback-models count for the free provider: how many
                    // configured upstreams carry a secondary model (e.g.
                    // nvidia 70B -> 8B). Surfaced so users know the chain
                    // can self-recover on a slow primary.
                    if provider == "free" {
                        let fb = free_fallback_upstream_count(&app.free_model_defaults);
                        if fb > 0 {
                            label.push_str(&format!(" \u{00b7} {} fb", fb));
                        }
                    }
                    spans.push(Span::styled(label, Style::default().fg(color)));
                    spans.push(Span::raw(" "));
                }
            }
        }

        // Empty-completion cooldowns (spec §6.3): show which upstreams are
        // cooled down for repeated empty completions, with the remaining
        // retry time. Only upstreams currently in cooldown are shown here;
        // sub-threshold counters are visible via /keys health. The badge
        // renders even when the row is otherwise empty (idle cooldown).
        let cooldowns = registry.empty_cooldown_summaries();
        let has_cooled = cooldowns
            .iter()
            .any(|(_, entries)| entries.iter().any(|(_, _, retry)| retry.is_some()));
        if has_cooled && active_provider == "free" {
            if !spans.is_empty() {
                spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            }
            for (provider, entries) in &cooldowns {
                for (upstream, _, retry_secs) in entries {
                    if let Some(secs) = retry_secs {
                        let label = format!(
                            "{}:{} empty-cooldown (retry in {})",
                            provider,
                            upstream,
                            format_duration_ms(secs * 1000)
                        );
                        spans.push(Span::styled(label, Style::default().fg(Color::Yellow)));
                        spans.push(Span::raw(" "));
                    }
                }
            }
        }

        // Routing strategy and aggregate key health moved to the prompt
        // config row (Row 2, render_input) so the transient status row only
        // shows live activity (streaming, exhaustion, errors).
    }

    // Append agent/task state badges (goal, fast mode, plan mode) to Row 1.
    // These are persistent config indicators that describe how the agent
    // behaves, not how the model is configured (those go in Row 2).
    // Skip when the row is otherwise empty — the mode badge in Row 2
    // already communicates the current mode, so a standalone "Plan"
    // line with no context is noise.
    // Note: plan_mode is NOT shown here — the [PLAN] badge in Row 2
    // already communicates the mode, so a standalone "Plan" in the
    // transient status row is redundant noise.
    if (app.active_goal_badge.is_some() || app.fast_mode) && !spans.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        if let Some(ref badge) = app.active_goal_badge {
            spans.push(Span::styled(
                format!("\u{1f3af} {}", badge),
                Style::default().fg(Color::Rgb(120, 200, 120)),
            ));
        }
        if app.fast_mode {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            }
            spans.push(Span::styled(
                "Fast",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    if spans.is_empty() {
        return;
    }

    frame.render_widget(
        Paragraph::new(spans_to_text(spans)).wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

/// Convert a run of styled spans into a `Text`, splitting any embedded `\n`
/// into separate lines. Ratatui's `Line` renders an embedded newline as
/// inline content, so multi-line strings (e.g. `/keys` error hints) would
/// glue into words like "hasno" when the status row wraps — splitting first
/// keeps multi-line messages readable. Single-line messages are unchanged.
fn spans_to_text(spans: Vec<Span<'static>>) -> ratatui::text::Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    for span in spans {
        for (i, part) in span.content.split('\n').enumerate() {
            if i > 0 {
                lines.push(Line::from(std::mem::take(&mut current)));
            }
            if !part.is_empty() {
                current.push(Span::styled(part.to_string(), span.style));
            }
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    ratatui::text::Text::from(lines)
}

/// Build spans for a text string with a right-to-left glimmer sweep, matching
/// the TS `GlimmerMessage` behaviour (glimmerSpeed=200ms, 3-char shimmer window).
///
/// At ~50ms per frame a 4-frame step ≈ 200ms, giving the same cadence as TS.
fn shimmer_spans(text: &str, frame_count: u64) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len == 0 {
        return Vec::new();
    }

    // Cycle length = text_len + 5 (~2-3 off-screen on each side)
    // Small off-screen padding keeps the pause between sweeps short.
    let cycle_len = len + 5;
    // One step every 3 frames (~150ms at 50ms/frame) — slower sweep
    let cycle_pos = (frame_count as usize / 3) % cycle_len;
    // Glimmer sweeps right→left: starts at len+2 (off right), ends at -3 (off left)
    // Uses raw subtraction (not saturating) so the center wraps past 0 into
    // negative territory and goes off-screen left before the cycle resets,
    // avoiding a "stuck shimmer" on the leftmost characters.
    let glimmer_center = (len + 2) as isize - cycle_pos as isize;

    let base = Style::default().fg(Color::DarkGray);
    let bright = Style::default().fg(Color::White);

    // Accumulate runs of same style to minimise span count
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_bright = false;

    for (i, &ch) in chars.iter().enumerate() {
        // Wider bright band: 5 characters glow (center ± 2) instead of 3
        let is_bright = (i as isize - glimmer_center).abs() <= 2
            && glimmer_center >= 0
            && glimmer_center < len as isize;

        if is_bright != run_bright && !run.is_empty() {
            spans.push(Span::styled(
                run.clone(),
                if run_bright { bright } else { base },
            ));
            run.clear();
        }
        run_bright = is_bright;
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_bright { bright } else { base }));
    }
    spans
}

/// Count how many configured free upstreams carry at least one secondary
/// (fallback) model in the catalog. Used for the footer's `N fb` badge next
/// to the free key-ring health label.
fn free_fallback_upstream_count(defaults: &[(String, String, String)]) -> usize {
    defaults
        .iter()
        .filter(|(upstream_id, _, _)| {
            clawde_api::FREE_CATALOG
                .iter()
                .any(|u| u.id == upstream_id && !u.fallback_models.is_empty())
        })
        .count()
}
// Keybinding hints footer
// -----------------------------------------------------------------------

/// Single footer line matching the TS contract more closely:
/// - `? for shortcuts` is suppressed once the prompt becomes non-empty
/// - the right side shows comprehensive status info and notifications
fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }

    // No verify badge on screen yet this frame — stale click targets must not
    // linger from a previous render (cleared here unconditionally, including
    // the voice-recording footer which never draws the badge).
    app.last_verify_badge_area.set(None);

    // Use only the first line of the footer area, leaving bottom padding
    let footer_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };

    // Left side: ordered pills — voice > PR badge > background task > vim > hint
    let left_spans: Vec<Span> = if app.voice_recording {
        vec![Span::styled(
            format!(" {} REC — speak now", figures::black_circle()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]
    } else {
        let mut spans: Vec<Span> = Vec::new();

        // Agent type badge (shown when running as subagent / coordinator)
        if let Some(ref badge) = app.agent_type_badge {
            spans.push(Span::styled(
                format!("\u{2699} {}", badge),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // PR badge — shows "PR #<n>" in cyan, with optional state in brackets.
        // State color: approved=green, changes_requested=red,
        //              review_required=yellow, else=gray.
        if let Some(pr_num) = app.pr_number {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let pr_label = match &app.pr_state {
                Some(state) => format!("PR #{} [{}]", pr_num, state),
                None => format!("PR #{}", pr_num),
            };
            // Colors mirror TS PrBadge getPrStatusColor + TS ink color names:
            //   approved → Green, changes_requested → Red (error),
            //   pending / review_required → Yellow (warning), merged → Magenta.
            let pr_color = match app.pr_state.as_deref() {
                Some("approved") => Color::Green,
                Some("changes_requested") => Color::Red,
                Some("merged") => Color::Magenta,
                Some("pending") | Some("review_required") => Color::Yellow,
                Some(_) => Color::Gray,
                None => Color::Cyan,
            };
            spans.push(Span::styled(
                pr_label,
                Style::default().fg(pr_color).add_modifier(Modifier::BOLD),
            ));
        }

        // Background task status pill — shows "⟳ N tasks" when count > 0.
        // Falls back to background_task_status pre-formatted string if set.
        if app.background_task_count > 0 {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let label = if app.background_task_count == 1 {
                "\u{27f3} 1 task".to_string()
            } else {
                format!("\u{27f3} {} tasks", app.background_task_count)
            };
            spans.push(Span::styled(label, Style::default().fg(Color::Yellow)));
        } else if let Some(ref task_status) = app.background_task_status {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("\u{27f3} {}", task_status),
                Style::default().fg(Color::Yellow),
            ));
        }

        // Last search backend indicator — shows which search backend was used.
        let search_backend = clawde_tools::web_search::get_last_search_backend();
        if !search_backend.is_empty() {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("search:{search_backend}"),
                Style::default().fg(Color::DarkGray),
            ));
        }

        // Vim mode indicator — shown for all modes using neovim "-- MODE --" convention.
        // INSERT is dim (common, low-noise); other modes use bright colour.
        if app.prompt_input.vim_enabled {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let (label, style) = match app.prompt_input.vim_mode {
                VimMode::Insert => ("-- INSERT --", Style::default().fg(Color::DarkGray)),
                VimMode::Normal => (
                    "-- NORMAL --",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                VimMode::Command => (
                    "-- COMMAND --",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                VimMode::Search => (
                    "-- SEARCH --",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            };
            spans.push(Span::styled(label, style));
        }

        // Verify badge — persistent at-a-glance outcome of the most recent
        // execute-and-verify round (auto-loop or /verify). Survives after
        // the boxed report scrolls out of view. Clickable: a click on the
        // badge jumps the transcript to the latest verify box.
        if let Some(report) = &app.verify {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let (label, color) = verify_footer_badge(report);
            // Record the badge's column span (relative to the left edge of
            // the footer's padded drawing area) so mouse clicks on it can be
            // recognised; translated to absolute screen columns at draw time.
            let offset_start: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let offset_end = offset_start + UnicodeWidthStr::width(label.as_str());
            // Row placeholder (0) is replaced with the real footer row at draw
            // time; only the column span is known at badge-construction time.
            app.last_verify_badge_area
                .set(Some((0, offset_start as u16, offset_end as u16)));
            spans.push(Span::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }

        // Ollama connectivity mode indicator.
        //   - Auto  + no VRAM loaded  →  dim "ollama:auto" (no icon)
        //   - Auto  + VRAM loaded     →  bright "\u{1f310} ollama:online" (globe)
        //   - Isolated               →  "\u{1f512} ollama:offline" (lock)
        {
            let has_loaded = !app.ollama_loaded_models.is_empty();
            let (label, color, modifier) = match app.ollama_mode {
                clawde_core::OllamaMode::Auto if has_loaded => (
                    " \u{1f310} ollama:online ",
                    Color::Rgb(80, 200, 80),
                    Modifier::BOLD,
                ),
                clawde_core::OllamaMode::Auto => {
                    (" ollama:auto ", Color::Rgb(80, 140, 80), Modifier::DIM)
                }
                clawde_core::OllamaMode::Isolated => (
                    " \u{1f512} ollama:offline ",
                    Color::Rgb(60, 140, 200),
                    Modifier::DIM,
                ),
            };
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                label,
                Style::default().fg(color).add_modifier(modifier),
            ));
        }

        // Health sweep indicator — red warning marker when the last background
        // probe found dead keys. Hidden when everything is healthy, and only
        // relevant when the active provider is the free composite.
        if app.config.provider.as_deref() == Some("free") {
            if let Some(sweep) = app.last_health_sweep.as_ref() {
                if sweep.unhealthy > 0 {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        format!(" \u{26a0} {} dead ", sweep.unhealthy),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                }
            }
        }

        // GitHub API quota — warning marker when the update-check quota is low
        // (≤ 5 requests left), mirroring the key-ring / health-sweep badges.
        if let Some(limit) = clawde_core::github::last_rate_limit() {
            if limit.remaining <= 5 {
                let gh_color = if limit.remaining == 0 {
                    Color::Red
                } else {
                    Color::Yellow
                };
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!(
                        " \u{26a0} gh {}/{} ({}) ",
                        limit.remaining,
                        limit.limit,
                        clawde_core::github::format_reset(
                            limit.reset_unix,
                            clawde_core::github::unix_now()
                        )
                    ),
                    Style::default().fg(gh_color).add_modifier(Modifier::BOLD),
                ));
            }
        }

        // Free-mode aggregate indicator — compact and unobtrusive, shown only
        // when the whole free chain is down: every key in the free key rings
        // is exhausted, or every configured upstream is in a 5xx/empty
        // cooldown. Mirrors the ollama indicator's placement.
        {
            let free_aggregate: Option<(usize, usize, Option<u64>)> =
                app.provider_registry.as_ref().and_then(|reg| {
                    let provider = reg.get(&clawde_core::ProviderId::new("free"))?;
                    let chain_len = app.free_model_defaults.len();
                    if chain_len == 0 {
                        return None;
                    }
                    // Key-ring exhaustion (multi-key upstreams): all keys down.
                    if let Some((active, total, retry)) = provider.key_ring_status() {
                        if total > 0 && active == 0 {
                            return Some((active, total, retry));
                        }
                    }
                    // All-upstreams-cooled case: every chain entry is in a
                    // 5xx / empty cooldown.
                    let (cooled_count, cooled_retry) = reg
                        .upstream_cooldown_summaries()
                        .into_iter()
                        .find(|(pid, _)| pid == "free")
                        .map(|(_, entries)| {
                            let retry = entries.iter().filter_map(|(_, _, r)| *r).min();
                            (entries.len(), retry)
                        })
                        .unwrap_or((0, None));
                    if cooled_count > 0 && cooled_count >= chain_len {
                        Some((cooled_count, chain_len, cooled_retry))
                    } else {
                        None
                    }
                });
            if let Some((active, total, retry)) = free_aggregate {
                let retry_label = retry
                    .map(|s| format!("retry in {}", format_duration_ms(s * 1000)))
                    .unwrap_or_default();
                let label = if retry_label.is_empty() {
                    format!(" free:{}/{} ", active, total)
                } else {
                    format!(" free:{}/{} ({}) ", active, total, retry_label)
                };
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    label,
                    Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
                ));
            }
        }

        // Bash prefix indicator — shown when prompt starts with '!'
        if app.prompt_input.text.starts_with('!') {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                "[BASH]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // Permission mode badge (left side, mirrors TS bottom-left indicator).
        // Default mode is silent; non-default modes show a badge.
        {
            use clawde_core::config::PermissionMode;
            match &app.config.permission_mode {
                PermissionMode::BypassPermissions => {
                    if !spans.is_empty() {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(
                        "\u{23f5}\u{23f5} bypass",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                }
                PermissionMode::AcceptEdits => {
                    if !spans.is_empty() {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled(
                        "accept-edits",
                        Style::default().fg(Color::Yellow),
                    ));
                }
                PermissionMode::Plan => {
                    if !spans.is_empty() {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(Span::styled("plan", Style::default().fg(Color::Blue)));
                }
                PermissionMode::Default => {}
            }
        }

        // Autopilot badge (Phase 4C) — shown only while autopilot is active,
        // with the session's pending count read from the shared autonomy state.
        // Blast-radius counters (Phase 4F) are shown when non-zero.
        if let Some(autonomy) = &app.autonomy {
            let (active, pending, blast) = {
                let state = autonomy.lock();
                (
                    state.is_active(&app.session_id),
                    state.pending_count(),
                    state.blast_radius.clone(),
                )
            };
            if active {
                if !spans.is_empty() {
                    spans.push(Span::raw("  "));
                }
                let (label, style) = if pending == 0 && !blast.has_activity() {
                    ("autopilot".to_string(), Style::default().fg(Color::Magenta))
                } else {
                    let mut parts = Vec::new();
                    if pending > 0 {
                        parts.push(format!("{} pending", pending));
                    }
                    if blast.files_changed > 0 {
                        parts.push(format!("{}f", blast.files_changed));
                    }
                    if blast.risky_actions_allowed > 0 {
                        parts.push(format!("{}r", blast.risky_actions_allowed));
                    }
                    if blast.irreversible_denied > 0 {
                        parts.push(format!("{}!", blast.irreversible_denied));
                    }
                    (
                        format!("autopilot · {}", parts.join(" ")),
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    )
                };
                spans.push(Span::styled(label, style));
            }
        }

        // During streaming show "esc to interrupt". The "? shortcuts" hint is
        // rendered in the top-right status bar (see render_prompt area), so do
        // not duplicate it here (issue #149 follow-up).
        if spans.is_empty() && app.is_streaming {
            spans.push(Span::styled(
                "esc interrupt",
                Style::default().fg(Color::DarkGray),
            ));
        }

        spans
    };

    // Right side: status metrics and lightweight badges.
    let mut right_spans: Vec<Span> = {
        let mut parts: Vec<Span> = Vec::new();

        // 1. Context window usage — show "N% until auto-compact" mirroring TS TokenWarning.
        //    When an update is available and context is below 85%, show the update notification
        //    instead to keep the status bar uncluttered.
        if app.context_window_size > 0 {
            let used_pct =
                (app.context_used_tokens as f64 / app.context_window_size as f64 * 100.0) as u64;

            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }

            if used_pct >= 85 {
                // High usage — always show context window info regardless of update status.
                if used_pct >= 95 {
                    parts.push(Span::styled(
                        format!("ctx: {used_pct}% — /compact now"),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                } else {
                    parts.push(Span::styled(
                        format!("ctx: {used_pct}% — compact soon"),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            } else if let Some(ref version) = app.update_available {
                // Update available and context is fine — show update nudge in bottom-right.
                parts.push(Span::styled(
                    format!("⬆ v{} available  Run: /update", version),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            } else if used_pct >= 70 {
                // 70–84%: mild warning with auto-compact status.
                let suffix = if app.auto_compact_enabled {
                    ""
                } else {
                    " (off)"
                };
                parts.push(Span::styled(
                    format!("ctx: {used_pct}%{suffix}"),
                    Style::default().fg(Color::Yellow),
                ));
            } else {
                // Normal: show context usage with color reflecting auto-compact state.
                // Green when auto-compact is on, dim gray with (off) when disabled.
                if app.auto_compact_enabled {
                    parts.push(Span::styled(
                        format!("ctx: {used_pct}%"),
                        Style::default().fg(Color::Rgb(80, 200, 80)),
                    ));
                } else {
                    parts.push(Span::styled(
                        format!("ctx: {used_pct}% (off)"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
        }

        // 3. Cost — mirrors TS formatCost: 4 decimal places for costs < $0.50, else 2.
        // Only show a cost readout when it's nonzero — free models price at $0.00,
        // so displaying "$0.0000" would be pure noise.
        if app.cost_usd > 0.0 {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            let cost_str = if app.cost_usd < 0.5 {
                format!("${:.4}", app.cost_usd)
            } else {
                format!("${:.2}", app.cost_usd)
            };
            parts.push(Span::styled(cost_str, Style::default().fg(Color::DarkGray)));
        }

        // 3b. Token budget (feature-gated)
        #[cfg(feature = "token_budget")]
        if let Some(max_tokens) = app.token_budget {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            let used = app.token_count as u64;
            let max = max_tokens as u64;
            let pct = if max > 0 {
                (used as f64 / max as f64 * 100.0) as u32
            } else {
                0
            };
            let color = if pct >= 90 {
                Color::Red
            } else if pct >= 75 {
                Color::Yellow
            } else {
                Color::DarkGray
            };
            parts.push(Span::styled(
                format!("Tokens: {}/{} ({}%)", used, max, pct),
                Style::default().fg(color),
            ));
        }

        // 4. Rate limits
        if let Some(pct) = app.rate_limit_5h_pct {
            if pct > 0.0 {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                let color = if pct >= 90.0 {
                    Color::Red
                } else {
                    Color::Yellow
                };
                parts.push(Span::styled(
                    format!("5h:{:.0}%", pct),
                    Style::default().fg(color),
                ));
            }
        }
        if let Some(pct) = app.rate_limit_7day_pct {
            if pct > 0.0 {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                let color = if pct >= 90.0 {
                    Color::Red
                } else {
                    Color::Yellow
                };
                parts.push(Span::styled(
                    format!("7d:{:.0}%", pct),
                    Style::default().fg(color),
                ));
            }
        }

        // 5. Vim mode — displayed on the left side as "-- MODE --"; nothing extra on right.

        // 5b. Goal badge — shown when a goal is active for this session.
        if let Some(ref badge) = app.active_goal_badge {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[goal: {}]", badge),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // 6. Agent type badge
        if let Some(ref badge) = app.agent_type_badge {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", badge),
                Style::default().fg(crate::theme_colors::current_palette().accent),
            ));
        }

        // 7. Worktree branch
        if let Some(ref branch) = app.worktree_branch {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", branch),
                Style::default().fg(Color::Green),
            ));
        }

        // Git branch (if settings enabled)
        if app.settings_screen.show_git_branch {
            if let Some(ref branch) = app.git_branch {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                parts.push(Span::styled(
                    format!("⎇ {}", branch),
                    Style::default().fg(Color::Cyan),
                ));
            }
        }

        // Output style indicator (only when non-default)
        if app.output_style != "auto" {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                format!("[{}]", app.output_style),
                Style::default().fg(Color::DarkGray),
            ));
        }

        // External status line override
        if let Some(ref override_text) = app.status_line_override {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            // Strip any ANSI escapes for terminal rendering (plain text)
            let clean: String = override_text
                .chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .collect();
            parts.push(Span::styled(clean, Style::default().fg(Color::DarkGray)));
        }

        // 8. Bridge badge
        if let Some(badge) = app.bridge_state.status_badge(app.frame_count) {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(badge);
        } else if app.pending_mcp_reconnect {
            if !parts.is_empty() {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                "MCP reconnecting",
                Style::default().fg(Color::Yellow),
            ));
        }

        parts
    };

    // Gap fill — when left + right exceed available width, drop trailing
    // right-side spans (least important last: bridge, cwd, output-style,
    // agent badge, goal badge, rate limits, cost) until the line fits.
    let usable = footer_area.width.saturating_sub(2) as usize; // minus 1-char padding each side
    let left_len: usize = left_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_available = usable.saturating_sub(left_len);
    // Trim trailing right-side spans that no longer fit.
    while spans_width(&right_spans) > right_available && !right_spans.is_empty() {
        right_spans.pop();
    }
    let right_len: usize = right_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let gap = usable.saturating_sub(left_len + right_len);

    let mut spans = left_spans;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right_spans);

    // Add padding: 1 char on each side
    let padded_area = Rect {
        x: footer_area.x + 1,
        y: footer_area.y,
        width: footer_area.width.saturating_sub(2),
        height: footer_area.height,
    };
    frame.render_widget(Paragraph::new(vec![Line::from(spans)]), padded_area);

    // Translate the verify badge's column span into absolute screen
    // coordinates for the mouse handler, carrying the footer's exact row.
    if let Some((_, start, end)) = app.last_verify_badge_area.get() {
        app.last_verify_badge_area.set(Some((
            footer_area.y,
            padded_area.x.saturating_add(start),
            padded_area.x.saturating_add(end),
        )));
    }
}

/// Compute the `[start, end)` slice of a suggestion list to render in the
/// popup, given the area height, the selected row, and whether the last row
/// is a faded hint.
///
/// Windows `max_visible` rows centered on `selected` (like the transcript),
/// but never lets a trailing faded hint fall below the fold — free-form
/// placeholders such as `<objective>` are "what to type next" guidance and
/// must stay visible even when the selectable rows fill the popup (e.g.
/// `/goal` shows all five subcommands plus the dimmed `<objective>` hint).
fn suggestion_window(
    len: usize,
    max_visible: usize,
    selected: usize,
    last_is_faded: bool,
) -> (usize, usize) {
    if len == 0 || max_visible == 0 {
        return (0, 0);
    }
    let selected = selected.min(len - 1);
    let mut start = selected
        .saturating_sub(max_visible / 2)
        .min(len.saturating_sub(max_visible));
    if last_is_faded && start + max_visible < len {
        start = len.saturating_sub(max_visible);
    }
    let end = (start + max_visible).min(len);
    (start, end)
}

fn render_prompt_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    let suggestions = &app.prompt_input.suggestions;
    if suggestions.is_empty() || area.height == 0 {
        return;
    }

    let selected = app.prompt_input.suggestion_index.unwrap_or(0);
    let (start, end) = suggestion_window(
        suggestions.len(),
        area.height as usize,
        selected,
        suggestions.last().is_some_and(|s| s.faded),
    );
    let label_width = area.width.saturating_div(3).max(12) as usize;

    for (row, suggestion) in suggestions[start..end].iter().enumerate() {
        // Faded rows are never selectable, so they never carry the highlight
        // marker or accent color — only dimmed placeholder styling.
        let is_selected = start + row == selected && !suggestion.faded;
        let accent_style = if is_selected {
            Style::default()
                .fg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let label_style = if is_selected {
            Style::default()
                .fg(CLAUDE_ORANGE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let detail_style = if is_selected {
            Style::default().fg(CLAUDE_ORANGE)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![Span::styled(
            if is_selected { "\u{203a} " } else { "  " },
            accent_style,
        )];
        match suggestion.source {
            TypeaheadSource::SlashCommand => {
                let display_name = truncate_text(&suggestion.text, label_width);
                spans.push(Span::styled(
                    format!("{display_name:<width$}", width = label_width),
                    label_style,
                ));
                spans.push(Span::styled(
                    " [cmd] ",
                    Style::default().fg(Color::DarkGray),
                ));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(
                        truncate_text(
                            &suggestion.description,
                            area.width.saturating_sub(label_width as u16 + 10) as usize,
                        ),
                        detail_style,
                    ));
                }
            }
            TypeaheadSource::ArgCompletion => {
                let value = suggestion.arg_value.as_deref().unwrap_or(&suggestion.text);
                let display_name = truncate_text(value, label_width);
                // Faded rows use the theme's muted hint color (no DIM — the
                // palette hint is already muted; stacking DIM on DarkGray
                // made placeholders like `<api-key>` unreadable).
                let hint_color = crate::theme_colors::current_palette().hint;
                let label_style = if suggestion.faded {
                    Style::default().fg(hint_color)
                } else {
                    label_style
                };
                spans.push(Span::styled(
                    format!("{display_name:<width$}", width = label_width),
                    label_style,
                ));
                if !suggestion.description.is_empty() {
                    let desc_style = if suggestion.faded {
                        Style::default().fg(hint_color)
                    } else {
                        detail_style
                    };
                    spans.push(Span::styled(
                        truncate_text(
                            &suggestion.description,
                            area.width.saturating_sub(label_width as u16 + 10) as usize,
                        ),
                        desc_style,
                    ));
                }
            }
            TypeaheadSource::FileRef => {
                spans.push(Span::styled("+ ", accent_style));
                spans.push(Span::styled(
                    truncate_middle(&suggestion.text, label_width),
                    label_style,
                ));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(
                        " \u{2014} ",
                        Style::default().fg(Color::DarkGray),
                    ));
                    spans.push(Span::styled(
                        truncate_text(&suggestion.description, area.width as usize / 2),
                        detail_style,
                    ));
                }
            }
            TypeaheadSource::History => {
                let display_name = truncate_text(&suggestion.text, label_width);
                spans.push(Span::styled(
                    format!("{display_name:<width$}", width = label_width),
                    label_style,
                ));
                spans.push(Span::styled(
                    " [history] ",
                    Style::default().fg(Color::DarkGray),
                ));
                if !suggestion.description.is_empty() {
                    spans.push(Span::styled(
                        truncate_text(&suggestion.description, area.width as usize / 2),
                        detail_style,
                    ));
                }
            }
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: area.x,
                y: area.y + row as u16,
                width: area.width,
                height: 1,
            },
        );
    }
}

// -----------------------------------------------------------------------
// Legacy simple help overlay (fallback when help_overlay is not open)
// -----------------------------------------------------------------------

fn render_simple_help_overlay(frame: &mut Frame, area: Rect) {
    let help_width = 50u16.min(area.width.saturating_sub(4));
    let help_height = 20u16.min(area.height.saturating_sub(4));
    let help_area = crate::overlays::centered_rect(help_width, help_height, area);

    frame.render_widget(Clear, help_area);

    let lines = vec![
        Line::from(vec![Span::styled(
            " Key Bindings",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )]),
        Line::from(""),
        kb_line("Enter", "Submit message"),
        kb_line("Ctrl+C", "Cancel streaming / Quit"),
        kb_line("Ctrl+D", "Quit (empty input)"),
        kb_line("Up / Down", "Navigate input history"),
        kb_line("Ctrl+R", "Search input history"),
        kb_line("PageUp / PageDown", "Scroll messages"),
        kb_line("F1 / ?", "Toggle this help"),
        Line::from(""),
        Line::from(vec![Span::styled(
            " Permission Dialog",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )]),
        Line::from(""),
        kb_line("1 / 2 / 3", "Select option"),
        kb_line("y / a / n", "Allow / Always / Deny"),
        kb_line("Enter", "Confirm selection"),
        kb_line("Esc", "Deny (close dialog)"),
        Line::from(""),
        Line::from(vec![Span::styled(
            " press F1 or ? to close ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Left);
    frame.render_widget(para, help_area);
}

fn kb_line<'a>(key: &str, desc: &str) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<20}", key),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(desc.to_string()),
    ])
}

// -----------------------------------------------------------------------
// Legacy history search overlay (used when history_search_overlay is not open)
// -----------------------------------------------------------------------

fn render_legacy_history_search(
    frame: &mut Frame,
    hs: &crate::app::HistorySearch,
    app: &App,
    area: Rect,
) {
    let dialog_width = 60u16.min(area.width.saturating_sub(4));
    let visible_matches = 8usize;
    let dialog_height = (4 + visible_matches.min(hs.matches.len().max(1)) as u16)
        .min(area.height.saturating_sub(4));
    let dialog_area = crate::overlays::centered_rect(dialog_width, dialog_height, area);

    frame.render_widget(Clear, dialog_area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::raw("  Search: "),
        Span::styled(
            hs.query.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2588}", Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(""));

    if hs.matches.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        let start = hs.selected.saturating_sub(visible_matches / 2);
        let end = (start + visible_matches).min(hs.matches.len());
        let start = end.saturating_sub(visible_matches).min(start);

        for (display_idx, &hist_idx) in hs.matches[start..end].iter().enumerate() {
            let real_idx = start + display_idx;
            let is_selected = real_idx == hs.selected;
            let entry = app
                .prompt_input
                .history
                .get(hist_idx)
                .map(String::as_str)
                .unwrap_or("");

            // truncate_end is width-aware, cuts on char boundaries, and appends
            // its own ellipsis. The old code did `String::truncate` on a raw
            // byte index (panics mid-codepoint) after a `usize` subtraction that
            // could underflow-panic on a narrow terminal (#221).
            let truncated = truncate_end(entry, (dialog_width as usize).saturating_sub(6));

            let (prefix, style) = if is_selected {
                (
                    "  \u{25BA} ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("    ", Style::default().fg(Color::White))
            };
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(truncated, style),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" History Search (Esc to cancel) ")
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, dialog_area);
}

// -----------------------------------------------------------------------
// Complete status line (T2-8)
// -----------------------------------------------------------------------

/// Complete status line data for rendering.
#[derive(Debug, Clone, Default)]
pub struct StatusLineData {
    pub model: String,
    pub tokens_used: u64,
    pub tokens_total: u64,
    pub cost_cents: f64,
    pub compact_warning_pct: Option<f64>, // None = no warning; Some(pct) = show warning
    pub vim_mode: Option<String>,         // None = no vim mode; Some("NORMAL") etc.
    pub bridge_connected: bool,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub agent_badge: Option<String>,
    pub rate_limit_pct_5h: Option<f64>,
    pub rate_limit_pct_7d: Option<f64>,
    /// Goal badge: Some("active · 5m · 3 turns") when a goal is running.
    pub goal_badge: Option<String>,
}

#[allow(dead_code)]
pub fn render_full_status_line(
    data: &StatusLineData,
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
) {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Paragraph, Widget},
    };

    let mut spans = Vec::new();

    // Model name
    if !data.model.is_empty() {
        spans.push(Span::styled(
            format!(" {} ", data.model),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled(" â”‚ ", Style::default().fg(Color::DarkGray)));
    }

    // Context window
    if data.tokens_total > 0 {
        let pct = data.tokens_used as f64 / data.tokens_total as f64;
        let ctx_color = if pct >= 0.95 {
            Color::Red
        } else if pct >= 0.80 {
            Color::Yellow
        } else {
            Color::Green
        };
        let used_k = data.tokens_used / 1000;
        let total_k = data.tokens_total / 1000;
        spans.push(Span::styled(
            format!("{}k/{}k ({:.0}%)", used_k, total_k, pct * 100.0),
            Style::default().fg(ctx_color),
        ));
        spans.push(Span::styled(" â”‚ ", Style::default().fg(Color::DarkGray)));
    }

    // Cost
    if data.cost_cents > 0.0 {
        spans.push(Span::styled(
            format!("${:.2}", data.cost_cents / 100.0),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::styled(" â”‚ ", Style::default().fg(Color::DarkGray)));
    }

    // Compact warning
    if let Some(pct) = data.compact_warning_pct {
        if pct >= 0.80 {
            let color = if pct >= 0.95 {
                Color::Red
            } else {
                Color::Yellow
            };
            spans.push(Span::styled(
                format!("âš  ctx {:.0}% ", pct * 100.0),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
    }

    // Vim mode
    if let Some(mode) = &data.vim_mode {
        let color = match mode.as_str() {
            "NORMAL" => Color::Green,
            "INSERT" => Color::Blue,
            "VISUAL" => Color::Magenta,
            _ => Color::White,
        };
        spans.push(Span::styled(
            format!("[{}]", mode),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }

    // Agent badge
    if let Some(badge) = &data.agent_badge {
        spans.push(Span::styled(
            format!("[{}]", badge),
            Style::default().fg(Color::Magenta),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }

    // Goal badge
    if let Some(goal) = &data.goal_badge {
        spans.push(Span::styled(
            format!("[goal: {}]", goal),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default()));
    }

    // Bridge connected
    if data.bridge_connected {
        spans.push(Span::styled("ðŸ”— ", Style::default().fg(Color::Green)));
    }

    // Session ID
    if let Some(sid) = &data.session_id {
        let short = &sid[..sid.len().min(8)];
        spans.push(Span::styled(
            format!("[session:{}]", short),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Worktree
    if let Some(wt) = &data.worktree {
        spans.push(Span::styled(
            format!("[worktree:{}]", wt),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let line = Line::from(spans);
    Paragraph::new(line)
        .style(Style::default().bg(Color::Black))
        .render(area, buf);
}

// ---------------------------------------------------------------------------
// Multi-agent UI components
// ---------------------------------------------------------------------------

/// Render a single progress-indicator row for a sub-agent.
///
/// Format: `[agent-<id>]` in cyan dim · space · status in colour · ` · ` · tool in dim gray
///
/// # Arguments
/// * `agent_id`    — short agent identifier (e.g. `"abc123"`)
/// * `status`      — current status string: `"working"`, `"done"`, `"error"`, or other
/// * `current_tool` — tool the agent is currently executing, if any
pub fn render_agent_progress_line(
    agent_id: &str,
    status: &str,
    current_tool: Option<&str>,
) -> Line<'static> {
    let status_color = match status {
        "working" | "running" => Color::Yellow,
        "done" | "complete" | "completed" => Color::Green,
        "error" | "failed" => Color::Red,
        _ => Color::DarkGray,
    };

    let mut spans = vec![
        Span::styled(
            format!("[agent-{}]", agent_id),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
        ),
        Span::raw(" "),
        Span::styled(status.to_string(), Style::default().fg(status_color)),
    ];

    if let Some(tool) = current_tool {
        spans.push(Span::styled(
            " · ".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            tool.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }

    Line::from(spans)
}

/// Render a multi-line coordinator status block for a multi-agent session.
///
/// Returns a `Vec<Line>` containing:
/// 1. A header: `Coordinator · N agents (M active)` in cyan bold
/// 2. One compact row per entry in `active_agents` using [`render_agent_progress_line`]
///
/// # Arguments
/// * `agent_count`   — total number of sub-agents spawned
/// * `completed`     — number of agents that have finished
/// * `active_agents` — slice of agent ID strings currently running
#[allow(dead_code)]
pub fn render_coordinator_status_lines(
    agent_count: usize,
    completed: usize,
    active_agents: &[&str],
) -> Vec<Line<'static>> {
    let active_count = active_agents.len();

    let header = Line::from(vec![
        Span::styled(
            "Coordinator".to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ".to_string(), Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{} agent{}",
                agent_count,
                if agent_count == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!(" ({} active)", active_count),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            if completed > 0 {
                format!("  ✔ {} done", completed)
            } else {
                String::new()
            },
            Style::default().fg(Color::Green),
        ),
    ]);

    let mut lines = vec![header];

    for agent_id in active_agents {
        let row = render_agent_progress_line(agent_id, "working", None);
        // Indent agent rows by two spaces
        let mut indented_spans = vec![Span::raw("  ")];
        indented_spans.extend(row.spans);
        lines.push(Line::from(indented_spans));
    }

    lines
}

/// Render a single header line for a teammate's message block.
///
/// Format: `┤ teammate: <id> ├` in magenta, optional `· <session_info>` in dim
///
/// # Arguments
/// * `teammate_id`  — teammate identifier string
/// * `session_info` — optional session info snippet to append
#[allow(dead_code)]
pub fn render_teammate_header(teammate_id: &str, session_info: Option<&str>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            "┤ teammate: ".to_string(),
            Style::default().fg(Color::Magenta),
        ),
        Span::styled(
            teammate_id.to_string(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ├".to_string(), Style::default().fg(Color::Magenta)),
    ];

    if let Some(info) = session_info {
        spans.push(Span::styled(
            "  · ".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            info.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }

    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Tests — tool-block rendering (icon headers, path shortening, todo checklist)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tool_block_tests {
    use super::*;
    use crate::app::{ToolStatus, ToolUseBlock};

    fn block(name: &str, status: ToolStatus, input: &str, preview: Option<&str>) -> ToolUseBlock {
        ToolUseBlock {
            id: "t".into(),
            name: name.into(),
            turn_index: None,
            status,
            output_preview: preview.map(|s| s.to_string()),
            input_json: input.into(),
        }
    }

    fn render(b: &ToolUseBlock) -> Vec<String> {
        let mut lines = Vec::new();
        render_tool_block_lines(&mut lines, b, 0);
        lines.iter().map(flatten_line_text).collect()
    }

    #[test]
    fn suggestion_window_fits_all_rows() {
        assert_eq!(suggestion_window(2, 5, 0, false), (0, 2));
        assert_eq!(suggestion_window(6, 10, 0, true), (0, 6));
    }

    #[test]
    fn suggestion_window_centers_on_selection() {
        // No faded hint: pure selection-centering window, as before.
        assert_eq!(suggestion_window(10, 5, 0, false), (0, 5));
        assert_eq!(suggestion_window(10, 5, 7, false), (5, 10));
        assert_eq!(suggestion_window(10, 5, 4, false), (2, 7));
    }

    #[test]
    fn suggestion_window_keeps_trailing_faded_hint_visible() {
        // 6 rows in a 5-row popup: the trailing <objective> hint must not be
        // windowed out, even though the selection sits at the top.
        assert_eq!(suggestion_window(6, 5, 0, true), (1, 6));
        // Selection in the middle: hint still wins the last row.
        assert_eq!(suggestion_window(6, 5, 2, true), (1, 6));
        // Larger lists scroll to the bottom so the hint is the last row.
        assert_eq!(suggestion_window(10, 5, 0, true), (5, 10));
        // Selected already at the end — hint already inside the window.
        assert_eq!(suggestion_window(6, 5, 5, true), (1, 6));
    }

    #[test]
    fn suggestion_window_empty_or_zero_height() {
        assert_eq!(suggestion_window(0, 5, 0, true), (0, 0));
        assert_eq!(suggestion_window(6, 0, 0, true), (0, 0));
    }

    #[test]
    fn icons_are_per_tool_and_ascii() {
        assert_eq!(tool_icon("bash"), "$");
        assert_eq!(tool_icon("read"), "<");
        assert_eq!(tool_icon("write"), ">");
        assert_eq!(tool_icon("glob"), "*");
        assert_eq!(tool_icon("grep"), "/");
        assert_eq!(tool_icon("todowrite"), ":");
        assert_eq!(tool_icon("something-unknown"), "~");
        // All markers must be single-byte ASCII (guaranteed one terminal cell).
        for t in [
            "bash",
            "read",
            "write",
            "glob",
            "grep",
            "webfetch",
            "websearch",
            "todo",
            "task",
            "x",
        ] {
            let icon = tool_icon(t);
            assert_eq!(icon.len(), 1, "{t} icon {icon:?} must be 1 ASCII byte");
            assert!(icon.is_ascii(), "{t} icon {icon:?} must be ASCII");
        }
    }

    #[test]
    fn shorten_home_replaces_prefix() {
        if let Some(home) = dirs::home_dir() {
            let p = home.join("projects").join("x.yaml");
            let shortened = shorten_home_path(&p.to_string_lossy());
            assert!(shortened.starts_with("~"), "got {shortened:?}");
            assert!(shortened.ends_with("x.yaml"));
            assert!(!shortened.contains(home.to_string_lossy().as_ref()));
        }
        // A non-home path is left untouched.
        assert_eq!(shorten_home_path("/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn bash_header_is_icon_led_and_not_duplicated() {
        let b = block(
            "bash",
            ToolStatus::Done,
            r#"{"command":"python3 - <<'PY'\nfrom pathlib import Path"}"#,
            Some("218183\nMarketing Outbound OS"),
        );
        let lines = render(&b);
        // Header: "$ python3 - <<'PY'"
        assert!(
            lines[0].contains('$'),
            "header should be icon-led: {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("python3 - <<'PY'"),
            "header shows command: {:?}",
            lines[0]
        );
        // The command must appear exactly once (no summary + $-line duplication).
        let joined = lines.join("\n");
        assert_eq!(
            joined.matches("python3 - <<'PY'").count(),
            1,
            "no dup: {joined:?}"
        );
        // Output preview still shown.
        assert!(joined.contains("218183"));
    }

    #[test]
    fn read_header_shortens_home_path() {
        if let Some(home) = dirs::home_dir() {
            let path = home.join("FOLLOWUPS.md");
            let input = serde_json::json!({
                "file_path": path.to_string_lossy().to_string(),
            })
            .to_string();
            let b = block("read", ToolStatus::Done, &input, None);
            let lines = render(&b);
            assert!(lines[0].contains('<'), "read icon: {:?}", lines[0]);
            assert!(lines[0].contains('~'), "home shortened: {:?}", lines[0]);
            assert!(!lines[0].contains(home.to_string_lossy().as_ref()));
        }
    }

    #[test]
    fn todo_renders_checklist_with_glyphs_and_counts() {
        let b = block(
            "TodoWrite",
            ToolStatus::Done,
            r#"{"todos":[
                {"content":"Locate files","status":"completed"},
                {"content":"Build importer","status":"in_progress"},
                {"content":"Wire adapter","status":"pending"}
            ]}"#,
            Some("Todo list updated (3 total)"),
        );
        let lines = render(&b);
        let joined = lines.join("\n");
        // Header shows count, not the raw "Todo list updated (...)".
        assert!(joined.contains("Todos"), "{joined:?}");
        assert!(joined.contains("1/3 done"), "{joined:?}");
        // Each status has its ASCII checkbox + content.
        assert!(
            joined.contains("[x] Locate files"),
            "done marker: {joined:?}"
        );
        assert!(
            joined.contains("[>] Build importer"),
            "in-progress marker: {joined:?}"
        );
        assert!(
            joined.contains("[ ] Wire adapter"),
            "pending marker: {joined:?}"
        );
        // The raw result-preview string must NOT leak into the checklist view.
        assert!(
            !joined.contains("Todo list updated"),
            "preview suppressed: {joined:?}"
        );
    }

    #[test]
    fn legacy_history_search_narrow_multibyte_no_panic() {
        use crate::app::{App, HistorySearch};
        use clawde_core::config::Config;
        use clawde_core::cost::CostTracker;
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = App::new(Config::default(), CostTracker::new());
        app.prompt_input.history = vec!["\u{4f60}\u{597d}\u{4e16}\u{754c}".repeat(6)]; // wide CJK
        let mut hs = HistorySearch::new();
        hs.matches = vec![0];

        // width 10 -> dialog_width 6 -> `dialog_width - 9` underflow-panicked
        // pre-fix, and `String::truncate` on a byte index sliced the CJK entry
        // mid-codepoint (#221). No panic == pass.
        let mut terminal = Terminal::new(TestBackend::new(10, 12)).unwrap();
        terminal
            .draw(|frame| render_legacy_history_search(frame, &hs, &app, frame.area()))
            .unwrap();
    }
}

/// Tests for the streaming transcript cache (issue #222): the committed prefix
/// must be reused across streaming deltas, and streaming output must be
/// byte-identical to a full (non-cached) rebuild.
#[cfg(test)]
mod stream_cache_tests {
    use super::*;
    use crate::app::App;
    use clawde_core::config::Config;
    use clawde_core::cost::CostTracker;
    use clawde_core::types::Message;

    const WIDTH: u16 = 80;

    fn test_app() -> App {
        App::new(Config::default(), CostTracker::new())
    }

    /// A per-item signature that captures the rendered spans+styles (via Debug)
    /// plus all metadata, so equality means byte-identical rendering.
    fn item_sig(item: &RenderedLineItem) -> (String, bool, Option<usize>, Option<u64>) {
        (
            format!("{:?}", item.line),
            item.is_header,
            item.message_index,
            item.thinking_hash,
        )
    }

    fn sigs(items: &[RenderedLineItem]) -> Vec<(String, bool, Option<usize>, Option<u64>)> {
        items.iter().map(item_sig).collect()
    }

    fn joined_text(items: &[RenderedLineItem]) -> String {
        items
            .iter()
            .map(|i| i.search_text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The completed-message items are reused (served from cache) across a
    /// streaming delta, while the live tail updates.
    #[test]
    fn completed_prefix_reused_across_streaming_delta() {
        let mut app = test_app();
        // Turn 0 is fully committed; turn 1 is the live/streaming turn.
        app.messages.push(Message::user("user one prompt"));
        app.messages
            .push(Message::assistant("assistant one committed reply"));
        app.messages.push(Message::user("user two prompt"));
        app.is_streaming = true;
        app.streaming_text = "streaming tail alpha".to_string();

        reset_render_caches();

        // First render: prefix is built fresh (a miss).
        let render1 = render_message_items(&app, WIDTH);
        assert_eq!(
            prefix_cache_counts(),
            (0, 1),
            "first render builds the prefix"
        );

        // A streaming delta arrives: only the live text grows. Real code bumps
        // transcript_version on every delta — assert that does NOT evict the
        // committed-prefix entry.
        app.streaming_text.push_str(" beta");
        app.invalidate_transcript();

        let render2 = render_message_items(&app, WIDTH);
        let (hits, misses) = prefix_cache_counts();
        assert_eq!(
            (hits, misses),
            (1, 1),
            "committed prefix served from cache after the delta (no rebuild)"
        );

        // The committed content is identical in both renders and appears before
        // the live tail diverges.
        let sig1 = sigs(&render1);
        let sig2 = sigs(&render2);
        let common = sig1
            .iter()
            .zip(sig2.iter())
            .take_while(|(a, b)| a == b)
            .count();
        assert!(common > 0, "some leading items must be identical");
        let leading_text = joined_text(&render1[..common]);
        assert!(
            leading_text.contains("user one prompt")
                && leading_text.contains("assistant one committed reply"),
            "the reused prefix contains the whole committed turn: {leading_text:?}"
        );
        // The reused prefix must not contain any live tail content.
        assert!(
            !leading_text.contains("streaming tail alpha"),
            "prefix must not include the live tail: {leading_text:?}"
        );

        // The live tail updated between renders.
        let text1 = joined_text(&render1);
        let text2 = joined_text(&render2);
        assert!(text1.contains("streaming tail alpha"));
        assert!(!text1.contains("streaming tail alpha beta"));
        assert!(
            text2.contains("streaming tail alpha beta"),
            "tail rebuilt with the delta"
        );
    }

    /// Streaming render (cached prefix + rebuilt tail) is byte-identical to a
    /// full rebuild for a multi-message transcript — no ghosting, no missing or
    /// stale content — both on the first (cold) frame and after a delta (warm).
    #[test]
    fn streaming_render_matches_full_rebuild() {
        let mut app = test_app();
        app.messages.push(Message::user("first user question"));
        app.messages.push(Message::assistant(
            "first assistant answer with **markdown**",
        ));
        app.messages.push(Message::user("second user question"));
        app.messages
            .push(Message::assistant("second assistant answer"));
        app.messages.push(Message::user("third user question"));
        app.is_streaming = true;
        app.streaming_thinking = "pondering the third answer".to_string();
        app.streaming_text = "third answer so far".to_string();

        reset_render_caches();

        // Cold frame: streaming path vs a direct full rebuild.
        let streamed_cold = render_message_items(&app, WIDTH);
        let full_cold = build_all_items(&app, WIDTH);
        assert_eq!(
            sigs(&streamed_cold),
            sigs(&full_cold),
            "cold streaming render must match a full rebuild"
        );

        // Warm frame: after a delta, the prefix is served from cache but the
        // concatenation must still equal a full rebuild.
        app.streaming_text.push_str(" plus more tokens");
        app.invalidate_transcript();
        let streamed_warm = render_message_items(&app, WIDTH);
        let (hits, _) = prefix_cache_counts();
        assert!(hits >= 1, "warm frame served the prefix from cache");
        let full_warm = build_all_items(&app, WIDTH);
        assert_eq!(
            sigs(&streamed_warm),
            sigs(&full_warm),
            "warm streaming render must match a full rebuild"
        );
    }

    /// Swapping the transcript (session switch / fork / revert / compaction)
    /// must NOT serve a stale committed prefix, even mid-stream.
    #[test]
    fn transcript_swap_does_not_ghost_stale_prefix() {
        let mut app = test_app();
        app.messages.push(Message::user("session A user"));
        app.messages
            .push(Message::assistant("session A assistant reply"));
        app.messages.push(Message::user("session A live turn"));
        app.is_streaming = true;
        app.streaming_text = "A tail".to_string();

        reset_render_caches();
        let render_a = render_message_items(&app, WIDTH);
        assert!(joined_text(&render_a).contains("session A assistant reply"));

        // Swap in a different transcript (new Vec) while still streaming. The
        // prefix cache must be re-keyed by identity, so no session-A content
        // leaks through.
        app.messages = vec![
            Message::user("session B user"),
            Message::assistant("session B assistant reply"),
            Message::user("session B live turn"),
        ];
        app.streaming_text = "B tail".to_string();
        app.invalidate_transcript();

        let render_b = render_message_items(&app, WIDTH);
        let text_b = joined_text(&render_b);
        assert!(
            text_b.contains("session B assistant reply"),
            "shows swapped content"
        );
        assert!(
            !text_b.contains("session A"),
            "no stale session-A content ghosts through: {text_b:?}"
        );
        // And the swapped render equals a full rebuild.
        assert_eq!(sigs(&render_b), sigs(&build_all_items(&app, WIDTH)));
    }

    /// The last message toggling streaming -> completed moves cleanly into the
    /// cached (non-streaming) set with identical content.
    #[test]
    fn streaming_to_completed_transition_is_clean() {
        let mut app = test_app();
        app.messages.push(Message::user("q1"));
        app.messages.push(Message::assistant("a1 committed"));
        app.messages.push(Message::user("q2"));
        app.is_streaming = true;
        app.streaming_text = "live answer body".to_string();

        reset_render_caches();
        let _streaming = render_message_items(&app, WIDTH);

        // Commit the streamed message (as flush_streamed_assistant_message would)
        // and end streaming.
        app.messages.push(Message::assistant("live answer body"));
        app.is_streaming = false;
        app.streaming_text.clear();
        app.invalidate_transcript();

        let completed = render_message_items(&app, WIDTH);
        // Non-streaming render equals a full rebuild (correct committed set).
        assert_eq!(sigs(&completed), sigs(&build_all_items(&app, WIDTH)));
        let text = joined_text(&completed);
        assert!(text.contains("a1 committed"));
        assert!(text.contains("live answer body"));
    }

    #[test]
    fn verify_annotation_renders_boxed_check_block() {
        let mut app = test_app();
        app.messages.push(Message::user("add tests"));
        app.messages.push(Message::assistant("done writing tests"));

        // Mixed round: one passing check, one failing check.
        let report = clawde_query::VerifyReport {
            verdict: clawde_query::VerifyVerdict::Fixable,
            results: vec![
                clawde_query::CheckResult {
                    label: "test: cargo test --workspace".to_string(),
                    ok: false,
                    output: "1 test failed".to_string(),
                    timed_out: false,
                    skipped: false,
                    elapsed_secs: None,
                },
                clawde_query::CheckResult {
                    label: "lint: cargo clippy".to_string(),
                    ok: true,
                    output: String::new(),
                    timed_out: false,
                    skipped: false,
                    elapsed_secs: None,
                },
            ],
            attempt: 1,
            max_retries: 3,
            headline: "Auto-fix attempt 1/3".to_string(),
            sandbox: clawde_core::config::VerifySandbox::Worktree,
            unavailable: false,
        };
        app.push_verify_annotation(report);

        reset_render_caches();
        let items = render_message_items(&app, WIDTH);
        let text = joined_text(&items);

        // Box chrome + per-check statuses + the attempt headline are all
        // present in the transcript, anchored after the assistant turn.
        assert!(text.contains("Verify · git worktree"), "text: {text}");
        assert!(text.contains("FAIL"), "text: {text}");
        assert!(text.contains("PASS"), "text: {text}");
        assert!(text.contains("Auto-fix attempt 1/3"), "text: {text}");
        assert!(text.contains("cargo test --workspace"), "text: {text}");
        assert!(text.contains("cargo clippy"), "text: {text}");

        // The rendered block must appear AFTER the assistant message (not
        // before the user prompt): the annotation is anchored at the end.
        let verify_pos = text.find("Verify").expect("verify box present");
        let done_pos = text
            .find("done writing tests")
            .expect("assistant text present");
        assert!(verify_pos > done_pos, "verify box must follow the turn");
    }
}

/// The `/effort` selector docks into the prompt area and replaces the prompt box
/// while open (issue #275).
#[cfg(test)]
mod effort_dock_tests {
    use super::*;
    use crate::app::App;
    use crate::model_picker::EffortLevel;
    use clawde_core::config::Config;
    use clawde_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};

    /// The prompt pointer glyph drawn by `render_prompt_input`.
    const PROMPT_POINTER: char = '\u{276f}';

    fn render_screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render_app(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
        }
        out
    }

    #[test]
    fn effort_picker_replaces_prompt_box_when_open() {
        let mut app = App::new(Config::default(), CostTracker::new());

        // Closed: the prompt box (its pointer) is drawn; no selector chrome.
        let closed = render_screen(&app);
        assert!(
            closed.contains(PROMPT_POINTER),
            "prompt pointer should be visible when the picker is closed"
        );
        assert!(
            !closed.contains("ultracode"),
            "selector labels must not show while the picker is closed"
        );

        // Open: the selector takes over the prompt area; the prompt box is gone.
        app.effort_picker.open(
            EffortLevel::High,
            vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
                EffortLevel::XHigh,
                EffortLevel::Max,
                EffortLevel::Ultracode,
            ],
        );
        let open = render_screen(&app);
        assert!(
            open.contains("Effort") && open.contains("ultracode"),
            "the docked Effort selector should render in the prompt area"
        );
        assert!(
            !open.contains(PROMPT_POINTER),
            "prompt input must NOT be drawn while the picker is open"
        );
    }
}

// ---------------------------------------------------------------------------
// Welcome screen: recent activity (issue #277)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod recent_activity_tests {
    use super::*;
    use crate::app::{App, RecentSession};
    use clawde_core::config::Config;
    use clawde_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::{Duration, SystemTime};

    fn recent(label: &str, secs_ago: u64) -> RecentSession {
        RecentSession {
            session_id: "test-session".to_string(),
            label: label.to_string(),
            mtime: SystemTime::now() - Duration::from_secs(secs_ago),
        }
    }

    fn lines_text(recent: &[RecentSession], width: usize) -> Vec<String> {
        recent_activity_lines(recent, width, None)
            .iter()
            .map(flatten_line_text)
            .collect()
    }

    // -- relative-time formatter ------------------------------------------

    #[test]
    fn short_relative_secs_buckets() {
        assert_eq!(short_relative_secs(0), "just now");
        assert_eq!(short_relative_secs(59), "just now");
        assert_eq!(short_relative_secs(60), "1m ago");
        assert_eq!(short_relative_secs(5 * 60), "5m ago");
        assert_eq!(short_relative_secs(2 * 3_600), "2h ago");
        assert_eq!(short_relative_secs(3 * 86_400), "3d ago");
    }

    #[test]
    fn short_relative_time_handles_future_mtime() {
        // Clock skew (mtime slightly in the future) must not panic.
        let future = SystemTime::now() + Duration::from_secs(120);
        assert_eq!(short_relative_time(future), "just now");
    }

    // -- render-from-state path -------------------------------------------

    #[test]
    fn empty_state_shows_placeholder() {
        let out = lines_text(&[], 40);
        assert_eq!(out, vec!["No recent activity".to_string()]);
    }

    #[test]
    fn populated_state_shows_titles_and_relative_times() {
        // Both sessions are under 24 h old → relative timestamps.
        let sessions = vec![
            recent("Fix the parser bug", 2 * 3_600),
            recent("Wire up onboarding", 6 * 3_600),
        ];
        let out = lines_text(&sessions, 40).join("\n");
        assert!(out.contains("Fix the parser bug"), "first title: {out:?}");
        assert!(out.contains("2h ago"), "first time: {out:?}");
        assert!(out.contains("Wire up onboarding"), "second title: {out:?}");
        assert!(out.contains("6h ago"), "second time: {out:?}");
        // The placeholder must NOT appear when there is real activity.
        assert!(
            !out.contains("No recent activity"),
            "no placeholder: {out:?}"
        );
    }

    #[test]
    fn stale_sessions_show_absolute_date_time() {
        // Older than 24 h → the list shows an absolute timestamp instead of
        // "3d ago", so cross-day sessions stay unambiguous.
        let sessions = vec![recent("Old task", 3 * 86_400)];
        let out = lines_text(&sessions, 40).join("\n");
        assert!(out.contains("Old task"), "title: {out:?}");
        assert!(
            !out.contains("3d ago"),
            "stale sessions must not show relative time: {out:?}"
        );
        // Absolute form contains a space-separated month/day.
        assert!(
            out.len() > "Old task".len() + 9,
            "expected trailing timestamp, got {out:?}"
        );
    }

    #[test]
    fn caps_at_five_entries() {
        let sessions: Vec<RecentSession> = (0..8)
            .map(|i| recent(&format!("session {i}"), 60))
            .collect();
        assert_eq!(recent_activity_lines(&sessions, 40, None).len(), 5);
    }

    #[test]
    fn long_label_is_truncated_and_leaves_room_for_time() {
        let sessions = vec![recent(
            "an extremely long session title that should be truncated to fit",
            60,
        )];
        let out = lines_text(&sessions, 20);
        assert_eq!(out.len(), 1);
        let line = &out[0];
        assert!(line.contains('\u{2026}'), "should be ellipsised: {line:?}");
        assert!(line.ends_with("1m ago"), "time preserved at end: {line:?}");
    }

    #[test]
    fn welcome_box_renders_recent_activity_from_state() {
        // Full-widget smoke test: the section header renders and, when state is
        // populated, a session label reaches the screen buffer without panic.
        let mut app = App::new(Config::default(), CostTracker::new());
        app.recent_sessions = vec![recent("Sortable label ABCDEF", 2 * 3_600)];

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render_welcome_box(frame, &app, frame.area()))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            screen.contains("Recent activity"),
            "header rendered: present"
        );
        assert!(screen.contains("Sortable label"), "session label rendered");
    }
}

// ---------------------------------------------------------------------------
// Status row: fast / plan / goal badges
// ---------------------------------------------------------------------------

#[cfg(test)]
mod status_row_badge_tests {
    use super::*;
    use crate::app::App;
    use clawde_core::config::Config;
    use clawde_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};

    fn render_screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render_app(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
        }
        out
    }

    #[test]
    fn fast_badge_appears_when_fast_mode_enabled() {
        let mut app = App::new(Config::default(), CostTracker::new());
        // Enable fast_mode AND set a status message so the status row renders.
        app.fast_mode = true;
        app.status_message = Some("Testing fast mode.".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("Fast"),
            "'Fast' badge should appear in the status row when fast_mode=true. Output: {:?}",
            out
        );
    }

    #[test]
    fn plan_mode_shows_in_prompt_row_not_status_row() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.plan_mode = true;
        app.status_message = Some("Testing plan mode.".to_string());
        let out = render_screen(&app);
        // The [PLAN] badge appears in the prompt config row (Row 2),
        // not as a standalone "Plan" in the transient status row (Row 1).
        assert!(
            out.contains("PLAN"),
            "'PLAN' mode badge should appear in the prompt row when plan_mode=true. Output: {:?}",
            out
        );
    }

    #[test]
    fn goal_badge_appears_when_active_goal_set() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.active_goal_badge = Some("active · 5m · 3 turns".to_string());
        app.status_message = Some("Testing goal.".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("active"),
            "Goal badge should appear in the status row when active_goal_badge is set. Output: {:?}",
            out
        );
    }

    #[test]
    fn fast_badge_appears_with_plan_mode() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.fast_mode = true;
        app.plan_mode = true;
        app.status_message = Some("Testing both.".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("Fast"),
            "'Fast' badge should appear. Output: {:?}",
            out
        );
        // plan_mode is communicated by [PLAN] in the prompt row,
        // not as a "Plan" badge in the status row.
        assert!(
            out.contains("PLAN"),
            "'PLAN' mode badge should appear. Output: {:?}",
            out
        );
    }

    #[test]
    fn status_row_hidden_when_no_reason_to_show() {
        let mut app = App::new(Config::default(), CostTracker::new());
        // No streaming, no status message, no modes, no exhausted keys.
        app.is_streaming = false;
        app.status_message = None;
        app.fast_mode = false;
        app.plan_mode = false;
        app.active_goal_badge = None;
        let out = render_screen(&app);
        // The status row line is at chunk[2]; when hidden `status_height` is 0
        // and the row is not rendered. We can't check for absence of something
        // that is not there, but we can verify the screen doesn't have unexpected
        // text from a spuriously rendered status row.
        assert!(
            !out.contains("Thinking") && !out.contains("Fast"),
            "Status row should be hidden when there is no reason to show it"
        );
    }

    #[test]
    fn fast_badge_appears_with_last_turn_verb() {
        // Simulate the idle state: a completed turn with last_turn_verb and
        // last_turn_elapsed set, plus fast_mode enabled.
        let mut app = App::new(Config::default(), CostTracker::new());
        app.last_turn_verb = Some("Purred");
        app.last_turn_elapsed = Some("5s".to_string());
        app.fast_mode = true;
        let out = render_screen(&app);
        assert!(
            out.contains("Purred"),
            "Turn verb should be visible. Output: {:?}",
            out
        );
        assert!(
            out.contains("Fast"),
            "'Fast' badge should appear after the turn verb. Output: {:?}",
            out
        );
    }

    #[test]
    fn fast_on_sets_fast_mode_and_shows_badge() {
        // The on/off args path: set fast_mode via intercept_slash_command_with_args.
        let mut app = App::new(Config::default(), CostTracker::new());
        assert!(app.intercept_slash_command_with_args("fast", "on"));
        assert!(app.fast_mode);
        let out = render_screen(&app);
        assert!(
            out.contains("Fast"),
            "'Fast' badge should appear after /fast on. Output: {:?}",
            out
        );
    }

    #[test]
    fn fast_off_clears_fast_mode() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.fast_mode = true;
        assert!(app.intercept_slash_command_with_args("fast", "off"));
        assert!(!app.fast_mode);
        let out = render_screen(&app);
        // The status message "Fast mode off." contains "Fast", so check for
        // the badge separator pattern instead.
        let badge_marker = " · Fast";
        assert!(
            !out.contains(badge_marker),
            "'Fast' badge should NOT appear after /fast off. Output: {:?}",
            out
        );
    }

    #[test]
    fn model_picker_does_not_clear_fast_mode() {
        // fast_mode is now decoupled from model selection.
        let mut app = App::new(Config::default(), CostTracker::new());
        app.fast_mode = true;
        // Simulate the model picker close path — confirm fast_mode survives.
        app.set_model("claude-sonnet-4-20250514".to_string());
        app.status_message = Some("Model changed.".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("Fast"),
            "'Fast' badge should survive a model change. Output: {:?}",
            out
        );
    }

    #[test]
    fn spans_to_text_splits_multiline_status_messages() {
        let text = spans_to_text(vec![Span::styled(
            "line one\nline two\nline three".to_string(),
            Style::default().fg(Color::Red),
        )]);
        let rendered: Vec<String> = text.lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(rendered, vec!["line one", "line two", "line three"]);
        // Style is preserved on every split fragment.
        for line in &text.lines {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(Color::Red));
            }
        }
    }

    #[test]
    fn spans_to_text_handles_single_line_and_trailing_newline() {
        let single = spans_to_text(vec![Span::raw("hello")]);
        assert_eq!(single.lines.len(), 1);
        // A trailing newline must not create a spurious empty final line.
        let trailing = spans_to_text(vec![Span::raw("a\nb\n")]);
        assert_eq!(trailing.lines.len(), 2);
        // Empty content produces no lines at all.
        assert!(spans_to_text(Vec::new()).lines.is_empty());
    }

    #[test]
    fn mode_preset_badge_applies_when_non_default() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.config.mode = Some("careful".to_string());
        app.status_message = Some("test".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("careful"),
            "'careful' mode badge should appear in prompt row when config.mode is set. Output: {:?}",
            out
        );
    }

    #[test]
    fn default_mode_does_not_show_badge() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.config.mode = Some("default".to_string());
        app.status_message = Some("test".to_string());
        let out = render_screen(&app);
        // 'default' should not appear as a badge — only non-default modes are shown.
        let badge_marker = " · default";
        assert!(
            !out.contains(badge_marker),
            "'default' mode badge should NOT appear. Output: {:?}",
            out
        );
    }

    #[test]
    fn followup_row_map_only_tracks_visible_rows_under_scroll() {
        let mut app = App::new(Config::default(), CostTracker::new());
        // Long transcript: enough turns that the followup block sits below the
        // 24-row viewport when scrolled to the top.
        for i in 0..30 {
            app.messages
                .push(clawde_core::types::Message::assistant(format!(
                    "filler message {i}"
                )));
        }
        app.current_followups = vec![clawde_core::RankedFollowup {
            text: "Run tests".into(),
            rank: clawde_core::FollowupRank::Recommended,
            reason: String::new(),
        }];
        // Scrolled to the top (scroll_offset counts lines above the bottom, so
        // a large offset pins the viewport to the start): the followups are
        // below the fold and must not be clickable (no row-map entries).
        app.auto_scroll = false;
        app.scroll_offset = 1000;
        render_screen(&app);
        assert!(
            app.followup_row_map.borrow().is_empty(),
            "followups below the fold must not be in the row map"
        );
        // Pinned to the bottom: the followups are visible and every row maps
        // back to the same current-response item.
        app.auto_scroll = true;
        app.scroll_offset = 0;
        render_screen(&app);
        let map = app.followup_row_map.borrow();
        assert!(!map.is_empty(), "visible followups must be in the row map");
        assert!(map
            .values()
            .all(|t| t.source == FollowupSource::Current && t.index == 0));
    }

    #[test]
    fn followup_arrow_selection_invalidates_cached_lines() {
        let mut app = App::new(Config::default(), CostTracker::new());
        // One message so the transcript (not the welcome box) renders.
        app.messages
            .push(clawde_core::types::Message::assistant("hello"));
        app.current_followups = vec![clawde_core::RankedFollowup {
            text: "Run tests".into(),
            rank: clawde_core::FollowupRank::Recommended,
            reason: String::new(),
        }];
        // First render populates the message-lines cache with no selection.
        let plain = render_screen(&app);
        assert!(
            !plain.contains('→'),
            "unselected followups must not render the highlight marker"
        );
        // Arrow-key navigation changes only `followup_selected` — the cache
        // must not serve the stale unselected lines.
        app.followup_selected = Some(0);
        let selected = render_screen(&app);
        assert!(
            selected.contains('→'),
            "selected followup must render the highlight marker"
        );
        // And deselecting restores the plain rendering.
        app.followup_selected = None;
        let deselected = render_screen(&app);
        assert!(!deselected.contains('→'));
    }
}

#[cfg(test)]
mod task_badge_tooltip_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn right_aligned_span_rect_math() {
        // Right-aligned line inside a 40-wide area starting at x=10.
        // Spans: "AB" (2) + " · " (3) + "coding" (6) + " · " (3) + "shortcut" (8)
        let spans = vec![
            Span::raw("AB"),
            Span::styled(" · ", Style::default()),
            Span::raw("coding"),
            Span::styled(" · ", Style::default()),
            Span::raw("shortcut"),
        ];
        let area = Rect {
            x: 10,
            y: 3,
            width: 40,
            height: 1,
        };
        // Line width = 2+3+6+3+8 = 22 → starts at 10+40-22 = 28.
        let badge = right_aligned_span_rect(&spans, 2, area).unwrap();
        assert_eq!(badge.x, 28 + 2 + 3); // offset of "coding" within the line
        assert_eq!(badge.y, 3);
        assert_eq!(badge.width, 6);
        assert_eq!(badge.height, 1);
        // Badge out of range → None.
        assert!(right_aligned_span_rect(&spans, 99, area).is_none());
        // Over-long line (wider than the area) clips on the left in the real
        // renderer — no rect to hover, so bail out with None.
        let long: Vec<Span> = vec![Span::raw("x".repeat(60))];
        assert!(right_aligned_span_rect(&long, 0, area).is_none());
    }

    #[test]
    fn tooltip_draws_isolated_from_real_settings() {
        // Guard: App::new restores free_task_sort from settings; point
        // CLAWDE_HOME at a temp dir so a malformed real settings file can
        // never flake this render test (mirrors app.rs TestHome). Serializes
        // on the crate-wide TEST_ENV_LOCK per AGENTS.md.
        let _lock = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("CLAWDE_HOME");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", tmp.path());

        let app = crate::app::App::new(
            clawde_core::config::Config::default(),
            clawde_core::cost::CostTracker::new(),
        );
        // The tooltip must draw for any task sort without touching disk.
        app.task_badge_rect.set(Rect {
            x: 60,
            y: 5,
            width: 6,
            height: 1,
        });
        app.last_mouse_pos.set(Some((62, 5)));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| render_task_badge_tooltip(f, &app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let drawn: String = buffer
            .content()
            .iter()
            .filter(|c| c.symbol() != " ")
            .map(|c| c.symbol())
            .collect();
        assert!(drawn.contains("coding"));

        match prev {
            Some(v) => std::env::set_var("CLAWDE_HOME", v),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
    }

    #[test]
    fn tooltip_lists_all_valid_task_names() {
        let lines = task_tooltip_lines(crate::model_picker::FreeTask::Coding);
        let text = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect::<Vec<_>>()
            .join("");
        for name in [
            "all",
            "coding",
            "reasoning",
            "creative",
            "fast",
            "multimodal",
            "long context",
        ] {
            assert!(text.contains(name), "tooltip missing '{name}': {text}");
        }
        // Affordance hints present.
        assert!(text.contains("/task <name>"));
        assert!(text.contains("alt+t"));
        assert!(text.contains("1-7"));
    }

    #[test]
    fn tooltip_draws_only_when_mouse_over_badge() {
        let app = crate::app::App::new(
            clawde_core::config::Config::default(),
            clawde_core::cost::CostTracker::new(),
        );
        // Pretend the task badge was drawn at (60, 5) last frame and the
        // cursor is hovering exactly over it.
        app.task_badge_rect.set(Rect {
            x: 60,
            y: 5,
            width: 6,
            height: 1,
        });
        app.last_mouse_pos.set(Some((62, 5)));

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| render_task_badge_tooltip(f, &app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let drawn: String = buffer
            .content()
            .iter()
            .filter(|c| c.symbol() != " ")
            .map(|c| c.symbol())
            .collect();
        assert!(
            drawn.contains("coding"),
            "tooltip should draw when hovering the badge"
        );

        // Move the cursor away — the tooltip must disappear.
        app.last_mouse_pos.set(Some((10, 20)));
        terminal
            .draw(|f| render_task_badge_tooltip(f, &app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let drawn: String = buffer
            .content()
            .iter()
            .filter(|c| c.symbol() != " ")
            .map(|c| c.symbol())
            .collect();
        assert!(
            !drawn.contains("coding"),
            "tooltip must not draw when the mouse is elsewhere"
        );

        // And with no recorded mouse position at all.
        app.last_mouse_pos.set(None);
        terminal
            .draw(|f| render_task_badge_tooltip(f, &app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let drawn: String = buffer
            .content()
            .iter()
            .filter(|c| c.symbol() != " ")
            .map(|c| c.symbol())
            .collect();
        assert!(!drawn.contains("coding"));
    }

    // ---- verify footer badge --------------------------------------------

    fn verify_report_with(results: Vec<(bool, bool)>) -> clawde_query::VerifyReport {
        // (ok, skipped) pairs -> CheckResult (constructors are pub(crate) in
        // query, so build the struct directly here).
        clawde_query::VerifyReport {
            verdict: if results.iter().all(|(ok, skipped)| *ok && !*skipped) {
                clawde_query::VerifyVerdict::Pass
            } else if results.iter().any(|(ok, skipped)| !*ok && !*skipped) {
                clawde_query::VerifyVerdict::Fixable
            } else {
                clawde_query::VerifyVerdict::Escalate
            },
            results: results
                .into_iter()
                .map(|(ok, skipped)| clawde_query::CheckResult {
                    label: "check".to_string(),
                    ok,
                    output: String::new(),
                    timed_out: false,
                    skipped,
                    elapsed_secs: None,
                })
                .collect(),
            attempt: 1,
            max_retries: 3,
            headline: "h".to_string(),
            sandbox: clawde_core::config::VerifySandbox::Direct,
            unavailable: false,
        }
    }

    #[test]
    fn verify_footer_badge_is_green_on_pass() {
        let report = verify_report_with(vec![(true, false), (true, false)]);
        let (label, color) = verify_footer_badge(&report);
        assert!(label.starts_with("✓"), "label: {label}");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn verify_footer_badge_is_red_on_failure() {
        let report = verify_report_with(vec![(true, false), (false, false)]);
        let (label, color) = verify_footer_badge(&report);
        assert!(label.starts_with("✗"), "label: {label}");
        assert_eq!(color, Color::Red);
    }

    #[test]
    fn verify_footer_badge_is_neutral_when_nothing_ran() {
        let report = verify_report_with(vec![]);
        let (label, color) = verify_footer_badge(&report);
        assert!(label.starts_with("△"), "label: {label}");
        assert_eq!(color, Color::DarkGray);
    }

    #[test]
    fn verify_footer_badge_is_unavailable_when_sandbox_cannot_run() {
        let mut report = verify_report_with(vec![]);
        report.unavailable = true;
        let (label, color) = verify_footer_badge(&report);
        assert!(label.starts_with("! verify"), "label: {label}");
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn verify_footer_badge_shows_attempt_only_for_mid_loop_rounds() {
        let mut report = verify_report_with(vec![(false, false)]);
        let (label, _) = verify_footer_badge(&report);
        assert!(
            !label.contains('('),
            "first round must stay compact: {label}"
        );

        // Mid-loop auto-fix round → counter shown.
        report.attempt = 2;
        let (label, _) = verify_footer_badge(&report);
        assert!(label.contains("(2/3)"), "attempt counter missing: {label}");

        // Exhausted (attempt > max) is a final round → back to compact.
        report.attempt = 4;
        let (label, _) = verify_footer_badge(&report);
        assert!(
            !label.contains('('),
            "exhausted round must stay compact: {label}"
        );
    }

    #[test]
    fn verify_block_shows_elapsed_timing() {
        let mut report = verify_report_with(vec![(true, false), (false, false)]);
        report.results[0].elapsed_secs = Some(42);
        report.results[1].elapsed_secs = Some(7);
        let mut lines = Vec::new();
        render_verify_block(&mut lines, &report, 80);
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(rendered.contains("PASS (42s)"), "rendered: {rendered}");
        assert!(rendered.contains("FAIL (7s)"), "rendered: {rendered}");
    }

    #[test]
    fn verify_block_hides_timing_for_skipped_checks() {
        let mut report = verify_report_with(vec![(false, true)]);
        report.results[0].elapsed_secs = Some(0);
        let mut lines = Vec::new();
        render_verify_block(&mut lines, &report, 80);
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(rendered.contains("SKIP"), "rendered: {rendered}");
        assert!(
            !rendered.contains("(0s)"),
            "a never-started check must not show timing: {rendered}"
        );
    }

    // ---- project_memory_line (spec §15.3) ----------------------------------

    #[test]
    fn project_memory_line_renders_when_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "# Index\n").unwrap();
        std::fs::write(
            dir.path().join("sessions").join("2026-08-01.md"),
            "summary\n",
        )
        .unwrap();
        let line = project_memory_line(dir.path()).expect("line should render");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Mnemosyne"), "got: {text}");
        // MEMORY.md + the session summary = 2 files.
        assert!(text.contains("2 files"), "got: {text}");
    }

    #[test]
    fn project_memory_line_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(project_memory_line(dir.path()).is_none());
    }

    #[test]
    fn project_memory_line_counts_pending_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "# Index\n").unwrap();
        std::fs::write(
            dir.path().join("prefs.md"),
            "---\ndescription: Concise\n---\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("verbose-claim.md"),
            "---\ndescription: Verbose\nconflicts: prefs.md\n---\n",
        )
        .unwrap();
        let line = project_memory_line(dir.path()).expect("line should render");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1 Lethesyne"), "got: {text}");
        assert!(text.contains("3 files"), "got: {text}");
    }

    #[test]
    fn project_memory_line_counts_pairs_not_claimant_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "# Index\n").unwrap();
        std::fs::write(dir.path().join("a.md"), "---\ndescription: A\n---\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "---\ndescription: B\n---\n").unwrap();
        // One claimant with two adjudicable pairs → the indicator shows the
        // pair count (2), matching the injected block's two lines.
        std::fs::write(
            dir.path().join("claim.md"),
            "---\ndescription: C\nconflicts: a.md, b.md\n---\n",
        )
        .unwrap();
        let line = project_memory_line(dir.path()).expect("line should render");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("2 Lethesyne"), "got: {text}");
        // A resolved pair is not adjudicable — it must not inflate the count.
        std::fs::write(
            dir.path().join("claim2.md"),
            "---\ndescription: D\nconflicts: a.md\nresolved: a.md\n---\n",
        )
        .unwrap();
        let line = project_memory_line(dir.path()).expect("line should render");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("2 Lethesyne"), "got: {text}");
    }

    #[test]
    fn project_memory_line_no_pending_conflicts_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "# Index\n").unwrap();
        std::fs::write(
            dir.path().join("prefs.md"),
            "---\ndescription: Concise\n---\n",
        )
        .unwrap();
        let line = project_memory_line(dir.path()).expect("line should render");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("Lethesyne"), "got: {text}");
    }
}

// ---------------------------------------------------------------------------
// Ollama footer indicator — 3-state test
// ---------------------------------------------------------------------------

#[cfg(test)]
mod ollama_indicator_tests {
    use super::*;
    use crate::app::App;
    use clawde_core::config::Config;
    use clawde_core::cost::CostTracker;
    use ratatui::{backend::TestBackend, Terminal};

    fn render_screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render_app(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn ollama_auto_no_vram_shows_dim_label() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.ollama_mode = clawde_core::OllamaMode::Auto;
        app.ollama_loaded_models = Vec::new();
        app.status_message = Some("test".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("ollama:auto"),
            "dim 'ollama:auto' should appear when auto + no VRAM. Output: {:?}",
            out
        );
        assert!(
            !out.contains("ollama:online"),
            "'ollama:online' must NOT appear when no models are loaded. Output: {:?}",
            out
        );
        assert!(
            !out.contains("ollama:offline"),
            "'ollama:offline' must NOT appear in auto mode. Output: {:?}",
            out
        );
    }

    #[test]
    fn ollama_auto_with_vram_shows_online() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.ollama_mode = clawde_core::OllamaMode::Auto;
        app.ollama_loaded_models = vec![clawde_core::OllamaLoadedModel {
            name: "llama3.2".to_string(),
            size: Some(2_000_000_000),
            size_vram: Some(2_000_000_000),
            expires_at: None,
            context_length: Some(8192),
        }];
        app.status_message = Some("test".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("ollama:online"),
            "'ollama:online' should appear when auto + VRAM loaded. Output: {:?}",
            out
        );
        assert!(
            !out.contains("ollama:auto"),
            "'ollama:auto' must NOT appear when models are loaded. Output: {:?}",
            out
        );
    }

    #[test]
    fn ollama_isolated_shows_offline_with_lock() {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.ollama_mode = clawde_core::OllamaMode::Isolated;
        app.ollama_loaded_models = Vec::new();
        app.status_message = Some("test".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("ollama:offline"),
            "'ollama:offline' should appear in isolated mode. Output: {:?}",
            out
        );
        assert!(
            !out.contains("ollama:auto"),
            "'ollama:auto' must NOT appear in isolated mode. Output: {:?}",
            out
        );
        assert!(
            !out.contains("ollama:online"),
            "'ollama:online' must NOT appear in isolated mode. Output: {:?}",
            out
        );
    }

    #[test]
    fn ollama_isolated_with_vram_still_shows_offline() {
        // Even with models loaded, isolated mode always shows offline.
        let mut app = App::new(Config::default(), CostTracker::new());
        app.ollama_mode = clawde_core::OllamaMode::Isolated;
        app.ollama_loaded_models = vec![clawde_core::OllamaLoadedModel {
            name: "llama3.2".to_string(),
            size: Some(2_000_000_000),
            size_vram: Some(2_000_000_000),
            expires_at: None,
            context_length: Some(8192),
        }];
        app.status_message = Some("test".to_string());
        let out = render_screen(&app);
        assert!(
            out.contains("ollama:offline"),
            "'ollama:offline' should appear even with VRAM loaded in isolated mode. Output: {:?}",
            out
        );
        assert!(
            !out.contains("ollama:online"),
            "'ollama:online' must NOT appear in isolated mode. Output: {:?}",
            out
        );
    }
}
