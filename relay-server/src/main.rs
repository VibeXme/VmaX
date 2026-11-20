// The app and the standalone relay use the same wire protocol implementation.
#![allow(dead_code)]

#[path = "../../src-tauri/src/signaling.rs"]
mod signaling;
#[path = "../../src-tauri/src/stun.rs"]
mod stun;
#[path = "../../src-tauri/src/vmax_relay_ice.rs"]
mod vmax_relay_ice;

#[cfg(test)]
#[path = "../../src-tauri/src/relay_probe.rs"]
mod relay_probe;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let port = std::env::var("VMAX_RELAY_PORT")
        .map(|value| value.parse::<u16>())
        .unwrap_or(Ok(8787))?;
    let mut server = signaling::LocalSignalingServer::start(&format!("127.0.0.1:{port}")).await?;
    eprintln!("VmaX relay listening at {}", server.local_addr);
    tokio::signal::ctrl_c().await?;
    server.stop();
    Ok(())
}
