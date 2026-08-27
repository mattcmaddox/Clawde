// AskUserQuestion tool: ask the human operator a question and wait for a response.
//
// In interactive mode the tool sends a `UserQuestionEvent` through the
// channel stored in `ToolContext`, suspending the query loop until the TUI
// collects the user's answer and sends it back through the oneshot reply
// channel.  In non-interactive / headless mode execution returns an error.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult, UserQuestionEvent};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

pub struct AskUserQuestionTool;

#[derive(Debug, Deserialize)]
struct AskUserInput {
    question: String,
    #[serde(default)]
    options: Option<Vec<String>>,
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_ASK_USER
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for their response. Use this when you \
         need clarification, confirmation, or additional information from the user. \
         The question will be displayed in the terminal and the user can type their \
         answer or select from options."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of predefined choices. When present the user can \
                                    select an option or type a custom answer."
                }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: AskUserInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        debug!(question = %params.question, "Asking user");

        if ctx.non_interactive {
            return ToolResult::error(
                "Cannot ask user questions in non-interactive mode".to_string(),
            );
        }

        // Autopilot (Phase 4B): never block on a dialog. Record the question
        // as a deferred item and return immediately; the user answers it later.
        if let Some(autonomy) = &ctx.autonomy {
            let mut state = autonomy.lock();
            if state.is_active(&ctx.session_id) {
                let project_root = ctx.working_dir.display().to_string();
                return match state.enqueue_question(
                    &ctx.session_id,
                    &project_root,
                    params.question.clone(),
                    params.options.clone(),
                ) {
                    Some(item) => {
                        let meta = json!({
                            "type": "ask_user_deferred",
                            "id": item.id,
                            "question": params.question,
                            "options": params.options,
                        });
                        ToolResult::success(format!(
                            "Question deferred for user review as {}. Continue with safe work; the user will answer it when they return.",
                            item.id
                        ))
                        .with_metadata(meta)
                    }
                    None => ToolResult::error(
                        "Denied: the autopilot review queue is full. Continue with safe work; do not ask again."
                            .to_string(),
                    ),
                };
            }
        }

        // Route through the TUI side-channel when available.
        if let Some(ref tx) = ctx.user_question_tx {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<String>();
            let event = UserQuestionEvent {
                question: params.question.clone(),
                options: params.options.clone(),
                reply_tx,
            };
            if tx.send(event).is_err() {
                return ToolResult::error("Question channel closed".to_string());
            }
            match reply_rx.await {
                Ok(answer) if answer.is_empty() => {
                    ToolResult::success("The user dismissed the question without answering.")
                }
                Ok(answer) => ToolResult::success(format!("The user answered: {}", answer)),
                Err(_) => {
                    ToolResult::error("Question channel closed before answer received".to_string())
                }
            }
        } else {
            // No channel wired up — return metadata so a future TUI integration
            // can intercept this result (legacy / non-TUI path).
            let meta = json!({
                "question": params.question,
                "options": params.options,
                "type": "ask_user",
            });
            ToolResult::success(format!("Question: {}", params.question)).with_metadata(meta)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn interactive_ctx() -> ToolContext {
        let mut ctx = crate::test_support::allow_all_context(std::path::PathBuf::from("."));
        ctx.non_interactive = false;
        ctx
    }

    #[tokio::test]
    async fn non_interactive_mode_errors() {
        let res = AskUserQuestionTool
            .execute(
                json!({ "question": "continue?" }),
                &crate::test_support::allow_all_context(".".into()),
            )
            .await;
        assert!(res.is_error);
        assert!(
            res.content.contains("non-interactive mode"),
            "{}",
            res.content
        );
    }

    #[tokio::test]
    async fn no_channel_uses_legacy_metadata_path() {
        let res = AskUserQuestionTool
            .execute(
                json!({ "question": "pick one", "options": ["a", "b"] }),
                &interactive_ctx(),
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert_eq!(res.content, "Question: pick one");
        let meta = res.metadata.unwrap();
        assert_eq!(meta["type"], "ask_user");
        assert_eq!(meta["question"], "pick one");
        assert_eq!(meta["options"], json!(["a", "b"]));
    }

    #[tokio::test]
    async fn invalid_input_errors() {
        let res = AskUserQuestionTool
            .execute(json!({ "question": 42 }), &interactive_ctx())
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("Invalid input"), "{}", res.content);
    }

    #[tokio::test]
    async fn autopilot_defers_question_without_awaiting_reply() {
        let mut ctx = interactive_ctx();
        ctx.session_id = "sess-1".to_string();
        let state = Arc::new(parking_lot::Mutex::new(
            clawde_core::autonomy::AutonomyState::new("sess-1"),
        ));
        state.lock().start_autopilot("sess-1");
        ctx.autonomy = Some(state.clone());

        // A live question channel exists, but autopilot must NOT use it.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UserQuestionEvent>();
        ctx.user_question_tx = Some(tx);

        let res = AskUserQuestionTool
            .execute(
                json!({ "question": "continue?", "options": ["yes", "no"] }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert!(res.content.contains("AP-001"), "{}", res.content);

        // No UserQuestionEvent may be emitted to the TUI channel.
        let channel_used = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
            .await
            .is_ok();
        assert!(
            !channel_used,
            "autopilot must not block on a question channel"
        );

        let state = state.lock();
        assert_eq!(state.pending_count(), 1);
        let item = &state.items[0];
        assert_eq!(item.kind, clawde_core::autonomy::DeferredKind::UserQuestion);
        match &item.payload {
            clawde_core::autonomy::DeferredPayload::UserQuestion { question, options } => {
                assert_eq!(question, "continue?");
                assert_eq!(
                    options.as_ref(),
                    Some(&vec!["yes".to_string(), "no".to_string()])
                );
            }
            other => panic!("expected UserQuestion payload, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn channel_path_returns_user_answer() {
        let mut ctx = interactive_ctx();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UserQuestionEvent>();
        ctx.user_question_tx = Some(tx);

        // Answer the question as the user would from the TUI side-channel.
        let responder = tokio::spawn(async move {
            if let Some(event) = rx.recv().await {
                let _ = event.reply_tx.send("yes".to_string());
            }
        });

        let res = AskUserQuestionTool
            .execute(json!({ "question": "proceed?" }), &ctx)
            .await;
        responder.await.unwrap();

        assert!(!res.is_error, "{}", res.content);
        assert_eq!(res.content, "The user answered: yes");
    }
}
