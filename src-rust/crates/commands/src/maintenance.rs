// Maintenance commands: `/voice`, `/upgrade`, `/release-notes`, `/rate-limit-options`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct VoiceCommand;
pub struct UpgradeCommand;
pub struct ReleaseNotesCommand;
pub struct RateLimitOptionsCommand;

// ---- /voice --------------------------------------------------------------

#[async_trait]
impl SlashCommand for VoiceCommand {
    fn name(&self) -> &str {
        "voice"
    }
    fn description(&self) -> &str {
        "Toggle voice input mode on/off"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "on".into(),
                description: "Enable voice mode".into(),
                available: true,
            },
            ArgCompletion {
                value: "off".into(),
                description: "Disable voice mode".into(),
                available: true,
            },
            ArgCompletion {
                value: "status".into(),
                description: "Show current voice mode and endpoint info".into(),
                available: true,
            },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /voice [on|off|status]\n\n\
         Enables or disables voice input (push-to-talk).\n\
         Setting is persisted to ~/.clawde/ui-settings.json.\n\n\
         Transcription is performed via a Whisper-compatible API.\n\
         Set one of these env vars for the API key:\n\
           OPENAI_API_KEY   — OpenAI Whisper (default endpoint)\n\
           ANTHROPIC_API_KEY — used as a fallback key\n\n\
         To use a local Whisper server instead of OpenAI:\n\
           export WHISPER_ENDPOINT_URL=http://localhost:8080/v1/audio/transcriptions\n\
           export OPENAI_API_KEY=any-value  (local servers often ignore the key)\n\n\
         On Linux, ALSA must be set up: sudo apt install libasound2-dev\n\
         Check available devices with: arecord -l\n\n\
         Controls:\n\
           Alt+V — start recording; Alt+V or Esc — stop and transcribe"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let current = load_ui_settings();
        let currently_enabled = current.voice_enabled.unwrap_or(false);

        let enable = match args.trim() {
            "on" | "enable" | "enabled" | "true" | "1" => true,
            "off" | "disable" | "disabled" | "false" | "0" => false,
            "" => !currently_enabled, // toggle
            "status" => {
                let state = if currently_enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                let endpoint = std::env::var("WHISPER_ENDPOINT_URL").unwrap_or_else(|_| {
                    "https://api.openai.com/v1/audio/transcriptions (default)".to_string()
                });
                let key_source = if std::env::var("OPENAI_API_KEY").is_ok() {
                    "OPENAI_API_KEY"
                } else if std::env::var("ANTHROPIC_API_KEY").is_ok() {
                    "ANTHROPIC_API_KEY"
                } else {
                    "(none — transcription will fail)"
                };
                return CommandResult::Message(format!(
                    "Voice mode: {}\n\
                     Endpoint:   {}\n\
                     API key:    {}",
                    state, endpoint, key_source
                ));
            }
            other => {
                return CommandResult::Error(format!(
                    "Unknown argument '{}'. Use: /voice [on|off|status]",
                    other
                ))
            }
        };

        match mutate_ui_settings(|s| s.voice_enabled = Some(enable)) {
            Ok(_) => {
                if enable {
                    let endpoint = std::env::var("WHISPER_ENDPOINT_URL")
                        .unwrap_or_else(|_| "OpenAI Whisper (default)".to_string());
                    let key_hint = if std::env::var("OPENAI_API_KEY").is_ok()
                        || std::env::var("ANTHROPIC_API_KEY").is_ok()
                    {
                        String::new()
                    } else {
                        "\nWarning: no OPENAI_API_KEY found — transcription will fail. \
                         Set OPENAI_API_KEY or WHISPER_ENDPOINT_URL for a local server."
                            .to_string()
                    };
                    CommandResult::Message(format!(
                        "Voice recording activated.\n\
                         Press Alt+V to start recording; Alt+V or Esc to stop and transcribe.\n\
                         Endpoint: {}{}",
                        endpoint, key_hint
                    ))
                } else {
                    CommandResult::Message("Voice recording deactivated.".to_string())
                }
            }
            Err(e) => CommandResult::Error(format!("Failed to save voice setting: {}", e)),
        }
    }
}

// ---- /upgrade ------------------------------------------------------------

#[async_trait]
impl SlashCommand for UpgradeCommand {
    fn name(&self) -> &str {
        "update"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["upgrade"]
    }
    fn description(&self) -> &str {
        "Check for updates and download the latest release"
    }
    fn help(&self) -> &str {
        "Usage: /update\n\n\
         Checks GitHub releases for the latest version of Clawde.\n\
         If a newer version is available, shows where to download it."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let current = clawde_core::constants::APP_VERSION;

        // Check GitHub releases API for latest version
        let client = clawde_core::github::api_client();

        let resp = client
            .get(format!(
                "{}/repos/{}/releases/latest",
                clawde_core::github::GITHUB_API_BASE,
                clawde_core::github::GITHUB_REPO
            ))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let rate_note = github_rate_note(&r);
                let json: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);

                let tag = json
                    .get("tag_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .trim_start_matches('v');

                let fallback_url = format!(
                    "https://github.com/{}/releases",
                    clawde_core::github::GITHUB_REPO
                );
                let url = json
                    .get("html_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&fallback_url);

                if tag == current || tag == "unknown" {
                    CommandResult::Message(format!(
                        "Clawde v{current} - you are up to date.\n\
                         Release page: {url}{rate_note}"
                    ))
                } else {
                    CommandResult::Message(format!(
                        "Update available!\n\
                         Current version:  v{current}\n\
                         Latest version:   v{tag}\n\
                         Release page:     {url}{rate_note}\n\n\
                         Upgrade in place (recommended):\n\
                           clawde upgrade\n\n\
                         Or reinstall with your original method:\n\
                           npm install -g clawde\n\
                           curl -fsSL https://github.com/{}/releases/latest/download/install.sh | bash   (macOS/Linux)\n\
                           irm https://github.com/{}/releases/latest/download/install.ps1 | iex          (Windows)",
                        clawde_core::github::GITHUB_REPO,
                        clawde_core::github::GITHUB_REPO
                    ))
                }
            }
            Ok(r) => {
                // On a 403 the quota is usually exhausted — GitHub sends
                // `Retry-After` (delta-seconds) plus the reset timestamp, so
                // tell the user when to try again instead of just failing.
                // Parse the CURRENT response headers (not the possibly stale
                // last_rate_limit) and store the fresh status for /ctx-viz.
                // This arm deliberately uses ONLY the retry hint (not
                // github_rate_note's generic warning) to avoid stating the
                // reset time twice in the same message.
                let status = r.status();
                let rate_note = if status == reqwest::StatusCode::FORBIDDEN {
                    match clawde_core::github::parse_rate_limit(r.headers()) {
                        Some(limit) => {
                            clawde_core::github::store_rate_limit(limit);
                            format_retry_hint(retry_after_secs(r.headers(), &limit))
                        }
                        None => github_rate_note(&r),
                    }
                } else {
                    github_rate_note(&r)
                };
                CommandResult::Message(format!(
                    "Current version: v{current}\n\
                     Could not check for updates (HTTP {status}).{rate_note}\n\
                     Visit https://github.com/{}/releases for updates.",
                    clawde_core::github::GITHUB_REPO
                ))
            }
            Err(e) => CommandResult::Message(format!(
                "Current version: v{current}\n\
                 Could not check for updates: {e}\n\
                 Visit https://github.com/{}/releases for updates.",
                clawde_core::github::GITHUB_REPO
            )),
        }
    }
}

/// Append a GitHub API rate-limit note to a `/update` / `/release-notes`
/// message when the remaining quota is low. Empty when the quota is healthy.
/// Also records the last-seen status for the `/ctx-viz` overlay.
fn github_rate_note(resp: &reqwest::Response) -> String {
    let limit = clawde_core::github::parse_rate_limit(resp.headers());
    if let Some(limit) = limit {
        clawde_core::github::store_rate_limit(limit);
    }
    rate_limit_note_text(limit)
}

/// Format the low-quota warning appended to `/update` / `/release-notes`
/// messages. Empty when the quota is healthy. Pure — unit-tested directly.
fn rate_limit_note_text(limit: Option<clawde_core::github::RateLimit>) -> String {
    limit
        .as_ref()
        .and_then(clawde_core::github::rate_limit_warning)
        .map(|w| format!("\n\n{w}"))
        .unwrap_or_default()
}

/// Seconds until the rate-limit window resets. Prefers the `Retry-After`
/// header (delta-seconds or RFC 7231 HTTP-date, via the api crate's parser)
/// and falls back to `X-RateLimit-Reset`.
fn retry_after_secs(
    headers: &reqwest::header::HeaderMap,
    limit: &clawde_core::github::RateLimit,
) -> u64 {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(clawde_api::extract_retry_after_header)
        .unwrap_or_else(|| {
            limit
                .reset_unix
                .saturating_sub(clawde_core::github::unix_now())
        })
}

/// Format the retry hint appended to a 403 rate-limit message. Pure —
/// unit-tested directly.
fn format_retry_hint(secs: u64) -> String {
    if secs >= 60 {
        format!(" Retry after ~{} min.", secs / 60)
    } else if secs > 0 {
        format!(" Retry after ~{} sec.", secs)
    } else {
        " Retry shortly.".to_string()
    }
}

// ---- /release-notes ------------------------------------------------------

#[async_trait]
impl SlashCommand for ReleaseNotesCommand {
    fn name(&self) -> &str {
        "release-notes"
    }
    fn description(&self) -> &str {
        "Show release notes for the current version"
    }
    fn help(&self) -> &str {
        "Usage: /release-notes [version]\n\n\
         Fetches and displays release notes from GitHub.\n\
         Without an argument, shows notes for the current version."
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let current = clawde_core::constants::APP_VERSION;
        let version = args.trim();

        let tag = if version.is_empty() {
            format!("v{}", current)
        } else if version.starts_with('v') {
            version.to_string()
        } else {
            format!("v{}", version)
        };

        let client = clawde_core::github::api_client();
        let url = format!(
            "{}/repos/{}/releases/tags/{}",
            clawde_core::github::GITHUB_API_BASE,
            clawde_core::github::GITHUB_REPO,
            tag
        );

        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let rate_note = github_rate_note(&r);
                let json: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);

                let body = json
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("No release notes found.");

                let published = json
                    .get("published_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown date");

                let html_url = json.get("html_url").and_then(|v| v.as_str()).unwrap_or("");

                CommandResult::Message(format!(
                    "Release Notes: Clawde {tag}\n\
                     Published: {published}\n\
                     URL: {html_url}{rate_note}\n\
                     ─────────────────────────────────\n\
                     {body}"
                ))
            }
            Ok(r) if r.status().as_u16() == 404 => CommandResult::Message(format!(
                "No release found for {tag}.\n\
                 View all releases: https://github.com/{}/releases",
                clawde_core::github::GITHUB_REPO
            )),
            Ok(r) => {
                let rate_note = github_rate_note(&r);
                CommandResult::Message(format!(
                    "Could not fetch release notes (HTTP {}).{rate_note}\n\
                     View at: https://github.com/{}/releases/tag/{}",
                    r.status(),
                    clawde_core::github::GITHUB_REPO,
                    tag
                ))
            }
            Err(e) => CommandResult::Message(format!(
                "Could not fetch release notes: {e}\n\
                 View at: https://github.com/{}/releases/tag/{tag}",
                clawde_core::github::GITHUB_REPO
            )),
        }
    }
}

// ---- /rate-limit-options -------------------------------------------------

#[async_trait]
impl SlashCommand for RateLimitOptionsCommand {
    fn name(&self) -> &str {
        "rate-limit-options"
    }
    fn description(&self) -> &str {
        "Show rate limit tiers and current rate limit status"
    }
    fn help(&self) -> &str {
        "Usage: /rate-limit-options\n\n\
         Displays available rate limit tiers and the current tier for your account.\n\
         Rate limits depend on your Clawde plan (Free, Pro, Max, API)."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        // Try to read from OAuth tokens file to get subscription/tier info
        let tier_info = match clawde_core::oauth::OAuthTokens::load().await {
            Some(tokens) => {
                let sub_type = tokens.subscription_type.as_deref().unwrap_or("unknown");
                format!(
                    "Account type:    {}\n\
                     Scopes:          {}",
                    sub_type,
                    if tokens.scopes.is_empty() {
                        "none".to_string()
                    } else {
                        tokens.scopes.join(", ")
                    }
                )
            }
            None => {
                // Check for API key auth
                if ctx.config.resolve_api_key().is_some() {
                    "Account type:    API key (Console)\n\
                     Rate limit tier: Depends on your API plan tier"
                        .to_string()
                } else {
                    "Not logged in. Run /login to see your rate limit tier.".to_string()
                }
            }
        };

        CommandResult::Message(format!(
            "Rate Limit Status\n\
             ─────────────────\n\
             {tier_info}\n\n\
             Available tiers:\n\
             ┌─────────────────────────────────────────────────┐\n\
             │ Free          │ Limited daily usage             │\n\
             │ Pro           │ Higher limits, faster resets    │\n\
             │ Max (5x)      │ 5× Pro limits                   │\n\
             │ Max (20x)     │ 20× Pro limits (highest tier)   │\n\
             │ API / Console │ Usage-billed, no hard cap       │\n\
             └─────────────────────────────────────────────────┘\n\n\
             To upgrade: /upgrade\n\
             Manage billing: https://claude.ai/settings/billing",
            tier_info = tier_info,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn limit(remaining: u64, reset_unix: u64) -> clawde_core::github::RateLimit {
        clawde_core::github::RateLimit {
            limit: 60,
            remaining,
            reset_unix,
        }
    }

    #[test]
    fn rate_limit_note_text_empty_when_healthy() {
        assert_eq!(rate_limit_note_text(None), "");
        let now = clawde_core::github::unix_now();
        assert_eq!(rate_limit_note_text(Some(limit(50, now + 1800))), "");
    }

    #[test]
    fn rate_limit_note_text_warns_when_low() {
        let now = clawde_core::github::unix_now();
        let note = rate_limit_note_text(Some(limit(2, now + 1800)));
        assert!(note.contains("rate limit low"), "got: {note}");
        assert!(note.contains("2 requests remaining"), "got: {note}");
    }

    #[test]
    fn rate_limit_note_text_warns_when_exhausted() {
        let now = clawde_core::github::unix_now();
        let note = rate_limit_note_text(Some(limit(0, now + 3600)));
        assert!(note.contains("exhausted"), "got: {note}");
    }

    #[test]
    fn format_retry_hint_minutes_seconds_shortly() {
        assert_eq!(format_retry_hint(120), " Retry after ~2 min.");
        assert_eq!(format_retry_hint(45), " Retry after ~45 sec.");
        assert_eq!(format_retry_hint(0), " Retry shortly.");
    }

    #[test]
    fn retry_after_prefers_header_then_reset() {
        let now = clawde_core::github::unix_now();
        let l = limit(0, now + 1800);

        // Delta-seconds Retry-After header wins.
        let mut headers = HeaderMap::new();
        headers.insert("Retry-After", HeaderValue::from_static("120"));
        assert_eq!(retry_after_secs(&headers, &l), 120);

        // Without the header, derive from X-RateLimit-Reset.
        assert_eq!(retry_after_secs(&HeaderMap::new(), &l), 1800);
    }

    #[test]
    fn retry_after_parses_http_date() {
        // RFC 7231 HTTP-date is handled by the api crate's parser. A date 30
        // days out keeps the test valid regardless of the wall clock.
        let l = limit(0, 0); // reset fallback would be ~0
        let future = chrono::Utc::now() + chrono::Duration::days(30);
        let date_str = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            "Retry-After",
            HeaderValue::from_str(&date_str).expect("ascii date header"),
        );
        let secs = retry_after_secs(&headers, &l);
        // A 30-day-out date yields a large delta (well past the reset
        // fallback), proving the HTTP-date branch was taken.
        assert!(secs > 60 * 60 * 24 * 29, "got {secs}s");
    }
}
