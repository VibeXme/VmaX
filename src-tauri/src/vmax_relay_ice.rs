//! VmaX Multi-Tier Network & Free Relay Selector
//! Priority: Direct LAN / Tailscale -> STUN + VMAX_PUNCH_V1 -> Free TURN / wss:// Relay

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionPath {
    DirectLan,
    TailscaleMesh,
    DirectInternetPunch,
    CloudflareTurn,
    MeteredOpenRelay,
    CustomWssRelay,
}

#[derive(Debug, Clone)]
pub struct IceEndpoint {
    pub path: ConnectionPath,
    pub url: &'static str,
    pub free_tier_notes: &'static str,
}

/// Verified free STUN & Relay endpoints for fallback when CGNAT blocks UDP hole-punching
pub fn default_free_ice_endpoints() -> Vec<IceEndpoint> {
    vec![
        IceEndpoint {
            path: ConnectionPath::DirectInternetPunch,
            url: "stun:stun.cloudflare.com:3478",
            free_tier_notes: "Unlimited free STUN (Cloudflare)",
        },
        IceEndpoint {
            path: ConnectionPath::DirectInternetPunch,
            url: "stun:stun.l.google.com:19302",
            free_tier_notes: "Unlimited free STUN (Google)",
        },
        IceEndpoint {
            path: ConnectionPath::CloudflareTurn,
            url: "turn:turn.cloudflare.com:3478?transport=udp",
            free_tier_notes: "1,000 GB/month free via Cloudflare Realtime TURN",
        },
        IceEndpoint {
            path: ConnectionPath::MeteredOpenRelay,
            url: "turn:global.relay.metered.ca:80",
            free_tier_notes: "20 GB/month free via Metered Open Relay",
        },
    ]
}

/// Selects the lowest-latency viable path based on local interface & NAT probe results
pub fn select_best_path(
    lan_reachable: bool,
    tailscale_ip_present: bool,
    stun_punch_succeeded: bool,
    cloudflare_turn_configured: bool,
    metered_turn_configured: bool,
) -> ConnectionPath {
    if lan_reachable {
        ConnectionPath::DirectLan
    } else if tailscale_ip_present {
        ConnectionPath::TailscaleMesh
    } else if stun_punch_succeeded {
        ConnectionPath::DirectInternetPunch
    } else if cloudflare_turn_configured {
        ConnectionPath::CloudflareTurn
    } else if metered_turn_configured {
        ConnectionPath::MeteredOpenRelay
    } else {
        ConnectionPath::CustomWssRelay
    }
}
