// providers/free/modelsdev.rs — Best-free-model auto-detection from models.dev.
//
// Fetches models.dev once per process and finds the cheapest (cost=0) model
// with tool-calling support for each FREE_CATALOG upstream. The result backs
// the chain's effective_model overrides.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::FREE_CATALOG;

// ---------------------------------------------------------------------------
// Auto-detect best free models from models.dev
// ---------------------------------------------------------------------------

/// Cache for the best free model per upstream, populated once at first use.
static AUTO_DETECTED_DEFAULTS: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Fetch `models.dev` and find the best free (cost=0) model with tool_call
/// support for each FREE_CATALOG upstream.
///
/// Selection criteria (matching Cline's approach from their source):
/// 1. Model must be free (cost.input == 0 && cost.output == 0)
/// 2. Model must support tool calling (tool_call == true)
/// 3. Model must not be deprecated
/// 4. Among qualifying models, pick the one with the largest context window
///
/// Results are cached in `AUTO_DETECTED_DEFAULTS` so the HTTP fetch
/// only happens once per process lifetime.
pub fn fetch_best_free_models_from_modelsdev() -> &'static HashMap<String, String> {
    AUTO_DETECTED_DEFAULTS.get_or_init(|| {
        // reqwest::blocking::Client creates an internal tokio runtime. Dropping
        // that runtime inside an existing tokio runtime context (e.g. under
        // #[tokio::main]) panics. Run the entire HTTP fetch on a plain OS thread
        // so the internal runtime is created and dropped outside any async context.
        std::thread::spawn(|| {
            let url = "https://models.dev/api.json";
            let Ok(response) = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .and_then(|client| client.get(url).send())
            else {
                tracing::warn!("fetch_best_free_models_from_modelsdev: HTTP request failed");
                return HashMap::new();
            };

            let Ok(data) = response.json::<serde_json::Value>() else {
                tracing::warn!("fetch_best_free_models_from_modelsdev: failed to parse JSON");
                return HashMap::new();
            };

            let mut result = HashMap::new();

            for upstream in FREE_CATALOG {
                let Some(provider) = data.get(upstream.id) else {
                    continue;
                };
                let Some(models) = provider.get("models") else {
                    continue;
                };
                let Some(models_obj) = models.as_object() else {
                    continue;
                };

                let mut candidates: Vec<(&str, u64)> = Vec::new();

                for (model_id, model_info) in models_obj {
                    // Must be free
                    let cost = model_info.get("cost").and_then(|c| c.as_object());
                    let cost_in = cost
                        .and_then(|c| c.get("input"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0);
                    let cost_out = cost
                        .and_then(|c| c.get("output"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0);
                    if cost_in != 0.0 || cost_out != 0.0 {
                        continue;
                    }

                    // Must support tool calling (matching Cline's filtering)
                    let tool_call = model_info
                        .get("tool_call")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if !tool_call {
                        continue;
                    }

                    // Must not be deprecated
                    let status = model_info
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if status == "deprecated" || status == "legacy" {
                        continue;
                    }

                    // Context window for ranking
                    let limit = model_info.get("limit").and_then(|l| l.as_object());
                    let context = limit
                        .and_then(|l| l.get("context"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    candidates.push((model_id, context));
                }

                // Sort by context window descending, pick the best
                candidates.sort_by(|a, b| b.1.cmp(&a.1));

                if let Some((model_id, _ctx)) = candidates.first() {
                    let prev = result.insert(upstream.id.to_string(), (*model_id).to_string());
                    if prev.is_none() {
                        tracing::info!(
                            "Auto-detected free model for {}: {} ({} context)",
                            upstream.id,
                            model_id,
                            _ctx,
                        );
                    }
                }
            }

            if result.is_empty() {
                tracing::warn!("fetch_best_free_models_from_modelsdev: no free models found");
            } else {
                tracing::info!("Auto-detected free models for {} upstreams", result.len(),);
            }

            result
        })
        .join()
        .unwrap_or_else(|_| {
            tracing::warn!("fetch_best_free_models_from_modelsdev: thread panicked");
            HashMap::new()
        })
    })
}
