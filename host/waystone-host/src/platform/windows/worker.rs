//! Dual-process console worker and Session 0 bridge (design doc 4.1).
//!
//! * [`run_worker`] is the entrypoint of the privileged child process spawned in
//!   the active physical console session (`LocalSystem` + `TokenUIAccess`). It
//!   runs DXGI capture, WASAPI loopback, `SendInput`, speaker muting, screen
//!   locking, and desktop-switch tracking (`Default` <-> `Winlogon`).
//! * [`WorkerSession`] runs inside the Session 0 Windows Service. It creates a
//!   unique per-session Named Pipe restricted to `SYSTEM` and `Administrators`,
//!   spawns the console worker, streams encoded media directly into
//!   [`FrameSink`], forwards raw `waystone-proto` input frames, recovers across
//!   console session changes, and coordinates deterministic teardown.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use waystone_proto::VideoCodec;

use super::capture::Capture;
use super::desktop::{self, DesktopWatcher, SwitchOutcome};
use super::security::{SecurityDescriptor, WORKER_PIPE_SDDL};
use super::session_spawn::{
    active_console_session_id, lock_console_session_fallback, spawn_privileged_worker, ChildProcess,
};
use super::system::{self, SpeakerMute};
use crate::capture::{FrameSink, MediaKind};
use crate::display::Display;
use crate::input::Injector;
use crate::ipc::{worker_pipe_name, AckOp, ServiceToWorker, WorkerToService};
use crate::plan::StreamPlan;

const PIPE_BUFFER_BYTES: u32 = 512 * 1024;
const WORKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ROLLBACK_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const MEDIA_CHANNEL_CAPACITY: usize = 48;

// MARK: Console-session worker process

/// Entrypoint for `waystone-host worker --pipe <pipe>`.
pub fn run_worker(pipe_name: &str, state_dir: &Path) -> Result<()> {
    init_worker_logging(state_dir);
    std::panic::set_hook(Box::new(|panic| tracing::error!("worker panic: {panic}")));
    info!(
        "console worker starting in session {} (pipe: {})",
        system::current_process_session_id(),
        pipe_name
    );

    if let SwitchOutcome::Switched(name) = desktop::sync_thread_to_input_desktop() {
        info!("worker main thread attached to desktop {name}");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("building worker runtime")?;

    runtime.block_on(worker_main(pipe_name))
}

async fn worker_main(pipe_name: &str) -> Result<()> {
    let pipe = connect_worker_pipe(pipe_name).await?;
    let (mut reader, mut writer) = tokio::io::split(pipe);

    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<WorkerToService>();
    let (media_tx, mut media_rx) = mpsc::channel::<WorkerToService>(MEDIA_CHANNEL_CAPACITY);

    let writer_task = tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                biased;
                Some(ctrl) = control_rx.recv() => ctrl,
                Some(media) = media_rx.recv() => media,
                else => break,
            };
            if let Err(error) = message.write_to(&mut writer).await {
                debug!("worker pipe write ended: {error:#}");
                break;
            }
        }
    });

    let switch_tx = control_tx.clone();
    let _desktop_watcher = DesktopWatcher::start(move |desktop| {
        let _ = switch_tx.send(WorkerToService::DesktopSwitched { desktop });
    });

    let display = wait_for_main_display().await?;
    let session_id = system::current_process_session_id();
    let input_permitted = system::input_permitted(false);
    let _ = control_tx.send(WorkerToService::Hello {
        display,
        input_permitted,
        session_id,
    });
    let (w, h) = display.pixels;
    info!(
        "worker ready: display={}x{} session={} elevated={}",
        w, h, session_id, input_permitted
    );

    let injector = Injector::start(display);
    let input_sender = injector.sender();
    let mut capture: Option<Capture> = None;
    let mut mute: Option<SpeakerMute> = None;

    loop {
        let command = match ServiceToWorker::read_from(&mut reader).await {
            Ok(command) => command,
            Err(error) => {
                info!("worker pipe closed: {error:#}");
                break;
            }
        };
        match command {
            ServiceToWorker::StartCapture {
                display_id,
                plan,
                codec,
                audio,
            } => {
                if let Some(old) = capture.take() {
                    tokio::task::block_in_place(|| drop(old));
                }
                let target_display = Display {
                    id: display_id,
                    ..display
                };
                let tx = media_tx.clone();
                let sink = FrameSink::with_callback(Arc::new(move |kind, data, micros| {
                    let msg = match kind {
                        MediaKind::Video => WorkerToService::VideoFrame {
                            micros,
                            data: data.to_vec(),
                        },
                        MediaKind::Audio => WorkerToService::AudioPacket {
                            micros,
                            data: data.to_vec(),
                        },
                    };
                    // Drop media frames if the pipe is saturated rather than
                    // blocking the GPU video or WASAPI audio capture threads.
                    let _ = tx.try_send(msg);
                }));
                match Capture::start(&target_display, &plan, codec, audio, sink).await {
                    Ok(started) => {
                        capture = Some(started);
                        let _ = control_tx.send(WorkerToService::CaptureStarted);
                    }
                    Err(error) => {
                        warn!("worker capture start failed: {error:#}");
                        let _ =
                            control_tx.send(WorkerToService::CaptureFailed(format!("{error:#}")));
                    }
                }
            }
            ServiceToWorker::StopCapture => {
                if let Some(active) = capture.take() {
                    tokio::task::block_in_place(|| drop(active));
                }
                let _ = control_tx.send(WorkerToService::Ack {
                    op: AckOp::StopCapture,
                    ok: true,
                    message: String::new(),
                });
            }
            ServiceToWorker::RequestKeyframe => {
                if let Some(active) = &capture {
                    active.request_keyframe();
                }
            }
            ServiceToWorker::SetBitrate(bitrate) => {
                if let Some(active) = &capture {
                    active.set_bitrate(bitrate);
                }
            }
            ServiceToWorker::SetFrameRate(fps) => {
                if let Some(active) = &capture {
                    active.set_frame_rate(fps);
                }
            }
            ServiceToWorker::SetResolution {
                width,
                height,
                bitrate,
            } => {
                if let Some(active) = &capture {
                    let _ = active.set_resolution(width, height, bitrate);
                }
            }
            ServiceToWorker::SetAudio(enabled) => {
                if let Some(active) = &capture {
                    active.set_audio(enabled);
                }
            }
            ServiceToWorker::InputFrame(data) => input_sender.submit(data),
            ServiceToWorker::ReleaseAllInput => {
                injector.release_all();
                let _ = control_tx.send(WorkerToService::Ack {
                    op: AckOp::ReleaseAllInput,
                    ok: true,
                    message: String::new(),
                });
            }
            ServiceToWorker::SetMute(muted) => {
                if muted {
                    if mute.is_none() {
                        mute = Some(tokio::task::block_in_place(SpeakerMute::engage));
                    }
                } else if let Some(guard) = mute.take() {
                    tokio::task::block_in_place(|| drop(guard));
                }
                let _ = control_tx.send(WorkerToService::Ack {
                    op: AckOp::SetMute,
                    ok: true,
                    message: String::new(),
                });
            }
            ServiceToWorker::LockScreen => {
                tokio::task::block_in_place(system::lock_screen);
                let _ = control_tx.send(WorkerToService::Ack {
                    op: AckOp::LockScreen,
                    ok: true,
                    message: String::new(),
                });
            }
            ServiceToWorker::Shutdown => {
                info!("worker received shutdown");
                break;
            }
        }
    }

    // Deterministic local cleanup even on unexpected pipe break.
    tokio::task::block_in_place(|| {
        drop(injector);
        drop(capture);
        drop(mute);
    });
    drop(control_tx);
    drop(media_tx);
    let _ = tokio::time::timeout(Duration::from_millis(500), writer_task).await;
    info!("console worker stopped");
    Ok(())
}

async fn connect_worker_pipe(pipe_name: &str) -> Result<NamedPipeClient> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => return Ok(client),
            Err(error) if Instant::now() < deadline => {
                debug!("waiting for worker pipe {pipe_name}: {error}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("opening worker pipe {pipe_name}"))
            }
        }
    }
}

async fn wait_for_main_display() -> Result<Display> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let _ = desktop::sync_thread_to_input_desktop();
        if let Some(display) = system::main_display() {
            return Ok(display);
        }
        if Instant::now() >= deadline {
            bail!("no display is attached in the active console session");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn init_worker_logging(dir: &Path) {
    let path = dir.join("worker.log");
    let _ = std::fs::rename(&path, dir.join("worker.old.log"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystone_host=info,waystone_rtc=warn".into()),
        )
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .try_init();
}

// MARK: Session 0 side (WorkerSession)

#[derive(Clone, Copy)]
struct ActiveCapture {
    display_id: u32,
    plan: StreamPlan,
    codec: VideoCodec,
    audio: bool,
}

enum SupervisorMsg {
    Command(ServiceToWorker),
    StartCapture {
        capture: ActiveCapture,
        sink: Arc<FrameSink>,
        reply: oneshot::Sender<Result<()>>,
    },
    SyncCommand {
        command: ServiceToWorker,
        ack_op: AckOp,
        reply: std::sync::mpsc::Sender<bool>,
    },
    Shutdown,
}

/// Service-side handle to the privileged console worker for one streaming session.
pub struct WorkerSession {
    display: Display,
    exe: PathBuf,
    tx: mpsc::UnboundedSender<SupervisorMsg>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    finished: bool,
}

impl WorkerSession {
    /// Spawns a console worker in the active physical console session, waits for
    /// its secure Named Pipe handshake, and optionally engages speaker mute.
    pub async fn start(
        state_dir: PathBuf,
        mute_speakers: bool,
    ) -> Result<(WorkerSession, mpsc::UnboundedReceiver<String>)> {
        let exe = system::current_exe();
        let initial = spawn_and_handshake(&exe, &state_dir).await?;
        let display = initial.display;

        let (tx, rx) = mpsc::unbounded_channel();
        let (dead_tx, dead_rx) = mpsc::unbounded_channel();

        let supervisor = tokio::spawn(supervisor_loop(
            initial,
            exe.clone(),
            state_dir,
            rx,
            dead_tx,
        ));

        let session = WorkerSession {
            display,
            exe,
            tx,
            supervisor: Some(supervisor),
            finished: false,
        };

        if mute_speakers {
            let _ = session
                .tx
                .send(SupervisorMsg::Command(ServiceToWorker::SetMute(true)));
        }

        Ok((session, dead_rx))
    }

    pub fn display(&self) -> Display {
        self.display
    }

    /// Cheap handle for queueing raw input frames from the engine callback thread.
    pub fn input_sender(&self) -> WorkerInputSender {
        WorkerInputSender(self.tx.clone())
    }

    pub async fn start_capture(
        &self,
        display_id: u32,
        plan: StreamPlan,
        codec: VideoCodec,
        audio: bool,
        sink: Arc<FrameSink>,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(SupervisorMsg::StartCapture {
                capture: ActiveCapture {
                    display_id,
                    plan,
                    codec,
                    audio,
                },
                sink,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("console worker is no longer running"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("console worker stopped while starting capture"))?
    }

    pub fn request_keyframe(&self) {
        let _ = self
            .tx
            .send(SupervisorMsg::Command(ServiceToWorker::RequestKeyframe));
    }

    pub fn set_bitrate(&self, bitrate: u32) {
        let _ = self
            .tx
            .send(SupervisorMsg::Command(ServiceToWorker::SetBitrate(bitrate)));
    }

    pub fn set_frame_rate(&self, fps: u32) {
        let _ = self
            .tx
            .send(SupervisorMsg::Command(ServiceToWorker::SetFrameRate(fps)));
    }

    pub fn set_resolution(&self, width: u32, height: u32, bitrate: u32) {
        let _ = self
            .tx
            .send(SupervisorMsg::Command(ServiceToWorker::SetResolution {
                width,
                height,
                bitrate,
            }));
    }

    pub fn set_audio(&self, enabled: bool) {
        let _ = self
            .tx
            .send(SupervisorMsg::Command(ServiceToWorker::SetAudio(enabled)));
    }

    pub fn release_input_blocking(&self) {
        let _ = self.send_sync(ServiceToWorker::ReleaseAllInput, AckOp::ReleaseAllInput);
    }

    pub fn stop_capture_blocking(&self) {
        let _ = self.send_sync(ServiceToWorker::StopCapture, AckOp::StopCapture);
    }

    pub fn restore_speakers_blocking(&self) {
        let _ = self.send_sync(ServiceToWorker::SetMute(false), AckOp::SetMute);
    }

    pub fn lock_screen_blocking(&self) {
        if !self.send_sync(ServiceToWorker::LockScreen, AckOp::LockScreen) {
            warn!("worker did not acknowledge LockScreen; trying fallback console lock");
            if let Err(error) = lock_console_session_fallback(&self.exe) {
                warn!("fallback console lock failed: {error:#}");
            }
        }
    }

    pub fn shutdown_blocking(mut self) {
        self.finished = true;
        let _ = self.tx.send(SupervisorMsg::Shutdown);
        if let Some(handle) = self.supervisor.take() {
            // Give the supervisor up to 2s to cleanly terminate and reap the worker.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !handle.is_finished() {
                handle.abort();
            }
        }
    }

    fn send_sync(&self, command: ServiceToWorker, ack_op: AckOp) -> bool {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if self
            .tx
            .send(SupervisorMsg::SyncCommand {
                command,
                ack_op,
                reply: reply_tx,
            })
            .is_err()
        {
            return false;
        }
        reply_rx.recv_timeout(ROLLBACK_ACK_TIMEOUT).unwrap_or(false)
    }
}

impl Drop for WorkerSession {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.tx.send(SupervisorMsg::Shutdown);
        }
        if let Some(handle) = self.supervisor.take() {
            handle.abort();
        }
    }
}

/// Forwarder for raw `waystone-proto` input frames from the WebRTC engine thread.
#[derive(Clone)]
pub struct WorkerInputSender(mpsc::UnboundedSender<SupervisorMsg>);

impl WorkerInputSender {
    pub fn submit(&self, frame: Vec<u8>) {
        let _ = self
            .0
            .send(SupervisorMsg::Command(ServiceToWorker::InputFrame(frame)));
    }
}

struct ConnectedWorker {
    child: ChildProcess,
    display: Display,
    reader: ReadHalf<NamedPipeServer>,
    writer: WriteHalf<NamedPipeServer>,
}

async fn spawn_and_handshake(exe: &Path, state_dir: &Path) -> Result<ConnectedWorker> {
    let session_id =
        active_console_session_id().ok_or_else(|| anyhow!("no active physical console session"))?;
    let pipe_name = worker_pipe_name();
    let server = create_secure_worker_pipe(&pipe_name)?;

    let args = format!(
        "--state-dir \"{}\" worker --pipe \"{}\"",
        state_dir.display(),
        pipe_name
    );
    let child = spawn_privileged_worker(session_id, exe, &args)?;

    if let Err(error) = tokio::time::timeout(WORKER_CONNECT_TIMEOUT, server.connect()).await {
        child.terminate();
        bail!(
            "console worker (pid={}) did not connect to pipe: {error}",
            child.pid()
        );
    }

    let (mut reader, writer) = tokio::io::split(server);
    let hello = tokio::time::timeout(
        WORKER_CONNECT_TIMEOUT,
        WorkerToService::read_from(&mut reader),
    )
    .await
    .map_err(|_| anyhow!("timed out waiting for worker Hello"))?
    .context("reading worker Hello")?;

    let WorkerToService::Hello {
        display,
        input_permitted,
        session_id: worker_session,
    } = hello
    else {
        child.terminate();
        bail!("worker did not start with Hello");
    };
    let (w, h) = display.pixels;
    info!(
        "console worker pid={} connected (session={}, elevated={}, display={}x{})",
        child.pid(),
        worker_session,
        input_permitted,
        w,
        h
    );

    Ok(ConnectedWorker {
        child,
        display,
        reader,
        writer,
    })
}

fn create_secure_worker_pipe(pipe_name: &str) -> Result<NamedPipeServer> {
    let sd = SecurityDescriptor::from_sddl(WORKER_PIPE_SDDL)?;
    let mut sa = sd.security_attributes();
    let mut opts = ServerOptions::new();
    opts.pipe_mode(PipeMode::Byte)
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .max_instances(1)
        .in_buffer_size(PIPE_BUFFER_BYTES)
        .out_buffer_size(PIPE_BUFFER_BYTES);
    unsafe {
        opts.create_with_security_attributes_raw(
            pipe_name,
            &mut sa as *mut _ as *mut std::ffi::c_void,
        )
    }
    .with_context(|| format!("creating secure worker pipe {pipe_name}"))
}

async fn supervisor_loop(
    mut worker: ConnectedWorker,
    exe: PathBuf,
    state_dir: PathBuf,
    mut rx: mpsc::UnboundedReceiver<SupervisorMsg>,
    dead_tx: mpsc::UnboundedSender<String>,
) {
    let mut sink: Option<Arc<FrameSink>> = None;
    let mut active_capture: Option<ActiveCapture> = None;
    let mut muted = false;
    let mut pending_start: Option<oneshot::Sender<Result<()>>> = None;
    let mut pending_acks: VecDeque<(AckOp, std::sync::mpsc::Sender<bool>)> = VecDeque::new();
    let mut session_check = tokio::time::interval(Duration::from_millis(500));

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(SupervisorMsg::Command(cmd)) => {
                    match &cmd {
                        ServiceToWorker::SetMute(on) => muted = *on,
                        ServiceToWorker::SetBitrate(b) => {
                            if let Some(cap) = active_capture.as_mut() {
                                cap.plan.bitrate = *b;
                            }
                        }
                        ServiceToWorker::SetFrameRate(fps) => {
                            if let Some(cap) = active_capture.as_mut() {
                                cap.plan.fps = *fps;
                            }
                        }
                        ServiceToWorker::SetResolution {
                            width,
                            height,
                            bitrate,
                        } => {
                            if let Some(cap) = active_capture.as_mut() {
                                cap.plan.width = *width;
                                cap.plan.height = *height;
                                cap.plan.bitrate = *bitrate;
                            }
                        }
                        ServiceToWorker::SetAudio(on) => {
                            if let Some(cap) = active_capture.as_mut() {
                                cap.audio = *on;
                            }
                        }
                        _ => {}
                    }
                    if let Err(error) = cmd.write_to(&mut worker.writer).await {
                        warn!("writing command to worker failed: {error:#}");
                        if !try_recover_on_session_change(
                            &mut worker,
                            &exe,
                            &state_dir,
                            muted,
                            active_capture,
                        )
                        .await
                        {
                            notify_dead(&dead_tx, &mut pending_start, &mut pending_acks, format!("worker pipe write failed: {error:#}"));
                            break;
                        }
                    }
                }
                Some(SupervisorMsg::StartCapture { capture, sink: new_sink, reply }) => {
                    sink = Some(new_sink);
                    active_capture = Some(capture);
                    pending_start = Some(reply);
                    let cmd = ServiceToWorker::StartCapture {
                        display_id: capture.display_id,
                        plan: capture.plan,
                        codec: capture.codec,
                        audio: capture.audio,
                    };
                    if let Err(error) = cmd.write_to(&mut worker.writer).await {
                        if let Some(tx) = pending_start.take() {
                            let _ = tx.send(Err(anyhow!("worker pipe failed: {error:#}")));
                        }
                        notify_dead(&dead_tx, &mut pending_start, &mut pending_acks, format!("worker died during StartCapture: {error:#}"));
                        break;
                    }
                }
                Some(SupervisorMsg::SyncCommand { command, ack_op, reply }) => {
                    if matches!(command, ServiceToWorker::StopCapture) {
                        active_capture = None;
                    }
                    if matches!(command, ServiceToWorker::SetMute(false)) {
                        muted = false;
                    }
                    if let Err(error) = command.write_to(&mut worker.writer).await {
                        warn!("sync command {ack_op:?} write failed: {error:#}");
                        let _ = reply.send(false);
                    } else {
                        pending_acks.push_back((ack_op, reply));
                    }
                }
                Some(SupervisorMsg::Shutdown) | None => {
                    let _ = ServiceToWorker::Shutdown.write_to(&mut worker.writer).await;
                    break;
                }
            },
            incoming = WorkerToService::read_from(&mut worker.reader) => match incoming {
                Ok(WorkerToService::VideoFrame { micros, data }) => {
                    if let Some(sink) = &sink {
                        sink.deliver(MediaKind::Video, &data, micros);
                    }
                }
                Ok(WorkerToService::AudioPacket { micros, data }) => {
                    if let Some(sink) = &sink {
                        sink.deliver(MediaKind::Audio, &data, micros);
                    }
                }
                Ok(WorkerToService::CaptureStarted) => {
                    if let Some(tx) = pending_start.take() {
                        let _ = tx.send(Ok(()));
                    }
                }
                Ok(WorkerToService::CaptureFailed(message)) => {
                    if let Some(tx) = pending_start.take() {
                        let _ = tx.send(Err(anyhow!("{message}")));
                    } else {
                        warn!("worker capture failed: {message}");
                    }
                }
                Ok(WorkerToService::Ack { op, ok, message }) => {
                    if !ok {
                        warn!("worker ack {op:?} failed: {message}");
                    }
                    if let Some(pos) = pending_acks.iter().position(|(wanted, _)| *wanted == op) {
                        if let Some((_, tx)) = pending_acks.remove(pos) {
                            let _ = tx.send(ok);
                        }
                    }
                }
                Ok(WorkerToService::DesktopSwitched { desktop }) => {
                    info!("console desktop switched to {desktop}");
                }
                Ok(WorkerToService::Hello { .. }) => {}
                Err(error) => {
                    warn!("console worker disconnected: {error:#}");
                    if !try_recover_on_session_change(
                        &mut worker,
                        &exe,
                        &state_dir,
                        muted,
                        active_capture,
                    )
                    .await
                    {
                        notify_dead(
                            &dead_tx,
                            &mut pending_start,
                            &mut pending_acks,
                            format!("console worker exited unexpectedly: {error:#}"),
                        );
                        break;
                    }
                }
            },
            _ = session_check.tick() => {
                if let Some(current_console) = active_console_session_id() {
                    if current_console != worker.child.session_id() {
                        info!(
                            "active console session changed: {} -> {}; respawning worker",
                            worker.child.session_id(),
                            current_console
                        );
                        if !respawn_worker(
                            &mut worker,
                            &exe,
                            &state_dir,
                            muted,
                            active_capture,
                        )
                        .await
                        {
                            notify_dead(
                                &dead_tx,
                                &mut pending_start,
                                &mut pending_acks,
                                "failed to respawn worker after console session switch".into(),
                            );
                            break;
                        }
                    }
                }
            }
        }
    }

    // Reap worker process cleanly.
    let child = worker.child;
    tokio::task::spawn_blocking(move || {
        if !child.wait_timeout(Duration::from_secs(2)) {
            warn!(
                "worker pid={} did not exit in time, terminating",
                child.pid()
            );
            child.terminate();
        }
    })
    .await
    .ok();
}

async fn try_recover_on_session_change(
    worker: &mut ConnectedWorker,
    exe: &Path,
    state_dir: &Path,
    muted: bool,
    active_capture: Option<ActiveCapture>,
) -> bool {
    let Some(current_console) = active_console_session_id() else {
        return false;
    };
    if current_console == worker.child.session_id() {
        // Same console session -> genuine crash, trigger deterministic rollback.
        return false;
    }
    respawn_worker(worker, exe, state_dir, muted, active_capture).await
}

async fn respawn_worker(
    worker: &mut ConnectedWorker,
    exe: &Path,
    state_dir: &Path,
    muted: bool,
    active_capture: Option<ActiveCapture>,
) -> bool {
    let _ = ServiceToWorker::Shutdown.write_to(&mut worker.writer).await;
    worker.child.terminate();
    match spawn_and_handshake(exe, state_dir).await {
        Ok(mut replacement) => {
            if muted {
                let _ = ServiceToWorker::SetMute(true)
                    .write_to(&mut replacement.writer)
                    .await;
            }
            if let Some(cap) = active_capture {
                let cmd = ServiceToWorker::StartCapture {
                    display_id: replacement.display.id,
                    plan: cap.plan,
                    codec: cap.codec,
                    audio: cap.audio,
                };
                if let Err(error) = cmd.write_to(&mut replacement.writer).await {
                    warn!("failed to restart capture on respawned worker: {error:#}");
                    replacement.child.terminate();
                    return false;
                }
            }
            *worker = replacement;
            true
        }
        Err(error) => {
            warn!("failed to respawn console worker: {error:#}");
            false
        }
    }
}

fn notify_dead(
    dead_tx: &mpsc::UnboundedSender<String>,
    pending_start: &mut Option<oneshot::Sender<Result<()>>>,
    pending_acks: &mut VecDeque<(AckOp, std::sync::mpsc::Sender<bool>)>,
    reason: String,
) {
    if let Some(tx) = pending_start.take() {
        let _ = tx.send(Err(anyhow!("{reason}")));
    }
    while let Some((_, tx)) = pending_acks.pop_front() {
        let _ = tx.send(false);
    }
    let _ = dead_tx.send(reason);
}
