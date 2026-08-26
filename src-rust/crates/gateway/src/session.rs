//! Response session state (D5 — ephemeral in-memory only).
//!
//! One bounded LRU (capacity + TTL) backs BOTH `store: false` and
//! `store: true` requests; neither writes to disk in v1. A continuation
//! (`previous_response_id`) hydrates the transcript as
//! `prev.input + prev.output + new input` per Open Responses, and is
//! serialized per session id (D11) so concurrent continuations cannot corrupt
//! the append-only transcript.
//!
//! Eviction (capacity or TTL) is the GC: a continuation referencing an
//! evicted session fails with `previous_response_not_found`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clawde_core::types::Message;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

/// A stored response session.
#[derive(Debug, Clone)]
pub struct ResponseSession {
    /// `resp_…` id (the `previous_response_id` key).
    pub id: String,
    /// The input messages sent with this response.
    pub input: Vec<Message>,
    /// The output items produced (Open Responses item JSON), in order.
    pub output: Vec<Value>,
    /// Wall-clock creation time (TTL sweeps use this).
    pub created_at: Instant,
}

/// Bounded, TTL'd in-memory session store (D5).
pub struct SessionStore {
    inner: Mutex<SessionInner>,
    capacity: usize,
    ttl: Duration,
    /// Continuation locks keyed by session id (D11). `Arc` so a lock can be
    /// cloned out of the map and held across the whole request.
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

struct SessionInner {
    map: HashMap<String, ResponseSession>,
    /// Recency order: front = most recently used.
    order: VecDeque<String>,
}

impl SessionStore {
    pub fn new(capacity: usize, ttl_secs: u64) -> Self {
        Self {
            inner: Mutex::new(SessionInner {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
            capacity: capacity.max(1),
            ttl: Duration::from_secs(ttl_secs),
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Store a session (always — `store: true` and `store: false` both retain
    /// for continuation, per D5). Evicts the least-recently-used entry beyond
    /// capacity.
    pub fn put(&self, session: ResponseSession) {
        let mut inner = self.inner.lock();
        self.evict_expired_locked(&mut inner);
        let id = session.id.clone();
        if inner.map.contains_key(&id) {
            // Refresh recency.
            inner.order.retain(|k| k != &id);
        }
        inner.order.push_front(id.clone());
        inner.map.insert(id.clone(), session);
        while inner.map.len() > self.capacity {
            if let Some(oldest) = inner.order.pop_back() {
                inner.map.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Look up a session by id; refreshes recency and drops expired entries.
    pub fn get(&self, id: &str) -> Option<ResponseSession> {
        let mut inner = self.inner.lock();
        self.evict_expired_locked(&mut inner);
        let session = inner.map.get(id)?.clone();
        inner.order.retain(|k| k != id);
        inner.order.push_front(id.to_string());
        Some(session)
    }

    /// Take the continuation lock for a session id (D11). The returned guard
    /// is held by the caller for the duration of the request; a concurrent
    /// continuation on the same id waits for it.
    pub async fn continuation_lock(&self, session_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock();
            locks
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    fn evict_expired_locked(&self, inner: &mut SessionInner) {
        let now = Instant::now();
        inner.order.retain(|id| {
            let expired = inner
                .map
                .get(id)
                .is_some_and(|s| now.duration_since(s.created_at) > self.ttl);
            if expired {
                inner.map.remove(id);
            }
            !expired
        });
    }

    /// Current retained count (tests / status surface).
    pub fn len(&self) -> usize {
        self.inner.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Convert a session's output items back into transcript messages so a
/// continuation can sample over `prev.input + prev.output + new input`.
///
/// Adjacent assistant message + `function_call` items merge into one assistant
/// `Message` (text + ToolUse blocks); `function_call_output` items become user
/// `ToolResult` messages. `reasoning` items are dropped (the model re-thinks).
pub fn output_items_to_messages(items: &[Value]) -> Vec<Message> {
    use clawde_core::types::{ContentBlock, MessageContent, Role, ToolResultContent};

    let mut out: Vec<Message> = Vec::new();
    // Assistant blocks (text + tool_use) accumulate until a user turn or the
    // end of the list; tool results accumulate into the next user message.
    let mut pending_assistant: Vec<ContentBlock> = Vec::new();
    let mut pending_user: Vec<ContentBlock> = Vec::new();

    fn flush_assistant(out: &mut Vec<Message>, blocks: &mut Vec<ContentBlock>) {
        if !blocks.is_empty() {
            out.push(Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(std::mem::take(blocks)),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            });
        }
    }

    fn flush_user(out: &mut Vec<Message>, blocks: &mut Vec<ContentBlock>) {
        if !blocks.is_empty() {
            out.push(Message {
                role: Role::User,
                content: MessageContent::Blocks(std::mem::take(blocks)),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            });
        }
    }

    for item in items {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        // Untyped {role, content} chat-style items are messages (Open
        // Responses tolerates the chat-completions input form).
        let item_type = if item_type.is_empty() && item.get("role").is_some() {
            "message"
        } else {
            item_type
        };
        match item_type {
            "message" => {
                let role = match item.get("role").and_then(|v| v.as_str()) {
                    Some("user") | Some("system") => Role::User,
                    _ => Role::Assistant,
                };
                // Message content is either a plain string or an array of
                // typed parts (Open Responses tolerates both).
                let text = match item.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                match role {
                    Role::User => {
                        // A user turn closes any pending assistant group and
                        // merges with any pending tool results.
                        flush_assistant(&mut out, &mut pending_assistant);
                        if !text.is_empty() {
                            pending_user.push(ContentBlock::Text { text });
                        }
                        flush_user(&mut out, &mut pending_user);
                    }
                    Role::Assistant => {
                        // A new assistant turn closes any pending tool
                        // results first — they belong to the previous turn
                        // and must precede this message for role alternation.
                        flush_user(&mut out, &mut pending_user);
                        if !text.is_empty() {
                            pending_assistant.push(ContentBlock::Text { text });
                        }
                    }
                }
            }
            "function_call" => {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or(id);
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                pending_assistant.push(ContentBlock::ToolUse {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    input: args,
                    thought_signature: None,
                });
            }
            "function_call_output" => {
                flush_assistant(&mut out, &mut pending_assistant);
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let output = item
                    .get("output")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                pending_user.push(ContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: ToolResultContent::Text(output),
                    is_error: None,
                });
            }
            // "reasoning" and unknown/provider items are context noise for the
            // continuation; drop them.
            _ => {}
        }
    }
    flush_assistant(&mut out, &mut pending_assistant);
    flush_user(&mut out, &mut pending_user);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_session(id: &str) -> ResponseSession {
        ResponseSession {
            id: id.to_string(),
            input: vec![Message::user("hello")],
            output: vec![
                json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}),
                json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read", "arguments": "{\"path\":\"/x\"}"}),
                json!({"type": "function_call_output", "call_id": "call_1", "output": "file contents"}),
            ],
            created_at: Instant::now(),
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let store = SessionStore::new(4, 3600);
        store.put(sample_session("resp_a"));
        store.put(sample_session("resp_b"));
        assert_eq!(store.len(), 2);
        let got = store.get("resp_a").expect("session present");
        assert_eq!(got.id, "resp_a");
        assert_eq!(got.output.len(), 3);
    }

    #[test]
    fn lru_evicts_oldest_beyond_capacity() {
        let store = SessionStore::new(2, 3600);
        store.put(sample_session("resp_a"));
        store.put(sample_session("resp_b"));
        // Touch resp_a so resp_b becomes the LRU victim.
        store.get("resp_a");
        store.put(sample_session("resp_c"));
        assert!(store.get("resp_a").is_some());
        assert!(store.get("resp_b").is_none());
        assert!(store.get("resp_c").is_some());
    }

    #[test]
    fn missing_session_returns_none() {
        let store = SessionStore::new(4, 3600);
        assert!(store.get("resp_missing").is_none());
    }

    #[test]
    fn output_items_rebuild_transcript() {
        let session = sample_session("resp_a");
        let msgs = output_items_to_messages(&session.output);
        assert_eq!(msgs.len(), 2);
        // Assistant message carries the text + the merged ToolUse block.
        let assistant = &msgs[0];
        use clawde_core::types::{ContentBlock, MessageContent};
        match &assistant.content {
            MessageContent::Blocks(blocks) => {
                assert!(matches!(blocks[0], ContentBlock::Text { .. }));
                assert!(matches!(blocks[1], ContentBlock::ToolUse { .. }));
            }
            _ => panic!("expected blocks"),
        }
        // Tool result message.
        match &msgs[1].content {
            MessageContent::Blocks(blocks) => {
                assert!(matches!(blocks[0], ContentBlock::ToolResult { .. }));
            }
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn continuation_locks_serialize_same_id() {
        let store = Arc::new(SessionStore::new(4, 3600));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let s1 = store.clone();
            let s2 = store.clone();
            let (tx1, rx1) = tokio::sync::oneshot::channel();
            let (tx2, mut rx2) = tokio::sync::oneshot::channel();
            let (tx3, rx3) = tokio::sync::oneshot::channel();
            // Task 1 grabs the lock and holds it until released.
            let t1 = tokio::spawn(async move {
                let _g = s1.continuation_lock("resp_a").await;
                tx1.send(()).unwrap();
                rx3.await.unwrap();
            });
            rx1.await.unwrap();
            // Spawn the contender only after task 1 provably holds the lock,
            // so it cannot race ahead and win acquisition.
            let t2 = tokio::spawn(async move {
                let _g = s2.continuation_lock("resp_a").await;
                tx2.send(()).unwrap();
            });
            let raced = tokio::time::timeout(std::time::Duration::from_millis(100), &mut rx2).await;
            assert!(
                raced.is_err(),
                "second continuation must wait for the first"
            );
            tx3.send(()).unwrap();
            rx2.await.unwrap();
            t1.await.unwrap();
            t2.await.unwrap();
        });
    }
}
