use crate::signaling::{generate_room_code, signal_as_client, signal_as_host};
use std::{
    io,
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::oneshot, time::timeout};

const PROBE_CLIENT_FINGERPRINT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HOST_MARKER: &[u8] = b"vmax-check/host";
const CLIENT_MARKER: &[u8] = b"vmax-check/client";

pub async fn check(endpoint: &str) -> io::Result<Duration> {
    timeout(Duration::from_secs(30), check_inner(endpoint))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "relay check exceeded 30 seconds"))?
}

/// Probe all candidates concurrently and rank working relays by packet RTT.
pub async fn select(endpoints: Vec<String>) -> Option<String> {
    let mut checks = tokio::task::JoinSet::new();
    for (priority, endpoint) in endpoints.into_iter().enumerate() {
        checks.spawn(async move {
            let rtt = timeout(Duration::from_secs(2), check_inner(&endpoint))
                .await
                .ok()?
                .ok()?;
            Some((rtt, priority, endpoint))
        });
    }
    let mut best = None;
    while let Some(checked) = checks.join_next().await {
        if let Ok(Some(candidate)) = checked
            && best.as_ref().is_none_or(|current| candidate < *current)
        {
            best = Some(candidate);
        }
    }
    best.map(|(_, _, endpoint)| endpoint)
}

async fn check_inner(endpoint: &str) -> io::Result<Duration> {
    let code = generate_room_code();
    let (ready_tx, ready_rx) = oneshot::channel();
    let host = signal_as_host(
        endpoint,
        code.clone(),
        "a".repeat(64),
        vec![],
        None,
        false,
        Some(vec![PROBE_CLIENT_FINGERPRINT.into()]),
        None,
        Some(ready_tx),
    );
    let client = async {
        ready_rx.await.map_err(io::Error::other)?;
        signal_as_client(endpoint, code, PROBE_CLIENT_FINGERPRINT.into(), vec![])
            .await
            .map_err(io::Error::other)
    };
    let (host, client) = tokio::try_join!(async { host.await.map_err(io::Error::other) }, client)?;
    let mut host_relay = host
        .relay
        .ok_or_else(|| io::Error::other("host was not offered a relay"))?;
    let mut client_relay = client
        .relay
        .ok_or_else(|| io::Error::other("client was not offered a relay"))?;
    tokio::try_join!(
        host_relay.agree_direct(false),
        client_relay.agree_direct(false)
    )
    .map_err(io::Error::other)?;

    let (host_udp, host_bridge) = loopback_pair().await?;
    let (client_udp, client_bridge) = loopback_pair().await?;
    tokio::select! {
        result = host_relay.forward(host_bridge) => {
            result.map_err(io::Error::other)?;
            Err(io::Error::other("host relay closed during the packet test"))
        }
        result = client_relay.forward(client_bridge) => {
            result.map_err(io::Error::other)?;
            Err(io::Error::other("client relay closed during the packet test"))
        }
        result = async {
            let mut samples = [Duration::ZERO; 3];
            for sample in &mut samples {
                let started = Instant::now();
                host_udp.send(HOST_MARKER).await?;
                expect_packet(&client_udp, HOST_MARKER).await?;
                client_udp.send(CLIENT_MARKER).await?;
                expect_packet(&host_udp, CLIENT_MARKER).await?;
                *sample = started.elapsed();
            }
            samples.sort_unstable();
            Ok(samples[1])
        } => result,
    }
}

async fn loopback_pair() -> io::Result<(UdpSocket, UdpSocket)> {
    let app = UdpSocket::bind("127.0.0.1:0").await?;
    let bridge = UdpSocket::bind("127.0.0.1:0").await?;
    app.connect(bridge.local_addr()?).await?;
    bridge.connect(app.local_addr()?).await?;
    Ok((app, bridge))
}

async fn expect_packet(socket: &UdpSocket, expected: &[u8]) -> io::Result<()> {
    let mut buf = [0; 64];
    let len = socket.recv(&mut buf).await?;
    if &buf[..len] == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay forwarded an unexpected packet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signaling::LocalSignalingServer;

    #[tokio::test]
    async fn selection_skips_a_websocket_that_cannot_relay_media() {
        use futures_util::StreamExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let signaling_only = format!("ws://{}", listener.local_addr().unwrap());
        let broken = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.close(None).await.unwrap();
        });
        let healthy = LocalSignalingServer::start("127.0.0.1:0").await.unwrap();
        assert_eq!(
            select(vec![signaling_only, healthy.ws_url.clone()]).await,
            Some(healthy.ws_url.clone())
        );
        broken.await.unwrap();
    }

    #[tokio::test]
    async fn checks_both_directions_through_a_real_relay() {
        let mut server = LocalSignalingServer::start("127.0.0.1:0").await.unwrap();
        check(&server.ws_url).await.unwrap();
        server.stop();
    }
}
