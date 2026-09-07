use clawde_api::providers::OllamaNativeProvider;
use clawde_core::{Config, ProviderConfig};

#[tokio::main]
async fn main() {
    let host = std::env::var("OLLAMA_HOST").expect("set OLLAMA_HOST");

    let native_host = host.trim_end_matches('/').to_string();
    let inner = clawde_api::providers::openai_compat_providers::ollama();

    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "qwen2.5-coder:7b".to_string());

    let requested = if model.starts_with("ollama/") {
        model.strip_prefix("ollama/").unwrap().to_string()
    } else {
        model.clone()
    };

    // Constructed to mirror the runtime path under test; the unload behavior
    // itself is exercised through the core helpers below.
    let _provider = OllamaNativeProvider::new(inner, native_host.clone());

    // All scenarios run against a Config that pins the core helpers to
    // OLLAMA_HOST (api_base wins over the env fallback), so the probe never
    // depends on the dev machine's settings file or its local CPU daemon.
    let config = config_with_host(&host);

    // ---- Scenario 1: exact-name round trip -------------------------------
    // Reload so the server has exactly one loaded model, then unload it by
    // the exact name /api/ps reports.
    reload_model_via_url(&native_host, &requested)
        .await
        .expect("reload");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let before = clawde_core::ollama_status_for_config(&config)
        .await
        .expect("status");
    eprintln!("scenario 1 (exact name) — before =====");
    for m in &before.models {
        eprintln!("  loaded: name={} size_vram={:?}", m.name, m.size_vram);
    }
    assert_eq!(
        before.models.len(),
        1,
        "expected exactly the reloaded model resident"
    );
    let reported_name = before.models[0].name.clone();

    let unloaded = clawde_core::ollama_unload_models_for_config(&config, Some(&reported_name))
        .await
        .expect("unload exact");
    eprintln!("  unload by exact name {reported_name:?}: unloaded {unloaded}");
    assert_eq!(
        unloaded, 1,
        "exact-name unload must unload the resident model"
    );
    assert_empty_server(&config, "after exact-name unload").await;

    // ---- Scenario 2: tag-variant -----------------------------------------
    // Request the model under the other tag spelling while the server
    // reports one form; the unload must still match (the fix under test).
    reload_model_via_url(&native_host, &requested)
        .await
        .expect("reload");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let before = clawde_core::ollama_status_for_config(&config)
        .await
        .expect("status");
    eprintln!("scenario 2 (tag variant) — before =====");
    for m in &before.models {
        eprintln!("  loaded: name={}", m.name);
    }
    assert_eq!(
        before.models.len(),
        1,
        "expected exactly the reloaded model resident"
    );
    let reported = before.models[0].name.clone();
    let variant = tag_variant(&reported);
    eprintln!("  server reports {reported:?}; unloading under variant {variant:?}");
    let unloaded = clawde_core::ollama_unload_models_for_config(&config, Some(&variant))
        .await
        .expect("unload variant");
    eprintln!("  unload by variant {variant:?}: unloaded {unloaded}");
    assert_eq!(
        unloaded, 1,
        "tag-variant unload must find the resident model"
    );
    assert_empty_server(&config, "after tag-variant unload").await;

    // ---- Scenario 3: not-loaded is a named error --------------------------
    eprintln!("scenario 3 (not loaded) =====");
    let err = clawde_core::ollama_unload_models_for_config(&config, Some(&requested))
        .await
        .expect_err("unloading a not-loaded model must be an error");
    eprintln!("  error: {err}");
    assert!(
        err.contains(requested.as_str()),
        "error must name the model: {err}"
    );
    assert!(
        err.to_lowercase().contains("not currently loaded"),
        "error must say not loaded: {err}"
    );

    // ---- Scenario 4: unload-all on an empty server -------------------------
    eprintln!("scenario 4 (unload-all, empty server) =====");
    let pre = clawde_core::ollama_status_for_config(&config)
        .await
        .expect("status");
    eprintln!("  pre-unload-all status: {} model(s)", pre.models.len());
    let unloaded = clawde_core::ollama_unload_models_for_config(&config, None)
        .await
        .expect("unload-all");
    eprintln!("  unload-all on empty server: unloaded {unloaded}");
    assert_eq!(unloaded, 0, "unload-all on an empty server must be Ok(0)");

    eprintln!("end — all four scenarios behaved as specified");
}

/// Build a Config whose ollama provider points at `host` so the core helpers
/// hit the same server the probe talks to, regardless of the dev machine's
/// settings file or local daemon.
fn config_with_host(host: &str) -> Config {
    let mut config = Config::default();
    config.provider_configs.insert(
        "ollama".to_string(),
        ProviderConfig {
            api_base: Some(host.trim_end_matches('/').to_string()),
            ..Default::default()
        },
    );
    config
}

/// The other spelling of the same model: `x:latest` <-> `x`.
fn tag_variant(name: &str) -> String {
    match name.rsplit_once(':') {
        Some((base, tag)) if !base.is_empty() && tag != "latest" => format!("{name}:latest"),
        Some((base, _)) if !base.is_empty() => base.to_string(),
        _ => format!("{name}:latest"),
    }
}

async fn assert_empty_server(config: &Config, when: &str) {
    let after = clawde_core::ollama_status_for_config(config)
        .await
        .expect("status");
    assert!(
        after.models.is_empty(),
        "{when} must leave the server empty, found {:?}",
        after.models.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    eprintln!("  {when}: server empty, as expected");
}

// reload the model first so the unload path is exercised against a loaded model.
// Ollama accepts the request and unloads/resident reloads serve
// asynchronously, so we do not need a full streamed completion here.
// a short no-op load plus a small wait is enough for the unload test.
//
// The LAN link is lossy (600ms RTT Wi-Fi) and cold loads have measured up
// to ~90s, so a single POST is not reliable: retry transport failures and
// allow a full load cycle per attempt.
async fn reload_model_via_url(base: &str, model: &str) -> Result<(), String> {
    let mut last_err = String::new();
    for attempt in 1..=3 {
        match reload_once(base, model).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                eprintln!("reload attempt {attempt} failed: {err}");
                last_err = err;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
    Err(last_err)
}

async fn reload_once(base: &str, model: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": model,
        "prompt": "",
        "stream": false,
        "keep_alive": -1,
    });
    let resp = client
        .post(format!("{}/api/generate", base))
        .json(&body)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| format!("generate request failed: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("unreadable response: {e}"))?;
    eprintln!("generate status={} body={}", status, text);
    if status != 200 {
        return Err(format!("generate returned {}", status));
    }
    if let Some(err) = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|v| v.as_str().map(|s| s.to_owned()))
        })
    {
        return Err(format!("generate error: {err}"));
    }
    Ok(())
}
