//! Local IPC framing between the Session 0 service, the console-session worker,
//! and the tray/CLI front end.
//!
//! Two protocols live here:
//!
//! * **Worker protocol** ([`ServiceToWorker`] / [`WorkerToService`]): compact
//!   binary framing (`kind: u8`, `len: u32 LE`, `payload`) for high-rate video,
//!   audio and raw `waystone-proto` input frames with minimal copies.
//! * **Control protocol** ([`ControlRequest`] / [`ControlResponse`]): length-
//!   prefixed JSON for status snapshots, pairing and settings updates between
//!   the tray/CLI and the running Windows service.

#![cfg_attr(not(windows), allow(dead_code))]

use anyhow::{anyhow, bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use waystone_proto::VideoCodec;

use crate::agent::Status;
use crate::display::Display;
use crate::plan::StreamPlan;

/// Hard upper bound on a single worker IPC frame (16 MiB).
pub const MAX_WORKER_FRAME: usize = 16 * 1024 * 1024;
/// Hard upper bound on a single control IPC JSON message (256 KiB).
pub const MAX_CONTROL_FRAME: usize = 256 * 1024;

/// Named pipe used by the tray and CLI to inspect and steer the Session 0 service.
pub const CONTROL_PIPE_NAME: &str = r"\\.\pipe\WaystoneHostControl";

/// Generates a unique per-session named pipe path for one console worker.
pub fn worker_pipe_name() -> String {
    let mut random = [0u8; 16];
    rand::Rng::fill(&mut rand::rng(), &mut random);
    format!(r"\\.\pipe\WaystoneHostWorker-{}", hex::encode(random))
}

// MARK: Worker binary protocol

mod tag {
    // Service -> Worker
    pub const START_CAPTURE: u8 = 0x01;
    pub const STOP_CAPTURE: u8 = 0x02;
    pub const REQUEST_KEYFRAME: u8 = 0x03;
    pub const SET_BITRATE: u8 = 0x04;
    pub const SET_FRAME_RATE: u8 = 0x05;
    pub const SET_AUDIO: u8 = 0x06;
    pub const INPUT_FRAME: u8 = 0x07;
    pub const RELEASE_ALL_INPUT: u8 = 0x08;
    pub const SET_MUTE: u8 = 0x09;
    pub const LOCK_SCREEN: u8 = 0x0A;
    pub const SHUTDOWN: u8 = 0x0B;
    pub const SET_RESOLUTION: u8 = 0x0C;

    // Worker -> Service
    pub const HELLO: u8 = 0x81;
    pub const CAPTURE_STARTED: u8 = 0x82;
    pub const CAPTURE_FAILED: u8 = 0x83;
    pub const VIDEO_FRAME: u8 = 0x84;
    pub const AUDIO_PACKET: u8 = 0x85;
    pub const ACK: u8 = 0x86;
    pub const DESKTOP_SWITCHED: u8 = 0x87;
}

/// Operation codes referenced by [`WorkerToService::Ack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AckOp {
    StopCapture = 1,
    SetMute = 2,
    LockScreen = 3,
    ReleaseAllInput = 4,
}

impl AckOp {
    fn from_u8(value: u8) -> Result<AckOp> {
        match value {
            1 => Ok(AckOp::StopCapture),
            2 => Ok(AckOp::SetMute),
            3 => Ok(AckOp::LockScreen),
            4 => Ok(AckOp::ReleaseAllInput),
            other => bail!("unknown ack op {other}"),
        }
    }
}

/// Commands sent from the Session 0 service to the console-session worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceToWorker {
    StartCapture {
        display_id: u32,
        plan: StreamPlan,
        codec: VideoCodec,
        audio: bool,
    },
    StopCapture,
    RequestKeyframe,
    SetBitrate(u32),
    SetFrameRate(u32),
    SetResolution {
        width: u32,
        height: u32,
        bitrate: u32,
    },
    SetAudio(bool),
    /// Raw `waystone-proto` binary input event frame, forwarded verbatim.
    InputFrame(Vec<u8>),
    ReleaseAllInput,
    SetMute(bool),
    LockScreen,
    Shutdown,
}

/// Events and encoded media frames sent from the console worker to Session 0.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerToService {
    Hello {
        display: Display,
        input_permitted: bool,
        session_id: u32,
    },
    CaptureStarted,
    CaptureFailed(String),
    VideoFrame {
        micros: u64,
        data: Vec<u8>,
    },
    AudioPacket {
        micros: u64,
        data: Vec<u8>,
    },
    Ack {
        op: AckOp,
        ok: bool,
        message: String,
    },
    DesktopSwitched {
        desktop: String,
    },
}

impl ServiceToWorker {
    pub fn encode(&self) -> Result<(u8, Vec<u8>)> {
        match self {
            ServiceToWorker::StartCapture {
                display_id,
                plan,
                codec,
                audio,
            } => {
                let mut payload = Vec::with_capacity(22);
                payload.extend_from_slice(&display_id.to_le_bytes());
                payload.extend_from_slice(&plan.width.to_le_bytes());
                payload.extend_from_slice(&plan.height.to_le_bytes());
                payload.extend_from_slice(&plan.fps.to_le_bytes());
                payload.extend_from_slice(&plan.bitrate.to_le_bytes());
                payload.push(encode_codec(*codec));
                payload.push(u8::from(*audio));
                Ok((tag::START_CAPTURE, payload))
            }
            ServiceToWorker::StopCapture => Ok((tag::STOP_CAPTURE, Vec::new())),
            ServiceToWorker::RequestKeyframe => Ok((tag::REQUEST_KEYFRAME, Vec::new())),
            ServiceToWorker::SetBitrate(bitrate) => {
                Ok((tag::SET_BITRATE, bitrate.to_le_bytes().to_vec()))
            }
            ServiceToWorker::SetFrameRate(fps) => {
                Ok((tag::SET_FRAME_RATE, fps.to_le_bytes().to_vec()))
            }
            ServiceToWorker::SetResolution {
                width,
                height,
                bitrate,
            } => {
                let mut payload = Vec::with_capacity(12);
                payload.extend_from_slice(&width.to_le_bytes());
                payload.extend_from_slice(&height.to_le_bytes());
                payload.extend_from_slice(&bitrate.to_le_bytes());
                Ok((tag::SET_RESOLUTION, payload))
            }
            ServiceToWorker::SetAudio(enabled) => Ok((tag::SET_AUDIO, vec![u8::from(*enabled)])),
            ServiceToWorker::InputFrame(data) => {
                ensure!(
                    data.len() <= MAX_WORKER_FRAME,
                    "input frame exceeds max worker frame size"
                );
                Ok((tag::INPUT_FRAME, data.clone()))
            }
            ServiceToWorker::ReleaseAllInput => Ok((tag::RELEASE_ALL_INPUT, Vec::new())),
            ServiceToWorker::SetMute(muted) => Ok((tag::SET_MUTE, vec![u8::from(*muted)])),
            ServiceToWorker::LockScreen => Ok((tag::LOCK_SCREEN, Vec::new())),
            ServiceToWorker::Shutdown => Ok((tag::SHUTDOWN, Vec::new())),
        }
    }

    pub fn decode(kind: u8, payload: Vec<u8>) -> Result<ServiceToWorker> {
        match kind {
            tag::START_CAPTURE => {
                ensure!(payload.len() == 22, "invalid StartCapture length");
                let display_id = read_u32(&payload[0..4])?;
                let width = read_u32(&payload[4..8])?;
                let height = read_u32(&payload[8..12])?;
                let fps = read_u32(&payload[12..16])?;
                let bitrate = read_u32(&payload[16..20])?;
                let codec = decode_codec(payload[20])?;
                let audio = payload[21] != 0;
                Ok(ServiceToWorker::StartCapture {
                    display_id,
                    plan: StreamPlan {
                        width,
                        height,
                        fps,
                        bitrate,
                    },
                    codec,
                    audio,
                })
            }
            tag::STOP_CAPTURE => Ok(ServiceToWorker::StopCapture),
            tag::REQUEST_KEYFRAME => Ok(ServiceToWorker::RequestKeyframe),
            tag::SET_BITRATE => Ok(ServiceToWorker::SetBitrate(read_u32(&payload)?)),
            tag::SET_FRAME_RATE => Ok(ServiceToWorker::SetFrameRate(read_u32(&payload)?)),
            tag::SET_RESOLUTION => {
                ensure!(payload.len() == 12, "invalid SetResolution length");
                Ok(ServiceToWorker::SetResolution {
                    width: read_u32(&payload[0..4])?,
                    height: read_u32(&payload[4..8])?,
                    bitrate: read_u32(&payload[8..12])?,
                })
            }
            tag::SET_AUDIO => {
                ensure!(payload.len() == 1, "invalid SetAudio length");
                Ok(ServiceToWorker::SetAudio(payload[0] != 0))
            }
            tag::INPUT_FRAME => Ok(ServiceToWorker::InputFrame(payload)),
            tag::RELEASE_ALL_INPUT => Ok(ServiceToWorker::ReleaseAllInput),
            tag::SET_MUTE => {
                ensure!(payload.len() == 1, "invalid SetMute length");
                Ok(ServiceToWorker::SetMute(payload[0] != 0))
            }
            tag::LOCK_SCREEN => Ok(ServiceToWorker::LockScreen),
            tag::SHUTDOWN => Ok(ServiceToWorker::Shutdown),
            other => bail!("unknown ServiceToWorker tag {other:#04x}"),
        }
    }

    pub async fn write_to<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> Result<()> {
        match self {
            // Avoid cloning the input buffer on the hot path.
            ServiceToWorker::InputFrame(data) => {
                write_raw_frame(writer, tag::INPUT_FRAME, data).await
            }
            _ => {
                let (kind, payload) = self.encode()?;
                write_raw_frame(writer, kind, &payload).await
            }
        }
    }

    pub async fn read_from<R: AsyncRead + Unpin>(reader: &mut R) -> Result<ServiceToWorker> {
        let (kind, payload) = read_raw_frame(reader, MAX_WORKER_FRAME).await?;
        ServiceToWorker::decode(kind, payload)
    }
}

impl WorkerToService {
    pub async fn write_to<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> Result<()> {
        match self {
            WorkerToService::Hello {
                display,
                input_permitted,
                session_id,
            } => {
                let mut payload = Vec::with_capacity(49);
                payload.extend_from_slice(&display.id.to_le_bytes());
                payload.extend_from_slice(&display.origin.0.to_le_bytes());
                payload.extend_from_slice(&display.origin.1.to_le_bytes());
                payload.extend_from_slice(&display.size.0.to_le_bytes());
                payload.extend_from_slice(&display.size.1.to_le_bytes());
                payload.extend_from_slice(&display.pixels.0.to_le_bytes());
                payload.extend_from_slice(&display.pixels.1.to_le_bytes());
                payload.push(u8::from(*input_permitted));
                payload.extend_from_slice(&session_id.to_le_bytes());
                write_raw_frame(writer, tag::HELLO, &payload).await
            }
            WorkerToService::CaptureStarted => {
                write_raw_frame(writer, tag::CAPTURE_STARTED, &[]).await
            }
            WorkerToService::CaptureFailed(message) => {
                write_raw_frame(writer, tag::CAPTURE_FAILED, message.as_bytes()).await
            }
            // Write header + micros + payload directly without allocating a combined buffer.
            WorkerToService::VideoFrame { micros, data } => {
                write_media_frame(writer, tag::VIDEO_FRAME, *micros, data).await
            }
            WorkerToService::AudioPacket { micros, data } => {
                write_media_frame(writer, tag::AUDIO_PACKET, *micros, data).await
            }
            WorkerToService::Ack { op, ok, message } => {
                let mut payload = Vec::with_capacity(2 + message.len());
                payload.push(*op as u8);
                payload.push(u8::from(*ok));
                payload.extend_from_slice(message.as_bytes());
                write_raw_frame(writer, tag::ACK, &payload).await
            }
            WorkerToService::DesktopSwitched { desktop } => {
                write_raw_frame(writer, tag::DESKTOP_SWITCHED, desktop.as_bytes()).await
            }
        }
    }

    pub async fn read_from<R: AsyncRead + Unpin>(reader: &mut R) -> Result<WorkerToService> {
        let (kind, payload) = read_raw_frame(reader, MAX_WORKER_FRAME).await?;
        WorkerToService::decode(kind, payload)
    }

    pub fn decode(kind: u8, mut payload: Vec<u8>) -> Result<WorkerToService> {
        match kind {
            tag::HELLO => {
                ensure!(payload.len() == 49, "invalid Hello length");
                let display = Display {
                    id: read_u32(&payload[0..4])?,
                    origin: (read_f64(&payload[4..12])?, read_f64(&payload[12..20])?),
                    size: (read_f64(&payload[20..28])?, read_f64(&payload[28..36])?),
                    pixels: (read_u32(&payload[36..40])?, read_u32(&payload[40..44])?),
                };
                let input_permitted = payload[44] != 0;
                let session_id = read_u32(&payload[45..49])?;
                Ok(WorkerToService::Hello {
                    display,
                    input_permitted,
                    session_id,
                })
            }
            tag::CAPTURE_STARTED => Ok(WorkerToService::CaptureStarted),
            tag::CAPTURE_FAILED => {
                let message = String::from_utf8(payload).context("invalid UTF-8 in error")?;
                Ok(WorkerToService::CaptureFailed(message))
            }
            tag::VIDEO_FRAME => {
                ensure!(payload.len() >= 8, "truncated VideoFrame");
                let micros = read_u64(&payload[0..8])?;
                let data = payload.split_off(8);
                Ok(WorkerToService::VideoFrame { micros, data })
            }
            tag::AUDIO_PACKET => {
                ensure!(payload.len() >= 8, "truncated AudioPacket");
                let micros = read_u64(&payload[0..8])?;
                let data = payload.split_off(8);
                Ok(WorkerToService::AudioPacket { micros, data })
            }
            tag::ACK => {
                ensure!(payload.len() >= 2, "truncated Ack");
                let op = AckOp::from_u8(payload[0])?;
                let ok = payload[1] != 0;
                let message =
                    String::from_utf8(payload.split_off(2)).context("invalid UTF-8 in Ack")?;
                Ok(WorkerToService::Ack { op, ok, message })
            }
            tag::DESKTOP_SWITCHED => {
                let desktop = String::from_utf8(payload).context("invalid UTF-8 desktop name")?;
                Ok(WorkerToService::DesktopSwitched { desktop })
            }
            other => bail!("unknown WorkerToService tag {other:#04x}"),
        }
    }
}

async fn write_raw_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    payload: &[u8],
) -> Result<()> {
    ensure!(
        payload.len() <= MAX_WORKER_FRAME,
        "frame too large ({} bytes)",
        payload.len()
    );
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header).await?;
    if !payload.is_empty() {
        writer.write_all(payload).await?;
    }
    writer.flush().await?;
    Ok(())
}

async fn write_media_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    micros: u64,
    data: &[u8],
) -> Result<()> {
    let total = data
        .len()
        .checked_add(8)
        .ok_or_else(|| anyhow!("media frame length overflow"))?;
    ensure!(
        total <= MAX_WORKER_FRAME,
        "media frame too large ({total} bytes)"
    );
    let mut header = [0u8; 13];
    header[0] = kind;
    header[1..5].copy_from_slice(&(total as u32).to_le_bytes());
    header[5..13].copy_from_slice(&micros.to_le_bytes());
    writer.write_all(&header).await?;
    if !data.is_empty() {
        writer.write_all(data).await?;
    }
    writer.flush().await?;
    Ok(())
}

async fn read_raw_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: usize,
) -> Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header).await?;
    let kind = header[0];
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    ensure!(len <= max_len, "IPC frame too large ({len} > {max_len})");
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload).await?;
    }
    Ok((kind, payload))
}

fn encode_codec(codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::H264 => 0,
        VideoCodec::H265 => 1,
    }
}

fn decode_codec(byte: u8) -> Result<VideoCodec> {
    match byte {
        0 => Ok(VideoCodec::H264),
        1 => Ok(VideoCodec::H265),
        other => bail!("unknown video codec {other}"),
    }
}

fn read_u32(bytes: &[u8]) -> Result<u32> {
    let arr: [u8; 4] = bytes
        .try_into()
        .map_err(|_| anyhow!("expected 4 bytes for u32"))?;
    Ok(u32::from_le_bytes(arr))
}

fn read_u64(bytes: &[u8]) -> Result<u64> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| anyhow!("expected 8 bytes for u64"))?;
    Ok(u64::from_le_bytes(arr))
}

fn read_f64(bytes: &[u8]) -> Result<f64> {
    let arr: [u8; 8] = bytes
        .try_into()
        .map_err(|_| anyhow!("expected 8 bytes for f64"))?;
    Ok(f64::from_le_bytes(arr))
}

// MARK: Control JSON protocol (Tray / CLI <-> Session 0 Service)

/// Serializable mirror of the agent status for the control pipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServiceAgentStatus {
    NotPaired,
    Connecting,
    Online,
    Streaming { controller: String },
    Offline { reason: String },
}

impl From<&Status> for ServiceAgentStatus {
    fn from(status: &Status) -> Self {
        match status {
            Status::Connecting => ServiceAgentStatus::Connecting,
            Status::Online => ServiceAgentStatus::Online,
            Status::Streaming { controller } => ServiceAgentStatus::Streaming {
                controller: controller.clone(),
            },
            Status::Offline { reason } => ServiceAgentStatus::Offline {
                reason: reason.clone(),
            },
        }
    }
}

impl ServiceAgentStatus {
    pub fn to_agent_status(&self) -> Status {
        match self {
            ServiceAgentStatus::NotPaired => Status::Offline {
                reason: "not paired".into(),
            },
            ServiceAgentStatus::Connecting => Status::Connecting,
            ServiceAgentStatus::Online => Status::Online,
            ServiceAgentStatus::Streaming { controller } => Status::Streaming {
                controller: controller.clone(),
            },
            ServiceAgentStatus::Offline { reason } => Status::Offline {
                reason: reason.clone(),
            },
        }
    }
}

/// Complete live snapshot reported by the Session 0 service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSnapshot {
    pub status: ServiceAgentStatus,
    pub paired: bool,
    pub device_id: Option<String>,
    pub name: Option<String>,
    pub endpoint: Option<String>,
    pub turn: Option<String>,
    pub max_bitrate: Option<u32>,
    pub lock_on_disconnect: bool,
    pub mute_speakers: bool,
    pub state_dir: String,
    pub active_console_session: Option<u32>,
}

/// Request sent from the tray or CLI over [`CONTROL_PIPE_NAME`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlRequest {
    GetStatus,
    Subscribe,
    EndSession,
    UpdateSettings {
        lock_on_disconnect: bool,
        mute_speakers: bool,
    },
    Pair {
        uri: String,
        name: Option<String>,
    },
}

/// Response or pushed event sent from the Session 0 service over [`CONTROL_PIPE_NAME`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    Snapshot(ServiceSnapshot),
    Ok,
    Error { message: String },
}

pub async fn write_json_frame<T: Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_CONTROL_FRAME,
        "control message too large ({} bytes)",
        bytes.len()
    );
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_json_frame<T: for<'de> Deserialize<'de>, R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf) as usize;
    ensure!(
        len <= MAX_CONTROL_FRAME,
        "control message too large ({len} > {MAX_CONTROL_FRAME})"
    );
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_pipe_names_are_unique_and_well_formed() {
        let a = worker_pipe_name();
        let b = worker_pipe_name();
        assert!(a.starts_with(r"\\.\pipe\WaystoneHostWorker-"));
        assert_eq!(a.len(), r"\\.\pipe\WaystoneHostWorker-".len() + 32);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn service_to_worker_roundtrip() {
        let messages = vec![
            ServiceToWorker::StartCapture {
                display_id: 0x0102,
                plan: StreamPlan {
                    width: 2560,
                    height: 1440,
                    fps: 60,
                    bitrate: 12_000_000,
                },
                codec: VideoCodec::H265,
                audio: true,
            },
            ServiceToWorker::StartCapture {
                display_id: 0,
                plan: StreamPlan {
                    width: 1920,
                    height: 1080,
                    fps: 45,
                    bitrate: 6_000_000,
                },
                codec: VideoCodec::H264,
                audio: false,
            },
            ServiceToWorker::StopCapture,
            ServiceToWorker::RequestKeyframe,
            ServiceToWorker::SetBitrate(4_000_000),
            ServiceToWorker::SetFrameRate(45),
            ServiceToWorker::SetResolution {
                width: 1920,
                height: 1080,
                bitrate: 8_000_000,
            },
            ServiceToWorker::SetAudio(false),
            ServiceToWorker::InputFrame(vec![1, 2, 3, 4, 5, 6]),
            ServiceToWorker::ReleaseAllInput,
            ServiceToWorker::SetMute(true),
            ServiceToWorker::SetMute(false),
            ServiceToWorker::LockScreen,
            ServiceToWorker::Shutdown,
        ];

        let (mut writer, mut reader) = tokio::io::duplex(64 * 1024);
        let expected = messages.clone();
        let write_task = tokio::spawn(async move {
            for msg in messages {
                msg.write_to(&mut writer).await.unwrap();
            }
        });
        for want in expected {
            let got = ServiceToWorker::read_from(&mut reader).await.unwrap();
            assert_eq!(got, want);
        }
        write_task.await.unwrap();
    }

    #[tokio::test]
    async fn worker_to_service_roundtrip() {
        let messages = vec![
            WorkerToService::Hello {
                display: Display {
                    id: 257,
                    origin: (-1920.0, 0.0),
                    size: (2560.0, 1440.0),
                    pixels: (2560, 1440),
                },
                input_permitted: true,
                session_id: 1,
            },
            WorkerToService::CaptureStarted,
            WorkerToService::CaptureFailed("DXGI failed".into()),
            WorkerToService::VideoFrame {
                micros: 123_456_789,
                data: vec![0, 0, 0, 1, 0x40, 0x01, 0x0c],
            },
            WorkerToService::AudioPacket {
                micros: 987_654_321,
                data: vec![0xf8, 0xff, 0xfe],
            },
            WorkerToService::Ack {
                op: AckOp::LockScreen,
                ok: true,
                message: String::new(),
            },
            WorkerToService::Ack {
                op: AckOp::SetMute,
                ok: false,
                message: "no endpoint".into(),
            },
            WorkerToService::DesktopSwitched {
                desktop: "Winlogon".into(),
            },
        ];

        let (mut writer, mut reader) = tokio::io::duplex(64 * 1024);
        let expected = messages.clone();
        let write_task = tokio::spawn(async move {
            for msg in messages {
                msg.write_to(&mut writer).await.unwrap();
            }
        });
        for want in expected {
            let got = WorkerToService::read_from(&mut reader).await.unwrap();
            assert_eq!(got, want);
        }
        write_task.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_oversized_worker_frames() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let mut bad_header = [0u8; 5];
        bad_header[0] = tag::VIDEO_FRAME;
        bad_header[1..5].copy_from_slice(&((MAX_WORKER_FRAME as u32) + 1).to_le_bytes());
        writer.write_all(&bad_header).await.unwrap();
        let err = WorkerToService::read_from(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[tokio::test]
    async fn control_json_protocol_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(16 * 1024);
        let req = ControlRequest::Pair {
            uri: "waystone://pair?ep=1.2.3.4:8443&turn=1.2.3.4:3478&fp=abc&tk=xyz".into(),
            name: Some("Gaming PC".into()),
        };
        write_json_frame(&mut a, &req).await.unwrap();
        let decoded: ControlRequest = read_json_frame(&mut b).await.unwrap();
        assert_eq!(decoded, req);

        let snap = ControlResponse::Snapshot(ServiceSnapshot {
            status: ServiceAgentStatus::Streaming {
                controller: "iPhone".into(),
            },
            paired: true,
            device_id: Some("ws_abc".into()),
            name: Some("Gaming PC".into()),
            endpoint: Some("1.2.3.4:8443".into()),
            turn: Some("1.2.3.4:3478".into()),
            max_bitrate: Some(8_000_000),
            lock_on_disconnect: true,
            mute_speakers: true,
            state_dir: r"C:\ProgramData\Waystone".into(),
            active_console_session: Some(1),
        });
        write_json_frame(&mut b, &snap).await.unwrap();
        let decoded_resp: ControlResponse = read_json_frame(&mut a).await.unwrap();
        assert_eq!(decoded_resp, snap);
    }

    #[test]
    fn status_conversion_preserves_variants() {
        let s = Status::Streaming {
            controller: "iPad".into(),
        };
        let wire = ServiceAgentStatus::from(&s);
        assert_eq!(wire.to_agent_status(), s);

        let not_paired = ServiceAgentStatus::NotPaired;
        assert!(matches!(
            not_paired.to_agent_status(),
            Status::Offline { .. }
        ));
    }
}
