//! Deterministic local HTTP mock for the OpenAI-compatible chat-completions
//! surface.
//!
//! This is the Phase 6 mock-provider harness from
//! `docs/free-provider-agent-reliability-plan.md`: a scripted per-connection
//! server that lets integration tests drive the *real* `OpenAiCompatProvider`
//! HTTP + SSE parsing against controlled success, pre-first-byte failure, and
//! mid-stream truncation responses — no network, no provider keys.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// One scripted response, served once to the next inbound connection.
#[derive(Clone, Debug)]
pub enum ScriptedResponse {
    /// A complete non-streaming JSON response with an arbitrary status — used
    /// both for pre-first-byte HTTP errors (500 / 429 / 401 / 413 / 400 with
    /// a structured `error.type` code) and for non-streaming 200 success.
    Json {
        status: u16,
        reason: &'static str,
        body: String,
    },
    /// A complete `text/event-stream` body ending in `data: [DONE]`.
    SseStream { frames: Vec<String> },
    /// An SSE stream whose `Content-Length` promises 64 more bytes than are
    /// actually written before the connection closes. reqwest/hyper surfaces
    /// the underrun as a read error *after* the already-written frames have
    /// been parsed into stream events — the deterministic stand-in for a
    /// provider dropping the connection mid-stream.
    SseTruncated { frames: Vec<String> },
}

/// A minimal record of one inbound request, for asserting dispatch order.
#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub method: String,
    pub path: String,
    pub body: String,
}

/// A scripted, single-accept-loop HTTP mock bound to an ephemeral loopback port.
pub struct MockServer {
    pub base_url: String,
    requests: Arc<Mutex<Vec<RequestRecord>>>,
    _thread: std::thread::JoinHandle<()>,
}

impl MockServer {
    /// Bind and start serving `responses` in FIFO order (one per connection).
    ///
    /// When more connections arrive than were scripted, each extra connection
    /// gets a 500 so a test that unexpectedly over-dispatches fails loudly
    /// instead of hanging.
    pub fn new(responses: Vec<ScriptedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let base_url = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("local addr").port()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));

        let queue_clone = Arc::clone(&queue);
        let requests_clone = Arc::clone(&requests);
        let expected = queue_clone.lock().unwrap().len();
        let thread = std::thread::spawn(move || {
            // Serve every scripted response plus a little slack, then stop so
            // the accept loop cannot linger past the script.
            for mut stream in listener.incoming().take(expected + 4).flatten() {
                let record = read_request(&mut stream);
                if let Some(record) = record {
                    requests_clone.lock().unwrap().push(record);
                }
                let response = queue_clone.lock().unwrap().pop_front().unwrap_or_else(|| {
                    ScriptedResponse::Json {
                        status: 500,
                        reason: "Internal Server Error",
                        body: r#"{"error":{"message":"no scripted response"}}"#.to_string(),
                    }
                });
                write_response(&mut stream, &response);
            }
        });

        Self {
            base_url,
            requests,
            _thread: thread,
        }
    }

    /// Snapshot the requests observed so far, in arrival order.
    pub fn requests(&self) -> Vec<RequestRecord> {
        self.requests.lock().unwrap().clone()
    }
}

/// Read one HTTP request (headers + Content-Length body) off `stream`.
fn read_request(stream: &mut TcpStream) -> Option<RequestRecord> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut method = String::new();
    let mut path = String::new();
    let mut content_length = 0usize;
    for (index, line) in header_text.split("\r\n").enumerate() {
        if index == 0 {
            let mut parts = line.split_whitespace();
            method = parts.next().unwrap_or("").to_string();
            path = parts.next().unwrap_or("").to_string();
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    Some(RequestRecord {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn write_response(stream: &mut TcpStream, response: &ScriptedResponse) {
    let (head, body): (String, Vec<u8>) = match response {
        ScriptedResponse::Json {
            status,
            reason,
            body,
        } => {
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\n\
                 content-type: application/json\r\n\
                 content-length: {}\r\n\
                 connection: close\r\n\
                 \r\n",
                body.len()
            );
            (head, body.as_bytes().to_vec())
        }
        ScriptedResponse::SseStream { frames } => {
            let body = sse_body(frames);
            let head = format!(
                "HTTP/1.1 200 OK\r\n\
                 content-type: text/event-stream\r\n\
                 content-length: {}\r\n\
                 connection: close\r\n\
                 \r\n",
                body.len()
            );
            (head, body.into_bytes())
        }
        ScriptedResponse::SseTruncated { frames } => {
            let body = sse_body(frames);
            // Promise 64 bytes more than we actually write; the early FIN
            // makes the body read fail once the written frames are consumed.
            let head = format!(
                "HTTP/1.1 200 OK\r\n\
                 content-type: text/event-stream\r\n\
                 content-length: {}\r\n\
                 connection: close\r\n\
                 \r\n",
                body.len() + 64
            );
            (head, body.into_bytes())
        }
    };

    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
    // Dropping the stream closes the connection (FIN). For `SseTruncated`,
    // that FIN arrives before the promised Content-Length is satisfied.
}

fn sse_body(frames: &[String]) -> String {
    frames.concat()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// SSE frame builders
// ---------------------------------------------------------------------------

fn sse_frame(value: serde_json::Value) -> String {
    format!("data: {}\n\n", value)
}

/// First delta frame: carries `id` + `model` so the decoder emits
/// `MessageStart` / `ContentBlockStart` before the first text.
pub fn sse_first_delta(model: &str, text: &str) -> String {
    sse_frame(serde_json::json!({
        "id": "chatcmpl-mock",
        "model": model,
        "choices": [{"delta": {"content": text}}],
    }))
}

pub fn sse_text_delta(text: &str) -> String {
    sse_frame(serde_json::json!({
        "choices": [{"delta": {"content": text}}],
    }))
}

pub fn sse_finish() -> String {
    sse_frame(serde_json::json!({
        "choices": [{"delta": {}, "finish_reason": "stop"}],
    }))
}

pub fn sse_done() -> String {
    "data: [DONE]\n\n".to_string()
}

/// A complete single-chunk text completion stream.
pub fn text_stream(model: &str, text: &str) -> Vec<String> {
    vec![sse_first_delta(model, text), sse_finish(), sse_done()]
}
