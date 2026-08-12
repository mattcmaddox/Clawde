// memory_file_selector.rs — Memory file selector overlay mirroring TS MemoryFileSelector.tsx

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::overlays::{
    centered_rect, render_dark_overlay_buf, render_dialog_bg_buf, CLAURST_ACCENT, CLAURST_MUTED,
    CLAURST_PANEL_BG, CLAURST_TEXT,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryFileType {
    User,
    Project,
    Local,
    ProjectMemory,
}

pub struct MemoryFile {
    pub path: String,
    pub display_path: String,
    pub file_type: MemoryFileType,
    pub exists: bool,
    /// Modification time in Unix seconds when the file exists.
    pub modified_secs: Option<u64>,
}

pub struct MemoryFileSelectorState {
    pub visible: bool,
    pub files: Vec<MemoryFile>,
    pub selected: usize,
    pub project_root: std::path::PathBuf,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl MemoryFileSelectorState {
    pub fn new() -> Self {
        Self {
            visible: false,
            files: Vec::new(),
            selected: 0,
            project_root: std::path::PathBuf::new(),
        }
    }

    /// Open the selector for the given project root.
    ///
    /// Populates the file list with:
    /// - User:    `~/.clawde/AGENTS.md`
    /// - Project: `{project_root}/AGENTS.md`
    /// - Local:   `{project_root}/.claurst/AGENTS.md`
    ///
    /// Each entry is marked `exists = true/false` based on the filesystem.
    pub fn open(&mut self, project_root: &std::path::Path) {
        self.project_root = project_root.to_path_buf();
        self.selected = 0;
        self.files.clear();

        // User-level: ~/.clawde/AGENTS.md
        let user_path = clawde_core::config::Settings::config_dir().join("AGENTS.md");
        let user_display = {
            let home = dirs::home_dir().unwrap_or_default();
            let rel = user_path.strip_prefix(&home).unwrap_or(&user_path);
            format!("~/{}", rel.display())
        };
        self.files
            .push(memory_file(user_path, user_display, MemoryFileType::User));

        // Project-level: {project_root}/AGENTS.md
        let project_path = project_root.join("AGENTS.md");
        let project_display = project_path.display().to_string();
        self.files.push(memory_file(
            project_path,
            project_display,
            MemoryFileType::Project,
        ));

        // Local-level: {project_root}/.claurst/AGENTS.md
        let local_path = project_root.join(".claurst").join("AGENTS.md");
        let local_display = local_path.display().to_string();
        self.files.push(memory_file(
            local_path,
            local_display,
            MemoryFileType::Local,
        ));

        // Project memory: expose the durable index plus the most recently
        // updated topic files. Keep the browser bounded so a large memory tree
        // remains usable while `/memory status` still reports the full count.
        let memory_dir = clawde_core::memdir::auto_memory_path(project_root);
        let memory_index = memory_dir.join(clawde_core::memdir::MEMORY_ENTRYPOINT);
        self.files.push(memory_file(
            memory_index,
            format!("{}/MEMORY.md", memory_dir.display()),
            MemoryFileType::ProjectMemory,
        ));
        for meta in clawde_core::memdir::scan_memory_dir(&memory_dir)
            .into_iter()
            .take(12)
        {
            self.files.push(MemoryFile {
                path: meta.path.to_string_lossy().into_owned(),
                display_path: format!("{}/{}", memory_dir.display(), meta.filename),
                file_type: MemoryFileType::ProjectMemory,
                exists: true,
                modified_secs: Some(meta.modified_secs),
            });
        }

        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    pub fn select_prev(&mut self) {
        let count = self.files.len();
        if count == 0 {
            return;
        }
        if self.selected == 0 {
            self.selected = count - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn select_next(&mut self) {
        let count = self.files.len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected + 1) % count;
    }

    /// Return the path of the currently highlighted file, if any.
    pub fn selected_path(&self) -> Option<&str> {
        self.files.get(self.selected).map(|f| f.path.as_str())
    }
}

fn memory_file(
    path: std::path::PathBuf,
    display_path: String,
    file_type: MemoryFileType,
) -> MemoryFile {
    let modified_secs = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    MemoryFile {
        exists: path.exists(),
        path: path.to_string_lossy().into_owned(),
        display_path,
        file_type,
        modified_secs,
    }
}

impl Default for MemoryFileSelectorState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the memory file selector as a centered floating dialog.
pub fn render_memory_file_selector(state: &MemoryFileSelectorState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    // Height: 2 border + 1 blank + N files + 1 blank + 1 footer = N + 5
    let dialog_height = (state.files.len() as u16 + 6).max(8);
    let dialog_area = centered_rect(70, dialog_height, area);
    render_dark_overlay_buf(buf, area);
    render_dialog_bg_buf(buf, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 2,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(4),
        height: dialog_area.height.saturating_sub(2),
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " Memory",
            Style::default()
                .fg(CLAURST_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" — choose a file", Style::default().fg(CLAURST_MUTED)),
        Span::styled(
            format!(
                "{:>width$}",
                "Esc close",
                width = inner.width.saturating_sub(24) as usize
            ),
            Style::default().fg(CLAURST_MUTED),
        ),
    ]));
    lines.push(Line::from(""));

    for (i, file) in state.files.iter().enumerate() {
        let type_label = match file.file_type {
            MemoryFileType::User => "User    ",
            MemoryFileType::Project => "Project ",
            MemoryFileType::Local => "Local   ",
            MemoryFileType::ProjectMemory => "Memory  ",
        };

        let freshness = file
            .modified_secs
            .map(|modified| format!(" · updated {}", clawde_core::memdir::memory_age(modified)))
            .unwrap_or_else(|| " · not created".to_string());
        let new_tag = Span::styled(
            if file.exists {
                freshness.clone()
            } else {
                " · new".to_string()
            },
            Style::default().fg(CLAURST_MUTED),
        );

        if i == state.selected {
            lines.push(Line::from(vec![Span::styled(
                pad_line(
                    &format!(
                        "  \u{203a} {type_label} {}{}",
                        file.display_path,
                        if file.exists {
                            freshness.as_str()
                        } else {
                            " · new"
                        }
                    ),
                    inner.width,
                ),
                Style::default()
                    .fg(Color::Black)
                    .bg(CLAURST_ACCENT)
                    .add_modifier(Modifier::BOLD),
            )]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {type_label} {}", file.display_path),
                    Style::default().fg(CLAURST_TEXT),
                ),
                new_tag,
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  \u{2191}\u{2193}/jk navigate  Enter open  e create/open  Esc close",
        Style::default().fg(CLAURST_MUTED),
    )]));

    let para = Paragraph::new(lines)
        .style(Style::default().bg(CLAURST_PANEL_BG).fg(CLAURST_TEXT))
        .alignment(Alignment::Left);

    use ratatui::widgets::Widget;
    para.render(inner, buf);
}

fn pad_line(text: &str, width: u16) -> String {
    let max_width = width as usize;
    let mut clipped: String = text.chars().take(max_width).collect();
    let visible = clipped.chars().count();
    if visible < max_width {
        clipped.push_str(&" ".repeat(max_width - visible));
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_records_existing_file_freshness_and_marks_missing_files() {
        let project = tempfile::tempdir().unwrap();
        let project_file = project.path().join("AGENTS.md");
        std::fs::write(&project_file, "# Project memory\n").unwrap();

        let mut state = MemoryFileSelectorState::new();
        state.open(project.path());

        let project_entry = state
            .files
            .iter()
            .find(|file| file.path == project_file.to_string_lossy())
            .unwrap();
        assert!(project_entry.exists);
        assert!(project_entry.modified_secs.is_some());

        let local_entry = state
            .files
            .iter()
            .find(|file| file.file_type == MemoryFileType::Local)
            .unwrap();
        assert!(!local_entry.exists);
        assert!(local_entry.modified_secs.is_none());
        assert!(state
            .files
            .iter()
            .any(|file| file.file_type == MemoryFileType::ProjectMemory));
    }

    #[test]
    fn open_includes_recent_project_memory_files() {
        let project = tempfile::tempdir().unwrap();
        let memory_dir = clawde_core::memdir::auto_memory_path(project.path());
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("MEMORY.md"), "# Index\n").unwrap();
        std::fs::write(memory_dir.join("architecture.md"), "# Architecture\n").unwrap();

        let mut state = MemoryFileSelectorState::new();
        state.open(project.path());

        assert!(state.files.iter().any(|file| {
            file.file_type == MemoryFileType::ProjectMemory && file.path.ends_with("MEMORY.md")
        }));
        assert!(state.files.iter().any(|file| {
            file.file_type == MemoryFileType::ProjectMemory
                && file.path.ends_with("architecture.md")
        }));
    }

    #[test]
    fn render_shows_freshness_and_new_file_state() {
        // The project row renders as `› Project <path> · updated today` and the
        // dialog is a fixed 70 columns. A long project path (e.g. an overridden
        // TMPDIR) pushes the freshness suffix past the clip point and the
        // assertion becomes environment-dependent, so use a short, fixed path.
        let base: std::path::PathBuf = if cfg!(unix) {
            "/tmp/clawde-memsel-test".into()
        } else {
            std::env::temp_dir().join("clawde-memsel-test")
        };
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("AGENTS.md"), "memory\n").unwrap();

        let mut state = MemoryFileSelectorState::new();
        state.open(&base);
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 20,
        };
        let mut buffer = Buffer::empty(area);
        render_memory_file_selector(&state, area, &mut buffer);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");

        assert!(rendered.contains("updated today"));
        assert!(rendered.contains(" · new"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
