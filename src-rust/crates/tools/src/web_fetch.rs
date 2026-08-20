// WebFetch tool: HTTP GET with HTML-to-text conversion and LLM-powered semantic extraction
// for edge cases (JS-heavy pages, minimal content).

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, warn};

pub struct WebFetchTool;

#[derive(Debug, Deserialize)]
struct WebFetchInput {
    url: String,
    #[serde(default)]
    #[allow(dead_code)]
    prompt: Option<String>,
}

/// Compute a simple hash of the URL for cache purposes.
fn url_hash(url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Get the cache directory for web_fetch content.
fn get_cache_dir() -> PathBuf {
    clawde_core::config::Settings::config_dir().join("web_cache")
}

/// Attempt to load cached extracted content for a URL.
fn load_cached_extraction(url: &str) -> Option<String> {
    let cache_dir = get_cache_dir();
    let cache_file = cache_dir.join(format!("{}.txt", url_hash(url)));

    if cache_file.exists() {
        match fs::read_to_string(&cache_file) {
            Ok(content) => {
                debug!(file = ?cache_file, "Loaded cached web content");
                return Some(content);
            }
            Err(e) => {
                debug!(file = ?cache_file, error = %e, "Failed to load cache");
            }
        }
    }
    None
}

/// Save extracted content to cache.
fn save_cached_extraction(url: &str, content: &str) {
    let cache_dir = get_cache_dir();
    if let Err(e) = fs::create_dir_all(&cache_dir) {
        warn!(dir = ?cache_dir, error = %e, "Failed to create cache directory");
        return;
    }

    let cache_file = cache_dir.join(format!("{}.txt", url_hash(url)));
    if let Err(e) = fs::write(&cache_file, content) {
        warn!(file = ?cache_file, error = %e, "Failed to write cache file");
    } else {
        debug!(file = ?cache_file, "Cached extracted web content");
    }
}

/// Detect if HTML is likely a JS-heavy page with minimal semantic content.
fn is_edge_case_html(html: &str, extracted_text: &str) -> bool {
    // Check word count (rough estimate)
    let word_count = extracted_text.split_whitespace().count();
    if word_count < 100 {
        debug!(word_count, "Edge case: low word count");
        return true;
    }

    // Check for semantic HTML tags
    let lower = html.to_lowercase();
    let has_semantic =
        lower.contains("<article") || lower.contains("<main") || lower.contains("<body");

    if !has_semantic {
        debug!("Edge case: no semantic HTML tags");
        return true;
    }

    false
}

/// Call Claude Haiku to extract main content from HTML.
async fn semantic_extraction(html: &str, ctx: &ToolContext) -> Option<String> {
    // Try to create an Anthropic client from the config
    let client = match clawde_api::AnthropicClient::from_config(&ctx.config) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to create Anthropic client for semantic extraction");
            return None;
        }
    };

    // Truncate HTML to avoid exceeding token limits
    let html_excerpt = if html.len() > 20000 {
        format!("{}...", &html[..20000])
    } else {
        html.to_string()
    };

    let system = "You are a content extraction expert. Given HTML, extract and return only the main text content. Return just plain text, no markdown or formatting.";
    let user_message = format!(
        "Extract the main content from this HTML and return only the text:\n\n{}",
        html_excerpt
    );

    // Use the builder API to construct the request
    let api_messages = vec![clawde_api::ApiMessage {
        role: "user".to_string(),
        content: serde_json::Value::String(user_message),
    }];

    let request = clawde_api::CreateMessageRequest::builder("claude-haiku-4-5", 2000)
        .messages(api_messages)
        .system(clawde_api::SystemPrompt::Text(system.to_string()))
        .build();

    match client.create_message(request).await {
        Ok(response) => {
            // Extract text from the response content (Vec<Value>)
            // Response content is JSON objects like {"type": "text", "text": "..."}
            let text = response.content.iter().find_map(|block| {
                if block.get("type")?.as_str()? == "text" {
                    block.get("text")?.as_str().map(str::to_owned)
                } else {
                    None
                }
            });

            if let Some(extracted) = text {
                debug!(
                    extracted_len = extracted.len(),
                    "Semantic extraction successful"
                );
                return Some(extracted);
            }

            warn!("No text block in semantic extraction response");
            None
        }
        Err(e) => {
            warn!(error = %e, "Semantic extraction API call failed");
            None
        }
    }
}

/// Naively strip HTML tags and decode common entities.
fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;

    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if !in_tag && chars[i] == '<' {
            in_tag = true;
            // Check for script/style open/close
            let rest: String = lower_chars[i..].iter().take(20).collect();
            if rest.starts_with("<script") {
                in_script = true;
            } else if rest.starts_with("</script") {
                in_script = false;
            } else if rest.starts_with("<style") {
                in_style = true;
            } else if rest.starts_with("</style") {
                in_style = false;
            }
            // Block tags => newline
            let block_tags = [
                "<br", "<p ", "<p>", "</p>", "<div", "</div>", "<h1", "<h2", "<h3", "<h4", "<h5",
                "<h6", "</h1", "</h2", "</h3", "</h4", "</h5", "</h6", "<li", "</li", "<tr",
                "</tr", "<hr",
            ];
            for tag in &block_tags {
                if rest.starts_with(tag) {
                    result.push('\n');
                    break;
                }
            }
            i += 1;
            continue;
        }

        if in_tag {
            if chars[i] == '>' {
                in_tag = false;
            }
            i += 1;
            continue;
        }

        if in_script || in_style {
            i += 1;
            continue;
        }

        // Decode basic entities
        if chars[i] == '&' {
            let rest: String = chars[i..].iter().take(10).collect();
            if rest.starts_with("&amp;") {
                result.push('&');
                i += 5;
            } else if rest.starts_with("&lt;") {
                result.push('<');
                i += 4;
            } else if rest.starts_with("&gt;") {
                result.push('>');
                i += 4;
            } else if rest.starts_with("&quot;") {
                result.push('"');
                i += 6;
            } else if rest.starts_with("&#39;") || rest.starts_with("&apos;") {
                result.push('\'');
                i += if rest.starts_with("&#39;") { 5 } else { 6 };
            } else if rest.starts_with("&nbsp;") {
                result.push(' ');
                i += 6;
            } else {
                result.push('&');
                i += 1;
            }
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    // Collapse multiple blank lines
    let mut collapsed = String::new();
    let mut blank_count = 0;
    for line in result.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                collapsed.push('\n');
            }
        } else {
            blank_count = 0;
            collapsed.push_str(trimmed);
            collapsed.push('\n');
        }
    }

    collapsed.trim().to_string()
}

#[async_trait]
impl Tool for WebFetchTool {
    // Gates itself: calls `ctx.check_permission` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_WEB_FETCH
    }

    fn description(&self) -> &str {
        "Fetches a web page URL and returns its content as text. HTML is \
         automatically converted to plain text. Use this for reading documentation, \
         APIs, and other web resources."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn network_capable(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "prompt": {
                    "type": "string",
                    "description": "Optional prompt for how to process the content"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: WebFetchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // Permission check
        if let Err(e) = ctx.check_permission_for_tool(
            self,
            &format!("Fetch {}", params.url),
            true, // read-only
        ) {
            return ToolResult::error(e.to_string());
        }

        debug!(url = %params.url, "Fetching web page");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("Failed to create HTTP client: {}", e)),
        };

        let resp = match client
            .get(&params.url)
            .header("User-Agent", "Claude-Code/1.0")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("Failed to fetch {}: {}", params.url, e)),
        };

        let status = resp.status();
        if !status.is_success() {
            return ToolResult::error(format!("HTTP {} when fetching {}", status, params.url));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(format!("Failed to read response body: {}", e)),
        };

        // Try to load from cache first
        if let Some(cached) = load_cached_extraction(&params.url) {
            return ToolResult::success(cached);
        }

        // Convert HTML to text if applicable
        let mut text = if content_type.contains("html") {
            strip_html(&body)
        } else {
            body.clone()
        };

        // Detect and handle edge cases with semantic extraction
        if content_type.contains("html") && is_edge_case_html(&body, &text) {
            debug!(url = %params.url, "Attempting semantic extraction for edge case");
            if let Some(extracted) = semantic_extraction(&body, ctx).await {
                text = extracted;
            } else {
                debug!("Semantic extraction failed, using basic HTML stripping");
            }
        }

        // Truncate very long content
        const MAX_LEN: usize = 100_000;
        let text = if text.len() > MAX_LEN {
            format!(
                "{}\n\n... (truncated, {} total characters)",
                &text[..MAX_LEN],
                text.len()
            )
        } else {
            text
        };

        // Cache the final result
        save_cached_extraction(&params.url, &text);

        ToolResult::success(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags_keeps_text() {
        assert_eq!(strip_html("<p>Hello <b>world</b></p>"), "Hello world");
        // Block open+close each emit a newline, leaving one blank line.
        assert_eq!(strip_html("<h1>Title</h1><p>Body</p>"), "Title\n\nBody");
    }

    #[test]
    fn strip_html_drops_script_and_style_content() {
        let html = "<script>var secret = 'xss';</script><style>.x{color:red}</style>visible";
        assert_eq!(strip_html(html), "visible");
        // Case-insensitive script/style detection.
        let upper = "<SCRIPT>bad()</SCRIPT>ok";
        assert_eq!(strip_html(upper), "ok");
    }

    #[test]
    fn strip_html_decodes_basic_entities() {
        assert_eq!(strip_html("a &amp; b &lt;c&gt;"), "a & b <c>");
        assert_eq!(
            strip_html("&quot;quoted&quot; &#39;apos&#39; &apos;x&apos;"),
            "\"quoted\" 'apos' 'x'"
        );
        assert_eq!(strip_html("a&nbsp;b"), "a b");
        // Unknown entities are kept verbatim.
        assert_eq!(strip_html("&unknown;"), "&unknown;");
    }

    #[test]
    fn strip_html_block_tags_become_newlines() {
        assert_eq!(strip_html("<div>one</div><div>two</div>"), "one\n\ntwo");
        assert_eq!(strip_html("<ul><li>a</li><li>b</li></ul>"), "a\n\nb");
        // Void block tags (no closer) emit a single newline.
        assert_eq!(strip_html("one<br>two"), "one\ntwo");
    }

    #[test]
    fn strip_html_handles_unclosed_tag_and_comment() {
        // Text after an unclosed tag is dropped (still in-tag).
        assert_eq!(strip_html("text <div"), "text");
        // Comments are consumed as tags; surrounding text survives.
        assert_eq!(strip_html("a <!-- hidden --> b"), "a  b");
    }

    #[test]
    fn strip_html_empty_and_whitespace_only() {
        assert_eq!(strip_html(""), "");
        assert_eq!(strip_html("   "), "");
        assert_eq!(strip_html("<p>   </p>"), "");
    }

    #[test]
    fn strip_html_collapses_blank_lines() {
        // Extra blank lines between paragraphs collapse to at most two.
        assert_eq!(strip_html("<p>a</p>\n\n\n<p>b</p>"), "a\n\n\nb");
    }

    #[test]
    fn is_edge_case_html_flags_low_word_count() {
        assert!(is_edge_case_html("<html><body>tiny</body></html>", "tiny")); // ~50 words is still below the 100-word threshold.
        let words = std::iter::repeat_n("word", 50)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(is_edge_case_html("<body>hi</body>", &words));
    }

    #[test]
    fn is_edge_case_html_requires_semantic_tags() {
        let words = std::iter::repeat_n("word", 200)
            .collect::<Vec<_>>()
            .join(" ");
        // 200 words but no <article>/<main>/<body> → flagged.
        assert!(is_edge_case_html("<div><span>ignored</span></div>", &words));
        // Same content with a semantic tag passes.
        assert!(!is_edge_case_html("<html><body>hi</body></html>", &words));
        // Tag detection is case-insensitive.
        assert!(!is_edge_case_html("<HTML><BODY>hi</BODY></HTML>", &words));
    }

    #[test]
    fn url_hash_is_deterministic_and_distinct() {
        let a = url_hash("https://example.com/page");
        assert_eq!(a, url_hash("https://example.com/page"));
        assert_ne!(a, url_hash("https://example.com/other"));
        assert!(!a.is_empty());
        // Hex output.
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ---- cache + execute paths -------------------------------------------

    /// Run a future with `CLAWDE_HOME` pointed at a fresh temp dir so cache
    /// reads/writes never touch the real config dir (and never race the real
    /// home's cache under parallelism). Serializes on the crate-wide
    /// [`crate::TEST_ENV_LOCK`] so all env-mutating tests in this crate share
    /// one mutex (AGENTS.md parallel-safe tests).
    #[allow(clippy::await_holding_lock)]
    // The guard must span the whole future: it serialises the CLAWDE_HOME
    // mutation against other env-mutating tests (same std::sync::Mutex
    // convention as crate::paths::ENV_LOCK). Test-only, single acquisition,
    // no re-entrancy — no deadlock risk.
    async fn with_temp_home<T>(f: impl FnOnce(std::path::PathBuf) -> T) -> T::Output
    where
        T: std::future::Future,
    {
        // Recover from poisoning so one failing test can't cascade
        // PoisonError failures into every other env-mutating test.
        let _lock = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("CLAWDE_HOME", dir.path());
        let out = f(dir.path().to_path_buf()).await;
        std::env::remove_var("CLAWDE_HOME");
        out
    }

    /// Serve a single canned HTTP response on a loopback port and return its
    /// URL. The listener thread accepts one connection, reads the request
    /// head, and replies — enough for a hermetic reqwest round-trip.
    fn serve_once(status_line: &str, content_type: &str, body: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status_line = status_line.to_string();
        let content_type = content_type.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let _ = std::io::Read::read(&mut sock, &mut buf);
                let response = format!(
                    "{status_line}\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut sock, response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn web_cache_round_trip() {
        with_temp_home(|_home| async move {
            let url = "https://example.com/cached-page";
            assert_eq!(load_cached_extraction(url), None);
            save_cached_extraction(url, "extracted body");
            assert_eq!(
                load_cached_extraction(url).as_deref(),
                Some("extracted body")
            );
            // A different URL never collides with the same cache file.
            assert_eq!(load_cached_extraction("https://example.com/other"), None);
        })
        .await;
    }

    #[tokio::test]
    async fn web_cache_missing_url_returns_none() {
        with_temp_home(|_home| async move {
            assert_eq!(load_cached_extraction("https://never.saved.example/"), None);
        })
        .await;
    }

    #[tokio::test]
    async fn fetch_invalid_input_errors_without_network() {
        let ctx = crate::test_support::allow_all_context(std::path::PathBuf::from("."));
        let res = WebFetchTool.execute(json!({ "url": 42 }), &ctx).await;
        assert!(res.is_error);
        assert!(res.content.contains("Invalid input"), "{}", res.content);
    }

    #[tokio::test]
    async fn fetch_plain_text_from_local_server() {
        with_temp_home(|home| async move {
            let url = serve_once(
                "HTTP/1.1 200 OK",
                "text/plain; charset=utf-8",
                "hello world",
            );
            let ctx = crate::test_support::allow_all_context(home);
            let res = WebFetchTool.execute(json!({ "url": url }), &ctx).await;
            assert!(!res.is_error, "fetch failed: {}", res.content);
            assert_eq!(res.content, "hello world");
        })
        .await;
    }

    #[tokio::test]
    async fn fetch_html_strips_tags_without_semantic_extraction() {
        with_temp_home(|home| async move {
            // 120 words inside a <body> tag: past the 100-word edge-case
            // threshold AND semantically tagged, so the basic strip path runs
            // with no semantic-extraction API call.
            let words = std::iter::repeat_n("word", 120)
                .collect::<Vec<_>>()
                .join(" ");
            let html = format!("<html><body><p>{words}</p></body></html>");
            let url = serve_once("HTTP/1.1 200 OK", "text/html; charset=utf-8", &html);
            let ctx = crate::test_support::allow_all_context(home);
            let res = WebFetchTool.execute(json!({ "url": url }), &ctx).await;
            assert!(!res.is_error, "fetch failed: {}", res.content);
            assert_eq!(res.content, words);
        })
        .await;
    }

    #[tokio::test]
    async fn fetch_http_error_reports_status() {
        with_temp_home(|home| async move {
            let url = serve_once("HTTP/1.1 404 Not Found", "text/plain", "nope");
            let ctx = crate::test_support::allow_all_context(home);
            let res = WebFetchTool.execute(json!({ "url": url }), &ctx).await;
            assert!(res.is_error);
            assert!(res.content.contains("HTTP 404"), "{}", res.content);
        })
        .await;
    }

    #[tokio::test]
    async fn fetch_result_is_saved_to_cache() {
        with_temp_home(|home| async move {
            let url = serve_once("HTTP/1.1 200 OK", "text/plain", "cached payload");
            let ctx = crate::test_support::allow_all_context(home);
            let first = WebFetchTool.execute(json!({ "url": url }), &ctx).await;
            assert!(!first.is_error, "fetch failed: {}", first.content);
            // execute saves the extracted text to the on-disk cache; the
            // fetch-then-check order means the cache saves extraction work,
            // not network round-trips, so verify the write directly.
            assert_eq!(
                load_cached_extraction(&url).as_deref(),
                Some("cached payload")
            );
        })
        .await;
    }
}
