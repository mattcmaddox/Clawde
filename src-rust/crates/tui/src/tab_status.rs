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
//! where `<state>` is one of `idle | busy | waiting` and the optional topic
//! identifies the current session/task:
//!
//! ```text
//! {"status":"busy","topic":"refactor parser"}
//! ```
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

use serde::Serialize;
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

/// An allow-listed operation that Clawde may request from Zellij.
///
/// This mirrors the server-side `zellij.action` allow-list. The enum is
/// deliberately structured: callers cannot submit an arbitrary shell command
/// or write to another pane's PTY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ZellijAction {
    /// Open a pane in the originating tab and run a direct executable.
    NewPane {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default)]
        floating: bool,
    },
    /// Switch to exactly one named or indexed tab.
    SwitchTab {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        index: Option<u32>,
    },
    /// Rename the tab containing the originating pane.
    RenameTab { name: String },
    /// Focus a terminal pane by its numeric ID.
    FocusPane { pane_id: u32 },
}

fn zellij_action_is_valid(action: &ZellijAction) -> bool {
    match action {
        ZellijAction::NewPane { command, .. } => !command.is_empty(),
        ZellijAction::SwitchTab { name, index } => name.is_some() ^ index.is_some(),
        ZellijAction::RenameTab { name } => !name.is_empty(),
        ZellijAction::FocusPane { .. } => true,
    }
}

/// Encode a structured action in the shared OSC 21337 envelope.
pub fn zellij_action_sequence(action: &ZellijAction) -> Option<String> {
    if !zellij_action_is_valid(action) {
        return None;
    }
    let envelope = serde_json::json!({
        "action": {
            "content": [{
                "type": "zellij.action",
                "value": action,
            }]
        }
    });
    Some(format!("\x1b]21337;{}\x07", envelope))
}

/// Emit a validated allow-listed `zellij.action` request.
///
/// This function is intentionally a low-level API rather than an automatic
/// model/tool hook. Callers must put it behind an explicit UI action or other
/// user-approved flow before asking Zellij to change the workspace.
pub fn emit_zellij_action(action: &ZellijAction) {
    if std::env::var_os("ZELLIJ").is_none() || !supports_tab_status_osc() {
        return;
    }
    let Some(seq) = zellij_action_sequence(action) else {
        return;
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Emit the OSC 21337 `tab.status` sequence for the given state and optional
/// topic.
///
/// This is safe alongside the ratatui alternate screen: it's an OSC sequence
/// addressed to the terminal/multiplexer, not a grid write, so it doesn't
/// disturb the rendered frame. Callers should only emit on transitions. The
/// topic is optional and JSON-escaped by `serde_json` so titles/tasks with
/// quotes or backslashes cannot corrupt the envelope.
pub fn set_tab_status(status: TabStatus, topic: Option<&str>) {
    if !supports_tab_status_osc() {
        return;
    }
    let value = serde_json::json!({
        "status": status.as_str(),
        "topic": topic.filter(|topic| !topic.is_empty()),
    });
    let envelope = serde_json::json!({
        "action": {
            "content": [{
                "type": "tab.status",
                "value": value,
            }]
        }
    });
    let seq = format!("\x1b]21337;{}\x07", envelope);
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
        let seq = serde_json::json!({
            "action": {
                "content": [{
                    "type": "tab.status",
                    "value": {"status": TabStatus::Busy.as_str()},
                }]
            }
        });
        let seq = format!("\x1b]21337;{}\x07", seq);
        assert!(seq.starts_with("\x1b]21337;"));
        assert!(seq.ends_with('\x07'));
        assert!(seq.contains("\"status\":\"busy\""));
    }

    #[test]
    fn action_sequence_matches_server_wire_shape() {
        let action = ZellijAction::NewPane {
            command: "cargo".to_owned(),
            args: vec!["test".to_owned(), "--lib".to_owned()],
            cwd: Some("/repo".to_owned()),
            name: Some("tests".to_owned()),
            floating: true,
        };
        let encoded = zellij_action_sequence(&action).unwrap();
        assert!(encoded.starts_with("\x1b]21337;"));
        assert!(encoded.ends_with('\x07'));
        let json: serde_json::Value = serde_json::from_str(
            encoded
                .trim_start_matches("\x1b]21337;")
                .trim_end_matches('\x07'),
        )
        .unwrap();
        assert_eq!(json["action"]["content"][0]["type"], "zellij.action");
        assert_eq!(json["action"]["content"][0]["value"]["action"], "new-pane");
        assert_eq!(json["action"]["content"][0]["value"]["args"][0], "test");
        assert_eq!(json["action"]["content"][0]["value"]["floating"], true);
    }

    #[test]
    fn action_sequence_rejects_ambiguous_or_empty_requests() {
        let both = ZellijAction::SwitchTab {
            name: Some("review".to_owned()),
            index: Some(2),
        };
        let neither = ZellijAction::SwitchTab {
            name: None,
            index: None,
        };
        let empty_rename = ZellijAction::RenameTab {
            name: String::new(),
        };
        assert!(zellij_action_sequence(&both).is_none());
        assert!(zellij_action_sequence(&neither).is_none());
        assert!(zellij_action_sequence(&empty_rename).is_none());
    }

    #[test]
    fn action_sequence_escapes_user_text() {
        let action = ZellijAction::RenameTab {
            name: "review \\\"\\\\ pass".to_owned(),
        };
        let encoded = zellij_action_sequence(&action).unwrap();
        let json: serde_json::Value = serde_json::from_str(
            encoded
                .trim_start_matches("\x1b]21337;")
                .trim_end_matches('\x07'),
        )
        .unwrap();
        assert_eq!(
            json["action"]["content"][0]["value"]["name"],
            "review \\\"\\\\ pass"
        );
    }

    #[test]
    fn topic_is_json_escaped_and_optional() {
        let value = serde_json::json!({
            "status": TabStatus::Busy.as_str(),
            "topic": "a\"\\b",
        });
        let envelope = serde_json::json!({
            "action": {
                "content": [{"type": "tab.status", "value": value}]
            }
        });
        let encoded = envelope.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(parsed["action"]["content"][0]["value"]["topic"], "a\"\\b");

        let no_topic = serde_json::json!({
            "status": TabStatus::Idle.as_str(),
            "topic": serde_json::Value::Null,
        });
        assert_eq!(no_topic["topic"], serde_json::Value::Null);
    }
}
