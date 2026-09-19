//! Session browser overlay (/session, /resume, /rename, /export).
//! Mirrors TS session management in REPL.tsx

use std::cell::{Cell, RefCell};

use chrono::TimeZone;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use clawde_core::session_digest::{status_line, DigestFlag};

use crate::overlays::{modal_search_line_with_insert, CLAWDE_MUTED};
use crate::vim_search::VimSearch;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One-key cycling view over the digest flags: "show me only the sessions with
/// uncommitted work", then only the failures, and so on. The flags are already
/// on every entry, so a facet is a pure predicate — no transcript re-reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FacetFilter {
    #[default]
    All,
    Uncommitted,
    Errored,
    Interrupted,
    Committed,
    NoEdits,
}

impl FacetFilter {
    /// The next facet in the cycle.
    pub fn next(self) -> Self {
        match self {
            FacetFilter::All => FacetFilter::Uncommitted,
            FacetFilter::Uncommitted => FacetFilter::Errored,
            FacetFilter::Errored => FacetFilter::Interrupted,
            FacetFilter::Interrupted => FacetFilter::Committed,
            FacetFilter::Committed => FacetFilter::NoEdits,
            FacetFilter::NoEdits => FacetFilter::All,
        }
    }

    /// Short label shown in the search line's facet chip.
    pub fn label(self) -> &'static str {
        match self {
            FacetFilter::All => "all",
            FacetFilter::Uncommitted => "uncommitted",
            FacetFilter::Errored => "errors",
            FacetFilter::Interrupted => "interrupted",
            FacetFilter::Committed => "committed",
            FacetFilter::NoEdits => "no edits",
        }
    }

    /// Whether a session belongs to this facet.
    pub fn matches(self, entry: &SessionEntry) -> bool {
        let has = |flag: DigestFlag| entry.flags.contains(&flag);
        match self {
            FacetFilter::All => true,
            FacetFilter::Uncommitted => has(DigestFlag::Uncommitted),
            FacetFilter::Errored => has(DigestFlag::Errored),
            FacetFilter::Interrupted => has(DigestFlag::Interrupted),
            FacetFilter::Committed => has(DigestFlag::Committed),
            FacetFilter::NoEdits => has(DigestFlag::NoEdits),
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The interaction mode of the session browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBrowserMode {
    /// Default: list sessions, navigate with arrow keys.
    Browse,
    /// User is typing a new name for the selected session.
    Rename,
    /// Waiting for the user to confirm a destructive action (delete / export).
    Confirm,
}

/// A single session entry shown in the browser list.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    /// Concatenated text the browser's full-text search matches against:
    /// title, the digest rows, and the last prompt. Kept out of rendering so
    /// message content does not widen the table — but a word the user can see
    /// in a row must be findable by the filter, so the rows are included.
    pub searchable_text: String,
    /// Modification time as milliseconds since the Unix epoch. The relative
    /// time in the list is derived from this on every render, so a modal left
    /// open does not keep displaying an age measured when the list loaded.
    pub mtime_ms: u64,
    pub message_count: usize,
    /// Estimated USD cost for the session.
    pub cost_usd: f64,
    /// Row 2: the most meaningful words of the session's first user message.
    /// Empty when the transcript could not be read.
    pub opening: String,
    /// Row 3: the words that describe the session's middle work, with the
    /// opening row's terms excluded so it reads as a later phase.
    pub middle: String,
    /// Row 4: status flags inferred from the transcript (interrupted / error /
    /// compacted / commit / uncommitted / no edits). Stored as flags rather
    /// than a joined string so each renders in its own colour; empty when
    /// nothing could be inferred.
    pub flags: Vec<DigestFlag>,
    /// AI-written synopsis of what the session was about. Empty when absent;
    /// shown in the detail popup rather than the list, which the digest rows
    /// occupy.
    pub synopsis_about: String,
    /// AI-written synopsis of where the session left off (last known working
    /// point). Shown in the detail popup only.
    pub synopsis_left_off: String,
    /// Transcript path for the detail popup's tail; empty when unavailable.
    pub transcript_path: std::path::PathBuf,
    /// True for the session currently being run. Marked in the list so it is
    /// never resumed or deleted by accident.
    pub is_current: bool,
}

/// State for the session browser overlay.
pub struct SessionBrowserState {
    pub visible: bool,
    pub selected_idx: usize,
    pub sessions: Vec<SessionEntry>,
    pub mode: SessionBrowserMode,
    /// Input buffer used while in `Rename` mode.
    pub rename_input: String,
    /// Live search/filter query for session titles and message content.
    pub search_query: String,
    /// The actual session ID captured when entering rename mode (to avoid
    /// filtered-index mismatch when confirming the rename).
    rename_session_id: String,
    /// Vim-modal insert-mode state for the search bar (only used when vim is enabled).
    pub vim_search: VimSearch,
    /// Tail preview popup state: (is_user, text) rows for the focused session.
    pub tail_preview: Option<Vec<(bool, String)>>,
    /// Session id the current `tail_preview` was loaded for.
    pub tail_preview_for: String,
    /// Scroll offset (rows from the top) inside the tail preview popup.
    pub tail_scroll: usize,
    /// True while the background tail load for the focused session is in flight.
    pub tail_loading: bool,
    /// True from `open()` until the background list load lands, so the body can
    /// say it is loading instead of claiming there are no sessions.
    pub loading: bool,
    /// Which digest flags the list is restricted to (`^F` cycles it).
    pub facet: FacetFilter,
    /// Filtered index the pointer is over, for the hover highlight.
    pub hover_idx: Option<usize>,
    /// Session id to re-select when the next list arrives, so reopening the
    /// browser returns to the row the user was on instead of always row 0.
    last_selected_id: String,
    /// Session id captured when a destructive action was requested, so the
    /// confirm step acts on the row the user chose even if the filter moves.
    pub confirm_session_id: String,
    /// Screen row of each rendered entry's title, with its filtered index —
    /// rebuilt by every render so a click or hover can resolve the entry under
    /// the cursor. Group headers and the adaptive row height make this
    /// arithmetic non-trivial, so the renderer records it rather than the
    /// hit-test re-deriving it.
    hit_rows: RefCell<Vec<(u16, usize)>>,
    /// Horizontal extent of the list, recorded at render for hit-testing.
    list_span: Cell<(u16, u16)>,
    /// Rows per entry used by the last render, so a click can tell an entry's
    /// own rows from a group header above them.
    rendered_row_height: Cell<u8>,
    /// Entries the last render could show — the page size for PgUp/PgDn.
    page_rows: Cell<usize>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl SessionBrowserState {
    /// Create a new, hidden browser with an empty session list.
    pub fn new() -> Self {
        Self {
            visible: false,
            selected_idx: 0,
            sessions: Vec::new(),
            mode: SessionBrowserMode::Browse,
            rename_input: String::new(),
            search_query: String::new(),
            rename_session_id: String::new(),
            vim_search: VimSearch::new(),
            tail_preview: None,
            tail_preview_for: String::new(),
            tail_scroll: 0,
            tail_loading: false,
            loading: false,
            facet: FacetFilter::All,
            hover_idx: None,
            last_selected_id: String::new(),
            confirm_session_id: String::new(),
            hit_rows: RefCell::new(Vec::new()),
            list_span: Cell::new((0, 0)),
            rendered_row_height: Cell::new(4),
            page_rows: Cell::new(5),
        }
    }

    /// Open the browser.
    ///
    /// The list itself arrives asynchronously through
    /// [`SessionBrowserState::set_sessions`]; until it does, the body shows a
    /// loading line rather than the empty-list message, which used to make a
    /// slow load look like a project with no history.
    pub fn open(&mut self) {
        self.selected_idx = 0;
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        self.search_query.clear();
        self.rename_session_id.clear();
        self.confirm_session_id.clear();
        self.vim_search.reset();
        self.facet = FacetFilter::All;
        self.hover_idx = None;
        self.loading = true;
        self.tail_preview = None;
        self.tail_preview_for.clear();
        self.tail_scroll = 0;
        self.hit_rows.borrow_mut().clear();
        self.visible = true;
    }

    /// Adopt a freshly loaded session list, restoring the previous selection
    /// when that session is still present.
    pub fn set_sessions(&mut self, sessions: Vec<SessionEntry>) {
        self.sessions = sessions;
        self.loading = false;
        self.selected_idx = if self.last_selected_id.is_empty() {
            0
        } else {
            self.sessions
                .iter()
                .position(|s| s.id == self.last_selected_id)
                .unwrap_or(0)
        };
        self.invalidate_tail();
    }

    /// Replace one session's title in place — after a rename or a regenerated
    /// model title — so the list reflects it without a full reload.
    pub fn set_title(&mut self, session_id: &str, title: &str) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.title = title.to_string();
        }
    }

    /// Append a character to the search filter.
    pub fn push_search_char(&mut self, c: char) {
        self.search_query.push(c);
        self.selected_idx = 0;
    }

    /// Remove the last character from the search filter.
    pub fn pop_search_char(&mut self) {
        self.search_query.pop();
        self.selected_idx = 0;
    }

    /// Invalidate the tail preview when the selection moves; the app layer
    /// reloads it for the newly focused session.
    pub fn invalidate_tail(&mut self) {
        self.tail_preview = None;
        self.tail_preview_for.clear();
        self.tail_scroll = 0;
    }

    /// Lowercased query terms.
    ///
    /// Every term must match for a session to be listed, but the terms may hit
    /// different fields and in any order — so `katban commit` finds the session
    /// whose keywords mention katban and whose status row says it landed a
    /// commit. The old filter required the exact phrase to appear verbatim.
    pub fn search_terms(&self) -> Vec<String> {
        self.search_query
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .collect()
    }

    /// Sessions matching the active facet and query, best match first while
    /// searching and in recency order (as loaded) otherwise.
    pub fn filtered_sessions(&self) -> Vec<&SessionEntry> {
        let terms = self.search_terms();
        let mut hits: Vec<(u32, usize, &SessionEntry)> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| self.facet.matches(s))
            .filter_map(|(idx, s)| match_score(s, &terms).map(|score| (score, idx, s)))
            .collect();
        // Stable: with no query every score is equal, so this leaves the
        // loader's recency order untouched.
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|(_, _, s)| s).collect()
    }

    /// Close the browser entirely, remembering the focused session so the next
    /// open lands on it.
    pub fn close(&mut self) {
        self.last_selected_id = self
            .selected_session()
            .map(|s| s.id.clone())
            .unwrap_or_default();
        self.visible = false;
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        self.search_query.clear();
        self.rename_session_id.clear();
        self.confirm_session_id.clear();
        self.vim_search.reset();
        self.facet = FacetFilter::All;
        self.hover_idx = None;
        self.loading = false;
        self.tail_preview = None;
        self.tail_preview_for.clear();
        self.tail_scroll = 0;
        self.tail_loading = false;
        self.hit_rows.borrow_mut().clear();
    }

    /// Move selection up one row, wrapping to the end.
    pub fn select_prev(&mut self) {
        let count = self.filtered_sessions().len();
        if count == 0 {
            self.selected_idx = 0;
            return;
        }
        if self.selected_idx == 0 {
            self.selected_idx = count - 1;
        } else {
            self.selected_idx -= 1;
        }
        self.invalidate_tail();
    }

    /// Move selection down one row, wrapping to the start.
    pub fn select_next(&mut self) {
        let count = self.filtered_sessions().len();
        if count == 0 {
            self.selected_idx = 0;
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % count;
        self.invalidate_tail();
    }

    /// Select the first session in the filtered list.
    pub fn select_first(&mut self) {
        self.selected_idx = 0;
        self.invalidate_tail();
    }

    /// Select the last session in the filtered list.
    pub fn select_last(&mut self) {
        let count = self.filtered_sessions().len();
        self.selected_idx = count.saturating_sub(1);
        self.invalidate_tail();
    }

    /// Move the selection by `delta` entries without wrapping — used by
    /// PgUp/PgDn and the mouse wheel, where wrapping to the far end is
    /// disorienting.
    pub fn scroll_by(&mut self, delta: isize) {
        let count = self.filtered_sessions().len();
        if count == 0 {
            self.selected_idx = 0;
            return;
        }
        let next = self.selected_idx as isize + delta;
        self.selected_idx = next.clamp(0, count as isize - 1) as usize;
        self.invalidate_tail();
    }

    /// Entries the last render could show (before the first render: five).
    pub fn page_rows(&self) -> usize {
        self.page_rows.get().max(1)
    }

    /// Move the selection by one page of the last rendered viewport.
    pub fn page_down(&mut self) {
        let page = self.page_rows() as isize;
        self.scroll_by(page);
    }

    /// Move the selection back one page of the last rendered viewport.
    pub fn page_up(&mut self) {
        let page = self.page_rows() as isize;
        self.scroll_by(-page);
    }

    /// Cycle the facet filter and reset the selection (the old index refers to
    /// a list that is about to be replaced).
    pub fn cycle_facet(&mut self) {
        self.facet = self.facet.next();
        self.selected_idx = 0;
        self.invalidate_tail();
    }

    /// Enter confirm mode for deleting the selected session.
    ///
    /// Refuses the session currently being run: its own transcript is still
    /// being appended to, so deleting it would leave the running session
    /// writing into a deleted file. Returns the id awaiting confirmation.
    pub fn request_delete(&mut self) -> Option<String> {
        let session = self.selected_session()?;
        if session.is_current {
            return None;
        }
        self.confirm_session_id = session.id.clone();
        self.mode = SessionBrowserMode::Confirm;
        Some(self.confirm_session_id.clone())
    }

    /// The session id a pending confirm refers to, if any.
    pub fn pending_confirm_id(&self) -> Option<&str> {
        (!self.confirm_session_id.is_empty()).then_some(self.confirm_session_id.as_str())
    }

    /// Confirm the pending destructive action. Returns the session id to
    /// delete, drops it from the local list and returns to browse mode.
    pub fn confirm_delete(&mut self) -> Option<String> {
        if self.mode != SessionBrowserMode::Confirm || self.confirm_session_id.is_empty() {
            return None;
        }
        let id = std::mem::take(&mut self.confirm_session_id);
        self.sessions.retain(|s| s.id != id);
        self.mode = SessionBrowserMode::Browse;
        let count = self.filtered_sessions().len();
        if self.selected_idx >= count {
            self.selected_idx = count.saturating_sub(1);
        }
        self.invalidate_tail();
        Some(id)
    }

    /// Record the layout of a render: the screen row of each entry's title and
    /// its filtered index, the list's horizontal extent, the entry height and
    /// how many entries were shown.
    pub fn record_layout(
        &self,
        rows: Vec<(u16, usize)>,
        span: (u16, u16),
        row_height: usize,
        page: usize,
    ) {
        *self.hit_rows.borrow_mut() = rows;
        self.list_span.set(span);
        self.rendered_row_height.set(row_height as u8);
        self.page_rows.set(page.max(1));
    }

    /// The filtered index of the entry under a screen position, if any.
    ///
    /// Requires a render to have happened since the list last changed, which
    /// is always the case before a mouse event can reach the overlay.
    pub fn hit_test(&self, col: u16, row: u16) -> Option<usize> {
        let (left, right) = self.list_span.get();
        if right <= left || col < left || col >= right {
            return None;
        }
        let height = self.rendered_row_height.get().max(1) as u16;
        self.hit_rows
            .borrow()
            .iter()
            .rev()
            .find(|(y, _)| row >= *y && row < y.saturating_add(height))
            .map(|(_, idx)| *idx)
    }

    /// Return a reference to the currently selected session, if any.
    /// Respects the active search filter — uses `filtered_sessions()` so
    /// `selected_idx` always maps to the correct session.
    pub fn selected_session(&self) -> Option<&SessionEntry> {
        self.filtered_sessions().get(self.selected_idx).copied()
    }

    /// Switch to rename mode, pre-populating the input with the current title.
    /// Captures the session ID so `confirm_rename()` can find the right session
    /// even when a search filter is active (filtered-index mismatch guard).
    pub fn start_rename(&mut self) {
        if let Some(session) = self.filtered_sessions().get(self.selected_idx).copied() {
            let title = session.title.clone();
            let sid = session.id.clone();
            self.rename_input = title;
            self.rename_session_id = sid;
            self.mode = SessionBrowserMode::Rename;
        }
    }

    /// Append a character to the rename input buffer.
    pub fn push_rename_char(&mut self, c: char) {
        if self.mode == SessionBrowserMode::Rename {
            self.rename_input.push(c);
        }
    }

    /// Remove the last character from the rename input buffer.
    pub fn pop_rename_char(&mut self) {
        if self.mode == SessionBrowserMode::Rename {
            self.rename_input.pop();
        }
    }

    /// Confirm the rename. Returns `(session_id, new_name)` when in rename mode
    /// with a non-empty name and a valid selection. Resets to browse mode.
    /// Uses the captured `rename_session_id` to find the right session even
    /// when a search filter was active when renaming started.
    pub fn confirm_rename(&mut self) -> Option<(String, String)> {
        if self.mode != SessionBrowserMode::Rename {
            return None;
        }
        let new_name = self.rename_input.trim().to_string();
        if new_name.is_empty() || self.rename_session_id.is_empty() {
            return None;
        }
        let session_id = self.rename_session_id.clone();
        // Apply the rename in the local list immediately for UI consistency.
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.title = new_name.clone();
        }
        self.mode = SessionBrowserMode::Browse;
        self.rename_input.clear();
        self.rename_session_id.clear();
        Some((session_id, new_name))
    }

    /// Cancel the current mode:
    /// - In `Rename` or `Confirm` mode: return to `Browse`.
    /// - In `Browse` mode: close the overlay.
    pub fn cancel(&mut self) {
        match self.mode {
            SessionBrowserMode::Browse => self.close(),
            SessionBrowserMode::Rename | SessionBrowserMode::Confirm => {
                self.mode = SessionBrowserMode::Browse;
                self.rename_input.clear();
                self.rename_session_id.clear();
                self.confirm_session_id.clear();
            }
        }
    }
}

impl Default for SessionBrowserState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Format a cost as a dollar string with 4 decimal places. Free sessions
/// price at $0.00, so zero renders as an empty cell instead of "$0.0000".
fn fmt_cost(usd: f64) -> String {
    if usd < 0.0001 {
        String::new()
    } else {
        format!("${:.4}", usd)
    }
}

/// Truncate `s` to fit within `max_width` display columns, appending `…` if cut.
fn truncate_display(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.width() + ch.len_utf8() + 1 > max_width {
            break;
        }
        out.push(ch);
    }
    format!("{}…", out)
}

/// Milliseconds since the Unix epoch, so the list can show live relative times
/// instead of an age frozen at load.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Rank a session against the lowercased query terms.
///
/// Every term must appear somewhere (AND), and each contributes the weight of
/// the strongest field it matched, so a title hit outranks a transcript hit and
/// the list reads best-match-first. `None` means the session does not match.
fn match_score(entry: &SessionEntry, terms: &[String]) -> Option<u32> {
    if terms.is_empty() {
        return Some(0);
    }
    let title = entry.title.to_lowercase();
    let status = status_line(&entry.flags).to_lowercase();
    let keywords = format!("{} {}", entry.opening, entry.middle).to_lowercase();
    let content = entry.searchable_text.to_lowercase();
    let mut total = 0u32;
    for term in terms {
        total += if title.contains(term.as_str()) {
            0
        } else if status.contains(term.as_str()) {
            1
        } else if keywords.contains(term.as_str()) {
            2
        } else if content.contains(term.as_str()) {
            3
        } else {
            return None;
        };
    }
    Some(total)
}

/// Coarse recency bucket for the list's group headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayBucket {
    Today,
    Yesterday,
    ThisWeek,
    Earlier,
}

impl DayBucket {
    /// Bucket for a session modified at `mtime_ms`, relative to `now_ms`.
    /// Calendar-day based, so "yesterday" means the previous local day rather
    /// than a rolling 24-48 hour window.
    pub fn of(mtime_ms: u64, now_ms: u64) -> Self {
        let day = |ms: u64| {
            chrono::Local
                .timestamp_millis_opt(ms as i64)
                .single()
                .map(|dt| dt.date_naive())
        };
        let (Some(then), Some(today)) = (day(mtime_ms), day(now_ms)) else {
            return DayBucket::Earlier;
        };
        let age = (today - then).num_days();
        if age <= 0 {
            DayBucket::Today
        } else if age == 1 {
            DayBucket::Yesterday
        } else if age < 7 {
            DayBucket::ThisWeek
        } else {
            DayBucket::Earlier
        }
    }

    /// Group header text.
    pub fn label(self) -> &'static str {
        match self {
            DayBucket::Today => "Today",
            DayBucket::Yesterday => "Yesterday",
            DayBucket::ThisWeek => "This week",
            DayBucket::Earlier => "Earlier",
        }
    }
}

/// Entry height for a list with `list_rows` lines available.
///
/// Four rows (title, opening keywords, middle keywords, status) whenever the
/// list can show whole entries that way; two rows (title, status) when it
/// cannot, so a tiny terminal still lists something.
///
/// `popup_shown` matters because the detail popup is the fallback home for the
/// keyword rows: with the popup on screen the list may compact down to four
/// visible entries, but with no popup, compacting would hide the keywords
/// entirely — so the full layout is kept until it shows fewer than two entries.
fn rows_per_entry_for(list_rows: usize, popup_shown: bool) -> usize {
    const FULL: usize = 4;
    const COMPACT: usize = 2;
    let full_needs = if popup_shown { FULL * 4 } else { FULL * 2 };
    if list_rows >= full_needs {
        FULL
    } else {
        COMPACT
    }
}

/// First visible line for a viewport over `total_lines` laid out as blocks: the
/// least scrolling that keeps the block `[sel_start, sel_end)` fully on screen,
/// never scrolling past that block's own top.
fn viewport_line_offset(
    total_lines: usize,
    viewport: usize,
    sel_start: usize,
    sel_end: usize,
) -> usize {
    if viewport == 0 || total_lines <= viewport {
        return 0;
    }
    let max_offset = total_lines - viewport;
    if sel_end <= viewport {
        return 0;
    }
    sel_end
        .saturating_sub(viewport)
        .min(sel_start)
        .min(max_offset)
}

/// Byte ranges of every occurrence of any of `terms` in `text`, merged and
/// ascending. Case-insensitive, and empty when lowercasing does not preserve
/// byte offsets (in which case slicing could cut a multi-byte character).
fn match_ranges(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let lowered = text.to_lowercase();
    if terms.is_empty() || lowered.len() != text.len() {
        return Vec::new();
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        if term.is_empty() {
            continue;
        }
        for (start, _) in lowered.match_indices(term.as_str()) {
            ranges.push((start, start + term.len()));
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Split `text` into spans, styling every query match so a filtered row shows
/// why it matched.
fn highlight_spans(text: &str, base: Style, hit: Style, terms: &[String]) -> Vec<Span<'static>> {
    let ranges = match_ranges(text, terms);
    if ranges.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in ranges {
        if start > cursor {
            spans.push(Span::styled(text[cursor..start].to_string(), base));
        }
        spans.push(Span::styled(text[start..end].to_string(), hit));
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), base));
    }
    spans
}

/// Width of the browser modal. The list is a table, so the width stays fixed
/// and readable rather than stretching to an ultrawide terminal.
const MODAL_W: u16 = 78;
/// Ceiling for the modal width. The keyword rows truncate at 78 columns, so a
/// wide terminal gets a wider table — but only up to a point, past which the
/// rows are long enough to be hard to scan.
const MODAL_MAX_W: u16 = 120;
/// Floor for the modal height: the size the browser shipped with, kept as the
/// minimum so a short terminal behaves exactly as it always has.
const MODAL_MIN_H: u16 = 26;
/// Ceiling for the modal height, so a very tall terminal gets a generous list
/// instead of a wall of rows.
const MODAL_MAX_H: u16 = 46;
/// Height the detail popup needs below the modal.
const DETAIL_POPUP_H: u16 = 14;

/// Geometry of the browser modal.
///
/// The list is what the user reads, so on a tall terminal the modal grows into
/// the rows that were previously left empty, up to [`MODAL_MAX_H`]. Growth only
/// happens when the extra rows are genuinely spare: the modal reserves room for
/// the detail popup beneath it, and the modal and popup are centred as one
/// block so the popup sits under the modal instead of being clamped up over the
/// bottom of the list.
fn modal_rect_for(area: Rect) -> Rect {
    let width = area
        .width
        .saturating_sub(4)
        .clamp(MODAL_W, MODAL_MAX_W)
        .min(area.width.saturating_sub(2));
    let room = area.height.saturating_sub(2);
    let height = if room <= MODAL_MIN_H + DETAIL_POPUP_H {
        MODAL_MIN_H.min(room).max(6)
    } else {
        room.saturating_sub(DETAIL_POPUP_H)
            .clamp(MODAL_MIN_H, MODAL_MAX_H)
    };
    let stack = height.saturating_add(DETAIL_POPUP_H);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(stack) / 2,
        width,
        height,
    )
}

/// One hint-bar row: each `key` paired with its label, dropped from the tail
/// when the modal is too narrow to hold them all.
fn hint_row(pairs: &[(&str, &str)], width: usize) -> Line<'static> {
    let mut spans = vec![Span::styled("  ", Style::default())];
    let mut used = 2usize;
    for (key, label) in pairs {
        let w = key.chars().count() + label.chars().count();
        if used + w > width {
            break;
        }
        used += w;
        spans.push(Span::styled(
            key.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// One line summarising the visible list: how many sessions, and how many of
/// them carry each meaningful signal, so the state of the project reads at a
/// glance rather than after scrolling.
fn attention_line(filtered: &[&SessionEntry]) -> Line<'static> {
    let count = |flag: DigestFlag| filtered.iter().filter(|s| s.flags.contains(&flag)).count();
    let mut spans = vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            format!(
                "{} session{}",
                filtered.len(),
                if filtered.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::Rgb(150, 170, 190)),
        ),
    ];
    for (flag, label) in [
        (DigestFlag::Uncommitted, "uncommitted"),
        (DigestFlag::Errored, "errored"),
        (DigestFlag::Interrupted, "interrupted"),
        (DigestFlag::Committed, "committed"),
    ] {
        let n = count(flag);
        if n == 0 {
            continue;
        }
        spans.push(Span::styled(
            "  \u{b7}  ",
            Style::default().fg(Color::Rgb(90, 90, 110)),
        ));
        spans.push(Span::styled(
            format!("{n} {label}"),
            flag_style(flag, Color::Reset),
        ));
    }
    Line::from(spans)
}

/// One recency group header line for the list.
fn group_header_line(label: &str, inner_w: usize) -> Line<'static> {
    let rule = "\u{2500}".repeat(inner_w.saturating_sub(label.chars().count() + 5));
    Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(Color::Rgb(110, 145, 175))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(rule, Style::default().fg(Color::Rgb(60, 70, 85))),
    ])
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the session browser overlay directly into `buf`.
///
/// Draws a centred modal (≈78 wide × ≈26 tall) with:
/// - A scrollable list of sessions, four rows each: the title row
///   (title / last updated / msgs / cost), the opening keywords from the
///   session's first message, the keywords that describe its middle work, and
///   an inferred status row
/// - Selection highlight on the focused entry
/// - Mode-sensitive hint bar at the bottom
/// - A rename input field shown when in `Rename` mode
///
/// A second popup under the browser carries the text that does not fit on a
/// list row (the full keyword rows, the AI synopsis when one was written) and
/// the last messages of the focused session.
pub fn render_session_browser(state: &SessionBrowserState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    let dialog_area = modal_rect_for(area);

    // --- Clear background -------------------------------------------------
    for y in dialog_area.y..dialog_area.y + dialog_area.height {
        for x in dialog_area.x..dialog_area.x + dialog_area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }

    let inner_w = dialog_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    // --- Search line (always shown in Browse mode) -------------------------
    if state.mode == SessionBrowserMode::Browse {
        let mut search_line = modal_search_line_with_insert(
            &state.search_query,
            "Type to filter sessions...",
            CLAWDE_MUTED,
            Color::Cyan,
            state.vim_search.insert,
        );
        // Facet chip, right-aligned in the search row: the active facet is a
        // mode, so it belongs next to the query rather than hidden in help.
        let chip = format!("[^F {}] ", state.facet.label());
        let used: usize = search_line
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        search_line.spans.push(Span::styled(
            " ".repeat(inner_w.saturating_sub(used + chip.chars().count()).max(1)),
            Style::default(),
        ));
        search_line.spans.push(Span::styled(
            chip,
            if state.facet == FacetFilter::All {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
                    .fg(Color::Rgb(220, 180, 120))
                    .add_modifier(Modifier::BOLD)
            },
        ));
        lines.push(search_line);
        lines.push(Line::from(""));
    }

    // --- Session list -----------------------------------------------------
    let filtered = state.filtered_sessions();
    let terms = state.search_terms();
    let now = now_ms();

    if filtered.is_empty() {
        // Distinguish "still loading" from "nothing here": a slow list used to
        // render the empty-list message, which reads as lost history.
        let text = if state.loading {
            "  Loading sessions..."
        } else if state.facet != FacetFilter::All {
            "  No sessions with that status."
        } else if state.search_query.is_empty() {
            "  No sessions found."
        } else {
            "  No sessions match your search."
        };
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        )));
        state.record_layout(Vec::new(), (0, 0), 4, 5);
    } else {
        // Column widths (approximate):
        //   title: ~40 chars  |  date: ~14 chars  |  msgs: 5  |  cost: 9
        let date_w: usize = 14;
        // Drop the Msgs column when no visible session has a nonzero count
        // (e.g. project-scoped JSONL store doesn't track costs).
        let any_msgs = filtered.iter().any(|s| s.message_count > 0);
        let msgs_w: usize = if any_msgs { 5 } else { 0 };
        let msgs_header = if any_msgs { "Msgs" } else { "" };
        // Drop the Cost column when no visible session has a nonzero cost.
        let any_cost = filtered.iter().any(|s| s.cost_usd >= 0.0001);
        let cost_w: usize = if any_cost { 9 } else { 0 };
        let cost_header = if any_cost { "Cost" } else { "" };
        // Each row is `  ` + title + `  ` + date + `  ` + msgs + `  ` + cost,
        // i.e. eight columns of separators the title must leave room for. A
        // smaller constant here made every title row two columns too wide, so
        // it wrapped and left a blank line inside each entry.
        let fixed = date_w + msgs_w + cost_w + 8;
        let title_w = inner_w.saturating_sub(fixed).max(10);

        // Header row
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  {:<title_w$}  {:<date_w$}  {:>msgs_w$}  {:>cost_w$}",
                "Title",
                "Last Updated",
                msgs_header,
                cost_header,
                title_w = title_w,
                date_w = date_w,
                msgs_w = msgs_w,
                cost_w = cost_w
            ),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        )]));

        // Attention summary: what the visible list contains, so the state of
        // the project (uncommitted work, failures) reads without scrolling.
        lines.push(attention_line(&filtered));

        // List geometry. The render may use whatever the modal body has left
        // after the lines already pushed and the hint bar reserved at the
        // bottom; the entry height then adapts to that budget.
        let hint_rows = if state.mode == SessionBrowserMode::Browse {
            3
        } else {
            2
        }; // spacer + hint line(s)
        let body_rows = dialog_area.height.saturating_sub(2) as usize; // minus borders
        let list_rows = body_rows
            .saturating_sub(lines.len())
            .saturating_sub(hint_rows)
            .max(1);
        // The popup renders only when the screen has real rows left under the
        // modal (see `render_detail_preview`); when it does not, the list is
        // the only place the keywords can appear.
        let popup_shown = area.height.saturating_sub(dialog_area.bottom()) >= 4;
        let row_h = rows_per_entry_for(list_rows, popup_shown);
        let row_w = inner_w.saturating_sub(6);
        // Group by recency only when browsing a long list: with a query the
        // ranking order is the useful structure, and headers cost rows.
        let group_by_day = terms.is_empty() && filtered.len() >= 8;

        // Blocks: an optional group header plus the entry's own rows. The
        // window is computed in lines, so a header can never push the list past
        // the modal and the selected block is always fully visible.
        let mut blocks: Vec<(Option<&'static str>, usize)> = Vec::with_capacity(filtered.len());
        let mut prev_bucket: Option<DayBucket> = None;
        for session in &filtered {
            let header = if group_by_day {
                let bucket = DayBucket::of(session.mtime_ms, now);
                let is_new = prev_bucket != Some(bucket);
                prev_bucket = Some(bucket);
                is_new.then(|| bucket.label())
            } else {
                None
            };
            blocks.push((header, row_h + usize::from(header.is_some())));
        }
        let total_lines: usize = blocks.iter().map(|(_, h)| *h).sum();
        let sel_start: usize = blocks
            .iter()
            .take(state.selected_idx.min(blocks.len()))
            .map(|(_, h)| *h)
            .sum();
        let sel_height = blocks
            .get(state.selected_idx)
            .map(|(_, h)| *h)
            .unwrap_or(row_h);
        // Reserve a line for the overflow hint when the list does not fit.
        let mut viewport = list_rows;
        if total_lines > viewport {
            viewport = viewport.saturating_sub(1).max(1);
        }
        let offset = viewport_line_offset(total_lines, viewport, sel_start, sel_start + sel_height);

        let mut consumed = 0usize; // lines above the window
        let mut drawn = 0usize; // lines emitted inside it
        let mut entries_above = 0usize;
        let mut entries_below = 0usize;
        let mut hit_rows: Vec<(u16, usize)> = Vec::new();

        for (i, (header, height)) in blocks.iter().enumerate() {
            let session = filtered[i];
            // Whole or partial block above the window: skip it, so the first
            // drawn block always starts at the window's top line.
            if consumed < offset {
                consumed += *height;
                entries_above += 1;
                continue;
            }
            // No room left for this block: everything from here is below.
            if drawn + *height > viewport {
                entries_below = filtered.len() - i;
                break;
            }
            if let Some(label) = header {
                lines.push(group_header_line(label, inner_w));
                drawn += 1;
            }

            let is_selected = i == state.selected_idx;
            let is_hovered = state.hover_idx == Some(i);
            let row_bg = if is_selected {
                Color::Rgb(40, 60, 80)
            } else if is_hovered {
                Color::Rgb(30, 42, 56)
            } else {
                // transparent — ratatui uses reset/default for "no background"
                Color::Reset
            };
            let title_style = if is_selected {
                Style::default()
                    .fg(Color::Cyan)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White).bg(row_bg)
            };
            // Query hits read as a highlight inside the row so a filtered list
            // shows *why* each entry matched.
            let title_hit = Style::default()
                .fg(Color::Rgb(255, 220, 130))
                .bg(row_bg)
                .add_modifier(Modifier::BOLD);
            let keyword_style = if is_selected {
                Style::default().fg(Color::Rgb(140, 175, 200)).bg(row_bg)
            } else {
                Style::default().fg(Color::Rgb(120, 120, 130)).bg(row_bg)
            };
            let keyword_hit = Style::default().fg(Color::Rgb(225, 200, 135)).bg(row_bg);
            let meta_style = if is_selected {
                Style::default().fg(Color::Rgb(180, 200, 220)).bg(row_bg)
            } else {
                Style::default().fg(Color::DarkGray).bg(row_bg)
            };
            let prefix_style = Style::default().bg(row_bg);

            // Title row. The age is derived at render, so it stays true for a
            // modal left open; the current session is marked and reads "now".
            let title_cell = truncate_display(&session.title, title_w);
            let title_pad = title_w.saturating_sub(title_cell.width());
            let date_text = if session.is_current {
                "now".to_string()
            } else {
                clawde_core::format_utils::format_relative_time(session.mtime_ms)
            };
            let date_cell = truncate_display(&date_text, date_w);
            let msgs_cell = if msgs_w == 0 {
                String::new()
            } else {
                format!("{:>msgs_w$}", session.message_count, msgs_w = msgs_w)
            };
            let cost_cell = format!("{:>cost_w$}", fmt_cost(session.cost_usd), cost_w = cost_w);

            hit_rows.push((dialog_area.y + 1 + lines.len() as u16, i));

            let mut title_spans = vec![Span::styled(
                if session.is_current {
                    "\u{25cf} "
                } else {
                    "  "
                },
                if session.is_current {
                    Style::default()
                        .fg(Color::Rgb(120, 200, 150))
                        .bg(row_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    prefix_style
                },
            )];
            title_spans.extend(highlight_spans(&title_cell, title_style, title_hit, &terms));
            title_spans.push(Span::styled(" ".repeat(title_pad), title_style));
            title_spans.push(Span::styled("  ", meta_style));
            title_spans.push(Span::styled(
                format!("{:<date_w$}", date_cell, date_w = date_w),
                meta_style,
            ));
            title_spans.push(Span::styled("  ", meta_style));
            title_spans.push(Span::styled(msgs_cell, meta_style));
            title_spans.push(Span::styled("  ", meta_style));
            title_spans.push(Span::styled(cost_cell, meta_style));
            lines.push(Line::from(title_spans));
            drawn += 1;

            // Rows 2-3: the opening keywords and the keywords describing the
            // middle work. In the compact layout (short terminal) these move to
            // the detail popup, which already carries them untruncated, so the
            // list keeps its fixed height per entry either way.
            if row_h >= 4 {
                let opening = if session.opening.is_empty() {
                    "\u{2026}"
                } else {
                    session.opening.as_str()
                };
                let middle = if session.middle.is_empty() {
                    "\u{2026}"
                } else {
                    session.middle.as_str()
                };
                let opening_cell = truncate_display(opening, row_w);
                let middle_cell = truncate_display(middle, row_w);
                let mut opening_spans = vec![Span::styled("    \u{276f} ", keyword_style)];
                opening_spans.extend(highlight_spans(
                    &opening_cell,
                    keyword_style,
                    keyword_hit,
                    &terms,
                ));
                lines.push(Line::from(opening_spans));
                let mut middle_spans = vec![Span::styled("    \u{b7} ", keyword_style)];
                middle_spans.extend(highlight_spans(
                    &middle_cell,
                    keyword_style,
                    keyword_hit,
                    &terms,
                ));
                lines.push(Line::from(middle_spans));
                drawn += 2;
            }

            // Status row: one coloured span per flag, so a failure reads
            // differently from a commit at a glance.
            let mut status_spans = vec![Span::styled("    ", prefix_style)];
            if session.flags.is_empty() {
                status_spans.push(Span::styled(
                    "\u{2014}",
                    Style::default().fg(Color::Rgb(120, 120, 130)).bg(row_bg),
                ));
            } else {
                let mut used = 0usize;
                for (f, flag) in session.flags.iter().enumerate() {
                    let text = if f == 0 {
                        flag.shorthand().to_string()
                    } else {
                        format!("  {}", flag.shorthand())
                    };
                    if used + text.chars().count() > row_w + 2 {
                        break;
                    }
                    used += text.chars().count();
                    status_spans.push(Span::styled(text, flag_style(*flag, row_bg)));
                }
            }
            lines.push(Line::from(status_spans));
            drawn += 1;
            consumed += *height;
        }

        // Overflow hint when the window hides entries.
        if entries_above > 0 || entries_below > 0 {
            let mut hint = String::new();
            if entries_above > 0 {
                hint.push_str(&format!("\u{2191} {} above", entries_above));
            }
            if entries_below > 0 {
                if !hint.is_empty() {
                    hint.push_str("  ");
                }
                hint.push_str(&format!("\u{2193} {} below", entries_below));
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", hint),
                Style::default().fg(Color::Rgb(90, 90, 110)),
            )));
        }

        // Hand the mouse layer the geometry it needs: which screen row holds
        // which entry, and where the list sits horizontally.
        state.record_layout(
            hit_rows,
            (
                dialog_area.x + 1,
                dialog_area.x + 1 + inner_w.min(u16::MAX as usize) as u16,
            ),
            row_h,
            (viewport / row_h).max(1),
        );
    }

    lines.push(Line::from(""));

    // --- Mode-sensitive bottom section -----------------------------------
    match &state.mode {
        SessionBrowserMode::Browse => {
            // Two rows, so every working key is discoverable; each row drops its
            // tail rather than wrapping when the modal is narrow.
            lines.push(hint_row(
                &[
                    ("Type", " filter   "),
                    ("\u{2191}\u{2193}", " move   "),
                    ("Enter", " resume   "),
                    ("^F", " facet   "),
                    ("^D", " delete   "),
                    ("Esc", " close"),
                ],
                inner_w,
            ));
            lines.push(hint_row(
                &[
                    ("Home/End", " first/last   "),
                    ("PgUp/PgDn", " page   "),
                    ("^E", " export   "),
                    ("^B", " fork   "),
                    ("^T", " title   "),
                    ("^R", " rename   "),
                    ("F5", " reload"),
                ],
                inner_w,
            ));
        }
        SessionBrowserMode::Rename => {
            // Show rename input field.
            let label = "  Rename: ";
            let cursor = "\u{2588}"; // block cursor
            let input_display = format!("{}{}", state.rename_input, cursor);
            lines.push(Line::from(vec![
                Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    input_display,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=confirm  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=cancel", Style::default().fg(Color::DarkGray)),
            ]));
        }
        SessionBrowserMode::Confirm => {
            // Name the row the confirm refers to: the selection can move
            // between the request and the answer, so the prompt has to say what
            // is about to be deleted.
            let what = state
                .pending_confirm_id()
                .and_then(|id| state.sessions.iter().find(|s| s.id == id))
                .map(|s| truncate_display(&s.title, 40))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  Delete \"{what}\" — confirm? "),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("=yes  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled("=no", Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Sessions ")
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false });

    use ratatui::widgets::Widget;
    para.render(dialog_area, buf);

    render_detail_preview(state, dialog_area, buf);
}

/// Colour for one status flag: failures and interruptions read hot, a landed
/// commit reads cool, and the weaker inferences stay muted. All of them hold
/// contrast on both the plain and the selected row background.
fn flag_style(flag: DigestFlag, row_bg: Color) -> Style {
    let colour = match flag {
        DigestFlag::Errored => Color::Rgb(225, 110, 110),
        DigestFlag::Interrupted => Color::Rgb(215, 155, 95),
        DigestFlag::Committed => Color::Rgb(130, 200, 140),
        DigestFlag::Uncommitted => Color::Rgb(210, 190, 130),
        DigestFlag::Compacted => Color::Rgb(170, 160, 200),
        DigestFlag::NoEdits => Color::Rgb(120, 120, 130),
    };
    Style::default().fg(colour).bg(row_bg)
}

/// Render one single-row paragraph into `buf` (Widget trait import lives at
/// the call site in `render_session_browser`).
fn render_tail_row(line: Line<'_>, area: Rect, buf: &mut Buffer) {
    use ratatui::widgets::Widget;
    Paragraph::new(line).render(area, buf);
}

/// Render the detail preview: a second popup under the browser carrying what a
/// single list row cannot hold — the untruncated keyword rows, the status
/// flags, the AI synopsis when one was written — plus the last messages of the
/// focused session, so a past session can be judged without resuming it.
fn render_detail_preview(state: &SessionBrowserState, browser_area: Rect, buf: &mut Buffer) {
    let Some(selected) = state.selected_session() else {
        return;
    };

    const DETAIL_H: u16 = DETAIL_POPUP_H;
    /// Rows the summary block may occupy above the divider, before wrapping.
    const SUMMARY_ROWS: u16 = 5;

    // Anchor directly under the browser modal, taking only the rows the screen
    // has left below it. This used to borrow rows from the modal (clamping the
    // popup up into the screen), which left it sitting over the bottom of the
    // list — including the hint bar. With fewer than four rows there is nothing
    // useful to show, so the list keeps the space instead.
    let below = buf.area.height.saturating_sub(browser_area.bottom());
    // Match the modal's width exactly, so the popup lines up with the list it
    // belongs to at every terminal size.
    let width = browser_area.width;
    let height = DETAIL_H.min(below);
    if width < 10 || height < 4 {
        return;
    }
    let tail_area = Rect::new(browser_area.x, browser_area.bottom(), width, height);

    // Clear + border.
    for iy in tail_area.y..tail_area.bottom() {
        for ix in tail_area.x..tail_area.right() {
            if let Some(cell) = buf.cell_mut((ix, iy)) {
                cell.reset();
            }
        }
    }
    let border_style = Style::default().fg(Color::Rgb(70, 110, 140));
    for iy in tail_area.y..tail_area.bottom() {
        for ix in tail_area.x..tail_area.right() {
            let is_top = iy == tail_area.y;
            let is_bot = iy == tail_area.bottom() - 1;
            let is_left = ix == tail_area.x;
            let is_right = ix == tail_area.right() - 1;
            if let Some(cell) = buf.cell_mut((ix, iy)) {
                let ch = match (is_top, is_bot, is_left, is_right) {
                    (true, _, true, _) => '╭',
                    (true, _, _, true) => '╮',
                    (_, true, true, _) => '╰',
                    (_, true, _, true) => '╯',
                    (true, _, _, _) | (_, true, _, _) => '─',
                    (_, _, true, _) | (_, _, _, true) => '│',
                    _ => continue,
                };
                cell.set_char(ch);
                cell.set_style(border_style);
            }
        }
    }

    // Title: session label, marked when it is the session being run.
    let title = format!(
        " {}{} ",
        if selected.is_current { "\u{25cf} " } else { "" },
        truncate_display(&selected.title, width.saturating_sub(10) as usize)
    );
    for (i, ch) in title.chars().enumerate() {
        if let Some(cell) = buf.cell_mut((tail_area.x + 2 + i as u16, tail_area.y)) {
            cell.set_char(ch);
            cell.set_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        }
    }

    let inner = Rect::new(
        tail_area.x + 2,
        tail_area.y + 1,
        width.saturating_sub(4),
        height.saturating_sub(2),
    );

    // --- Summary block: the text a list row has to truncate --------------
    // The list rows are width-limited, so the untruncated keywords, the AI
    // synopsis (when one was written) and the full status line live here.
    let labelled = |name: &str, body: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!(" {name:<9}"),
                Style::default().fg(Color::Rgb(110, 145, 175)),
            ),
            Span::styled(
                body.to_string(),
                Style::default().fg(Color::Rgb(180, 195, 210)),
            ),
        ])
    };
    let about = if selected.synopsis_about.is_empty() {
        selected.opening.as_str()
    } else {
        selected.synopsis_about.as_str()
    };
    let mut summary: Vec<Line> = vec![labelled("about", about), labelled("work", &selected.middle)];
    if !selected.synopsis_left_off.is_empty() {
        summary.push(labelled("left off", &selected.synopsis_left_off));
    }
    let status = status_line(&selected.flags);
    summary.push(labelled(
        "status",
        if status.is_empty() {
            "no signals inferred"
        } else {
            &status
        },
    ));

    // Place the divider directly under the text (wrapped lines included, and
    // capped) so a session with only a few summary lines does not leave a gap
    // above it. The floor of one row keeps `clamp` well-ordered on a popup too
    // small for the block.
    let max_summary_rows = SUMMARY_ROWS.min(inner.height.saturating_sub(3)).max(1);
    let summary_rows = summary
        .iter()
        .map(|line| {
            let width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            width.max(1).div_ceil(inner.width.max(1) as usize) as u16
        })
        .sum::<u16>()
        .clamp(1, max_summary_rows);

    use ratatui::widgets::Widget;
    Paragraph::new(summary)
        .wrap(Wrap { trim: false })
        .render(Rect::new(inner.x, inner.y, inner.width, summary_rows), buf);

    // Divider, then the transcript tail in the rows that remain.
    let divider_y = inner.y + summary_rows;
    if divider_y < inner.bottom() {
        render_tail_row(
            Line::from(Span::styled(
                "\u{2500}".repeat(inner.width as usize),
                Style::default().fg(Color::Rgb(70, 110, 140)),
            )),
            Rect::new(inner.x, divider_y, inner.width, 1),
            buf,
        );
    }
    let tail_y = divider_y + 1;
    let tail_h = inner.bottom().saturating_sub(tail_y);
    if tail_h == 0 {
        return;
    }

    let Some(rows) = state.tail_preview.as_ref() else {
        let note = if state.tail_loading {
            " last messages: loading…"
        } else {
            " last messages: (no transcript preview)"
        };
        render_tail_row(
            Line::from(Span::styled(note, Style::default().fg(Color::DarkGray))),
            Rect::new(inner.x, tail_y, inner.width, 1),
            buf,
        );
        return;
    };

    if rows.is_empty() {
        render_tail_row(
            Line::from(Span::styled(
                " last messages: (none found)",
                Style::default().fg(Color::DarkGray),
            )),
            Rect::new(inner.x, tail_y, inner.width, 1),
            buf,
        );
        return;
    }

    // Scroll clamp: content rows minus viewport rows.
    let max_scroll = rows.len().saturating_sub(tail_h as usize);
    let scroll = state.tail_scroll.min(max_scroll);

    // Status right-aligned in the title row: row position.
    let pos = format!("{}/{} ", (scroll + 1).min(rows.len()), rows.len());
    let pos_x = tail_area.right().saturating_sub(pos.len() as u16 + 2);
    for (i, ch) in pos.chars().enumerate() {
        if let Some(cell) = buf.cell_mut((pos_x + i as u16, tail_area.y)) {
            cell.set_char(ch);
            cell.set_style(Style::default().fg(Color::DarkGray));
        }
    }

    for (i, (is_user, text)) in rows.iter().skip(scroll).take(tail_h as usize).enumerate() {
        let prefix = if *is_user { "❯ " } else { "  " };
        let style = if *is_user {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Rgb(170, 170, 180))
        };
        let display = truncate_display(text, inner.width.saturating_sub(3) as usize);
        let line = Line::from(Span::styled(format!("{}{}", prefix, display), style));
        render_tail_row(
            line,
            Rect::new(inner.x, tail_y + i as u16, inner.width, 1),
            buf,
        );
    }
}
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// One hour in milliseconds, for building deterministic ages.
    const HOUR_MS: u64 = 3_600_000;

    /// A timestamp inside the current local day, so a test that needs a
    /// "today" entry cannot drift across midnight into "yesterday".
    fn start_of_today_ms() -> u64 {
        let today = chrono::Local::now().date_naive();
        let dt = chrono::Local
            .from_local_datetime(&today.and_hms_opt(0, 30, 0).expect("00:30 exists"))
            .single()
            .expect("00:30 is unambiguous");
        dt.timestamp_millis() as u64
    }

    /// A session entry with every optional field empty, for tests that only
    /// care about one property.
    fn plain_session(i: usize) -> SessionEntry {
        SessionEntry {
            id: format!("s{i:03}"),
            title: format!("session {i}"),
            searchable_text: String::new(),
            mtime_ms: now_ms().saturating_sub(HOUR_MS),
            message_count: 1,
            cost_usd: 0.0,
            opening: String::new(),
            middle: String::new(),
            flags: Vec::new(),
            synopsis_about: String::new(),
            synopsis_left_off: String::new(),
            transcript_path: std::path::PathBuf::new(),
            is_current: false,
        }
    }

    /// An entry whose title and id are `id`, carrying `flags`.
    fn entry_with_flags(id: &str, flags: Vec<DigestFlag>) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            title: id.to_string(),
            searchable_text: format!("{id} transcript"),
            flags,
            ..plain_session(0)
        }
    }

    /// Index of the first rendered row containing `needle`.
    fn row_index(buf: &Buffer, area: Rect, needle: &str) -> u16 {
        for y in 0..area.height {
            let row: String = (0..area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                .collect();
            if row.contains(needle) {
                return y;
            }
        }
        panic!("no rendered row contains {needle:?}");
    }

    /// The rendered text of the first row containing `needle`.
    fn row_containing(buf: &Buffer, area: Rect, needle: &str) -> String {
        let y = row_index(buf, area, needle);
        (0..area.width)
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
            .collect()
    }

    fn sample_sessions() -> Vec<SessionEntry> {
        vec![
            SessionEntry {
                id: "sess-001".to_string(),
                title: "Refactor auth module".to_string(),
                searchable_text: "rotate authentication tokens".to_string(),
                mtime_ms: now_ms().saturating_sub(2 * HOUR_MS),
                message_count: 34,
                cost_usd: 0.0124,
                opening: "oauth \u{b7} tokens \u{b7} rotation".to_string(),
                middle: "refresh \u{b7} flow \u{b7} store".to_string(),
                flags: vec![DigestFlag::Committed],
                synopsis_about: "Fixing OAuth token rotation".to_string(),
                synopsis_left_off: "Mid-refactor of the refresh flow".to_string(),
                transcript_path: std::path::PathBuf::new(),
                is_current: false,
            },
            SessionEntry {
                id: "sess-002".to_string(),
                title: "Write unit tests".to_string(),
                searchable_text: "coverage report".to_string(),
                mtime_ms: now_ms().saturating_sub(30 * HOUR_MS),
                message_count: 12,
                cost_usd: 0.0045,
                opening: "coverage \u{b7} report".to_string(),
                middle: String::new(),
                flags: vec![DigestFlag::NoEdits],
                synopsis_about: String::new(),
                synopsis_left_off: String::new(),
                transcript_path: std::path::PathBuf::new(),
                is_current: false,
            },
            SessionEntry {
                id: "sess-003".to_string(),
                title: "Debug memory leak".to_string(),
                searchable_text: "heap profile".to_string(),
                mtime_ms: now_ms().saturating_sub(100 * HOUR_MS),
                message_count: 57,
                cost_usd: 0.0289,
                opening: "memory \u{b7} leak \u{b7} heap".to_string(),
                middle: "renderer \u{b7} allocations".to_string(),
                flags: vec![DigestFlag::Interrupted, DigestFlag::Uncommitted],
                synopsis_about: "Tracking a leak in the renderer".to_string(),
                synopsis_left_off: String::new(),
                transcript_path: std::path::PathBuf::new(),
                is_current: false,
            },
        ]
    }

    // 1. new() starts hidden with no sessions.
    #[test]
    fn new_starts_hidden() {
        let s = SessionBrowserState::new();
        assert!(!s.visible);
        assert!(s.sessions.is_empty());
        assert_eq!(s.mode, SessionBrowserMode::Browse);
    }

    // 2. open() populates sessions and becomes visible.
    #[test]
    fn open_populates_and_shows() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        assert!(s.visible);
        assert_eq!(s.sessions.len(), 3);
        assert_eq!(s.selected_idx, 0);
        assert_eq!(s.mode, SessionBrowserMode::Browse);
    }

    // 3. select_next() advances selection and wraps to the start.
    #[test]
    fn select_next_wraps_to_start() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.select_next();
        assert_eq!(s.selected_idx, 1);
        s.select_next();
        assert_eq!(s.selected_idx, 2);
        s.select_next();
        assert_eq!(s.selected_idx, 0);
    }

    // 4. select_prev() decrements and wraps to the end.
    #[test]
    fn select_prev_wraps_to_end() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.select_prev();
        assert_eq!(s.selected_idx, 2);
    }

    // 5. Content search matches message text and keeps selection in range.
    #[test]
    fn content_search_matches_message_text() {
        let mut state = SessionBrowserState::new();
        state.open();
        state.set_sessions(sample_sessions());
        state.push_search_char('t');
        state.push_search_char('o');
        state.push_search_char('k');
        let matches = state.filtered_sessions();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "sess-001");
        state.select_next();
        assert_eq!(
            state.selected_session().map(|session| session.id.as_str()),
            Some("sess-001")
        );

        state.search_query = "does-not-exist".to_string();
        state.select_next();
        assert!(state.selected_session().is_none());
        assert_eq!(state.selected_idx, 0);
    }

    // 6. selected_session() returns correct entry.
    #[test]
    fn selected_session_correct() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.selected_idx = 1;
        let sess = s.selected_session().unwrap();
        assert_eq!(sess.id, "sess-002");
    }

    // 6. start_rename() switches mode and pre-fills input.
    #[test]
    fn start_rename_prefills_title() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.selected_idx = 0;
        s.start_rename();
        assert_eq!(s.mode, SessionBrowserMode::Rename);
        assert_eq!(s.rename_input, "Refactor auth module");
    }

    // 7. push_rename_char / pop_rename_char edit the input buffer.
    #[test]
    fn rename_char_editing() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.start_rename();
        s.rename_input.clear(); // clear prefill for clean test
        s.push_rename_char('H');
        s.push_rename_char('i');
        assert_eq!(s.rename_input, "Hi");
        s.pop_rename_char();
        assert_eq!(s.rename_input, "H");
    }

    // 8. confirm_rename() returns (id, new_name) and resets mode.
    #[test]
    fn confirm_rename_returns_pair() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.selected_idx = 0;
        s.start_rename();
        s.rename_input = "  New Title  ".to_string(); // intentional whitespace
        let result = s.confirm_rename();
        assert_eq!(
            result,
            Some(("sess-001".to_string(), "New Title".to_string()))
        );
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert!(s.rename_input.is_empty());
        // Also check local title was updated
        assert_eq!(s.sessions[0].title, "New Title");
    }

    // 9. confirm_rename() with empty input returns None.
    #[test]
    fn confirm_rename_empty_returns_none() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.start_rename();
        s.rename_input = "   ".to_string(); // whitespace only
        let result = s.confirm_rename();
        assert!(result.is_none());
    }

    // 10. cancel() in Rename mode returns to Browse without closing.
    #[test]
    fn cancel_rename_goes_to_browse() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.start_rename();
        s.cancel();
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert!(
            s.visible,
            "overlay should remain visible after cancel-from-rename"
        );
    }

    // 11. cancel() in Browse mode closes the overlay.
    #[test]
    fn cancel_browse_closes() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        s.cancel();
        assert!(!s.visible);
    }

    // 12. render_session_browser does not panic.
    #[test]
    fn render_does_not_panic() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
    }

    #[test]
    fn render_shows_four_rows_per_entry_and_detail_popup() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        // Tail rows for the focused session.
        s.tail_preview = Some(vec![
            (true, "how does the refresh flow work?".to_string()),
            (false, "it rotates tokens via the auth module".to_string()),
        ]);
        s.tail_preview_for = "sess-001".to_string();
        render_session_browser(&s, area, &mut buf);

        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        // Row 1: the title line.
        assert!(text.contains("Refactor auth module"), "title row missing");
        // Row 2: opening keywords from the first message.
        assert!(
            text.contains("oauth \u{b7} tokens \u{b7} rotation"),
            "opening row missing"
        );
        // Row 3: the middle-work keywords.
        assert!(
            text.contains("refresh \u{b7} flow \u{b7} store"),
            "middle row missing"
        );
        // Row 4: the inferred status.
        assert!(text.contains("\u{2714} commit"), "status row missing");
        // Detail popup: labelled summary plus the transcript tail.
        assert!(text.contains("about"), "summary label missing");
        assert!(
            text.contains("Fixing OAuth token rotation"),
            "AI synopsis should win the about line"
        );
        assert!(
            text.contains("how does the refresh flow work?"),
            "tail user row missing"
        );
        assert!(
            text.contains("it rotates tokens via the auth module"),
            "tail assistant row missing"
        );
    }

    #[test]
    fn status_flags_render_in_distinct_colours() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(vec![SessionEntry {
            id: "sess-err".to_string(),
            title: "broke something".to_string(),
            searchable_text: String::new(),
            mtime_ms: now_ms().saturating_sub(HOUR_MS),
            message_count: 9,
            cost_usd: 0.0,
            opening: "parse".to_string(),
            middle: "rewrite".to_string(),
            flags: vec![DigestFlag::Errored, DigestFlag::Uncommitted],
            synopsis_about: String::new(),
            synopsis_left_off: String::new(),
            transcript_path: std::path::PathBuf::new(),
            is_current: false,
        }]);
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);

        let colour_of = |symbol: char| -> Option<Color> {
            buf.content
                .iter()
                .find(|cell| cell.symbol() == symbol.to_string())
                .map(|cell| cell.fg)
        };
        let error = colour_of('\u{2717}').expect("error flag rendered");
        let dirty = colour_of('\u{25cf}').expect("uncommitted flag rendered");
        assert_ne!(
            error, dirty,
            "a failure and an uncommitted tree must not share a colour"
        );
    }

    #[test]
    fn status_row_never_wraps_the_entry() {
        // Every flag at once on a narrow terminal: a wrapped row would shift
        // every following entry, so the second title must sit exactly four
        // rows below the first.
        let all_flags = vec![
            DigestFlag::Interrupted,
            DigestFlag::Errored,
            DigestFlag::Compacted,
            DigestFlag::Committed,
            DigestFlag::Uncommitted,
            DigestFlag::NoEdits,
        ];
        let entry = |id: &str, title: &str, flags: Vec<DigestFlag>| SessionEntry {
            id: id.to_string(),
            title: title.to_string(),
            searchable_text: String::new(),
            mtime_ms: now_ms().saturating_sub(HOUR_MS),
            message_count: 3,
            cost_usd: 0.0,
            opening: "alpha beta gamma delta epsilon zeta eta theta".to_string(),
            middle: String::new(),
            flags,
            synopsis_about: String::new(),
            synopsis_left_off: String::new(),
            transcript_path: std::path::PathBuf::new(),
            is_current: false,
        };
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(vec![
            entry("a", "first entry", all_flags),
            entry("b", "second entry", Vec::new()),
        ]);
        let area = Rect::new(0, 0, 60, 30);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);

        let row_of = |needle: &str| -> usize {
            let mut found = None;
            for y in 0..area.height {
                let row: String = (0..area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                    .collect();
                if row.contains(needle) {
                    found = Some(y as usize);
                    break;
                }
            }
            found.unwrap_or_else(|| panic!("row containing {needle:?} not rendered"))
        };
        let first = row_of("first entry");
        let second = row_of("second entry");
        assert_eq!(
            second - first,
            4,
            "each entry must occupy exactly four rows (no wrapping)"
        );
        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(
            text.contains("interrupted"),
            "the first flag must survive the narrow width"
        );
    }

    #[test]
    fn render_placeholder_keeps_rows_four_tall() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(sample_sessions());
        s.selected_idx = 1; // sess-002 has no middle row and no synopsis
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(text.contains("Write unit tests"));
        assert!(
            text.contains("coverage \u{b7} report"),
            "opening row missing for the second entry"
        );
        assert!(
            text.contains("\u{2026}"),
            "an empty digest row must still render its placeholder"
        );
        assert!(text.contains("\u{25e6} no edits"), "status row missing");
    }

    #[test]
    fn viewport_keeps_the_selected_block_visible() {
        // 40 blocks of 4 lines, seen through a 12-line window.
        let (total, viewport, row_h) = (160usize, 12usize, 4usize);
        let off = viewport_line_offset(total, viewport, 30 * row_h, 31 * row_h);
        assert!(
            off <= 30 * row_h && 31 * row_h <= off + viewport,
            "off={off}"
        );
        // A block near the start stays at the top of the window.
        assert_eq!(viewport_line_offset(total, viewport, 0, row_h), 0);
        // Never scroll past the end of the list.
        assert_eq!(
            viewport_line_offset(total, viewport, 39 * row_h, 40 * row_h),
            total - viewport
        );
        // A list that already fits needs no scrolling.
        assert_eq!(viewport_line_offset(8, 12, 0, 4), 0);
    }

    #[test]
    fn entry_height_adapts_to_the_available_lines() {
        // With the popup on screen, four rows survive until only four whole
        // entries fit; the keyword rows then live in the popup.
        assert_eq!(rows_per_entry_for(40, true), 4);
        assert_eq!(rows_per_entry_for(16, true), 4);
        assert_eq!(rows_per_entry_for(15, true), 2);
        // With no popup to fall back on, the full layout is kept until it can
        // show fewer than two whole entries.
        assert_eq!(rows_per_entry_for(15, false), 4);
        assert_eq!(rows_per_entry_for(8, false), 4);
        assert_eq!(rows_per_entry_for(7, false), 2);
        assert_eq!(rows_per_entry_for(4, false), 2);
    }

    #[test]
    fn day_bucket_uses_local_calendar_days() {
        let day = 24 * HOUR_MS;
        let now = now_ms();
        assert_eq!(DayBucket::of(now, now), DayBucket::Today);
        assert_eq!(DayBucket::of(now - day, now), DayBucket::Yesterday);
        assert_eq!(DayBucket::of(now - 3 * day, now), DayBucket::ThisWeek);
        assert_eq!(DayBucket::of(now - 30 * day, now), DayBucket::Earlier);
        assert_eq!(DayBucket::Today.label(), "Today");
    }

    // 13. render is a no-op when hidden.
    #[test]
    fn render_noop_when_hidden() {
        let s = SessionBrowserState::new(); // visible = false
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        for cell in buf.content() {
            assert_eq!(
                cell.symbol(),
                " ",
                "buffer should be empty when browser is hidden"
            );
        }
    }

    // 14. fmt_cost formats correctly — zero renders as an empty cell so free
    //     sessions don't fill the table with "$0.0000" readouts.
    #[test]
    fn fmt_cost_formats() {
        assert_eq!(fmt_cost(0.0), "");
        assert_eq!(fmt_cost(0.00005), "");
        assert_eq!(fmt_cost(0.0124), "$0.0124");
        assert_eq!(fmt_cost(1.5), "$1.5000");
    }

    // 15. truncate_display trims long strings.
    #[test]
    fn truncate_display_trims() {
        let long = "abcdefghij"; // 10 chars
        let result = truncate_display(long, 5);
        assert!(
            result.width() <= 6,
            "truncated string should fit within budget"
        );
        assert!(result.ends_with('…'));
    }

    // -----------------------------------------------------------------
    // Smart filtering, facets, grouping and mouse hit-testing
    // -----------------------------------------------------------------

    #[test]
    fn facet_cycle_filters_by_status() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(vec![
            entry_with_flags("dirty", vec![DigestFlag::Uncommitted]),
            entry_with_flags("clean", vec![DigestFlag::NoEdits]),
            entry_with_flags("broke", vec![DigestFlag::Errored]),
        ]);
        assert_eq!(s.filtered_sessions().len(), 3);

        s.cycle_facet();
        assert_eq!(s.facet, FacetFilter::Uncommitted);
        let filtered = s.filtered_sessions();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "dirty");

        s.cycle_facet();
        assert_eq!(s.facet, FacetFilter::Errored);
        assert_eq!(s.filtered_sessions()[0].id, "broke");

        // Around the rest of the cycle and back to everything.
        for _ in 0..4 {
            s.cycle_facet();
        }
        assert_eq!(s.facet, FacetFilter::All);
        assert_eq!(s.filtered_sessions().len(), 3);
    }

    #[test]
    fn multi_word_search_requires_every_term_and_ranks_title_first() {
        let mut title_match = plain_session(1);
        title_match.title = "katban commit".to_string();
        let mut content_match = plain_session(2);
        content_match.title = "unrelated".to_string();
        content_match.searchable_text = "the katban board got a commit".to_string();
        let partial = plain_session(3);

        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(vec![content_match, partial, title_match]);
        s.search_query = "katban commit".to_string();

        let filtered = s.filtered_sessions();
        // Both terms must match somewhere — `partial` only has one — and the
        // title hit outranks the transcript hit.
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].title, "katban commit");
        assert_eq!(filtered[1].title, "unrelated");
    }

    #[test]
    fn matches_are_highlighted_and_merged() {
        let terms = vec!["auth".to_string(), "token".to_string()];
        assert_eq!(
            match_ranges("auth token rotation", &terms),
            vec![(0, 4), (5, 10)]
        );
        assert_eq!(match_ranges("authentication", &terms), vec![(0, 4)]);
        assert_eq!(
            match_ranges("AUTH", &terms),
            vec![(0, 4)],
            "case-insensitive"
        );
        assert!(match_ranges("nothing here", &terms).is_empty());

        let hit = Style::default().fg(Color::Red);
        let spans = highlight_spans("auth token", Style::default(), hit, &terms);
        assert_eq!(spans.len(), 3, "match, gap, match");
        assert_eq!(spans[0].style.fg, Some(Color::Red));

        // Offsets never cut through a character: when lowercasing changes the
        // byte length, highlighting is skipped rather than corrupting the row.
        assert!(match_ranges("\u{130}", &terms).is_empty());
    }

    #[test]
    fn render_groups_by_recency_when_the_list_is_long() {
        let mut s = SessionBrowserState::new();
        s.open();
        let mut sessions: Vec<SessionEntry> = (0..4)
            .map(|i| {
                let mut recent = plain_session(i);
                recent.mtime_ms = start_of_today_ms();
                recent
            })
            .chain((0..4).map(|i| {
                let mut old = plain_session(i + 100);
                old.mtime_ms = now_ms().saturating_sub(30 * 24 * HOUR_MS);
                old
            }))
            .collect();
        // Recency order, as the loader produces it.
        sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime_ms));
        s.set_sessions(sessions);

        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(text.contains("Today"), "today header missing");
        assert!(
            !text.contains("Earlier"),
            "with the window at the top only the first group shows"
        );

        // Moving into the older group scrolls the window far enough that its
        // header is on screen too.
        s.selected_idx = 4;
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(text.contains("Earlier"), "older group header missing");
    }

    #[test]
    fn render_marks_the_current_session() {
        let mut s = SessionBrowserState::new();
        s.open();
        let mut current = plain_session(1);
        current.is_current = true;
        current.title = "the running one".to_string();
        s.set_sessions(vec![current, plain_session(2)]);
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);

        let row = row_containing(&buf, area, "the running one");
        assert!(
            row.contains('\u{25cf}'),
            "the running session must be marked: {row:?}"
        );
        assert!(row.contains("now"), "the running session reads as now");
    }

    #[test]
    fn render_shows_the_attention_summary() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions(vec![
            entry_with_flags("dirty", vec![DigestFlag::Uncommitted]),
            entry_with_flags("broke", vec![DigestFlag::Errored]),
        ]);
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(text.contains("2 sessions"), "session count missing");
        assert!(text.contains("1 uncommitted"), "uncommitted tally missing");
        assert!(text.contains("1 errored"), "error tally missing");
    }

    #[test]
    fn render_shows_a_loading_line_while_the_list_is_in_flight() {
        let mut s = SessionBrowserState::new();
        s.open(); // no list yet
        assert!(s.loading, "open() must mark the list as loading");
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(text.contains("Loading sessions"), "loading line missing");
        assert!(
            !text.contains("No sessions found"),
            "a slow load must not read as an empty history"
        );

        s.set_sessions(vec![plain_session(1)]);
        assert!(!s.loading, "arriving results clear the loading state");
    }

    #[test]
    fn relative_times_are_derived_from_mtime_at_render() {
        let mut s = SessionBrowserState::new();
        s.open();
        let mut recent = plain_session(1);
        recent.title = "recent one".to_string();
        recent.mtime_ms = now_ms().saturating_sub(HOUR_MS);
        let mut old = plain_session(2);
        old.title = "old one".to_string();
        old.mtime_ms = now_ms().saturating_sub(30 * 24 * HOUR_MS);
        s.set_sessions(vec![recent, old]);

        let area = Rect::new(0, 0, 130, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);
        let recent_row = row_containing(&buf, area, "recent one");
        let old_row = row_containing(&buf, area, "old one");
        assert!(recent_row.contains("1 hour ago"), "got: {recent_row:?}");
        assert!(old_row.contains("30 days ago"), "got: {old_row:?}");
    }

    #[test]
    fn click_hit_test_resolves_the_entry_under_the_cursor() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions((0..3).map(plain_session).collect());
        let area = Rect::new(0, 0, 120, 40);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);

        let second = row_index(&buf, area, "session 1");
        // The list is centred, so a column inside its span resolves.
        assert_eq!(s.hit_test(30, second), Some(1));
        // Any row of the entry's block maps to that entry.
        assert_eq!(s.hit_test(30, second + 3), Some(1));
        // The next block belongs to the next entry.
        assert_eq!(s.hit_test(30, second + 4), Some(2));
        // Left of the list, or above it, nothing resolves.
        assert_eq!(s.hit_test(0, second), None);
        assert_eq!(s.hit_test(30, 0), None);
    }

    #[test]
    fn delete_confirm_removes_the_row_and_refuses_the_running_session() {
        let mut s = SessionBrowserState::new();
        s.open();
        let mut current = plain_session(1);
        current.is_current = true;
        s.set_sessions(vec![current, plain_session(2)]);

        // The session being run cannot be deleted.
        assert_eq!(s.selected_idx, 0);
        assert_eq!(s.request_delete(), None);
        assert_eq!(s.mode, SessionBrowserMode::Browse);

        // Cancelling keeps the row.
        s.selected_idx = 1;
        assert_eq!(s.request_delete().as_deref(), Some("s002"));
        assert_eq!(s.mode, SessionBrowserMode::Confirm);
        s.cancel();
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert_eq!(s.sessions.len(), 2);

        // Confirming removes it locally and reports the id.
        assert_eq!(s.request_delete().as_deref(), Some("s002"));
        assert_eq!(s.confirm_delete().as_deref(), Some("s002"));
        assert_eq!(s.mode, SessionBrowserMode::Browse);
        assert_eq!(s.sessions.len(), 1);
    }

    #[test]
    fn reopening_restores_the_previous_selection() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions((0..5).map(plain_session).collect());
        s.selected_idx = 3;
        s.close();

        s.open();
        assert_eq!(s.facet, FacetFilter::All, "the facet resets on open");
        s.set_sessions((0..5).map(plain_session).collect());
        assert_eq!(s.selected_idx, 3, "reopening should land on the same row");
    }

    #[test]
    fn modal_grows_into_spare_terminal_rows() {
        // A short terminal keeps the size the browser has always had.
        assert_eq!(modal_rect_for(Rect::new(0, 0, 120, 30)).height, MODAL_MIN_H);
        // A tall one takes the spare rows, stopping short of the popup.
        assert_eq!(
            modal_rect_for(Rect::new(0, 0, 120, 60)).height,
            60 - 2 - DETAIL_POPUP_H
        );
        // And stops growing eventually.
        assert_eq!(
            modal_rect_for(Rect::new(0, 0, 120, 200)).height,
            MODAL_MAX_H
        );
        // Never larger than the space it is given, at any size.
        for h in [8u16, 16, 24, 26, 30, 42, 60, 200] {
            let area = Rect::new(0, 0, 100, h);
            let rect = modal_rect_for(area);
            assert!(rect.bottom() <= area.bottom(), "h={h}");
        }
    }

    #[test]
    fn wide_terminals_widen_the_modal() {
        // The floor keeps narrow terminals exactly as they were.
        assert_eq!(modal_rect_for(Rect::new(0, 0, 80, 40)).width, MODAL_W);
        assert_eq!(modal_rect_for(Rect::new(0, 0, 60, 40)).width, 58);
        // Spare columns are used, up to the cap.
        assert_eq!(modal_rect_for(Rect::new(0, 0, 91, 40)).width, 87);
        assert_eq!(modal_rect_for(Rect::new(0, 0, 130, 40)).width, MODAL_MAX_W);
        assert_eq!(modal_rect_for(Rect::new(0, 0, 300, 40)).width, MODAL_MAX_W);
        // Centred, and never wider than the area.
        for w in [40u16, 60, 80, 91, 130, 300] {
            let area = Rect::new(0, 0, w, 40);
            let rect = modal_rect_for(area);
            assert!(rect.right() <= area.right(), "w={w}");
            assert_eq!(
                rect.x,
                area.x + area.width.saturating_sub(rect.width) / 2,
                "w={w}"
            );
        }
    }

    #[test]
    fn a_wider_modal_shows_more_of_the_keyword_rows() {
        let keyword_row = "alpha bravo charlie delta echo foxtrot golf hotel \
                           india juliet kilo lima mike november oscar"
            .to_string();
        assert!(keyword_row.len() > 70, "long enough to truncate at 78 cols");
        let render = |width: u16| -> String {
            let mut s = SessionBrowserState::new();
            s.open();
            let mut entry = plain_session(1);
            entry.middle = keyword_row.clone();
            s.set_sessions(vec![entry]);
            let area = Rect::new(0, 0, width, 40);
            let mut buf = Buffer::empty(area);
            render_session_browser(&s, area, &mut buf);
            buf.content.iter().map(|c| c.symbol().to_string()).collect()
        };
        // A narrow modal truncates with an ellipsis...
        let narrow = render(80);
        assert!(
            narrow.contains("\u{2026}"),
            "expected truncation at 80 cols"
        );
        // ...a wide one has room for the whole row.
        let wide = render(130);
        assert!(
            wide.contains(&keyword_row),
            "the widened modal should show the full keyword row"
        );
    }

    #[test]
    fn modal_and_popup_never_overlap() {
        // The popup used to be clamped up over the bottom of the list whenever
        // the terminal could not hold both. It now only takes rows under the
        // modal, and when there are fewer than four it is not drawn at all.
        for h in [16u16, 26, 30, 34, 42, 50, 60, 80] {
            let area = Rect::new(0, 0, 120, h);
            let modal = modal_rect_for(area);
            let below = area.bottom().saturating_sub(modal.bottom());
            let popup_rows = DETAIL_POPUP_H.min(below);
            assert!(
                popup_rows == 0 || modal.bottom() + popup_rows <= area.bottom(),
                "popup overlaps the modal at h={h}"
            );
        }
    }

    #[test]
    fn a_tall_terminal_lists_more_entries_at_once() {
        let visible = |height: u16| -> usize {
            let mut s = SessionBrowserState::new();
            s.open();
            s.set_sessions((0..30).map(plain_session).collect());
            let area = Rect::new(0, 0, 120, height);
            let mut buf = Buffer::empty(area);
            render_session_browser(&s, area, &mut buf);
            let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
            // Titles are padded to the column width, so the trailing space
            // keeps "session 1" from matching "session 10".
            (0..30)
                .filter(|i| text.contains(&format!("session {i} ")))
                .count()
        };
        let short = visible(30);
        let tall = visible(60);
        assert_eq!(short, 3, "a 30-row terminal shows three whole entries");
        assert!(
            tall >= 6,
            "a 60-row terminal should list many more, got {tall}"
        );
        assert!(tall > short);
    }

    #[test]
    fn a_terminal_with_no_room_under_the_modal_keeps_the_keyword_rows() {
        // 26 rows cannot hold the modal and the popup, so the popup is dropped
        // and the list keeps the four-row layout — the keywords stay visible
        // instead of being pushed into a popup that would cover the list.
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions((0..12).map(plain_session).collect());
        let area = Rect::new(0, 0, 100, 26);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);

        let first = row_index(&buf, area, "session 0");
        let second = row_index(&buf, area, "session 1");
        assert_eq!(second - first, 4, "entries keep their four rows");
        let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert!(
            !text.contains("about"),
            "no room for the popup: it must not be drawn over the list"
        );
        assert!(
            text.contains("\u{276f}") && text.contains("\u{b7}"),
            "the keyword rows must be in the list when there is no popup"
        );
        // The hint bar survives too: nothing covers the bottom of the modal.
        assert!(text.contains("Enter resume"));
    }

    #[test]
    fn a_tiny_terminal_falls_back_to_the_two_row_layout() {
        let mut s = SessionBrowserState::new();
        s.open();
        s.set_sessions((0..3).map(plain_session).collect());
        let area = Rect::new(0, 0, 100, 16);
        let mut buf = Buffer::empty(area);
        render_session_browser(&s, area, &mut buf);

        let first = row_index(&buf, area, "session 0");
        let second = row_index(&buf, area, "session 1");
        assert_eq!(
            second - first,
            2,
            "with no room at all the list compacts to title + status"
        );
    }
}
