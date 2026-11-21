//! Iroh endpoint and invitation setup for the VmaX session transport.

pub(crate) mod client;
pub(crate) mod control;
pub(crate) mod host;
mod input;
mod media;

use std::time::Duration;

use anyhow::{Context, Result};
use iroh::{Endpoint, endpoint::presets};
use iroh_tickets::{Ticket, endpoint::EndpointTicket};

pub(crate) const STREAM_ALPN: &[u8] = b"vmax/stream/1";
const RELAY_READY_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) async fn bind_host() -> Result<Endpoint> {
    let endpoint = bind_endpoint().await?;
    endpoint.set_alpns(vec![STREAM_ALPN.to_vec()]);
    Ok(endpoint)
}

pub(crate) async fn bind_client() -> Result<Endpoint> {
    bind_endpoint().await
}

pub(crate) fn host_ticket(endpoint: &Endpoint) -> String {
    EndpointTicket::new(endpoint.addr()).encode_string()
}

pub(crate) fn parse_host_ticket(invite: &str) -> Result<EndpointTicket> {
    invite
        .trim()
        .parse()
        .context("Invalid VmaX endpoint ticket")
}

async fn bind_endpoint() -> Result<Endpoint> {
    let endpoint = Endpoint::bind(presets::N0)
        .await
        .context("Could not bind the VmaX Iroh endpoint")?;

    // Iroh's online future waits for a home relay, which is unnecessary on LAN.
    if tokio::time::timeout(RELAY_READY_TIMEOUT, endpoint.online())
        .await
        .is_err()
    {
        tracing::debug!("Iroh home relay still pending; local endpoint is bound");
    }
    Ok(endpoint)
}
