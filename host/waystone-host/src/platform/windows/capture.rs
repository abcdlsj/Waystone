//! A capture session: the video supervisor/thread (see `video`) and the audio
//! thread (see `audio`), steered through shared atomics.
//!
//! When Windows switches desktops (`Default` <-> `Winlogon` for UAC or the lock
//! screen), the video thread tries `SetThreadDesktop` in place first. If a GPU
//! driver or Media Foundation MFT attached a hidden window to that thread
//! (`ERROR_BUSY`), the supervisor respawns a clean OS thread which attaches to
//! the new input desktop before initializing D3D11 and Media Foundation.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::oneshot;
use tracing::{info, warn};
use waystone_proto::VideoCodec;

use super::desktop::{self, SwitchOutcome};
use super::{audio, com, video};
use crate::capture::{FrameSink, MediaKind};
use crate::display::Display;
use crate::plan::StreamPlan;

/// What the session changes while capture runs.
pub struct Controls {
    pub stop: AtomicBool,
    pub keyframe: AtomicBool,
    pub bitrate: AtomicU32,
    pub fps: AtomicU32,
    pub audio: AtomicBool,
    pub resolution: Mutex<Option<(u32, u32)>>,
}

/// A running capture session; stopping it is dropping it.
pub struct Capture {
    controls: Arc<Controls>,
    video: Option<JoinHandle<()>>,
    audio: Option<JoinHandle<()>>,
}

impl Capture {
    /// Starts capturing `display` and resolves once the pipeline is up.
    pub async fn start(
        display: &Display,
        plan: &StreamPlan,
        codec: VideoCodec,
        audio: bool,
        sink: Arc<FrameSink>,
    ) -> Result<Capture> {
        let controls = Arc::new(Controls {
            stop: AtomicBool::new(false),
            keyframe: AtomicBool::new(false),
            bitrate: AtomicU32::new(plan.bitrate),
            fps: AtomicU32::new(plan.fps),
            audio: AtomicBool::new(audio),
            resolution: Mutex::new(None),
        });
        let settings = video::Settings {
            width: plan.width,
            height: plan.height,
            fps: plan.fps,
            bitrate: plan.bitrate,
            codec,
        };
        let (ready, started) = oneshot::channel();
        let video = {
            let (controls, sink, display) = (controls.clone(), sink.clone(), display.id);
            std::thread::Builder::new()
                .name("waystone-video-sup".into())
                .spawn(move || run_video_supervisor(display, settings, sink, controls, ready))?
        };
        let mut capture = Capture {
            controls: controls.clone(),
            video: Some(video),
            audio: None,
        };
        // On error the thread has already returned; dropping `capture`
        // joins it.
        started
            .await
            .map_err(|_| anyhow!("the video thread died while starting"))??;
        capture.audio = Some(
            std::thread::Builder::new()
                .name("waystone-audio".into())
                .spawn(move || audio::run(sink, controls))?,
        );
        Ok(capture)
    }

    pub fn request_keyframe(&self) {
        self.controls.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        self.controls.bitrate.store(bitrate, Ordering::Relaxed);
    }

    pub fn set_frame_rate(&self, fps: u32) {
        self.controls.fps.store(fps, Ordering::Relaxed);
    }

    pub fn set_audio(&self, enabled: bool) {
        self.controls.audio.store(enabled, Ordering::Relaxed);
    }

    pub fn set_resolution(&self, width: u32, height: u32, bitrate: u32) -> bool {
        self.controls.bitrate.store(bitrate, Ordering::Relaxed);
        *self.controls.resolution.lock().unwrap() = Some((width, height));
        true
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Both threads check the flag at least every few milliseconds, and
        // no frame is delivered once they are joined.
        self.controls.stop.store(true, Ordering::Relaxed);
        for thread in [self.video.take(), self.audio.take()].into_iter().flatten() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoExit {
    Stopped,
    Respawn,
    Failed,
}

fn run_video_supervisor(
    display: u32,
    settings: video::Settings,
    sink: Arc<FrameSink>,
    controls: Arc<Controls>,
    ready: oneshot::Sender<Result<()>>,
) {
    let mut initial_ready = Some(ready);
    let mut settings = settings;
    while !controls.stop.load(Ordering::Relaxed) {
        if let Some((width, height)) = controls.resolution.lock().unwrap().take() {
            settings.width = width;
            settings.height = height;
        }
        let current_settings = video::Settings {
            bitrate: controls.bitrate.load(Ordering::Relaxed),
            fps: controls.fps.load(Ordering::Relaxed),
            ..settings
        };
        let ready_tx = initial_ready.take();
        let (sink_c, controls_c) = (sink.clone(), controls.clone());
        let handle = match std::thread::Builder::new()
            .name("waystone-video".into())
            .spawn(move || run_video_once(display, current_settings, sink_c, controls_c, ready_tx))
        {
            Ok(handle) => handle,
            Err(error) => {
                warn!("failed to spawn video thread: {error}");
                break;
            }
        };
        match handle.join() {
            Ok(VideoExit::Respawn) if !controls.stop.load(Ordering::Relaxed) => {
                info!("respawning video thread on active input desktop");
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(VideoExit::Stopped | VideoExit::Failed) | Ok(VideoExit::Respawn) => break,
            Err(_) => {
                warn!("video thread panicked");
                break;
            }
        }
    }
}

fn run_video_once(
    display: u32,
    settings: video::Settings,
    sink: Arc<FrameSink>,
    controls: Arc<Controls>,
    ready: Option<oneshot::Sender<Result<()>>>,
) -> VideoExit {
    // Attach this fresh OS thread to the active input desktop before COM,
    // D3D11 or Media Foundation can bind any desktop-scoped state to it.
    if let SwitchOutcome::Switched(name) = desktop::sync_thread_to_input_desktop() {
        info!("video thread attached to desktop {name}");
    }
    com::enter_mta();
    let mut pipeline = match video::Pipeline::new(display, settings) {
        Ok(pipeline) => {
            if let Some(ready) = ready {
                let _ = ready.send(Ok(()));
            }
            pipeline
        }
        Err(error) => {
            if let Some(ready) = ready {
                let _ = ready.send(Err(error));
                return VideoExit::Failed;
            }
            // During a desktop transition (e.g. Winlogon initialization), retry
            // briefly on a fresh thread until capture comes back or stop is set.
            warn!("video pipeline restart failed: {error:#}");
            std::thread::sleep(Duration::from_millis(250));
            return VideoExit::Respawn;
        }
    };
    let mut seen_desktop_gen = desktop::desktop_generation();
    let mut deliver = |frame: video::EncodedFrame| {
        sink.deliver(MediaKind::Video, &frame.data, frame.micros);
    };
    while !controls.stop.load(Ordering::Relaxed) {
        let current_gen = desktop::desktop_generation();
        if current_gen != seen_desktop_gen {
            seen_desktop_gen = current_gen;
            if !pipeline.handle_desktop_switch() {
                return VideoExit::Respawn;
            }
        }
        if controls.keyframe.swap(false, Ordering::Relaxed) {
            pipeline.request_keyframe();
        }
        pipeline.set_bitrate(controls.bitrate.load(Ordering::Relaxed));
        pipeline.set_frame_rate(controls.fps.load(Ordering::Relaxed));
        if let Some((width, height)) = controls.resolution.lock().unwrap().take() {
            let bitrate = controls.bitrate.load(Ordering::Relaxed);
            if let Err(error) = pipeline.set_resolution(width, height, bitrate) {
                warn!("resolution change to {width}x{height} failed: {error:#}");
            }
        }
        match pipeline.step(&mut deliver) {
            Ok(video::StepStatus::Continue) => {}
            Ok(video::StepStatus::RespawnThread) => return VideoExit::Respawn,
            Err(error) => {
                warn!("video pipeline failed: {error:#}");
                // The session notices the missing frames and the controller
                // reconnects; spinning on a broken encoder helps no one.
                return VideoExit::Failed;
            }
        }
    }
    VideoExit::Stopped
}
