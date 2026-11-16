#[path = "../../src-tauri/src/relay_probe.rs"]
mod probe;
#[path = "../../src-tauri/src/stun.rs"]
pub mod stun;
#[path = "../../src-tauri/src/signaling.rs"]
pub mod signaling;
#[path = "../../src-tauri/src/vmax_relay_ice.rs"]
mod vmax_relay_ice;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();
    let mut args = std::env::args().skip(1);
    let first_arg = args.next();
    if first_arg.as_deref() == Some("--check") {
        let endpoint = args.next().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "usage: vmax-signaling --check wss://relay.example.com",
            )
        })?;
        probe::check(&endpoint).await?;
        println!(
            "Relay check passed: room registration, join, and packet forwarding in both directions."
        );
        return Ok(());
    }
    let address = first_arg.unwrap_or_else(|| "127.0.0.1:8787".into());
    let mut server = signaling::LocalSignalingServer::start(&address).await?;
    eprintln!(
        "VmaX signaling and relay server listening on {}",
        server.local_addr
    );
    tokio::signal::ctrl_c().await?;
    server.stop();
    Ok(())
}
