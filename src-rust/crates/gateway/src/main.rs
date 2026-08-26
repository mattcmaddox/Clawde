//! Standalone gateway binary: `clawde-gateway`.

use clawde_gateway::config::EffectiveGatewayConfig;

fn main() -> anyhow::Result<()> {
    // Minimal arg parsing: --port N, --key K, --allow-non-loopback, --tls-cert/--tls-key.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut port: Option<u16> = None;
    let mut cli_key: Option<String> = None;
    let mut allow_non_loopback = false;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--port needs a value"))?
                        .parse()?,
                );
            }
            "--key" => {
                i += 1;
                cli_key = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--key needs a value"))?
                        .clone(),
                );
            }
            "--allow-non-loopback" => allow_non_loopback = true,
            "--tls-cert" => {
                i += 1;
                tls_cert = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--tls-cert needs a value"))?
                        .clone(),
                );
            }
            "--tls-key" => {
                i += 1;
                tls_key = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--tls-key needs a value"))?
                        .clone(),
                );
            }
            "--help" | "-h" => {
                println!(
                    "Usage: clawde-gateway [--port N] [--key K] [--allow-non-loopback] \
                     [--tls-cert PATH] [--tls-key PATH]"
                );
                return Ok(());
            }
            other => return Err(anyhow::anyhow!("unknown argument: {other}")),
        }
        i += 1;
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let settings = clawde_core::config::Settings::load()
            .await
            .unwrap_or_default();
        let mut base = settings.gateway.clone();
        if let Some(p) = port {
            base.listen = format!("127.0.0.1:{p}");
        }
        if allow_non_loopback {
            base.allow_non_loopback = true;
        }
        if let Some(cert) = tls_cert {
            base.tls_cert_path = Some(cert);
        }
        if let Some(key) = tls_key {
            base.tls_key_path = Some(key);
        }
        let config = EffectiveGatewayConfig::from_settings(&base, cli_key)
            .map_err(|e| anyhow::anyhow!(e))?;
        clawde_gateway::run_gateway(&config).await
    })
}
