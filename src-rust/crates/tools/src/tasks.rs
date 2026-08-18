// Task management tools: TaskCreate, TaskGet, TaskUpdate, TaskList, TaskStop, TaskOutput.
//
// Implements a simple in-process task store backed by a global Arc<Mutex<HashMap>>.
// Tasks have id, subject, description, status, owner, blocks/blocked-by dependencies,
// and optional output.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Task store (global singleton)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Deleted,
    Running, // for background shell tasks
    Failed,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Deleted => "deleted",
            TaskStatus::Running => "running",
            TaskStatus::Failed => "failed",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    pub owner: Option<String>,
    /// IDs of tasks this task blocks (i.e., those tasks depend on this one completing).
    pub blocks: Vec<String>,
    /// IDs of tasks that must complete before this task can start.
    pub blocked_by: Vec<String>,
    pub metadata: Option<Value>,
    pub output: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Task {
    fn new(subject: impl Into<String>, description: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            subject: subject.into(),
            description: description.into(),
            status: TaskStatus::Pending,
            owner: None,
            blocks: vec![],
            blocked_by: vec![],
            metadata: None,
            output: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn to_summary_value(&self) -> Value {
        // Compute effective blocked_by (exclude completed tasks)
        let blocked_by = self.blocked_by.clone();
        json!({
            "id": self.id,
            "subject": self.subject,
            "status": self.status.to_string(),
            "owner": self.owner,
            "blocked_by": blocked_by,
        })
    }

    fn to_full_value(&self) -> Value {
        json!({
            "id": self.id,
            "subject": self.subject,
            "description": self.description,
            "status": self.status.to_string(),
            "owner": self.owner,
            "blocks": self.blocks,
            "blocked_by": self.blocked_by,
            "metadata": self.metadata,
            "output": self.output,
            "created_at": self.created_at.to_rfc3339(),
            "updated_at": self.updated_at.to_rfc3339(),
        })
    }
}

/// Global task store shared across all tool invocations.
/// Public so the TUI can access and display tasks.
pub static TASK_STORE: Lazy<Arc<DashMap<String, Task>>> = Lazy::new(|| Arc::new(DashMap::new()));

// ---------------------------------------------------------------------------
// TaskCreate
// ---------------------------------------------------------------------------

pub struct TaskCreateTool;

#[derive(Debug, Deserialize)]
struct TaskCreateInput {
    subject: String,
    description: String,
    #[serde(default)]
    metadata: Option<Value>,
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_TASK_CREATE
    }
    fn description(&self) -> &str {
        "Create a new task to track work items. Returns the task ID."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn stateful(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": { "type": "string", "description": "Brief title for the task" },
                "description": { "type": "string", "description": "Detailed description of what needs to be done" },
                "metadata": { "type": "object", "description": "Optional arbitrary metadata" }
            },
            "required": ["subject", "description"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: TaskCreateInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let mut task = Task::new(&params.subject, &params.description);
        task.metadata = params.metadata;
        let task_id = task.id.clone();

        debug!(task_id = %task_id, subject = %params.subject, "Creating task");
        TASK_STORE.insert(task_id.clone(), task);

        ToolResult::success(
            serde_json::to_string_pretty(&json!({
                "task_id": task_id,
                "subject": params.subject,
            }))
            .unwrap_or_default(),
        )
    }
}

// ---------------------------------------------------------------------------
// TaskGet
// ---------------------------------------------------------------------------

pub struct TaskGetTool;

#[derive(Debug, Deserialize)]
struct TaskGetInput {
    #[serde(alias = "taskId")]
    task_id: String,
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_TASK_GET
    }
    fn description(&self) -> &str {
        "Get full details of a task by ID."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task ID to retrieve" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: TaskGetInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        match TASK_STORE.get(&params.task_id) {
            Some(task) => ToolResult::success(
                serde_json::to_string_pretty(&task.to_full_value()).unwrap_or_default(),
            ),
            None => {
                ToolResult::success(serde_json::to_string_pretty(&json!(null)).unwrap_or_default())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TaskUpdate
// ---------------------------------------------------------------------------

pub struct TaskUpdateTool;

#[derive(Debug, Deserialize)]
struct TaskUpdateInput {
    #[serde(alias = "taskId")]
    task_id: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default, rename = "addBlocks")]
    add_blocks: Option<Vec<String>>,
    #[serde(default, rename = "addBlockedBy")]
    add_blocked_by: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    output: Option<String>,
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_TASK_UPDATE
    }
    fn description(&self) -> &str {
        "Update a task's properties (status, subject, description, etc.)."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn stateful(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task ID to update" },
                "subject": { "type": "string" },
                "description": { "type": "string" },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "deleted", "failed"]
                },
                "owner": { "type": "string" },
                "addBlocks": { "type": "array", "items": { "type": "string" } },
                "addBlockedBy": { "type": "array", "items": { "type": "string" } },
                "metadata": { "type": "object" },
                "output": { "type": "string" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: TaskUpdateInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let mut task = match TASK_STORE.get_mut(&params.task_id) {
            Some(t) => t,
            None => return ToolResult::error(format!("Task '{}' not found", params.task_id)),
        };

        let mut updated_fields: Vec<&str> = vec![];

        if let Some(subject) = &params.subject {
            task.subject = subject.clone();
            updated_fields.push("subject");
        }
        if let Some(desc) = &params.description {
            task.description = desc.clone();
            updated_fields.push("description");
        }
        if let Some(status_str) = &params.status {
            task.status = match status_str.as_str() {
                "pending" => TaskStatus::Pending,
                "in_progress" | "in-progress" => TaskStatus::InProgress,
                "completed" => TaskStatus::Completed,
                "deleted" => TaskStatus::Deleted,
                "running" => TaskStatus::Running,
                "failed" => TaskStatus::Failed,
                other => return ToolResult::error(format!("Unknown status: {}", other)),
            };
            updated_fields.push("status");
        }
        if let Some(owner) = &params.owner {
            task.owner = Some(owner.clone());
            updated_fields.push("owner");
        }
        if let Some(blocks) = &params.add_blocks {
            for b in blocks {
                if !task.blocks.contains(b) {
                    task.blocks.push(b.clone());
                }
            }
            updated_fields.push("blocks");
        }
        if let Some(blocked_by) = &params.add_blocked_by {
            for b in blocked_by {
                if !task.blocked_by.contains(b) {
                    task.blocked_by.push(b.clone());
                }
            }
            updated_fields.push("blocked_by");
        }
        if let Some(meta) = &params.metadata {
            task.metadata = Some(meta.clone());
            updated_fields.push("metadata");
        }
        if let Some(out) = &params.output {
            task.output = Some(out.clone());
            updated_fields.push("output");
        }

        task.updated_at = chrono::Utc::now();

        // Handle deletion
        let task_id = task.id.clone();
        let task_status = task.status.clone();
        drop(task); // release the lock

        if task_status == TaskStatus::Deleted {
            TASK_STORE.remove(&task_id);
        }

        ToolResult::success(
            serde_json::to_string_pretty(&json!({
                "success": true,
                "task_id": task_id,
                "updated_fields": updated_fields,
            }))
            .unwrap_or_default(),
        )
    }
}

// ---------------------------------------------------------------------------
// TaskList
// ---------------------------------------------------------------------------

pub struct TaskListTool;

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_TASK_LIST
    }
    fn description(&self) -> &str {
        "List all active tasks (excluding deleted/completed)."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "include_completed": {
                    "type": "boolean",
                    "description": "Include completed tasks (default false)"
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let include_completed = input
            .get("include_completed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let tasks: Vec<Value> = TASK_STORE
            .iter()
            .filter(|entry| {
                let status = &entry.value().status;
                match status {
                    TaskStatus::Deleted => false,
                    TaskStatus::Completed => include_completed,
                    _ => true,
                }
            })
            .map(|entry| entry.value().to_summary_value())
            .collect();

        ToolResult::success(serde_json::to_string_pretty(&tasks).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// TaskStop
// ---------------------------------------------------------------------------

pub struct TaskStopTool;

#[derive(Debug, Deserialize)]
struct TaskStopInput {
    #[serde(alias = "shell_id")]
    task_id: String,
}

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_TASK_STOP
    }
    fn description(&self) -> &str {
        "Stop a running background task."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "ID of the task to stop" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: TaskStopInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        match TASK_STORE.get_mut(&params.task_id) {
            Some(mut task) => {
                if task.status != TaskStatus::Running && task.status != TaskStatus::InProgress {
                    return ToolResult::error(format!(
                        "Task '{}' is not running (status: {})",
                        params.task_id, task.status
                    ));
                }
                task.status = TaskStatus::Completed;
                task.updated_at = chrono::Utc::now();
                ToolResult::success(
                    serde_json::to_string_pretty(&json!({
                        "message": "Task stopped",
                        "task_id": params.task_id,
                    }))
                    .unwrap_or_default(),
                )
            }
            None => ToolResult::error(format!("Task '{}' not found", params.task_id)),
        }
    }
}

// ---------------------------------------------------------------------------
// TaskOutput
// ---------------------------------------------------------------------------

pub struct TaskOutputTool;

#[derive(Debug, Deserialize)]
struct TaskOutputInput {
    task_id: String,
    #[serde(default = "default_block")]
    block: bool,
}

fn default_block() -> bool {
    true
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_TASK_OUTPUT
    }
    fn description(&self) -> &str {
        "Get the output of a task."
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task ID to get output for" },
                "block": { "type": "boolean", "description": "Wait for task to complete (default true)" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: TaskOutputInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        match TASK_STORE.get(&params.task_id) {
            Some(task) => {
                let retrieval_status = match &task.status {
                    TaskStatus::Completed | TaskStatus::Failed => "success",
                    TaskStatus::Running | TaskStatus::InProgress => {
                        if params.block {
                            "success"
                        } else {
                            "not_ready"
                        }
                    }
                    _ => "success",
                };
                ToolResult::success(
                    serde_json::to_string_pretty(&json!({
                        "retrieval_status": retrieval_status,
                        "task": task.to_full_value(),
                    }))
                    .unwrap_or_default(),
                )
            }
            None => ToolResult::error(format!("Task '{}' not found", params.task_id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_new_defaults_to_pending() {
        let t = Task::new("subject", "description");
        assert_eq!(t.status, TaskStatus::Pending);
        assert_eq!(t.subject, "subject");
        assert_eq!(t.description, "description");
        assert!(t.owner.is_none());
        assert!(t.blocks.is_empty());
        assert!(t.blocked_by.is_empty());
        assert!(!t.id.is_empty());
    }

    #[test]
    fn task_status_display_strings() {
        assert_eq!(TaskStatus::Pending.to_string(), "pending");
        assert_eq!(TaskStatus::InProgress.to_string(), "in_progress");
        assert_eq!(TaskStatus::Completed.to_string(), "completed");
        assert_eq!(TaskStatus::Deleted.to_string(), "deleted");
        assert_eq!(TaskStatus::Running.to_string(), "running");
        assert_eq!(TaskStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn task_summary_value_shape() {
        let t = Task::new("subject", "description");
        let v = t.to_summary_value();
        assert_eq!(v["id"], t.id);
        assert_eq!(v["subject"], "subject");
        assert_eq!(v["status"], "pending");
        assert_eq!(v["owner"], Value::Null);
        assert_eq!(v["blocked_by"], json!([]));
    }

    #[test]
    fn task_full_value_shape() {
        let t = Task::new("subject", "description");
        let v = t.to_full_value();
        assert_eq!(v["description"], "description");
        assert_eq!(v["blocks"], json!([]));
        assert_eq!(v["output"], Value::Null);
        assert_eq!(v["status"], "pending");
    }

    #[test]
    fn task_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(TaskStatus::InProgress).unwrap(),
            json!("in_progress")
        );
        assert_eq!(
            serde_json::to_value(TaskStatus::Running).unwrap(),
            json!("running")
        );
        assert_eq!(
            serde_json::to_value(TaskStatus::Deleted).unwrap(),
            json!("deleted")
        );
    }

    // ---- global-store execute path ---------------------------------------

    /// Serialises tests against `TASK_STORE`: it is a process-global singleton
    /// shared by every task tool, so parallel tests would otherwise observe
    /// each other's tasks. Each test starts and ends with an empty store.
    static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run a future against a freshly cleared `TASK_STORE`, restoring the
    /// empty state afterwards so no task leaks into the next test.
    #[allow(clippy::await_holding_lock)]
    // The guard must span the whole future: it serialises TASK_STORE access
    // across all task tests (same convention as the ENV_LOCK guards used for
    // env-mutating tests). Test-only, single acquisition, no re-entrancy.
    async fn with_empty_store<T>(f: impl FnOnce() -> T) -> T::Output
    where
        T: std::future::Future,
    {
        let _lock = STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        TASK_STORE.clear();
        let out = f().await;
        TASK_STORE.clear();
        out
    }

    fn ctx() -> crate::ToolContext {
        crate::test_support::allow_all_context(std::path::PathBuf::from("."))
    }

    fn task_id_from(content: &str) -> String {
        let v: Value = serde_json::from_str(content).expect("task create JSON");
        v["task_id"].as_str().expect("task_id").to_string()
    }

    async fn create_task(subject: &str) -> String {
        let res = TaskCreateTool
            .execute(json!({ "subject": subject, "description": "d" }), &ctx())
            .await;
        assert!(!res.is_error, "{}", res.content);
        task_id_from(&res.content)
    }

    #[tokio::test]
    async fn create_then_get_round_trip() {
        with_empty_store(|| async move {
            let res = TaskCreateTool
                .execute(
                    json!({ "subject": "ship it", "description": "do the thing", "metadata": { "k": 1 } }),
                    &ctx(),
                )
                .await;
            assert!(!res.is_error, "{}", res.content);
            let id = task_id_from(&res.content);

            let got = TaskGetTool.execute(json!({ "task_id": id }), &ctx()).await;
            assert!(!got.is_error, "{}", got.content);
            let v: Value = serde_json::from_str(&got.content).unwrap();
            assert_eq!(v["subject"], "ship it");
            assert_eq!(v["description"], "do the thing");
            assert_eq!(v["status"], "pending");
            assert_eq!(v["metadata"], json!({ "k": 1 }));
        })
        .await;
    }

    #[tokio::test]
    async fn get_missing_task_returns_null_success() {
        with_empty_store(|| async move {
            let res = TaskGetTool
                .execute(json!({ "task_id": "no-such-id" }), &ctx())
                .await;
            assert!(!res.is_error);
            assert_eq!(res.content.trim(), "null");
        })
        .await;
    }

    #[tokio::test]
    async fn update_changes_fields_and_status() {
        with_empty_store(|| async move {
            let id = create_task("before").await;
            let res = TaskUpdateTool
                .execute(
                    json!({ "task_id": id, "subject": "after", "status": "in_progress", "owner": "buffy" }),
                    &ctx(),
                )
                .await;
            assert!(!res.is_error, "{}", res.content);
            let v: Value = serde_json::from_str(&res.content).unwrap();
            assert_eq!(v["success"], true);
            assert_eq!(v["updated_fields"], json!(["subject", "status", "owner"]));

            let got: Value =
                serde_json::from_str(&TaskGetTool.execute(json!({ "task_id": id }), &ctx()).await.content)
                    .unwrap();
            assert_eq!(got["subject"], "after");
            assert_eq!(got["status"], "in_progress");
            assert_eq!(got["owner"], "buffy");
        })
        .await;
    }

    #[tokio::test]
    async fn update_missing_task_errors() {
        with_empty_store(|| async move {
            let res = TaskUpdateTool
                .execute(json!({ "task_id": "missing", "subject": "x" }), &ctx())
                .await;
            assert!(res.is_error);
            assert!(res.content.contains("not found"), "{}", res.content);
        })
        .await;
    }

    #[tokio::test]
    async fn update_status_deleted_removes_task() {
        with_empty_store(|| async move {
            let id = create_task("doomed").await;
            let res = TaskUpdateTool
                .execute(json!({ "task_id": id, "status": "deleted" }), &ctx())
                .await;
            assert!(!res.is_error, "{}", res.content);
            // Gone from the store: get returns null, list omits it.
            let got = TaskGetTool.execute(json!({ "task_id": id }), &ctx()).await;
            assert_eq!(got.content.trim(), "null");
            let list = TaskListTool.execute(json!({}), &ctx()).await;
            let v: Value = serde_json::from_str(&list.content).unwrap();
            assert_eq!(v, json!([]));
        })
        .await;
    }

    #[tokio::test]
    async fn list_filters_completed_by_default() {
        with_empty_store(|| async move {
            let keep = create_task("keep me").await;
            let done = create_task("done").await;
            TaskUpdateTool
                .execute(json!({ "task_id": done, "status": "completed" }), &ctx())
                .await;

            let list = TaskListTool.execute(json!({}), &ctx()).await;
            assert!(!list.is_error, "{}", list.content);
            let v: Value = serde_json::from_str(&list.content).unwrap();
            let ids: Vec<&str> = v
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, vec![keep.as_str()]);

            let all = TaskListTool
                .execute(json!({ "include_completed": true }), &ctx())
                .await;
            let v: Value = serde_json::from_str(&all.content).unwrap();
            assert_eq!(v.as_array().unwrap().len(), 2);
        })
        .await;
    }

    #[tokio::test]
    async fn stop_requires_running_task_and_completes_it() {
        with_empty_store(|| async move {
            let pending = create_task("idle").await;
            let res = TaskStopTool
                .execute(json!({ "task_id": pending }), &ctx())
                .await;
            assert!(res.is_error);
            assert!(res.content.contains("not running"), "{}", res.content);

            let running = create_task("busy").await;
            TaskUpdateTool
                .execute(json!({ "task_id": running, "status": "running" }), &ctx())
                .await;
            let stop = TaskStopTool
                .execute(json!({ "task_id": running }), &ctx())
                .await;
            assert!(!stop.is_error, "{}", stop.content);
            let got: Value = serde_json::from_str(
                &TaskGetTool
                    .execute(json!({ "task_id": running }), &ctx())
                    .await
                    .content,
            )
            .unwrap();
            assert_eq!(got["status"], "completed");
        })
        .await;
    }

    #[tokio::test]
    async fn stop_missing_task_errors() {
        with_empty_store(|| async move {
            let res = TaskStopTool
                .execute(json!({ "task_id": "ghost" }), &ctx())
                .await;
            assert!(res.is_error);
            assert!(res.content.contains("not found"), "{}", res.content);
        })
        .await;
    }

    #[tokio::test]
    async fn output_reports_task_and_status() {
        with_empty_store(|| async move {
            let id = create_task("producer").await;
            let res = TaskOutputTool
                .execute(json!({ "task_id": id, "block": false }), &ctx())
                .await;
            assert!(!res.is_error, "{}", res.content);
            let v: Value = serde_json::from_str(&res.content).unwrap();
            assert_eq!(v["retrieval_status"], "success");
            assert_eq!(v["task"]["subject"], "producer");
            assert_eq!(v["task"]["output"], Value::Null);

            TaskUpdateTool
                .execute(json!({ "task_id": id, "output": "done!" }), &ctx())
                .await;
            let v: Value = serde_json::from_str(
                &TaskOutputTool
                    .execute(json!({ "task_id": id }), &ctx())
                    .await
                    .content,
            )
            .unwrap();
            assert_eq!(v["task"]["output"], "done!");
        })
        .await;
    }

    #[tokio::test]
    async fn output_missing_task_errors() {
        with_empty_store(|| async move {
            let res = TaskOutputTool
                .execute(json!({ "task_id": "ghost" }), &ctx())
                .await;
            assert!(res.is_error);
            assert!(res.content.contains("not found"), "{}", res.content);
        })
        .await;
    }

    #[tokio::test]
    async fn create_invalid_input_errors() {
        with_empty_store(|| async move {
            let res = TaskCreateTool
                .execute(json!({ "subject": "no description" }), &ctx())
                .await;
            assert!(res.is_error);
            assert!(res.content.contains("Invalid input"), "{}", res.content);
        })
        .await;
    }
}
