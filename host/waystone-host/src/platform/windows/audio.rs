//! System sound: WASAPI loopback of the default playback device, encoded
//! as 20 ms stereo Opus packets (design doc 4.4).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tracing::{debug, info, warn};
use unsafe_libopus::{
    opus_encode_float, opus_encoder_create, opus_encoder_ctl, opus_encoder_destroy, OpusEncoder,
    OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_OK, OPUS_SET_BITRATE_REQUEST,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use super::capture::Controls;
use super::com;
use crate::capture::{FrameSink, MediaKind};

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// Samples per channel in one 20 ms packet.
const PACKET: usize = 960;
const BITRATE: i32 = 96_000;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// Runs until `controls.stop`, reopening the device when it goes away
/// (headphones unplugged, default device switched).
pub fn run(sink: Arc<FrameSink>, controls: Arc<Controls>) {
    com::enter_mta();
    let mut encoder = match Opus::new() {
        Ok(encoder) => encoder,
        Err(error) => {
            warn!("no audio: {error:#}");
            return;
        }
    };
    while !controls.stop.load(Ordering::Relaxed) {
        match Loopback::open() {
            Ok(mut loopback) => {
                if let Err(error) = loopback.pump(&mut encoder, &sink, &controls) {
                    info!("audio capture restarting: {error:#}");
                }
            }
            Err(error) => {
                debug!("no audio device to capture: {error:#}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    /// Interleaved samples waiting for a full packet.
    pending: Vec<f32>,
    /// Time of the first pending sample.
    pending_micros: u64,
}

impl Loopback {
    fn open() -> Result<Loopback> {
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = devices.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let block = CHANNELS * 4;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: CHANNELS,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * u32::from(block),
                nBlockAlign: block,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            // The mix is converted to 48 kHz stereo float whatever the
            // device runs at. 100 ms of buffer absorbs scheduling hiccups.
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    1_000_000,
                    0,
                    &format,
                    None,
                )
                .context("starting loopback capture")?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Loopback {
                client,
                capture,
                pending: Vec::with_capacity(PACKET * 4),
                pending_micros: 0,
            })
        }
    }

    fn pump(&mut self, encoder: &mut Opus, sink: &FrameSink, controls: &Controls) -> Result<()> {
        while !controls.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            loop {
                let frames = unsafe { self.capture.GetNextPacketSize() }?;
                if frames == 0 {
                    break;
                }
                self.read_packet()?;
            }
            while self.pending.len() >= PACKET * usize::from(CHANNELS) {
                let samples: Vec<f32> = self
                    .pending
                    .drain(..PACKET * usize::from(CHANNELS))
                    .collect();
                let micros = self.pending_micros;
                self.pending_micros += 20_000;
                if controls.audio.load(Ordering::Relaxed) {
                    let packet = encoder.encode(&samples)?;
                    sink.deliver(MediaKind::Audio, &packet, micros);
                }
            }
        }
        Ok(())
    }

    fn read_packet(&mut self) -> Result<()> {
        let mut data = std::ptr::null_mut();
        let mut frames = 0;
        let mut flags = 0;
        let mut qpc = 0u64;
        unsafe {
            self.capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
        }
        let count = frames as usize * usize::from(CHANNELS);
        // Loopback delivers nothing while nothing plays, so a gap starts a
        // new timeline at the packet's own capture time (100 ns units).
        if self.pending.is_empty() {
            self.pending_micros = qpc / 10;
        }
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
            self.pending.resize(self.pending.len() + count, 0.0);
        } else {
            let samples = unsafe { std::slice::from_raw_parts(data as *const f32, count) };
            self.pending.extend_from_slice(samples);
        }
        unsafe { self.capture.ReleaseBuffer(frames) }?;
        Ok(())
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// libopus (transpiled to Rust, so nothing to cross-compile) in its
/// restricted low-delay mode.
struct Opus(*mut OpusEncoder);

impl Opus {
    fn new() -> Result<Opus> {
        let mut error = 0;
        let encoder = unsafe {
            opus_encoder_create(
                RATE as i32,
                i32::from(CHANNELS),
                OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut error,
            )
        };
        if encoder.is_null() || error != OPUS_OK {
            bail!("cannot create the Opus encoder ({error})");
        }
        unsafe { opus_encoder_ctl!(encoder, OPUS_SET_BITRATE_REQUEST, BITRATE) };
        Ok(Opus(encoder))
    }

    fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>> {
        let mut packet = vec![0u8; 1500];
        let length = unsafe {
            opus_encode_float(
                self.0,
                samples.as_ptr(),
                PACKET as i32,
                packet.as_mut_ptr(),
                packet.len() as i32,
            )
        };
        if length < 0 {
            return Err(anyhow!("Opus encoding failed ({length})"));
        }
        packet.truncate(length as usize);
        Ok(packet)
    }
}

impl Drop for Opus {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.0) };
    }
}
