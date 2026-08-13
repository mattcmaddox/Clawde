//! Terminal tab-status reporting (OSC 21337 `tab.status`).
//!
//! Clawde reports its working state to the host terminal so that a terminal
//! multiplexer (or terminal itself) can surface an "agent status" indicator.
//! The convention used here follows the terminal "tab status" OSC proposal:
//!
//! ```text
//! ESC ] 21337 ; {"action":{"content":[{"type":"tab.status","value":{"status":"<state>"}}]}} BEL
//! ```
//!
//! where `<state>` is one of `idle | busy | waiting`:
//!
//! * `busy`    — a turn is actively streaming/computing.
//! * `waiting` — Clawde is blocked on user input (a permission prompt, pending
//!   input, or an open dialog).
//! * `idle`    — neither of the above.
//!
//! Zellij (and the `clawde-status` plugin shipped with the Zellij fork) watches
//! pane output for this sequence; terminals that don't understand the OSC
//! ignore it. Sequences are only emitted on state transitions to keep the
//! pane output clean.

use std::io::Write;

/// The working state of the agent, as reported to the host terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabStatus {
    Idle,
    Busy,
    Waiting,
}

impl TabStatus {
    /// The canonical wire value.
    pub fn as_str(&self) -> &'static str {
        match self {
            TabStatus::Idle => "idle",
            TabStatus::Busy => "busy",
            TabStatus::Waiting => "waiting",
        }
    }
}

/// Whether the current terminal environment understands (or at least tolerates)
/// the OSC 21337 tab-status sequence.
///
/// We are deliberately permissive inside a Zellij session (ZELLIJ is set),
/// because the Zellij server itself consumes the sequence from the pane's pty
/// stream before any terminal rendering happens. Outside Zellij we require a
/// real tty and an allow-listed terminal to avoid leaking control sequences
/// into unsupported terminals (tmux/screen don't forward unknown OSCs by
/// default).
pub fn supports_tab_status_osc() -> bool {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return false;
    }
    if std::env::var_os("ZELLIJ").is_some() {
        return true;
    }
    if std::env::var_os("TMUX").is_some() {
        return false;
    }
    if let Ok(term) = std::env::var("TERM") {
        if term.starts_with("screen") || term.starts_with("tmux") {
            return false;
        }
    }
    matches!(
        std::env::var("TERM_PROGRAM").unwrap_or_default().as_str(),
        "iTerm.app" | "WezTerm" | "ghostty"
    )
}

/// Emit the OSC 21337 `tab.status` sequence for the given state.
///
/// This is safe alongside the ratatui alternate screen: it's an OSC sequence
/// addressed to the terminal/multiplexer, not a grid write, so it doesn't
/// disturb the rendered frame. Callers should only emit on transitions.
pub fn set_tab_status(status: TabStatus) {
    if !supports_tab_status_osc() {
        return;
    }
    let seq = format!(
        "\x1b]21337;{{\"action\":{{\"content\":[{{\"type\":\"tab.status\",\"value\":{{\"status\":\"{}\"}}}}]}}}}\x07",
        status.as_str()
    );
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_as_str_matches_wire() {
        assert_eq!(TabStatus::Idle.as_str(), "idle");
        assert_eq!(TabStatus::Busy.as_str(), "busy");
        assert_eq!(TabStatus::Waiting.as_str(), "waiting");
    }

    #[test]
    fn sequence_matches_expected_shape() {
        let seq = format!(
            "\x1b]21337;{{\"action\":{{\"content\":[{{\"type\":\"tab.status\",\"value\":{{\"status\":\"{}\"}}}}]}}}}\x07",
            TabStatus::Busy.as_str()
        );
        assert!(seq.starts_with("\x1b]21337;"));
        assert!(seq.ends_with('\x07'));
        assert!(seq.contains("\"status\":\"busy\""));
    }
}
