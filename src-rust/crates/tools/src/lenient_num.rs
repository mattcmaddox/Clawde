// lenient_num.rs — lenient numeric deserialization for tool inputs.
//
// Models routinely emit integer tool arguments as JSON floats (`5.0` instead
// of `5`) or as numeric strings (`"5"`). Strict `usize`/`u64` serde fields
// then reject the whole tool call with `invalid type: floating point \`5.0\`,
// expected usize`, wasting a turn on a formatting nit. These
// `deserialize_with` helpers accept any non-negative numeric value (floats
// rounded to the nearest integer, negatives clamped to 0) and pass through
// plain integers unchanged.
//
// Use on tool-input struct fields:
//
//   #[serde(default, deserialize_with = "lenient_num::opt_usize")]
//   limit: Option<usize>,

use serde::{Deserialize, Deserializer};

/// Core coercion: accept integers, floats (rounded), and numeric strings.
/// Returns `None` for null / non-numeric values; negatives clamp to 0.
fn coerce_to_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_u64() {
                Some(i)
            } else if let Some(f) = n.as_f64() {
                // Floats like 5.0 (or 4.7 from a confused model) round to the
                // nearest integer; negatives clamp to 0.
                Some(f.max(0.0).round() as u64)
            } else {
                // Negative integer — clamp to 0 rather than failing.
                Some(0)
            }
        }
        serde_json::Value::String(s) => {
            let t = s.trim();
            t.parse::<u64>()
                .ok()
                .or_else(|| t.parse::<f64>().ok().map(|f| f.max(0.0).round() as u64))
        }
        _ => None,
    }
}

/// Deserializer for `Option<usize>` fields: accepts int / float / numeric
/// string; null and missing stay `None`; non-numeric values fall back to
/// `None` instead of failing the whole tool call.
pub fn opt_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(|v| coerce_to_u64(&v)).map(|n| n as usize))
}

/// Shared coercion used by the concrete default wrappers below
/// (`deserialize_with` takes a path, so parameterized closures don't work —
/// each needed default gets its own named function).
fn coerce_field<'de, D>(deserializer: D, fallback: u64) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(|v| coerce_to_u64(&v)).unwrap_or(fallback))
}

/// `#[serde(default = "...")]` companions for the wrappers below. NOTE:
/// `deserialize_with` is not invoked when a field is *missing* — serde's
/// `default` path supplies the value then — so wrappers on fields that
/// should have a missing-value default need an explicit default fn (plain
/// `#[serde(default)]` would yield 0). Fields that should ERROR when
/// missing (e.g. sleep's `ms`) get `deserialize_with` alone, no default.
pub fn default_5() -> usize {
    5
}

/// Deserializer for `usize` fields defaulting to 5 (search result counts).
pub fn usize_or_5<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(coerce_field(deserializer, 5)? as usize)
}

/// Deserializer for `u64` fields (sleep ms): lenient when present
/// (int/float/numeric-string/null→1000); the field errors when missing,
/// preserving the tool's required-argument contract.
pub fn u64_or_1000<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    coerce_field(deserializer, 1000)
}

/// Deserializer for `Option<u64>` fields (same leniency as `opt_usize`).
#[allow(dead_code)] // available for future u64 tool inputs; guard-tested elsewhere
pub fn opt_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(|v| coerce_to_u64(&v)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize)]
    struct Opt {
        #[serde(default, deserialize_with = "opt_usize")]
        n: Option<usize>,
    }

    #[derive(Debug, Deserialize)]
    struct Req {
        #[serde(default = "default_5", deserialize_with = "usize_or_5")]
        n: usize,
    }

    #[test]
    fn accepts_float_form_of_integer() {
        let v: Opt = serde_json::from_value(json!({ "n": 5.0 })).unwrap();
        assert_eq!(v.n, Some(5));
    }

    #[test]
    fn accepts_plain_integer() {
        let v: Opt = serde_json::from_value(json!({ "n": 5 })).unwrap();
        assert_eq!(v.n, Some(5));
    }

    #[test]
    fn accepts_numeric_string() {
        let v: Opt = serde_json::from_value(json!({ "n": "5" })).unwrap();
        assert_eq!(v.n, Some(5));
    }

    #[test]
    fn rounds_and_clamps_floats() {
        let v: Opt = serde_json::from_value(json!({ "n": 4.7 })).unwrap();
        assert_eq!(v.n, Some(5));
        let v: Opt = serde_json::from_value(json!({ "n": -2.5 })).unwrap();
        assert_eq!(v.n, Some(0));
    }

    #[test]
    fn null_and_missing_stay_none() {
        let v: Opt = serde_json::from_value(json!({})).unwrap();
        assert_eq!(v.n, None);
        let v: Opt = serde_json::from_value(json!({ "n": null })).unwrap();
        assert_eq!(v.n, None);
    }

    #[test]
    fn garbage_falls_back_to_none_not_error() {
        let v: Opt = serde_json::from_value(json!({ "n": "soon" })).unwrap();
        assert_eq!(v.n, None);
    }

    #[test]
    fn required_style_falls_back_on_garbage() {
        let v: Req = serde_json::from_value(json!({ "n": 5.0 })).unwrap();
        assert_eq!(v.n, 5);
        let v: Req = serde_json::from_value(json!({})).unwrap();
        assert_eq!(v.n, 5, "missing falls back to the wrapper default");
        let v: Req = serde_json::from_value(json!({ "n": null })).unwrap();
        assert_eq!(v.n, 5);
        let v: Req = serde_json::from_value(json!({ "n": "many" })).unwrap();
        assert_eq!(v.n, 5);
    }
}
