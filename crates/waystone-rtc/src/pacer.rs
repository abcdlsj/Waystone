//! Leaky-bucket datagram pacer for outgoing WebRTC media.
//!
//! Without pacing (`str0m`'s `NullPacer`), a single 80-130 KB IDR keyframe emits
//! 70-115 UDP packets back-to-back in under a millisecond, overflowing shallow
//! residential/CGNAT uplink queues (~50-60 KB burst limit) and causing tail
//! drops even when the average bitrate is well below link capacity.
//!
//! Only large RTP video packets enter the pacer; STUN, DTLS/SCTP input, RTCP
//! feedback (NACK/PLI) and small Opus audio packets always bypass immediately.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Small packets (Opus audio, small control frames) bypass pacing completely.
const SMALL_PACKET_BYTES: usize = 300;
/// Short initial burst allowance (~8 MTU packets) so normal 60 fps P-frames
/// suffer almost zero added latency while large IDR keyframes smooth out.
const MAX_BURST_BYTES: f64 = 10_000.0;
/// Default ceiling for direct P2P pacing (24 Mbps), staying safely under
/// typical 30-40 Mbps residential uplink policers.
const DEFAULT_DIRECT_MAX_BPS: f64 = 24_000_000.0;
const DIRECT_MIN_BPS: f64 = 8_000_000.0;
const RELAY_MIN_BPS: f64 = 1_000_000.0;
const MIN_QUEUE_BYTES: usize = 96 * 1024;
const MAX_QUEUE_BYTES: usize = 512 * 1024;
/// Tolerance for f64 token refill rounding so `poll_ready(next_deadline())`
/// always makes forward progress.
const TOKEN_EPSILON: f64 = 1e-3;

/// One outgoing datagram queued for paced transmission.
#[derive(Debug, Clone)]
pub struct PacedDatagram {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub payload: Vec<u8>,
}

/// Returns true only for large RTP media datagrams (RFC 7983 / RFC 5761).
///
/// STUN (`0..=3`), DTLS/SCTP (`20..=63`), TURN ChannelData (`64..=79`),
/// RTCP (`payload[1]` in `192..=223`) and small packets (Opus audio) return
/// false so they are never delayed behind video bursts.
pub fn should_pace(payload: &[u8]) -> bool {
    if payload.len() <= SMALL_PACKET_BYTES || payload.len() < 2 {
        return false;
    }
    let version = payload[0] & 0xc0;
    if version != 0x80 {
        return false;
    }
    // RFC 5761: RTCP packet types 192..=223 are multiplexed alongside RTP.
    !(192..=223).contains(&payload[1])
}

/// Token-bucket pacer owned by the single `Driver` task.
pub struct MediaPacer {
    queue: VecDeque<PacedDatagram>,
    queued_bytes: usize,
    rate_bps: f64,
    max_direct_bps: f64,
    tokens: f64,
    last_refill: Instant,
}

impl MediaPacer {
    pub fn new(now: Instant, initial_bitrate: u32) -> Self {
        let max_direct_bps = std::env::var("WAYSTONE_PACER_MAX_BPS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v >= DIRECT_MIN_BPS)
            .unwrap_or(DEFAULT_DIRECT_MAX_BPS);
        let mut pacer = MediaPacer {
            queue: VecDeque::new(),
            queued_bytes: 0,
            rate_bps: DEFAULT_DIRECT_MAX_BPS,
            max_direct_bps,
            tokens: MAX_BURST_BYTES,
            last_refill: now,
        };
        pacer.configure(initial_bitrate, false, 0);
        pacer
    }

    /// Updates the pacing rate from the current video target and path type.
    pub fn configure(&mut self, video_bitrate: u32, relayed: bool, relay_ceiling: u32) {
        self.rate_bps = if relayed && relay_ceiling > 0 {
            let effective = f64::from(video_bitrate.min(relay_ceiling));
            let upper = (f64::from(relay_ceiling) * 1.35).max(RELAY_MIN_BPS);
            (effective * 1.25).clamp(RELAY_MIN_BPS, upper)
        } else {
            let scaled = f64::from(video_bitrate) * 1.8;
            scaled.clamp(DIRECT_MIN_BPS, self.max_direct_bps)
        };
    }

    /// Current pacing rate in bits per second.
    #[cfg(test)]
    pub fn rate_bps(&self) -> f64 {
        self.rate_bps
    }

    /// Attempts to consume tokens for an immediate send when the queue is empty.
    ///
    /// Returns true if the caller may send the packet immediately without
    /// queueing or allocating a copy.
    pub fn try_consume_immediate(&mut self, payload_len: usize, now: Instant) -> bool {
        self.refill(now);
        let cost = payload_len as f64;
        if self.queue.is_empty() && self.tokens + TOKEN_EPSILON >= cost {
            self.tokens = (self.tokens - cost).max(0.0);
            true
        } else {
            false
        }
    }

    /// Enqueues a datagram for paced delivery, dropping oldest queued packets
    /// if the backlog exceeds ~250 ms of the current pacing rate.
    pub fn enqueue(&mut self, source: SocketAddr, destination: SocketAddr, payload: Vec<u8>) {
        let cap = self.max_queue_bytes();
        while self.queued_bytes.saturating_add(payload.len()) > cap {
            let Some(dropped) = self.queue.pop_front() else {
                break;
            };
            self.queued_bytes = self.queued_bytes.saturating_sub(dropped.payload.len());
        }
        self.queued_bytes = self.queued_bytes.saturating_add(payload.len());
        self.queue.push_back(PacedDatagram {
            source,
            destination,
            payload,
        });
    }

    /// Pops the next datagram whose token budget is available.
    pub fn poll_ready(&mut self, now: Instant) -> Option<PacedDatagram> {
        self.refill(now);
        let front_len = self.queue.front()?.payload.len() as f64;
        let required = front_len.min(MAX_BURST_BYTES);
        if self.tokens + TOKEN_EPSILON < required {
            return None;
        }
        let datagram = self.queue.pop_front()?;
        self.queued_bytes = self.queued_bytes.saturating_sub(datagram.payload.len());
        self.tokens = (self.tokens - datagram.payload.len() as f64).max(0.0);
        Some(datagram)
    }

    /// Next instant at which a queued datagram will be eligible to send.
    pub fn next_deadline(&self) -> Option<Instant> {
        let front = self.queue.front()?;
        let required = (front.payload.len() as f64).min(MAX_BURST_BYTES);
        let deficit = (required - self.tokens).max(0.0);
        if deficit <= TOKEN_EPSILON {
            return Some(self.last_refill);
        }
        let wait_nanos = ((deficit * 8.0 / self.rate_bps) * 1e9).ceil().max(1.0) as u64;
        Some(self.last_refill + Duration::from_nanos(wait_nanos))
    }

    fn refill(&mut self, now: Instant) {
        if let Some(elapsed) = now.checked_duration_since(self.last_refill) {
            let added = elapsed.as_secs_f64() * (self.rate_bps / 8.0);
            self.tokens = (self.tokens + added).min(MAX_BURST_BYTES);
            self.last_refill = now;
        }
    }

    fn max_queue_bytes(&self) -> usize {
        (((self.rate_bps / 8.0) * 0.25) as usize).clamp(MIN_QUEUE_BYTES, MAX_QUEUE_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_addr() -> SocketAddr {
        "127.0.0.1:5000".parse().unwrap()
    }

    #[test]
    fn classifies_rtp_vs_control_and_small_packets() {
        // STUN binding request
        let mut stun = vec![0u8; 400];
        stun[0] = 0x00;
        assert!(!should_pace(&stun));

        // DTLS / SCTP record
        let mut dtls = vec![0u8; 800];
        dtls[0] = 23;
        assert!(!should_pace(&dtls));

        // RTCP packet (PT = 205 RTPFB / NACK)
        let mut rtcp = vec![0u8; 400];
        rtcp[0] = 0x80;
        rtcp[1] = 205;
        assert!(!should_pace(&rtcp));

        // Small Opus audio RTP packet (180 bytes)
        let mut audio = vec![0u8; 180];
        audio[0] = 0x80;
        audio[1] = 111;
        assert!(!should_pace(&audio));

        // Large video RTP packet (1150 bytes, PT = 96)
        let mut video = vec![0u8; 1150];
        video[0] = 0x80;
        video[1] = 96;
        assert!(should_pace(&video));
    }

    #[test]
    fn paces_large_burst_smoothly() {
        let start = Instant::now();
        let mut pacer = MediaPacer::new(start, 8_000_000);
        // 8 Mbps * 1.8 = 14.4 Mbps
        assert!((pacer.rate_bps() - 14_400_000.0).abs() < 1.0);

        // Simulate a 100 KB keyframe (87 packets of 1150 bytes).
        let mut sent_immediately = 0;
        for _ in 0..87 {
            let pkt = vec![0x80; 1150];
            if pacer.try_consume_immediate(pkt.len(), start) {
                sent_immediately += 1;
            } else {
                pacer.enqueue(dummy_addr(), dummy_addr(), pkt);
            }
        }
        // Only the initial burst (~8 packets) goes out at t=0.
        assert_eq!(sent_immediately, 8);
        assert!(pacer.next_deadline().unwrap() > start);

        // Advance time step by step; all remaining packets drain cleanly.
        let mut now = start;
        let mut drained = 0;
        while let Some(deadline) = pacer.next_deadline() {
            now = now.max(deadline);
            while pacer.poll_ready(now).is_some() {
                drained += 1;
            }
        }
        assert_eq!(sent_immediately + drained, 87);
        // Draining ~90 KB at 14.4 Mbps takes ~50 ms.
        let total = now.duration_since(start);
        assert!(total >= Duration::from_millis(45) && total <= Duration::from_millis(60));
    }

    #[test]
    fn clamps_rate_when_relayed() {
        let now = Instant::now();
        let mut pacer = MediaPacer::new(now, 8_000_000);
        pacer.configure(8_000_000, true, 2_200_000);
        assert!((pacer.rate_bps() - 2_750_000.0).abs() < 1.0);
        pacer.configure(8_000_000, false, 2_200_000);
        assert!((pacer.rate_bps() - 14_400_000.0).abs() < 1.0);
    }

    #[test]
    fn bounds_queue_depth_under_overload() {
        let now = Instant::now();
        let mut pacer = MediaPacer::new(now, 2_000_000);
        pacer.configure(2_000_000, true, 2_200_000);
        for _ in 0..1000 {
            pacer.enqueue(dummy_addr(), dummy_addr(), vec![0x80; 1150]);
        }
        assert!(pacer.queued_bytes <= MIN_QUEUE_BYTES);
    }
}
