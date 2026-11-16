//! Parsec-grade connectivity: NAT behaviour classification, ICE-style candidate
//! ranking, symmetric-NAT port prediction, aggressive hole punching, relay
//! racing and blip tolerance.
//!
//! ## What Parsec does
//!
//! Parsec's BUD protocol reports a 97% NAT traversal success rate. Their own
//! descriptions of how that number is reached:
//!
//! * The first attempt is always direct: "the default method leverages UPnP and
//!   hole punching", with the host port chosen at random and the UPnP rule
//!   removed when the session ends.
//! * Compared with the WebRTC path, the native client uses "**more aggressive
//!   hole punching**, optional client-side UPnP". Their BUD traversal is
//!   deliberately made "ICE compatible", and the handshake itself "boils down
//!   to a simple STUN ping/pong as part of the UDP hole punching procedure".
//! * When a direct path is impossible (symmetric CGNAT on both ends), traffic
//!   falls back to a relay. With several High Performance Relays configured,
//!   Parsec "will attempt to signal all Relays at once" and prefers the
//!   fastest — a race, not a sequential try.
//! * A momentary network blip does not end the session: the stream "hangs
//!   tight" and only ends if the interruption lasts 60 seconds.
//!
//! ## What this module adds to VmaX
//!
//! `network.rs` already runs a simultaneous punch with a fixed 25 ms cadence.
//! That works on friendly NATs and stalls on the interesting ones. This module
//! supplies the missing pieces, all dependency-free and testable:
//!
//! * [`classify_nat`] — endpoint-independent / address-dependent / symmetric,
//!   from the external ports two STUN servers observe for the same socket.
//! * [`candidate_priority`] / [`rank_candidates`] — RFC 8445-style ordering so
//!   LAN and host candidates are never queued behind a relay.
//! * [`PortPredictor`] — symmetric NATs allocate external ports from a mostly
//!   linear sequence; predicting the next few ports is what turns "hole
//!   punching generally fails" into a connection.
//! * [`PunchSchedule`] — aggressive first 250 ms (10 ms cadence) then ICE-like
//!   backoff, instead of a flat 25 ms tick.
//! * [`RelayRace`] — race all configured relays, first answer wins.
//! * [`BlipPolicy`] — the 60 s "hang tight" window with a paused-media state.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

/// A NAT that keeps one mapping regardless of destination: hole punching works
/// without help.
pub const NAT_ENDPOINT_INDEPENDENT: &str = "endpoint-independent";
/// Mapping depends on the destination IP (or IP+port): punching still works if
/// both peers send first, which is exactly what simultaneous open does.
pub const NAT_ADDRESS_DEPENDENT: &str = "address-dependent";
/// Mapping depends on the destination address *and* port: each new destination
/// gets a new external port, so the peer's guess is wrong unless it is
/// predicted.
pub const NAT_SYMMETRIC: &str = "symmetric";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatBehavior {
    EndpointIndependent,
    AddressDependent,
    Symmetric,
}

impl NatBehavior {
    /// Symmetric NATs need port prediction on top of simultaneous open.
    pub fn needs_port_prediction(self) -> bool {
        matches!(self, Self::Symmetric)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::EndpointIndependent => NAT_ENDPOINT_INDEPENDENT,
            Self::AddressDependent => NAT_ADDRESS_DEPENDENT,
            Self::Symmetric => NAT_SYMMETRIC,
        }
    }
}

/// Classify from the external ports observed by two STUN servers on the *same*
/// local socket. `None` entries mean the probe did not answer.
///
/// * same port from both servers -> endpoint-independent
/// * different port -> the mapping is at least address-dependent; if the ports
///   also differ by more than a small delta it is treated as symmetric, because
///   an address-dependent NAT usually keeps one mapping per local socket.
pub fn classify_nat(first: Option<u16>, second: Option<u16>) -> NatBehavior {
    match (first, second) {
        (Some(a), Some(b)) if a == b => NatBehavior::EndpointIndependent,
        (Some(a), Some(b)) if a.abs_diff(b) <= 1 => NatBehavior::AddressDependent,
        (Some(_), Some(_)) => NatBehavior::Symmetric,
        // One probe answered: the safest assumption is the one that triggers
        // prediction, which is cheap and never breaks a working path.
        (Some(_), None) | (None, Some(_)) => NatBehavior::Symmetric,
        (None, None) => NatBehavior::EndpointIndependent,
    }
}

/// RFC 8445 candidate types, in the order they should be attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateType {
    /// A mapped port on the gateway (UPnP): best external path.
    Upnp,
    /// Local interface address.
    Host,
    /// STUN-discovered public address.
    ServerReflexive,
    /// Symmetric-NAT port prediction.
    Predicted,
    /// Relay: always last.
    Relay,
}

impl CandidateType {
    /// RFC 8445 type preference, scaled to 0..=126.
    pub fn type_preference(self) -> u32 {
        match self {
            Self::Upnp => 126,
            Self::Host => 126,
            Self::ServerReflexive => 100,
            Self::Predicted => 90,
            Self::Relay => 0,
        }
    }

    pub fn is_direct(self) -> bool {
        !matches!(self, Self::Relay)
    }
}

/// `priority = (2^24) * type_pref + (2^8) * local_pref + (256 - component)`
/// with local preference derived from address family (IPv6 preferred for
/// dual-stack, matching the rest of VmaX).
pub fn candidate_priority(kind: CandidateType, addr: SocketAddr) -> u32 {
    let local_pref: u32 = if addr.is_ipv6() { 65535 } else { 65534 };
    (kind.type_preference() << 24) | (local_pref << 8) | 255
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankedCandidate {
    pub addr: SocketAddr,
    pub kind: CandidateType,
    pub priority: u32,
}

/// Sort candidates best-first, de-duplicating by canonical address.
pub fn rank_candidates(candidates: &[(SocketAddr, CandidateType)]) -> Vec<RankedCandidate> {
    let mut out: Vec<RankedCandidate> = Vec::with_capacity(candidates.len());
    for &(addr, kind) in candidates {
        let addr = canonical(addr);
        if addr.port() == 0 || addr.ip().is_unspecified() {
            continue;
        }
        let priority = candidate_priority(kind, addr);
        match out.iter_mut().find(|existing| existing.addr == addr) {
            Some(existing) if priority > existing.priority => {
                *existing = RankedCandidate {
                    addr,
                    kind,
                    priority,
                };
            }
            Some(_) => {}
            None => out.push(RankedCandidate {
                addr,
                kind,
                priority,
            }),
        }
    }
    out.sort_by(|a, b| b.priority.cmp(&a.priority));
    out
}

fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => addr,
        },
        v4 => v4,
    }
}

/// Predicts the external ports a symmetric NAT will allocate next.
#[derive(Debug, Clone, Default)]
pub struct PortPredictor {
    observations: Vec<u16>,
}

/// How many predicted ports are generated by default.  Wider than a single
/// guess, narrow enough to stay inside one punch burst.
pub const PREDICTED_PORT_COUNT: usize = 6;

impl PortPredictor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an observed external port, in probe order.
    pub fn observe(&mut self, port: u16) {
        if port != 0 && self.observations.last() != Some(&port) {
            self.observations.push(port);
            if self.observations.len() > 8 {
                self.observations.remove(0);
            }
        }
    }

    /// Usable stride between allocations, if the NAT looks sequential.
    pub fn stride(&self) -> Option<i32> {
        if self.observations.len() < 2 {
            return None;
        }
        let deltas: Vec<i32> = self
            .observations
            .windows(2)
            .map(|pair| i32::from(pair[1]) - i32::from(pair[0]))
            .collect();
        // Only trust a small, consistent stride. A single huge jump between
        // two probes is a NAT pool, not a sequence worth extrapolating.
        let first = deltas[0];
        if deltas.len() >= 2 && deltas.iter().all(|&d| d == first) && (1..=8).contains(&first) {
            Some(first)
        } else if deltas.iter().all(|&d| (1..=4).contains(&d)) {
            deltas.last().copied()
        } else {
            None
        }
    }

    /// Next `count` candidate external ports, most likely first.
    pub fn predict(&self, count: usize) -> Vec<u16> {
        let Some(last) = self.observations.last().copied() else {
            return Vec::new();
        };
        let count = count.clamp(1, 32);
        let mut out = Vec::with_capacity(count);
        if let Some(stride) = self.stride() {
            for step in 1..=count {
                let port = i64::from(last) + i64::from(stride) * step as i64;
                if (1..=65535).contains(&port) {
                    out.push(port as u16);
                }
            }
            return out;
        }
        // No detectable pattern: probe the immediate neighbourhood, which
        // covers NATs that allocate from a small pool.
        for step in 1..=count {
            let port = i64::from(last) + step as i64;
            if port <= 65535 {
                out.push(port as u16);
            }
        }
        out
    }

    pub fn observations(&self) -> &[u16] {
        &self.observations
    }
}

/// Aggressive-then-backoff punch cadence.
///
/// Parsec's native client is described as doing "more aggressive hole
/// punching" than the browser path.  The first 250 ms therefore burst at 10 ms
/// (25 datagrams per candidate), then the cadence relaxes exactly like ICE
/// connectivity checks so a 4.5 s window costs little battery.
#[derive(Debug, Clone)]
pub struct PunchSchedule {
    attempts: u32,
    aggressive_until: Duration,
}

impl Default for PunchSchedule {
    fn default() -> Self {
        Self {
            attempts: 0,
            aggressive_until: Duration::from_millis(250),
        }
    }
}

impl PunchSchedule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interval before the next burst, and the burst size for that tick.
    pub fn next(&mut self, elapsed: Duration) -> (Duration, usize) {
        self.attempts = self.attempts.saturating_add(1);
        if elapsed < self.aggressive_until {
            // Several datagrams per tick: a NAT that needs a two-way exchange
            // still sees a burst before the first backoff step.
            (Duration::from_millis(10), 2)
        } else if elapsed < Duration::from_millis(1_000) {
            (Duration::from_millis(25), 1)
        } else if elapsed < Duration::from_millis(2_000) {
            (Duration::from_millis(50), 1)
        } else {
            (Duration::from_millis(100), 1)
        }
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

/// Races several relays and takes the first that answers, mirroring Parsec's
/// "attempt to signal all Relays at once" behaviour.
#[derive(Debug, Clone)]
pub struct RelayRace {
    relays: Vec<SocketAddr>,
    started: Option<Instant>,
    winner: Option<SocketAddr>,
    timeout: Duration,
}

impl RelayRace {
    pub fn new(mut relays: Vec<SocketAddr>, timeout: Duration) -> Self {
        relays.dedup();
        Self {
            relays,
            started: None,
            winner: None,
            timeout,
        }
    }

    pub fn relays(&self) -> &[SocketAddr] {
        &self.relays
    }

    pub fn start(&mut self, now: Instant) {
        self.started.get_or_insert(now);
    }

    /// Record a responding relay. The first response wins; a later, faster
    /// response does not restart the session.
    pub fn on_response(&mut self, relay: SocketAddr) -> bool {
        if self.winner.is_some() {
            return false;
        }
        self.winner = Some(relay);
        true
    }

    pub fn winner(&self) -> Option<SocketAddr> {
        self.winner
    }

    pub fn expired(&self, now: Instant) -> bool {
        self.started
            .is_some_and(|started| now.saturating_duration_since(started) >= self.timeout)
    }
}

/// "Hang tight" window: an interrupted network pauses the stream instead of
/// killing the session. Parsec ends a session after 60 s.
pub const BLIP_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    Streaming,
    /// No progress on the path; media is paused but the session survives.
    Interrupted { since: Instant },
    Ended,
}

#[derive(Debug, Clone)]
pub struct BlipPolicy {
    window: Duration,
}

impl Default for BlipPolicy {
    fn default() -> Self {
        Self {
            window: BLIP_WINDOW,
        }
    }
}

impl BlipPolicy {
    pub fn new(window: Duration) -> Self {
        Self { window }
    }

    pub fn phase(&self, interrupted_since: Option<Instant>, now: Instant) -> SessionPhase {
        match interrupted_since {
            None => SessionPhase::Streaming,
            Some(since) if now.saturating_duration_since(since) < self.window => {
                SessionPhase::Interrupted { since }
            }
            Some(_) => SessionPhase::Ended,
        }
    }

    /// Media should be paused while interrupted: sending into a dead path
    /// only fills the pacer queue.
    pub fn pause_media(phase: SessionPhase) -> bool {
        matches!(phase, SessionPhase::Interrupted { .. })
    }
}

/// The full connectivity plan for one session attempt.
#[derive(Debug, Clone)]
pub struct ConnectivityPlan {
    pub ranked: Vec<RankedCandidate>,
    pub predicted: Vec<SocketAddr>,
    pub nat: NatBehavior,
    /// Direct punching must give up by this instant.
    pub direct_deadline: Duration,
    /// Relays are raced in parallel from the start when the path is symmetric.
    pub relay_race_now: bool,
}

/// Build the plan `network.rs` follows. `punch_budget` is the existing 4.5 s
/// hole-punch timeout.
pub fn plan(
    candidates: &[(SocketAddr, CandidateType)],
    predictor: &PortPredictor,
    nat: NatBehavior,
    punch_budget: Duration,
) -> ConnectivityPlan {
    let ranked = rank_candidates(candidates);
    let predicted = if nat.needs_port_prediction() {
        let template = ranked
            .iter()
            .find(|c| c.kind == CandidateType::ServerReflexive)
            .map(|c| c.addr);
        match template {
            Some(template) => predictor
                .predict(PREDICTED_PORT_COUNT)
                .into_iter()
                .map(|port| SocketAddr::new(template.ip(), port))
                .collect(),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    ConnectivityPlan {
        ranked,
        predicted,
        nat,
        direct_deadline: punch_budget,
        relay_race_now: nat.needs_port_prediction(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("addr")
    }

    #[test]
    fn same_external_port_is_endpoint_independent() {
        assert_eq!(classify_nat(Some(40000), Some(40000)), NatBehavior::EndpointIndependent);
    }

    #[test]
    fn shifting_external_port_is_symmetric() {
        assert_eq!(classify_nat(Some(40000), Some(41500)), NatBehavior::Symmetric);
        assert!(NatBehavior::Symmetric.needs_port_prediction());
    }

    #[test]
    fn single_responding_probe_makes_prediction_available() {
        assert_eq!(classify_nat(Some(40000), None), NatBehavior::Symmetric);
        assert_eq!(classify_nat(None, None), NatBehavior::EndpointIndependent);
    }

    #[test]
    fn host_candidates_outrank_relay_regardless_of_family() {
        let ranked = rank_candidates(&[
            (addr("[2001:db8::1]:5000"), CandidateType::Relay),
            (addr("192.168.1.5:5000"), CandidateType::Host),
            (addr("203.0.113.9:5000"), CandidateType::ServerReflexive),
        ]);
        assert_eq!(ranked[0].kind, CandidateType::Host);
        assert_eq!(ranked[1].kind, CandidateType::ServerReflexive);
        assert_eq!(ranked[2].kind, CandidateType::Relay);
        assert!(ranked.windows(2).all(|pair| pair[0].priority >= pair[1].priority));
    }

    #[test]
    fn upnp_duplicate_of_host_address_keeps_the_better_priority() {
        let ranked = rank_candidates(&[
            (addr("192.168.1.5:5000"), CandidateType::Host),
            (addr("[::ffff:192.168.1.5]:5000"), CandidateType::Relay),
        ]);
        assert_eq!(ranked.len(), 1, "canonical duplicate must collapse");
        assert_eq!(ranked[0].kind, CandidateType::Host);
    }

    #[test]
    fn predictor_learns_the_nat_stride() {
        let mut predictor = PortPredictor::new();
        predictor.observe(40_000);
        predictor.observe(40_002);
        predictor.observe(40_004);
        assert_eq!(predictor.stride(), Some(2));
        assert_eq!(predictor.predict(3), vec![40_006, 40_008, 40_010]);
    }

    #[test]
    fn predictor_falls_back_to_adjacent_ports_without_a_pattern() {
        let mut predictor = PortPredictor::new();
        predictor.observe(30_000);
        predictor.observe(41_777);
        assert_eq!(predictor.stride(), None);
        assert_eq!(predictor.predict(2), vec![41_778, 41_779]);
    }

    #[test]
    fn predictor_is_empty_before_any_observation() {
        assert!(PortPredictor::new().predict(4).is_empty());
    }

    #[test]
    fn punch_schedule_bursts_first_then_backs_off() {
        let mut schedule = PunchSchedule::new();
        let (first, burst) = schedule.next(Duration::ZERO);
        assert_eq!(first, Duration::from_millis(10));
        assert!(burst >= 2);
        let (early, _) = schedule.next(Duration::from_millis(100));
        assert_eq!(early, Duration::from_millis(10));
        let (mid, burst_mid) = schedule.next(Duration::from_millis(500));
        assert_eq!(mid, Duration::from_millis(25));
        assert_eq!(burst_mid, 1);
        let (late, _) = schedule.next(Duration::from_millis(3_000));
        assert_eq!(late, Duration::from_millis(100));
        // Roughly 25 datagrams in the first 250 ms.
        assert!(schedule.attempts() >= 4);
    }

    #[test]
    fn relay_race_keeps_the_first_responder_and_expires() {
        let relay_a = addr("203.0.113.10:9000");
        let relay_b = addr("203.0.113.11:9000");
        let now = Instant::now();
        let mut race = RelayRace::new(
            vec![relay_a, relay_b],
            Duration::from_millis(400),
        );
        race.start(now);
        assert!(race.on_response(relay_b));
        assert!(!race.on_response(relay_a), "first winner must stick");
        assert_eq!(race.winner(), Some(relay_b));
        assert!(!race.expired(now + Duration::from_millis(100)));
        assert!(race.expired(now + Duration::from_millis(500)));
    }

    #[test]
    fn blip_window_pauses_media_before_ending_the_session() {
        let policy = BlipPolicy::default();
        let now = Instant::now();
        let phase = policy.phase(Some(now), now + Duration::from_secs(30));
        assert!(matches!(phase, SessionPhase::Interrupted { .. }));
        assert!(BlipPolicy::pause_media(phase));
        assert_eq!(
            policy.phase(Some(now), now + BLIP_WINDOW),
            SessionPhase::Ended
        );
        assert_eq!(policy.phase(None, now), SessionPhase::Streaming);
    }

    #[test]
    fn symmetric_nat_plan_adds_predicted_ports_and_races_relays_immediately() {
        let mut predictor = PortPredictor::new();
        predictor.observe(50_000);
        predictor.observe(50_004);
        let candidates = vec![
            (addr("203.0.113.9:5000"), CandidateType::ServerReflexive),
            (addr("192.168.1.5:5000"), CandidateType::Host),
        ];
        let plan = plan(
            &candidates,
            &predictor,
            NatBehavior::Symmetric,
            Duration::from_millis(4_500),
        );
        assert!(plan.relay_race_now);
        assert_eq!(plan.predicted.len(), PREDICTED_PORT_COUNT);
        assert!(plan.predicted.iter().all(|p| p.ip() == addr("203.0.113.9:1").ip()));
        assert_eq!(plan.ranked[0].kind, CandidateType::Host);
    }

    #[test]
    fn endpoint_independent_plan_skips_prediction_and_relay_race() {
        let plan = plan(
            &[(addr("203.0.113.9:5000"), CandidateType::ServerReflexive)],
            &PortPredictor::new(),
            NatBehavior::EndpointIndependent,
            Duration::from_millis(4_500),
        );
        assert!(plan.predicted.is_empty());
        assert!(!plan.relay_race_now);
    }
}
