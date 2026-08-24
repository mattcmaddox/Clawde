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

/// Cache for the FULL free model set per upstream, populated once at first
/// use. Backs the Alt+J/K popup's model-first list for OpenAI-compatible
/// upstreams (models.dev's per-provider free set is the authoritative
/// "everything free on this provider" signal; the live `/v1/models` list is
/// intersected against it at discovery time).
static MODELSDEV_FREE_IDS: OnceLock<HashMap<String, Vec<String>>> = OnceLock::new();

/// Return every models.dev-listed free (cost=0), tool-calling, non-deprecated
/// model id for `upstream_id`, ordered by context window descending (the
/// largest/fastest candidate first, matching the best-pick ranking). Empty
/// when models.dev has no entry for the upstream or the fetch failed.
///
/// Fetched at most once per process; the network call runs on a plain OS
/// thread (see [`fetch_modelsdev_free_ids_blocking`]).
pub fn modelsdev_free_model_ids(upstream_id: &str) -> Vec<String> {
    MODELSDEV_FREE_IDS
        .get_or_init(fetch_modelsdev_free_ids_blocking)
        .get(upstream_id)
        .cloned()
        .unwrap_or_default()
}

/// The blocking models.dev fetch that collects every free model per upstream.
/// Shares the same endpoint and filtering rules as
/// [`fetch_modelsdev_defaults_blocking`] but keeps the whole candidate list
/// instead of the single best pick.
fn fetch_modelsdev_free_ids_blocking() -> HashMap<String, Vec<String>> {
    std::thread::spawn(|| {
        let url = "https://models.dev/api.json";
        let Ok(response) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .and_then(|client| client.get(url).send())
        else {
            tracing::warn!("fetch_modelsdev_free_ids: HTTP request failed");
            return HashMap::new();
        };

        let Ok(data) = response.json::<serde_json::Value>() else {
            tracing::warn!("fetch_modelsdev_free_ids: failed to parse JSON");
            return HashMap::new();
        };

        let mut result: HashMap<String, Vec<String>> = HashMap::new();
        for upstream in FREE_CATALOG {
            let Some(provider) = data.get(upstream.id) else {
                continue;
            };
            let Some(models) = provider.get("models").and_then(|m| m.as_object()) else {
                continue;
            };

            let mut candidates: Vec<(&str, u64)> = Vec::new();
            for (model_id, model_info) in models {
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
                let tool_call = model_info
                    .get("tool_call")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !tool_call {
                    continue;
                }
                let status = model_info
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if status == "deprecated" || status == "legacy" {
                    continue;
                }
                let limit = model_info.get("limit").and_then(|l| l.as_object());
                let context = limit
                    .and_then(|l| l.get("context"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                candidates.push((model_id, context));
            }

            candidates.sort_by(|a, b| b.1.cmp(&a.1));
            let ids: Vec<String> = candidates
                .into_iter()
                .map(|(id, _)| id.to_string())
                .collect();
            if !ids.is_empty() {
                result.insert(upstream.id.to_string(), ids);
            }
        }
        result
    })
    .join()
    .unwrap_or_else(|_| {
        tracing::warn!("fetch_modelsdev_free_ids: thread panicked");
        HashMap::new()
    })
}

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
        // F2 (audit fix): prefer the persisted cache so a fresh CLI process does
        // not block on a network fetch at startup. The cache is refreshed below
        // and re-fetched on a later process once it goes stale.
        if let Some(cached) = super::load_modelsdev_defaults_cache() {
            tracing::debug!(
                "models.dev auto-detection loaded from cache ({} upstreams)",
                cached.len()
            );
            return cached;
        }
        let result = fetch_modelsdev_defaults_blocking();
        super::save_modelsdev_defaults_cache(&result);
        result
    })
}

/// The blocking models.dev fetch — runs on a plain OS thread so the internal
/// reqwest runtime is created and dropped outside any async context (dropping
/// it inside an existing tokio runtime context, e.g. under `#[tokio::main]`,
/// would panic).
fn fetch_modelsdev_defaults_blocking() -> HashMap<String, String> {
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
}
