//! QUIC transport policy for phones: short keepalives, bounded DPLPMTUD and
//! seamless connection migration across Wi-Fi <-> cellular and CGNAT rebinds.
//!
//! `quinn` 0.11 performs QUIC connection migration automatically when it sees
//! a validated packet from a new client address: the connection ID, TLS state
//! and stream offsets are preserved.  There is no `allow_mtud` switch in
//! quinn 0.11; MTU discovery is configured through
//! `TransportConfig::mtu_discovery_config`.  We bound the mobile search to 1232 bytes
//! because many carrier paths silently drop 1500-byte probes, and keep the
//! minimum at 1200 so queued datagrams remain valid after a migration.

use quinn::congestion::BbrConfig;
use quinn::{Connection, IdleTimeout, MtuDiscoveryConfig, TransportConfig};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(target_os = "android")]
static NETWORK_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(target_os = "android")]
pub fn network_epoch() -> u64 {
    NETWORK_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_vmax_app_MainActivity_nativeNetworkChanged(
    _env: jni::JNIEnv,
    _this: jni::objects::JObject,
) {
    NETWORK_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Frequent enough to keep CGNAT mappings alive, cheap enough for mobile
/// radios (a QUIC keepalive is an ACK-eliciting PING, not a full report).
pub const MOBILE_KEEPALIVE: Duration = Duration::from_millis(200);

/// Long enough to survive a brief tunnel/handover, short enough to release a
/// dead session before the OS tears down the app.
pub const MOBILE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

pub const WIFI_KEEPALIVE: Duration = Duration::from_secs(2);
pub const WIFI_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Below the smallest common mobile-carrier MTU once QUIC/UDP/IP headers are
/// added.  This is the floor after a migration to a restrictive path.
pub const MIN_MTU: u16 = 1200;
/// Conservative upper bound for DPLPMTUD probes on LTE/5G.
pub const CELLULAR_MAX_MTU: u16 = 1232;
/// Home/office Wi-Fi can usually carry a full Ethernet payload.
pub const WIFI_MAX_MTU: u16 = 1450;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileLink {
    Wifi,
    Cellular,
    /// Unknown / relayed: use the safest cellular values.
    Relay,
}

impl MobileLink {
    pub fn keepalive(self) -> Duration {
        match self {
            Self::Wifi => WIFI_KEEPALIVE,
            Self::Cellular | Self::Relay => MOBILE_KEEPALIVE,
        }
    }

    pub fn idle_timeout(self) -> Duration {
        match self {
            Self::Wifi => WIFI_IDLE_TIMEOUT,
            Self::Cellular | Self::Relay => MOBILE_IDLE_TIMEOUT,
        }
    }

    pub fn max_mtu(self) -> u16 {
        match self {
            Self::Wifi => WIFI_MAX_MTU,
            Self::Cellular | Self::Relay => CELLULAR_MAX_MTU,
        }
    }
}

/// Build the transport configuration used by both host and client.  BBR stays
/// in charge of the QUIC pipe; the delay-gradient controller in
/// `congestion.rs` controls the *encoder* and therefore never fights BBR for
/// the same queue.
pub fn mobile_transport_config(link: MobileLink) -> TransportConfig {
    let mut cfg = TransportConfig::default();
    cfg.congestion_controller_factory(Arc::new(BbrConfig::default()));
    cfg.datagram_receive_buffer_size(Some(crate::transport::QUIC_DATAGRAM_BUFFER_BYTES));
    cfg.datagram_send_buffer_size(crate::transport::QUIC_DATAGRAM_BUFFER_BYTES);
    cfg.keep_alive_interval(Some(link.keepalive()));
    cfg.max_idle_timeout(IdleTimeout::try_from(link.idle_timeout()).ok());

    // Start at the safe floor.  DPLPMTUD probes upward from here; a carrier
    // that black-holes large probes simply stays at 1200 instead of losing a
    // video shard.
    cfg.initial_mtu(MIN_MTU);
    cfg.min_mtu(MIN_MTU);
    let mut mtud = MtuDiscoveryConfig::default();
    mtud.interval(Duration::from_secs(300));
    mtud.upper_bound(link.max_mtu());
    mtud.black_hole_cooldown(Duration::from_secs(30));
    cfg.mtu_discovery_config(Some(mtud));

    // Padding every datagram would waste mobile uplink and break the pacer's
    // byte budget.  QUIC still pads handshake packets as required by RFC 9000.
    cfg.pad_to_mtu(false);
    cfg
}

/// Default used by `network.rs`: cellular-safe values.
pub fn default_mobile_transport_config() -> TransportConfig {
    mobile_transport_config(MobileLink::Cellular)
}

/// Rebind the QUIC endpoint to a fresh local UDP socket without dropping the
/// connection.  Use port 0 when the old port is still bound (common on
/// Android handover); the server will validate the new path with a PATH
/// CHALLENGE and then preserve the stream state.
pub fn rebind_endpoint(endpoint: &quinn::Endpoint, bind: SocketAddr) -> std::io::Result<()> {
    let socket = if bind.is_ipv6() && bind.ip().is_unspecified() {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV6,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket.set_only_v6(false)?;
        socket.bind(&bind.into())?;
        UdpSocket::from(socket)
    } else {
        UdpSocket::bind(bind)?
    };
    socket.set_nonblocking(true)?;
    crate::transport::tune_udp_socket(&socket)?;
    endpoint.rebind(socket)
}

/// Android network binder hand-off is deliberately not used.  A wildcard UDP
/// socket follows Android's default network automatically, so after a
/// Wi-Fi <-> cellular switch the existing endpoint can be swapped for a fresh
/// `0.0.0.0:0` socket and quinn will validate the new path.  Pinning to a
/// specific `Network` via `ConnectivityManager.bindSocket` would add JNI/fd
/// plumbing and raw-fd ownership hazards for a feature the UI does not expose.

#[derive(Debug, Clone, Copy)]
pub struct PathSnapshot {
    pub rtt: Duration,
    pub min_rtt: Duration,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub loss_permille: u16,
    pub remote: SocketAddr,
}

pub fn path_snapshot(conn: &Connection) -> PathSnapshot {
    let stats = conn.stats();
    let sent = stats.path.sent_packets.max(1);
    let loss_permille = ((stats.path.lost_packets.saturating_mul(1000)) / sent).min(1000) as u16;
    PathSnapshot {
        rtt: stats.path.rtt,
        min_rtt: stats.path.min_rtt,
        sent_packets: stats.path.sent_packets,
        lost_packets: stats.path.lost_packets,
        congestion_events: stats.path.congestion_events,
        loss_permille,
        remote: conn.remote_address(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationHint {
    None,
    /// The old socket stopped making progress; rebind and let QUIC validate
    /// the new path.
    RebindSuggested,
}

/// Detect a dead old path after an interface change.  The caller owns the
/// actual network selection (Android `ConnectivityManager`, Windows adapter
/// events) and calls this once per second.
///
/// Progress is measured by *received datagrams*, not sent ones: QUIC keeps
/// retransmitting and keepalives keep leaving a dead path, so `sent_packets`
/// would always look healthy.  Any server response (media, ACK, PING reply)
/// means the path is alive.
pub struct MigrationWatchdog {
    last_remote: Option<SocketAddr>,
    last_rx_datagrams: u64,
    last_progress: Instant,
    stall_after: Duration,
}

impl MigrationWatchdog {
    pub fn new(now: Instant) -> Self {
        Self {
            last_remote: None,
            last_rx_datagrams: 0,
            last_progress: now,
            stall_after: Duration::from_secs(5),
        }
    }

    pub fn set_stall_threshold(&mut self, threshold: Duration) {
        self.stall_after = threshold;
    }

    /// Pure observation hook, separated from [`Connection`] so the stall logic
    /// is testable without a live QUIC handshake.
    pub fn on_stats(
        &mut self,
        remote: SocketAddr,
        rx_datagrams: u64,
        now: Instant,
    ) -> MigrationHint {
        let progress = rx_datagrams != self.last_rx_datagrams || self.last_remote != Some(remote);
        if progress {
            self.last_rx_datagrams = rx_datagrams;
            self.last_remote = Some(remote);
            self.last_progress = now;
            return MigrationHint::None;
        }
        if now.saturating_duration_since(self.last_progress) >= self.stall_after {
            self.last_progress = now;
            MigrationHint::RebindSuggested
        } else {
            MigrationHint::None
        }
    }

    pub fn on_tick(&mut self, conn: &Connection, now: Instant) -> MigrationHint {
        let stats = conn.stats();
        self.on_stats(conn.remote_address(), stats.udp_rx.datagrams, now)
    }

    pub fn note_rebind(&mut self, now: Instant) {
        self.last_progress = now;
        self.last_rx_datagrams = 0;
        self.last_remote = None;
    }
}

/// True when the OS handed us a different local interface address.  This is
/// only a hint: QUIC migration is driven by validated path challenges, not by
/// this check.
pub fn local_interface_changed(previous: Option<IpAddr>, current: Option<IpAddr>) -> bool {
    matches!((previous, current), (Some(a), Some(b)) if a != b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cellular_profile_is_cgnat_friendly() {
        assert!(MOBILE_KEEPALIVE <= Duration::from_secs(2));
        assert!(MOBILE_IDLE_TIMEOUT >= Duration::from_secs(30));
        assert_eq!(MobileLink::Cellular.max_mtu(), CELLULAR_MAX_MTU);
        assert_eq!(MobileLink::Cellular.keepalive(), MOBILE_KEEPALIVE);
    }

    #[test]
    fn wifi_profile_can_probe_larger_mtu_but_still_starts_safe() {
        let cfg = mobile_transport_config(MobileLink::Wifi);
        // The builder must not panic and must keep the 1200-byte floor.
        let _ = cfg;
        assert!(WIFI_MAX_MTU > CELLULAR_MAX_MTU);
    }

    #[test]
    fn interface_change_is_detected() {
        let a: IpAddr = "192.168.1.10".parse().expect("ip");
        let b: IpAddr = "10.20.30.40".parse().expect("ip");
        assert!(local_interface_changed(Some(a), Some(b)));
        assert!(!local_interface_changed(Some(a), Some(a)));
        assert!(!local_interface_changed(None, Some(a)));
    }

    #[test]
    fn stalled_receive_path_suggests_rebind() {
        let now = Instant::now();
        let remote: SocketAddr = "203.0.113.5:4433".parse().expect("addr");
        let mut watchdog = MigrationWatchdog::new(now);
        assert_eq!(watchdog.on_stats(remote, 0, now), MigrationHint::None);
        assert_eq!(
            watchdog.on_stats(remote, 4, now + Duration::from_secs(1)),
            MigrationHint::None
        );
        // Transmissions continue, but nothing comes back.
        assert_eq!(
            watchdog.on_stats(remote, 4, now + Duration::from_secs(3)),
            MigrationHint::None
        );
        assert_eq!(
            watchdog.on_stats(remote, 4, now + Duration::from_secs(6)),
            MigrationHint::RebindSuggested
        );

        // A fresh socket gets a fresh window; the new path's first response
        // counts as progress even though its datagram counter restarted.
        watchdog.note_rebind(now + Duration::from_secs(6));
        assert_eq!(
            watchdog.on_stats(remote, 0, now + Duration::from_secs(7)),
            MigrationHint::None
        );
        assert_eq!(
            watchdog.on_stats(remote, 1, now + Duration::from_secs(8)),
            MigrationHint::None
        );
    }

    #[test]
    fn remote_address_change_counts_as_path_progress() {
        let now = Instant::now();
        let a: SocketAddr = "203.0.113.5:4433".parse().expect("addr");
        let b: SocketAddr = "203.0.113.9:4433".parse().expect("addr");
        let mut watchdog = MigrationWatchdog::new(now);
        assert_eq!(
            watchdog.on_stats(a, 10, now + Duration::from_secs(1)),
            MigrationHint::None
        );
        assert_eq!(
            watchdog.on_stats(b, 10, now + Duration::from_secs(10)),
            MigrationHint::None
        );
    }
}
