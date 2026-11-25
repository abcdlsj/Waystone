//! The frame path from a platform capture session into the engine.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use waystone_rtc::Engine;

/// What an encoded buffer carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// One Annex-B access unit.
    Video,
    /// One 20 ms Opus packet.
    Audio,
}

/// Custom sink target used by the Windows console worker to forward encoded
/// frames over the local Named Pipe instead of directly into an in-process engine.
#[cfg_attr(not(windows), allow(dead_code))]
pub type FrameCallback = Arc<dyn Fn(MediaKind, &[u8], u64) + Send + Sync>;

/// Where encoded frames go. Lives as long as the capture session that calls
/// into it, and converts host clock timestamps into RTP time.
#[derive(Default)]
pub struct FrameSink {
    engine: Mutex<Option<Engine>>,
    callback: Option<FrameCallback>,
    video_origin: Mutex<Option<u64>>,
    audio_origin: Mutex<Option<u64>>,
    video_frames: AtomicU64,
    audio_packets: AtomicU64,
}

impl FrameSink {
    pub fn new(engine: Option<Engine>) -> Arc<FrameSink> {
        Arc::new(FrameSink {
            engine: Mutex::new(engine),
            callback: None,
            ..FrameSink::default()
        })
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn with_callback(callback: FrameCallback) -> Arc<FrameSink> {
        Arc::new(FrameSink {
            callback: Some(callback),
            ..FrameSink::default()
        })
    }

    pub fn counts(&self) -> (u64, u64) {
        (
            self.video_frames.load(Ordering::Relaxed),
            self.audio_packets.load(Ordering::Relaxed),
        )
    }

    /// Hands one encoded buffer to the engine. `micros` is its presentation
    /// time on any monotonic host clock, the same clock for both kinds.
    /// Callable from any thread.
    pub fn deliver(&self, kind: MediaKind, data: &[u8], micros: u64) {
        let (origin, counter, clock_khz) = match kind {
            MediaKind::Video => (&self.video_origin, &self.video_frames, 90),
            MediaKind::Audio => (&self.audio_origin, &self.audio_packets, 48),
        };
        counter.fetch_add(1, Ordering::Relaxed);
        if let Some(callback) = &self.callback {
            callback(kind, data, micros);
            return;
        }
        let rtp_time = rtp_time(origin, micros, clock_khz);
        let Some(engine) = self.engine.lock().unwrap().clone() else {
            return;
        };
        match kind {
            MediaKind::Video => engine.send_video(data.to_vec(), rtp_time),
            MediaKind::Audio => engine.send_audio(data.to_vec(), rtp_time),
        }
    }
}

/// RTP time relative to the first timestamp seen: 90 kHz video, 48 kHz Opus.
fn rtp_time(origin: &Mutex<Option<u64>>, micros: u64, clock_khz: u64) -> u64 {
    let base = *origin.lock().unwrap().get_or_insert(micros);
    micros.saturating_sub(base) * clock_khz / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_time_starts_at_zero_and_follows_the_clock() {
        let origin = Mutex::new(None);
        assert_eq!(rtp_time(&origin, 5_000_000, 90), 0);
        // 33 ms at 90 kHz.
        assert_eq!(rtp_time(&origin, 5_033_000, 90), 2_970);

        let origin = Mutex::new(None);
        assert_eq!(rtp_time(&origin, 1_000_000, 48), 0);
        // One 20 ms Opus packet at 48 kHz.
        assert_eq!(rtp_time(&origin, 1_020_000, 48), 960);
    }

    #[test]
    fn frames_without_an_engine_are_counted_and_dropped() {
        let sink = FrameSink::new(None);
        sink.deliver(MediaKind::Video, b"frame", 1);
        sink.deliver(MediaKind::Audio, b"packet", 1);
        assert_eq!(sink.counts(), (1, 1));
    }

    #[test]
    fn callback_sink_forwards_frames_and_counts() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let sink = FrameSink::with_callback(Arc::new(move |kind, data, micros| {
            seen_cb.lock().unwrap().push((kind, data.to_vec(), micros));
        }));
        sink.deliver(MediaKind::Video, b"nalu", 42);
        sink.deliver(MediaKind::Audio, b"opus", 84);
        assert_eq!(sink.counts(), (1, 1));
        let records = seen.lock().unwrap();
        assert_eq!(
            *records,
            vec![
                (MediaKind::Video, b"nalu".to_vec(), 42),
                (MediaKind::Audio, b"opus".to_vec(), 84),
            ]
        );
    }
}
