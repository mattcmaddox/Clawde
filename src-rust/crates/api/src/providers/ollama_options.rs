//! Centralized Ollama configuration helpers.
//!
//! One authoritative conversion point for Ollama request options (spec
//! `ollama-tui-centralization-spec.md` §Request option wire shape): the
//! settings layer persists raw values, the TUI consumes preset tables from
//! here, and the request pipeline builds the native `options` object from
//! the canonical keys via [`native_options_value`].
//!
//! Transport (2026-09): Ollama chat goes over the **native `/api/chat`**
//! endpoint (`OllamaNativeProvider`). Verified against Ollama 0.33: the
//! native endpoint honors every option below (`options.num_ctx`,
//! `options.num_predict`, top-level `keep_alive`, sampling controls),
//! whereas the OpenAI-compatible `/v1` shim silently drops nested
//! `options.*` — one reason `/v1` is no longer the chat transport.
//!
//! `keep_alive` is unload/residency semantics: on the wire it is a
//! top-level `/api/chat` field, not an `options` entry. [`native_options_value`]
//! still returns it (keyed by its canonical name); the provider splits it
//! out when shaping the request body.

use serde_json::{json, Value};

// The Go-style duration parser lives in clawde-core so the lifecycle
// machinery (preload/unload) can read the persisted `keep_alive` without a
// dependency on this crate (core has no clawde-api dependency). This module
// re-exports it as the single user-facing conversion point.
pub use clawde_core::config::{
    ollama_keep_alive_value_to_secs as keep_alive_value_to_secs,
    ollama_parse_keep_alive_str as parse_keep_alive_str,
};

// ---------------------------------------------------------------------------
// Preset tables (single source for the TUI screens)
// ---------------------------------------------------------------------------

/// Context-window presets offered by the Ollama UI. `0` = unset (the UI
/// shows "Ollama/model default").
pub const OLLAMA_CTX_PRESETS: &[(&str, u64)] = &[
    ("2K", 2_048),
    ("4K", 4_096),
    ("8K", 8_192),
    ("12K", 12_288),
    ("16K", 16_384),
    ("32K", 32_768),
    ("64K", 65_536),
    ("128K", 131_072),
];

/// Max-output-token presets. `0` = unset.
pub const OLLAMA_PREDICT_PRESETS: &[(&str, u64)] = &[
    ("512", 512),
    ("1K", 1_024),
    ("2K", 2_048),
    ("4K", 4_096),
    ("8K", 8_192),
    ("16K", 16_384),
];

/// Keep-alive presets in seconds. `-1` = keep loaded forever, `0` = unload
/// immediately after the request.
pub const OLLAMA_KEEP_ALIVE_PRESETS: &[(&str, i64)] = &[
    ("unload after request", 0),
    ("5 min", 300),
    ("10 min", 600),
    ("30 min", 1_800),
    ("1 hour", 3_600),
    ("forever", -1),
];

/// Sampling-temperature presets (f64; `None`-like sentinel handled by the
/// string layer). Labels are display strings; conversion helpers below map
/// both directions.
pub const OLLAMA_TEMPERATURE_PRESETS: &[(&str, f64)] = &[
    ("0 (deterministic)", 0.0),
    ("0.2 (precise)", 0.2),
    ("0.7 (balanced)", 0.7),
    ("1.0 (creative)", 1.0),
];

/// Top-p presets.
pub const OLLAMA_TOP_P_PRESETS: &[(&str, f64)] = &[
    ("0.5", 0.5),
    ("0.9 (typical)", 0.9),
    ("0.95", 0.95),
    ("1.0 (off)", 1.0),
];

/// Canonical Ollama option keys persisted under
/// `provider_configs["ollama"].options` (and mirrored to the top-level
/// `providers` map by the settings layer).
pub const OLLAMA_OPTION_KEYS: &[&str] = &[
    "num_ctx",
    "num_predict",
    "keep_alive",
    "temperature",
    "top_p",
    "seed",
    "stop",
    "repeat_penalty",
    "repeat_last_n",
    "min_p",
    "typical_p",
    "tfs_z",
    "mirostat",
    "mirostat_tau",
    "mirostat_eta",
];

// ---------------------------------------------------------------------------
// Preset conversion helpers (display string <-> raw value)
// ---------------------------------------------------------------------------
// Round-trip rule: `*_to_label` output must always parse back through the
// matching `label_to_*`. Non-preset values render as `"<raw> (custom)"` and
// every parser strips that suffix, so a hand-set value survives a
// display-label round-trip instead of being treated as unset.

/// The five common options the `/ollama` screen edits, in display order
/// (mirrored by the TUI dialog's `OPTION_KEYS_ORDER`).
pub const COMMON_OPTION_KEYS: &[&str] = &[
    "num_ctx",
    "num_predict",
    "keep_alive",
    "temperature",
    "top_p",
];

// keep_alive parsing moved to `clawde_core::config` (re-exported above) so
// the preload/unload lifecycle in core shares the exact same wire-format
// semantics as the chat transport.

/// Human label for a raw num_ctx value, or `"Ollama/model default"` when
/// unset/zero.
pub fn num_ctx_to_label(n: u64) -> String {
    if n == 0 {
        return "Ollama/model default".to_string();
    }
    for (label, val) in OLLAMA_CTX_PRESETS {
        if *val == n {
            return (*label).to_string();
        }
    }
    // Raw token count, not a K approximation: integer division claimed e.g.
    // 49152 was "48K" (= 49152, fine) but 65537 was "64K" (= 65536, wrong).
    format!("{n} (custom)")
}

/// Parse a preset label (or custom integer string) into a raw num_ctx value.
/// `None` = unset.
pub fn label_to_num_ctx(label: &str) -> Option<u64> {
    for (name, val) in OLLAMA_CTX_PRESETS {
        if *name == label {
            return Some(*val);
        }
    }
    let trimmed = label.trim();
    let bare = trimmed.strip_suffix(" (custom)").unwrap_or(trimmed);
    bare.strip_suffix('K')
        .and_then(|s| s.parse::<u64>().ok())
        .map(|k| k * 1024)
        .or_else(|| bare.parse::<u64>().ok())
}

/// Human label for a raw num_predict value.
pub fn num_predict_to_label(n: u64) -> String {
    if n == 0 {
        return "Ollama/model default".to_string();
    }
    for (label, val) in OLLAMA_PREDICT_PRESETS {
        if *val == n {
            return (*label).to_string();
        }
    }
    format!("{n} (custom)")
}

/// Parse a preset label (or custom integer string) into raw num_predict.
pub fn label_to_num_predict(label: &str) -> Option<u64> {
    for (name, val) in OLLAMA_PREDICT_PRESETS {
        if *name == label {
            return Some(*val);
        }
    }
    let trimmed = label.trim();
    let bare = trimmed.strip_suffix(" (custom)").unwrap_or(trimmed);
    bare.strip_suffix('K')
        .and_then(|s| s.parse::<u64>().ok())
        .map(|k| k * 1024)
        .or_else(|| bare.parse::<u64>().ok())
}

/// Human label for a raw keep_alive value (seconds).
pub fn keep_alive_to_label(n: i64) -> String {
    for (label, val) in OLLAMA_KEEP_ALIVE_PRESETS {
        if *val == n {
            return (*label).to_string();
        }
    }
    if n < 0 {
        return "forever".to_string();
    }
    format!("{n} (custom)")
}

/// Parse a preset label into raw keep_alive seconds. Duration strings
/// ("5m", "1h30m", "90s") and bare integers are accepted.
pub fn label_to_keep_alive(label: &str) -> Option<i64> {
    for (name, val) in OLLAMA_KEEP_ALIVE_PRESETS {
        if *name == label {
            return Some(*val);
        }
    }
    let trimmed = label.trim();
    let bare = trimmed.strip_suffix(" (custom)").unwrap_or(trimmed);
    parse_keep_alive_str(bare)
}

/// Human label for a raw temperature value.
pub fn temperature_to_label(t: f64) -> String {
    for (label, val) in OLLAMA_TEMPERATURE_PRESETS {
        if (*val - t).abs() < 1e-9 {
            return (*label).to_string();
        }
    }
    format!("{t} (custom)")
}

/// Parse a preset label (or custom float string) into a temperature.
pub fn label_to_temperature(label: &str) -> Option<f64> {
    for (name, val) in OLLAMA_TEMPERATURE_PRESETS {
        if *name == label {
            return Some(*val);
        }
    }
    let trimmed = label.trim();
    let bare = trimmed.strip_suffix(" (custom)").unwrap_or(trimmed);
    bare.parse::<f64>().ok()
}

/// Human label for a raw top_p value.
pub fn top_p_to_label(t: f64) -> String {
    for (label, val) in OLLAMA_TOP_P_PRESETS {
        if (*val - t).abs() < 1e-9 {
            return (*label).to_string();
        }
    }
    format!("{t} (custom)")
}

/// Parse a preset label (or custom float string) into a top_p.
pub fn label_to_top_p(label: &str) -> Option<f64> {
    for (name, val) in OLLAMA_TOP_P_PRESETS {
        if *name == label {
            return Some(*val);
        }
    }
    let trimmed = label.trim();
    let bare = trimmed.strip_suffix(" (custom)").unwrap_or(trimmed);
    bare.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Canonical value normalization (settings raw value <-> canonical JSON)
// ---------------------------------------------------------------------------

/// Normalize one persisted value for the common options into its canonical
/// JSON shape: numbers over numeric strings, keep_alive duration strings
/// resolved to seconds, zero token counts treated as unset. Returns `None`
/// for unset-shaped (null/empty) or unrepresentable values so callers can
/// omit the key entirely instead of persisting a value that renders as
/// "unset".
pub fn normalize_common_option(key: &str, value: &Value) -> Option<Value> {
    if value.is_null() {
        return None;
    }
    if let Value::String(s) = value {
        if s.trim().is_empty() {
            return None;
        }
    }
    match key {
        "num_ctx" | "num_predict" => {
            let n = value
                .as_u64()
                .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                .or_else(|| {
                    value
                        .as_f64()
                        .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                        .map(|f| f as u64)
                })?;
            (n > 0).then_some(json!(n))
        }
        "keep_alive" => keep_alive_value_to_secs(value).map(|n| json!(n)),
        "temperature" | "top_p" => {
            let t = value
                .as_f64()
                .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
            Some(json!(t))
        }
        _ => Some(value.clone()),
    }
}

/// Display label for one canonical raw common-option value. Callers
/// representing "unset" use an absent raw value, never an empty string;
/// this helper is only invoked for values that are actually set.
pub fn common_option_label(key: &str, value: &Value) -> String {
    match key {
        "num_ctx" => num_ctx_to_label(value.as_u64().unwrap_or(0)),
        "num_predict" => num_predict_to_label(value.as_u64().unwrap_or(0)),
        "keep_alive" => keep_alive_to_label(value.as_i64().unwrap_or(-1)),
        "temperature" => value
            .as_f64()
            .map(temperature_to_label)
            .unwrap_or_else(|| value.to_string()),
        "top_p" => value
            .as_f64()
            .map(top_p_to_label)
            .unwrap_or_else(|| value.to_string()),
        _ => value.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Canonical option extraction + native wire mapping
// ---------------------------------------------------------------------------

/// Extract the canonical Ollama options object from the settings layer.
/// Reads the embedded config's `provider_configs["ollama"].options` (the
/// `/settings` UI write target) and the top-level `providers["ollama"].options`
/// (the documented location); the embedded config wins on collision, matching
/// `Settings::effective_config` semantics. Only the canonical keys are
/// returned — arbitrary other options are passed through verbatim by the
/// request pipeline and are not this module's concern.
pub fn canonical_options(
    settings: &clawde_core::config::Settings,
) -> serde_json::Map<String, Value> {
    let mut merged = serde_json::Map::new();
    if let Some(top) = settings.providers.get("ollama") {
        for key in OLLAMA_OPTION_KEYS {
            if let Some(value) = top.options.get(*key) {
                merged.insert((*key).to_string(), value.clone());
            }
        }
    }
    if let Some(embedded) = settings.config.provider_configs.get("ollama") {
        for key in OLLAMA_OPTION_KEYS {
            if let Some(value) = embedded.options.get(*key) {
                merged.insert((*key).to_string(), value.clone());
            }
        }
    }
    merged
}

/// Build the native-transport options payload for a dispatch from the
/// canonical persisted options.
///
/// Returns the JSON object carried in `ProviderRequest.provider_options`;
/// `OllamaNativeProvider` splits it into the `/api/chat` body (`options`
/// entries plus the top-level `keep_alive`). Omitted-unless-set semantics:
/// `null` and empty-string values are dropped. All-unset collapses to
/// `Value::Null` so nothing extra is sent.
///
/// `num_predict` participates with a precedence rule: the request pipeline
/// may already carry an effort-derived `max_tokens` as a typed request
/// field, and an explicitly persisted `num_predict` pins the cap on top —
/// the provider applies `request.max_tokens` first and lets an explicit
/// option override it (mirrors the old `/v1` "request field wins" rule).
pub fn native_options_value(options: &serde_json::Map<String, Value>) -> Value {
    let mut obj = serde_json::Map::new();
    for key in OLLAMA_OPTION_KEYS {
        let Some(value) = options.get(*key) else {
            continue;
        };
        let is_set = !(value.is_null()
            || value.is_string() && value.as_str().unwrap_or_default().is_empty());
        if !is_set {
            continue;
        }
        // Zero means "Ollama/model default" for the token-count options (the
        // UI preset tables map 0 to unset); other keys treat 0 as a real
        // value (keep_alive 0 = unload after request, temperature 0 = greedy).
        // Common keys also canonicalize here (numeric strings over strings,
        // keep_alive durations to seconds) so hand-edited settings values
        // reach the wire in Ollama's expected shape.
        if COMMON_OPTION_KEYS.contains(key) {
            let Some(canonical) = normalize_common_option(key, value) else {
                continue;
            };
            obj.insert((*key).to_string(), canonical);
            continue;
        }
        if *key == "stop" {
            match normalize_stop(value) {
                Ok(normalized) if !normalized.is_null() => {
                    obj.insert((*key).to_string(), normalized);
                }
                _ => {}
            }
        } else {
            obj.insert((*key).to_string(), value.clone());
        }
    }
    if obj.is_empty() {
        Value::Null
    } else {
        Value::Object(obj)
    }
}

/// Human-readable summary of the effective configuration: each set option
/// with its display value and where it applies. On the native transport
/// every request-shaping option is applied for real; `keep_alive` is
/// labeled separately because it controls server residency, not the
/// request.
pub fn effective_preview(options: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for key in OLLAMA_OPTION_KEYS {
        let Some(value) = options.get(*key) else {
            continue;
        };
        if value.is_null() || (value.is_string() && value.as_str().unwrap_or_default().is_empty()) {
            continue;
        }
        let display = match *key {
            "num_ctx" => num_ctx_to_label(value.as_u64().unwrap_or(0)),
            "num_predict" => num_predict_to_label(value.as_u64().unwrap_or(0)),
            "keep_alive" => keep_alive_to_label(value.as_i64().unwrap_or(-1)),
            "temperature" => value
                .as_f64()
                .map(temperature_to_label)
                .unwrap_or_else(|| value.to_string()),
            "top_p" => value
                .as_f64()
                .map(top_p_to_label)
                .unwrap_or_else(|| value.to_string()),
            _ => match value.as_str() {
                Some(s) => s.to_string(),
                None => value.to_string(),
            },
        };
        let status = if *key == "keep_alive" {
            "applied (unload timer)"
        } else {
            "applied"
        };
        rows.push((format!("{key}: {display}"), status.to_string()));
    }
    rows
}

/// Validate `stop`: accepted shapes are a string or an array of strings
/// (normalized to an array). Returns a normalized array or an error message.
pub fn normalize_stop(value: &Value) -> Result<Value, String> {
    match value {
        Value::String(s) => Ok(json!([s])),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(Value::String(s.to_string())),
                    None => return Err("stop sequences must be strings".to_string()),
                }
            }
            Ok(Value::Array(out))
        }
        Value::Null => Ok(Value::Null),
        _ => Err("stop sequences must be a string or array of strings".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_round_trips() {
        assert_eq!(label_to_num_ctx("12K"), Some(12_288));
        assert_eq!(num_ctx_to_label(12_288), "12K");
        assert_eq!(label_to_num_ctx("48K"), Some(49_152));
        assert_eq!(label_to_num_ctx("Ollama/model default"), None);

        assert_eq!(label_to_num_predict("4K"), Some(4_096));
        assert_eq!(num_predict_to_label(2_048), "2K");

        assert_eq!(label_to_keep_alive("forever"), Some(-1));
        assert_eq!(label_to_keep_alive("5 min"), Some(300));
        assert_eq!(keep_alive_to_label(0), "unload after request");

        assert_eq!(label_to_temperature("0.2 (precise)"), Some(0.2));
        assert_eq!(temperature_to_label(0.7), "0.7 (balanced)");
        assert_eq!(label_to_top_p("0.9 (typical)"), Some(0.9));
    }

    #[test]
    fn custom_labels_round_trip_losslessly() {
        // Every custom label must parse back to the exact raw value — the
        // dialog round-trips persisted values through these labels, and a
        // lossy label used to delete the setting on save.
        for n in [65537u64, 49152, 5000] {
            let label = num_ctx_to_label(n);
            assert!(label.ends_with(" (custom)"), "{n}: {label}");
            assert_eq!(label_to_num_ctx(&label), Some(n));
        }
        // Raw token count, not the integer-division K approximation
        // (65537 was formerly rendered as "64K (custom)").
        assert_eq!(num_ctx_to_label(65537), "65537 (custom)");

        for n in [3000u64, 96, 1] {
            let label = num_predict_to_label(n);
            assert!(label.ends_with(" (custom)"), "{n}: {label}");
            assert_eq!(label_to_num_predict(&label), Some(n));
        }

        for n in [90i64, 7200, 1] {
            let label = keep_alive_to_label(n);
            assert!(label.ends_with(" (custom)"), "{n}: {label}");
            assert_eq!(label_to_keep_alive(&label), Some(n));
        }

        for t in [0.15f64, 1.3, 0.05] {
            let label = temperature_to_label(t);
            assert!(label.ends_with(" (custom)"), "{t}: {label}");
            assert_eq!(label_to_temperature(&label), Some(t));
            let label = top_p_to_label(t);
            assert!(label.ends_with(" (custom)"), "{t}: {label}");
            assert_eq!(label_to_top_p(&label), Some(t));
        }
    }

    #[test]
    fn keep_alive_parses_duration_strings() {
        assert_eq!(parse_keep_alive_str("5m"), Some(300));
        assert_eq!(parse_keep_alive_str("1h"), Some(3_600));
        assert_eq!(parse_keep_alive_str("1h30m"), Some(5_400));
        assert_eq!(parse_keep_alive_str("90s"), Some(90));
        assert_eq!(parse_keep_alive_str("600"), Some(600));
        assert_eq!(parse_keep_alive_str(""), None);
        assert_eq!(parse_keep_alive_str("abc"), None);
        assert_eq!(parse_keep_alive_str("5x"), None);
        assert_eq!(keep_alive_value_to_secs(&json!("10m")), Some(600));
        assert_eq!(keep_alive_value_to_secs(&json!(120)), Some(120));
        assert_eq!(keep_alive_value_to_secs(&json!(true)), None);
    }

    #[test]
    fn normalize_common_option_canonicalizes_shapes() {
        assert_eq!(
            normalize_common_option("num_ctx", &json!(32768)),
            Some(json!(32768))
        );
        assert_eq!(normalize_common_option("num_ctx", &json!(0)), None);
        assert_eq!(
            normalize_common_option("num_ctx", &json!("65536")),
            Some(json!(65536))
        );
        assert_eq!(normalize_common_option("num_ctx", &json!(null)), None);
        assert_eq!(normalize_common_option("num_ctx", &json!("")), None);
        assert_eq!(
            normalize_common_option("num_predict", &json!("4096")),
            Some(json!(4096))
        );
        assert_eq!(
            normalize_common_option("keep_alive", &json!("5m")),
            Some(json!(300))
        );
        assert_eq!(
            normalize_common_option("keep_alive", &json!(0)),
            Some(json!(0))
        );
        assert_eq!(
            normalize_common_option("keep_alive", &json!("forever")),
            None
        );
        assert_eq!(
            normalize_common_option("temperature", &json!("0.15")),
            Some(json!(0.15))
        );
        assert_eq!(
            normalize_common_option("temperature", &json!(0.2)),
            Some(json!(0.2))
        );
        assert_eq!(
            normalize_common_option("top_p", &json!("0.9")),
            Some(json!(0.9))
        );
        // Unknown keys pass through untouched (arbitrary passthrough options).
        let passthrough = json!({"a": 1});
        assert_eq!(
            normalize_common_option("other", &passthrough),
            Some(passthrough)
        );
    }

    #[test]
    fn native_options_carries_everything_set() {
        let options = serde_json::json!({
            "num_predict": 4_096,
            "temperature": 0.2,
            "num_ctx": 32_768,
            "keep_alive": 0,
        })
        .as_object()
        .expect("object")
        .clone();
        let value = native_options_value(&options);
        assert_eq!(
            value,
            json!({
                "num_predict": 4_096,
                "temperature": 0.2,
                "num_ctx": 32_768,
                "keep_alive": 0,
            })
        );
    }

    #[test]
    fn native_options_normalizes_stop_and_drops_unset() {
        let options = serde_json::json!({
            "stop": "END",
            "temperature": null,
            "top_p": "",
            "num_ctx": 0,
        })
        .as_object()
        .unwrap()
        .clone();
        let value = native_options_value(&options);
        assert_eq!(value, json!({ "stop": ["END"] }));
    }

    #[test]
    fn all_unset_collapses_to_null() {
        let empty = serde_json::Map::new();
        assert!(native_options_value(&empty).is_null());
        let unset = serde_json::json!({"temperature": null})
            .as_object()
            .unwrap()
            .clone();
        assert!(native_options_value(&unset).is_null());
    }

    #[test]
    fn effective_preview_labels_tiers() {
        let options = serde_json::json!({
            "num_ctx": 16_384,
            "temperature": 0.2,
            "num_predict": 4_096,
            "keep_alive": 300,
        })
        .as_object()
        .unwrap()
        .clone();
        let rows = effective_preview(&options);
        assert!(rows
            .iter()
            .any(|(label, status)| label.contains("num_ctx") && status == "applied"));
        assert!(rows
            .iter()
            .any(|(label, status)| label.contains("temperature") && status == "applied"));
        assert!(
            rows.iter()
                .any(|(label, status)| label.contains("keep_alive")
                    && status.contains("unload timer"))
        );
    }

    #[test]
    fn normalize_stop_shapes() {
        assert_eq!(normalize_stop(&json!("END")).unwrap(), json!(["END"]));
        assert_eq!(
            normalize_stop(&json!(["a", "b"])).unwrap(),
            json!(["a", "b"])
        );
        assert!(normalize_stop(&json!([1])).is_err());
        assert!(normalize_stop(&Value::Null).unwrap().is_null());
    }
}
