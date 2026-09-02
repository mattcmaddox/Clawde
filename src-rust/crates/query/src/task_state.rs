//! Runtime-owned task state for the agent loop.
//!
//! The transcript remains the historical record. `TaskState` is the compact,
//! deterministic projection used to keep the active task, evidence, and next
//! action visible to the model on every turn.

use clawde_core::types::{ContentBlock, Message, MessageContent, Role};
use std::path::PathBuf;

const MAX_TEXT_CHARS: usize = 700;
const MAX_ITEMS: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusState {
    Active,
    Blocked,
    AwaitingClarification,
    Complete,
    Suspended,
}

impl FocusState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::AwaitingClarification => "awaiting clarification",
            Self::Complete => "complete",
            Self::Suspended => "suspended",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDecision {
    pub statement: String,
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFailure {
    pub source: String,
    pub summary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComplexityLedger {
    pub files_touched: usize,
    pub tool_calls: usize,
    pub failed_tools: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEvidence {
    pub plan_step: Option<String>,
    pub validation: Option<String>,
    pub snapshot_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    pub objective: Option<String>,
    pub focus: FocusState,
    pub active_step: Option<String>,
    pub constraints: Vec<String>,
    pub decisions: Vec<TaskDecision>,
    pub changed_files: Vec<PathBuf>,
    pub failures: Vec<TaskFailure>,
    pub next_action: Option<String>,
    pub complexity: ComplexityLedger,
    pub runtime: RuntimeEvidence,
    pub turn: u32,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            objective: None,
            focus: FocusState::Active,
            active_step: None,
            constraints: Vec::new(),
            decisions: Vec::new(),
            changed_files: Vec::new(),
            failures: Vec::new(),
            next_action: None,
            complexity: ComplexityLedger::default(),
            runtime: RuntimeEvidence {
                plan_step: None,
                validation: None,
                snapshot_files: Vec::new(),
            },
            turn: 0,
        }
    }
}

impl TaskState {
    pub fn from_messages(messages: &[Message]) -> Self {
        let mut state = Self::default();
        state.apply_messages(messages);
        state
    }

    pub fn apply_messages(&mut self, messages: &[Message]) {
        for message in messages {
            self.apply_message(message);
        }
        self.trim();
    }

    pub fn apply_message(&mut self, message: &Message) {
        match message.role {
            Role::User => self.apply_user_message(message),
            Role::Assistant => self.apply_assistant_message(message),
        }
        self.turn = self.turn.saturating_add(1);
    }

    fn apply_user_message(&mut self, message: &Message) {
        if is_tool_result(message) {
            self.apply_tool_results(message);
            return;
        }
        let text = message.get_all_text();
        let text = text.trim();
        if text.is_empty() || text.contains("<compact-summary>") {
            return;
        }

        self.objective = Some(truncate(text));
        self.next_action =
            Some("Continue from the latest user instruction using existing evidence.".to_string());
        self.focus = FocusState::Active;
        self.extract_constraints(text);
    }

    fn apply_assistant_message(&mut self, message: &Message) {
        let mut saw_tool = false;
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                if let ContentBlock::ToolUse { name, input, .. } = block {
                    saw_tool = true;
                    self.complexity.tool_calls = self.complexity.tool_calls.saturating_add(1);
                    if let Some(path) = input
                        .get("file_path")
                        .or_else(|| input.get("path"))
                        .and_then(|value| value.as_str())
                    {
                        let path = PathBuf::from(path);
                        if !self.changed_files.contains(&path) && is_mutating_tool(name) {
                            self.changed_files.push(path);
                            self.complexity.files_touched =
                                self.complexity.files_touched.saturating_add(1);
                        }
                    }
                    self.next_action = Some(format!(
                        "Process the `{name}` result before taking another action."
                    ));
                }
            }
        }
        if !saw_tool && !message.get_all_text().trim().is_empty() {
            self.next_action = Some(
                "Check whether the requested work is complete and report evidence.".to_string(),
            );
        }
    }

    fn apply_tool_results(&mut self, message: &Message) {
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                if let ContentBlock::ToolResult {
                    content, is_error, ..
                } = block
                {
                    if is_error.unwrap_or(false) {
                        self.complexity.failed_tools =
                            self.complexity.failed_tools.saturating_add(1);
                        self.failures.push(TaskFailure {
                            source: "tool".to_string(),
                            summary: truncate(&tool_result_text(content)),
                        });
                        self.focus = FocusState::Blocked;
                        self.next_action = Some(
                            "Diagnose the latest tool failure and retry with a changed approach."
                                .to_string(),
                        );
                    } else {
                        self.focus = FocusState::Active;
                        self.next_action = Some(
                            "Use the successful tool evidence to advance the current task."
                                .to_string(),
                        );
                    }
                }
            }
        }
    }

    fn extract_constraints(&mut self, text: &str) {
        let lower = text.to_ascii_lowercase();
        if [
            "must ", "must not", "don't ", "do not ", "never ", "only ", "without ", "keep ",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            let constraint = truncate(text);
            if !self.constraints.contains(&constraint) {
                self.constraints.push(constraint);
            }
        }
    }

    fn trim(&mut self) {
        self.constraints.truncate(MAX_ITEMS);
        self.decisions.truncate(MAX_ITEMS);
        self.changed_files.truncate(MAX_ITEMS);
        self.failures.truncate(MAX_ITEMS);
    }

    pub fn set_runtime_evidence(
        &mut self,
        plan_step: Option<String>,
        validation: Option<String>,
        snapshot_files: Vec<PathBuf>,
    ) {
        self.runtime.plan_step = plan_step;
        self.runtime.validation = validation.clone();
        self.runtime.snapshot_files = snapshot_files;
        self.runtime.snapshot_files.truncate(MAX_ITEMS);
        if !self.runtime.snapshot_files.is_empty() {
            self.changed_files = self.runtime.snapshot_files.clone();
            self.complexity.files_touched = self.changed_files.len();
        }
        if validation
            .as_deref()
            .is_some_and(|value| value.contains("failed"))
        {
            self.focus = FocusState::Blocked;
            self.failures.push(TaskFailure {
                source: "validation".to_string(),
                summary: validation.unwrap_or_default(),
            });
            self.next_action = Some(
                "Diagnose the failed validation and change the implementation approach."
                    .to_string(),
            );
        }
        self.trim();
    }

    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        if let Some(objective) = &self.objective {
            lines.push(format!("Objective: {objective}"));
        }
        lines.push(format!("Focus: {}", self.focus.as_str()));
        if let Some(step) = self
            .runtime
            .plan_step
            .as_ref()
            .or(self.active_step.as_ref())
        {
            lines.push(format!("Active step: {step}"));
        }
        if let Some(validation) = &self.runtime.validation {
            lines.push(format!("Validation: {validation}"));
        }
        if !self.constraints.is_empty() {
            lines.push(format!("Constraints: {}", self.constraints.join(" | ")));
        }
        if !self.changed_files.is_empty() {
            let files = self
                .changed_files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>();
            lines.push(format!("Changed files: {}", files.join(", ")));
        }
        if !self.failures.is_empty() {
            lines.push(format!(
                "Recent failures: {}",
                self.failures
                    .iter()
                    .map(|f| format!("{}: {}", f.source, f.summary))
                    .collect::<Vec<_>>()
                    .join(" | ")
            ));
        }
        lines.push(format!(
            "Activity: {} tool calls, {} failed",
            self.complexity.tool_calls, self.complexity.failed_tools
        ));
        if let Some(next) = &self.next_action {
            lines.push(format!("Next action: {next}"));
        }
        lines.push("Preserve the objective and constraints. Do not restart completed exploration or expand scope without evidence.".to_string());
        lines.join("\n")
    }
}

fn is_tool_result(message: &Message) -> bool {
    matches!(&message.content, MessageContent::Blocks(blocks) if blocks.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })))
}

fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "file_write"
            | "file_edit"
            | "batch_edit"
            | "apply_patch"
            | "write_file"
            | "edit_file"
            | "Edit"
            | "Write"
            | "BatchEdit"
            | "ApplyPatch"
    )
}

fn tool_result_text(content: &clawde_core::types::ToolResultContent) -> String {
    match content {
        clawde_core::types::ToolResultContent::Text(text) => text.clone(),
        clawde_core::types::ToolResultContent::Blocks(_) => {
            "tool returned structured content".to_string()
        }
    }
}

fn truncate(value: &str) -> String {
    if value.chars().count() <= MAX_TEXT_CHARS {
        return value.to_string();
    }
    format!(
        "{}…",
        value.chars().take(MAX_TEXT_CHARS).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_instruction_replaces_objective_and_preserves_constraints() {
        let messages = vec![
            Message::user("Implement the parser. Do not add dependencies."),
            Message::assistant("I inspected the parser."),
            Message::user("Use the existing tokenizer and keep the API stable."),
        ];
        let state = TaskState::from_messages(&messages);
        assert_eq!(
            state.objective.as_deref(),
            Some("Use the existing tokenizer and keep the API stable.")
        );
        assert_eq!(state.constraints.len(), 2);
        assert_eq!(state.focus, FocusState::Active);
    }

    #[test]
    fn runtime_evidence_updates_plan_validation_and_snapshot_files() {
        let mut state = TaskState::from_messages(&[Message::user("Implement the feature")]);
        state.set_runtime_evidence(
            Some("Implement parser changes".to_string()),
            Some("tests passed".to_string()),
            vec![PathBuf::from("src/parser.rs")],
        );
        assert_eq!(
            state.runtime.plan_step.as_deref(),
            Some("Implement parser changes")
        );
        assert_eq!(state.runtime.validation.as_deref(), Some("tests passed"));
        assert_eq!(state.changed_files, vec![PathBuf::from("src/parser.rs")]);
        assert_eq!(state.focus, FocusState::Active);
    }

    #[test]
    fn failed_validation_blocks_focus() {
        let mut state = TaskState::from_messages(&[Message::user("Implement the feature")]);
        state.set_runtime_evidence(None, Some("tests failed: parser".to_string()), Vec::new());
        assert_eq!(state.focus, FocusState::Blocked);
        assert_eq!(state.failures.len(), 1);
    }

    #[test]
    fn failed_tool_changes_focus_and_records_failure() {
        let message = Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "tool-1".to_string(),
            content: clawde_core::types::ToolResultContent::Text("not found".to_string()),
            is_error: Some(true),
        }]);
        let state = TaskState::from_messages(&[message]);
        assert_eq!(state.focus, FocusState::Blocked);
        assert_eq!(state.failures.len(), 1);
    }
}
