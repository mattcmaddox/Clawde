use clawde_api::provider::LlmProvider;
use clawde_api::providers::OllamaNativeProvider;

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

    // reload the model first so the unload path is exercised against a loaded model.
    // Ollama accepts the request and unloads/resident reloads serve
    // asynchronously, so we do not need a full streamed completion here.
    // a short no-op load plus a small wait is enough for the unload test.
    reload_model_via_url(&native_host, &requested)
        .await
        .expect("reload");
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;

    let tags = inner.discover_models().await.expect("discover");
    eprintln!(
        "discovered models via compat layer: {:?}",
        tags.iter().map(|m| &m.id).collect::<Vec<_>>()
    );

    // Constructed to mirror the runtime path under test; the unload behavior
    // itself is exercised through the core helpers below.
    let _provider = OllamaNativeProvider::new(inner, native_host);

    // 1) status via Clawde's core helper path
    let config = clawde_core::Settings::load_sync()
        .map(|s| s.effective_config())
        .unwrap_or_default();
    let before = clawde_core::ollama_status_for_config(&config)
        .await
        .expect("ollama_status_for_config");
    eprintln!("before status =====");
    for m in &before.models {
        eprintln!("  loaded: name={} size_vram={:?}", m.name, m.size_vram);
    }

    // 2) unload via Clawde's core helper path
    let requested = if model.starts_with("ollama/") {
        model.strip_prefix("ollama/").unwrap().to_string()
    } else {
        model.clone()
    };
    let unloaded = clawde_core::ollama_unload_models_for_config(&config, Some(&requested))
        .await
        .expect("ollama_unload_models_for_config");
    eprintln!("unload result: unloaded {unloaded}");

    // 3) status after
    let after = clawde_core::ollama_status_for_config(&config)
        .await
        .expect("ollama_status_for_config");
    eprintln!("after status =====");
    for m in &after.models {
        eprintln!("  loaded: name={} size_vram={:?}", m.name, m.size_vram);
    }

    eprintln!("end");
}

async fn reload_model_via_url(base: &str, model: &str) -> Result<(), String> {
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
        .timeout(std::time::Duration::from_secs(45))
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
