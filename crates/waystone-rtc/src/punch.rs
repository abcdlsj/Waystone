//! Birthday-attack UDP hole punching for Cone (NAT3) <-> Symmetric (NAT4) paths.
//!
//! Standard ICE exchanges one STUN reflexive port per peer and fails whenever a
//! Port-Restricted Cone host (NAT3) meets a Symmetric NAT controller (NAT4):
//! the controller's NAT rewrites the source port for the host's address, so the
//! host's NAT3 drops the packet.
//!
//! When a session connects over a TURN relay and both peers advertised public
//! IPv4 `srflx` candidates, this module runs a short one-shot birthday punch:
//!
//! 1. The **host** sprays [`HOST_SPRAY_TOTAL`] random ports on the controller's
//!    external IP from its primary UDP socket, opening that many NAT3 pinholes.
//! 2. The **controller** opens waves of up to [`CONTROLLER_WAVE_SOCKETS`]
//!    ephemeral UDP sockets (bounded well below iOS's default FD limit) and
//!    probes the host's fixed `srflx` address.
//! 3. As soon as one probe lands inside an open pinhole, the host replies with
//!    an ack and registers the observed peer address as a direct remote
//!    candidate, while the controller promotes the winning socket as a local
//!    host candidate. `str0m` then runs normal ICE checks over the punched
//!    pinhole and upgrades media seamlessly from TURN relay to direct P2P.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use rand::Rng;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tracing::{debug, info};

/// Packet tag sent by the host to open NAT3 outbound pinholes.
const MAGIC_SPRAY: [u8; 4] = *b"WSP1";
/// Packet tag sent by controller burst sockets toward the host's `srflx`.
const MAGIC_HIT: [u8; 4] = *b"WSP2";
/// Packet tag sent by the host to confirm a pinhole hit.
const MAGIC_ACK: [u8; 4] = *b"WSP3";

/// Exact byte length of every punch packet (`4-byte magic + 8-byte token`).
pub const PACKET_LEN: usize = 12;

/// Ephemeral port range sampled for NAT3 pinholes.
const PORT_MIN: u16 = 10_000;
const PORT_MAX: u16 = 65_000;

/// Host spray shape: 6 gentle waves of 128 ports = 768 NAT3 pinholes (~225 ms).
pub const HOST_SPRAY_WAVE_SIZE: usize = 128;
pub const HOST_SPRAY_WAVES: usize = 6;
pub const HOST_SPRAY_TOTAL: usize = HOST_SPRAY_WAVE_SIZE * HOST_SPRAY_WAVES;
pub const HOST_SPRAY_INTERVAL: Duration = Duration::from_millis(45);

/// Controller burst shape: up to 4 waves of 128 sockets (~99.9% hit rate against
/// 768 open pinholes), keeping simultaneous open FDs <= 128 for iOS safety.
const CONTROLLER_START_DELAY: Duration = Duration::from_millis(320);
const CONTROLLER_WAVE_SOCKETS: usize = 128;
const CONTROLLER_WAVES: usize = 4;
const CONTROLLER_WAVE_WAIT: Duration = Duration::from_millis(380);
const CONTROLLER_RESEND_AFTER: Duration = Duration::from_millis(120);
const CONTROLLER_POLL_STEP: Duration = Duration::from_millis(15);

/// A decoded punch control datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchPacket {
    Spray([u8; 8]),
    Hit([u8; 8]),
    Ack([u8; 8]),
}

pub fn encode_spray(token: [u8; 8]) -> [u8; PACKET_LEN] {
    encode(MAGIC_SPRAY, token)
}

pub fn encode_hit(token: [u8; 8]) -> [u8; PACKET_LEN] {
    encode(MAGIC_HIT, token)
}

pub fn encode_ack(token: [u8; 8]) -> [u8; PACKET_LEN] {
    encode(MAGIC_ACK, token)
}

fn encode(magic: [u8; 4], token: [u8; 8]) -> [u8; PACKET_LEN] {
    let mut out = [0u8; PACKET_LEN];
    out[..4].copy_from_slice(&magic);
    out[4..].copy_from_slice(&token);
    out
}

/// Parses a punch packet, returning `None` for any non-punch traffic (STUN,
/// DTLS, SRTP, TURN).
pub fn decode(bytes: &[u8]) -> Option<PunchPacket> {
    if bytes.len() != PACKET_LEN {
        return None;
    }
    let mut token = [0u8; 8];
    token.copy_from_slice(&bytes[4..]);
    match [bytes[0], bytes[1], bytes[2], bytes[3]] {
        MAGIC_SPRAY => Some(PunchPacket::Spray(token)),
        MAGIC_HIT => Some(PunchPacket::Hit(token)),
        MAGIC_ACK => Some(PunchPacket::Ack(token)),
        _ => None,
    }
}

/// True when `addr` is a public IPv4 address worth birthday-punching against.
pub fn is_punchable_ipv4(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => {
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && !ip.is_multicast()
        }
        IpAddr::V6(_) => false,
    }
}

/// Paced port spray state driven on the host's primary UDP socket.
pub struct HostSpray {
    peer_ip: IpAddr,
    ports: Vec<u16>,
    sent: usize,
    token: [u8; 8],
    next_send: Instant,
}

impl HostSpray {
    pub fn new(peer_ip: IpAddr, now: Instant) -> Self {
        let mut rng = rand::rng();
        let mut token = [0u8; 8];
        rng.fill(&mut token);
        HostSpray {
            peer_ip,
            ports: sample_distinct_ports(HOST_SPRAY_TOTAL),
            sent: 0,
            token,
            next_send: now,
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        (self.sent < self.ports.len()).then_some(self.next_send)
    }

    pub fn is_done(&self) -> bool {
        self.sent >= self.ports.len()
    }

    /// Sends one wave when due and returns how many probes went out.
    pub fn step(&mut self, socket: &UdpSocket, now: Instant) -> usize {
        if self.sent >= self.ports.len() || now < self.next_send {
            return 0;
        }
        let packet = encode_spray(self.token);
        let end = (self.sent + HOST_SPRAY_WAVE_SIZE).min(self.ports.len());
        let mut dispatched = 0;
        for &port in &self.ports[self.sent..end] {
            let target = SocketAddr::new(self.peer_ip, port);
            if socket.try_send_to(&packet, target).is_ok() {
                dispatched += 1;
            }
        }
        self.sent = end;
        self.next_send = now + HOST_SPRAY_INTERVAL;
        dispatched
    }
}

fn sample_distinct_ports(count: usize) -> Vec<u16> {
    let mut rng = rand::rng();
    let mut seen = HashSet::with_capacity(count);
    let mut ports = Vec::with_capacity(count);
    while ports.len() < count {
        let port = rng.random_range(PORT_MIN..=PORT_MAX);
        if seen.insert(port) {
            ports.push(port);
        }
    }
    ports
}

/// Winning socket produced by [`run_controller_burst`].
pub struct PunchedSocket {
    pub socket: UdpSocket,
    /// Concrete local address matching the interface routing to the host.
    pub local_addr: SocketAddr,
}

/// Opens waves of ephemeral UDP sockets on the controller to collide with the
/// host's sprayed NAT3 pinholes.
///
/// Resolves with the first socket that receives a valid `WSP3` ack from
/// `host_srflx`, or `None` if all waves time out or `cancel` fires.
pub async fn run_controller_burst(
    host_srflx: SocketAddr,
    local_ip: IpAddr,
    mut cancel: oneshot::Receiver<()>,
) -> Option<PunchedSocket> {
    // Let the host open its first pinhole waves before bursting.
    tokio::select! {
        _ = &mut cancel => return None,
        _ = tokio::time::sleep(CONTROLLER_START_DELAY) => {}
    }

    let mut token = [0u8; 8];
    rand::rng().fill(&mut token);
    let hit = encode_hit(token);
    let bind_addr = if local_ip.is_ipv4() && !local_ip.is_unspecified() {
        SocketAddr::new(local_ip, 0)
    } else {
        SocketAddr::from(([0, 0, 0, 0], 0))
    };

    for wave in 0..CONTROLLER_WAVES {
        let mut sockets = Vec::with_capacity(CONTROLLER_WAVE_SOCKETS);
        for _ in 0..CONTROLLER_WAVE_SOCKETS {
            let bound = match UdpSocket::bind(bind_addr).await {
                Ok(sock) => sock,
                Err(_) if !bind_addr.ip().is_unspecified() => {
                    match UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await {
                        Ok(sock) => sock,
                        Err(_) => break,
                    }
                }
                Err(_) => break,
            };
            let _ = bound.try_send_to(&hit, host_srflx);
            sockets.push(bound);
        }
        if sockets.is_empty() {
            debug!("birthday punch wave {wave}: could not bind any UDP sockets");
            return None;
        }
        debug!(
            "birthday punch wave {}/{}: sent {} probes to {host_srflx}",
            wave + 1,
            CONTROLLER_WAVES,
            sockets.len()
        );

        let wave_start = Instant::now();
        let mut resent = false;
        let mut buf = [0u8; 64];

        while wave_start.elapsed() < CONTROLLER_WAVE_WAIT {
            if !resent && wave_start.elapsed() >= CONTROLLER_RESEND_AFTER {
                resent = true;
                for sock in &sockets {
                    let _ = sock.try_send_to(&hit, host_srflx);
                }
            }

            for idx in 0..sockets.len() {
                while let Ok((len, from)) = sockets[idx].try_recv_from(&mut buf) {
                    if from == host_srflx && decode(&buf[..len]) == Some(PunchPacket::Ack(token)) {
                        let winner = sockets.swap_remove(idx);
                        drop(sockets);
                        let _ = crate::net::enable_destination_info(&winner);
                        let port = winner.local_addr().ok()?.port();
                        let concrete_ip = winner
                            .local_addr()
                            .ok()
                            .map(|a| a.ip())
                            .filter(|ip| !ip.is_unspecified())
                            .unwrap_or(local_ip);
                        let local_addr = SocketAddr::new(concrete_ip, port);
                        info!(
                            "birthday punch succeeded in wave {} on {local_addr} -> {host_srflx}",
                            wave + 1
                        );
                        return Some(PunchedSocket {
                            socket: winner,
                            local_addr,
                        });
                    }
                }
            }

            tokio::select! {
                _ = &mut cancel => return None,
                _ = tokio::time::sleep(CONTROLLER_POLL_STEP) => {}
            }
        }
    }

    debug!("birthday punch did not collide after {CONTROLLER_WAVES} waves");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip_and_non_punch_rejection() {
        let token = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(
            decode(&encode_spray(token)),
            Some(PunchPacket::Spray(token))
        );
        assert_eq!(decode(&encode_hit(token)), Some(PunchPacket::Hit(token)));
        assert_eq!(decode(&encode_ack(token)), Some(PunchPacket::Ack(token)));

        // STUN Binding Request / short / random packets are never mistaken for punch.
        assert_eq!(
            decode(&[0x00, 0x01, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0]),
            None
        );
        assert_eq!(decode(b"WSP2short"), None);
        assert_eq!(decode(&[0u8; 20]), None);
    }

    #[test]
    fn distinct_port_sampling_stays_in_range() {
        let ports = sample_distinct_ports(HOST_SPRAY_TOTAL);
        assert_eq!(ports.len(), HOST_SPRAY_TOTAL);
        let unique: HashSet<u16> = ports.iter().copied().collect();
        assert_eq!(unique.len(), HOST_SPRAY_TOTAL);
        assert!(ports.iter().all(|&p| (PORT_MIN..=PORT_MAX).contains(&p)));
    }

    #[test]
    fn filters_private_and_loopback_addresses() {
        assert!(!is_punchable_ipv4("127.0.0.1:1234".parse().unwrap()));
        assert!(!is_punchable_ipv4("192.168.1.10:1234".parse().unwrap()));
        assert!(!is_punchable_ipv4("10.0.0.5:1234".parse().unwrap()));
        assert!(!is_punchable_ipv4("[240e::1]:1234".parse().unwrap()));
        assert!(is_punchable_ipv4("218.82.1.230:9530".parse().unwrap()));
    }

    #[tokio::test]
    async fn controller_burst_wins_when_host_replies_with_ack() {
        let host = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let host_addr = host.local_addr().unwrap();

        let echo = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let (len, from) = host.recv_from(&mut buf).await.unwrap();
                if let Some(PunchPacket::Hit(token)) = decode(&buf[..len]) {
                    let ack = encode_ack(token);
                    host.send_to(&ack, from).await.unwrap();
                    break;
                }
            }
        });

        let (_tx, cancel) = oneshot::channel();
        let punched = run_controller_burst(host_addr, "127.0.0.1".parse().unwrap(), cancel)
            .await
            .expect("burst should win on loopback echo");
        assert_eq!(punched.local_addr.ip(), IpAddr::from([127, 0, 0, 1]));
        assert_ne!(punched.local_addr.port(), 0);
        echo.await.unwrap();
    }
}
