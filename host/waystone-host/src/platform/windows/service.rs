//! Windows Service Control Manager (SCM) integration and local control pipe
//! (design doc 4.1).
//!
//! Running as a `LocalSystem` service in Session 0 keeps this computer online
//! across reboots, user sign-outs, and the lock screen. During an active stream
//! the service spawns a privileged console worker (see `worker.rs`), while the
//! taskbar tray and CLI talk to the service over [`CONTROL_PIPE_NAME`].

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, error, info, warn};
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE,
    NO_ERROR,
};
use windows::Win32::System::Services::*;

use super::security::{protect_file_for_system_and_admins, SecurityDescriptor, CONTROL_PIPE_SDDL};
use super::session_spawn::active_console_session_id;
use super::system;
use crate::agent::{self, Command, Status};
use crate::config::{Config, DeviceKey};
use crate::ipc::{
    read_json_frame, write_json_frame, ControlRequest, ControlResponse, ServiceAgentStatus,
    ServiceSnapshot, CONTROL_PIPE_NAME,
};
use crate::session::SessionMode;

pub const SERVICE_NAME: &str = "WaystoneHost";
const SERVICE_NAME_W: PCWSTR = w!("WaystoneHost");
const SERVICE_DISPLAY_NAME_W: PCWSTR = w!("Waystone Host Service");
const SERVICE_DESCRIPTION_TEXT: &str = "Waystone remote streaming host service (Session 0 privileged agent for elevated windows, UAC prompts, and the lock screen).";

const SCM_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Standard Win32 `DELETE` access right (`0x0001_0000`) for `DeleteService`.
const SERVICE_DELETE: u32 = 0x0001_0000;

/// State of the `WaystoneHost` service reported by SCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmStatus {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    Other(u32),
}

impl ScmStatus {
    pub fn label(self) -> &'static str {
        match self {
            ScmStatus::NotInstalled => "not installed",
            ScmStatus::Stopped => "stopped",
            ScmStatus::StartPending => "starting",
            ScmStatus::StopPending => "stopping",
            ScmStatus::Running => "running",
            ScmStatus::Other(_) => "unknown",
        }
    }

    pub fn is_running(self) -> bool {
        matches!(self, ScmStatus::Running | ScmStatus::StartPending)
    }
}

struct ScHandleGuard(SC_HANDLE);

impl Drop for ScHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseServiceHandle(self.0) };
        }
    }
}

/// Queries SCM for the current state of `WaystoneHost`. Works without elevation.
pub fn query_scm_status() -> ScmStatus {
    let Ok(scm) = (unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }) else {
        return ScmStatus::NotInstalled;
    };
    let _scm = ScHandleGuard(scm);
    let service = match unsafe { OpenServiceW(scm, SERVICE_NAME_W, SERVICE_QUERY_STATUS) } {
        Ok(service) => ScHandleGuard(service),
        Err(error) if error.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            return ScmStatus::NotInstalled;
        }
        Err(_) => return ScmStatus::NotInstalled,
    };
    query_handle_status(service.0).unwrap_or(ScmStatus::Other(0))
}

fn query_handle_status(service: SC_HANDLE) -> Result<ScmStatus> {
    let mut ssp = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0u32;
    let slice = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut ssp as *mut SERVICE_STATUS_PROCESS) as *mut u8,
            std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
        )
    };
    unsafe { QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(slice), &mut needed) }
        .context("QueryServiceStatusEx failed")?;
    Ok(match ssp.dwCurrentState {
        SERVICE_STOPPED => ScmStatus::Stopped,
        SERVICE_START_PENDING => ScmStatus::StartPending,
        SERVICE_STOP_PENDING => ScmStatus::StopPending,
        SERVICE_RUNNING => ScmStatus::Running,
        other => ScmStatus::Other(other.0),
    })
}

/// Installs (or updates) `WaystoneHost` as an auto-start `LocalSystem` service.
pub fn install(state_dir: &Path, explicit_state_dir: Option<&Path>, start_now: bool) -> Result<()> {
    if !system::input_permitted(false) {
        bail!("installing the Windows service requires an elevated Administrator terminal");
    }
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    system::migrate_legacy_state(state_dir);
    let key_path = state_dir.join("device.key");
    if key_path.exists() {
        if let Err(error) = protect_file_for_system_and_admins(&key_path) {
            warn!("could not tighten ACL on {}: {error:#}", key_path.display());
        }
    }

    let exe = system::current_exe();
    let command = match explicit_state_dir {
        Some(dir) => format!(
            "\"{}\" --state-dir \"{}\" service run",
            exe.display(),
            dir.display()
        ),
        None => format!("\"{}\" service run", exe.display()),
    };
    let command_w = HSTRING::from(&command);

    let scm = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CREATE_SERVICE) }
        .context("opening Service Control Manager")?;
    let _scm = ScHandleGuard(scm);

    let service = match unsafe { OpenServiceW(scm, SERVICE_NAME_W, SERVICE_ALL_ACCESS) } {
        Ok(existing) => {
            let guard = ScHandleGuard(existing);
            let was_running = query_handle_status(guard.0)
                .map(ScmStatus::is_running)
                .unwrap_or(false);
            if was_running {
                let _ = stop_handle(guard.0);
            }
            unsafe {
                ChangeServiceConfigW(
                    guard.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    &command_w,
                    None,
                    None,
                    None,
                    None,
                    None,
                    SERVICE_DISPLAY_NAME_W,
                )
            }
            .context("updating existing WaystoneHost service configuration")?;
            guard
        }
        Err(error) if error.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            let created = unsafe {
                CreateServiceW(
                    scm,
                    SERVICE_NAME_W,
                    SERVICE_DISPLAY_NAME_W,
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    &command_w,
                    None,
                    None,
                    None,
                    None, // LocalSystem
                    None,
                )
            }
            .context("creating WaystoneHost service")?;
            ScHandleGuard(created)
        }
        Err(error) => return Err(error).context("opening existing WaystoneHost service"),
    };

    configure_metadata_and_recovery(service.0);

    if start_now {
        start_handle(service.0)?;
    }
    Ok(())
}

/// Stops (if running) and removes the `WaystoneHost` service from SCM.
pub fn uninstall() -> Result<()> {
    if !system::input_permitted(false) {
        bail!("uninstalling the Windows service requires an elevated Administrator terminal");
    }
    let scm = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }
        .context("opening Service Control Manager")?;
    let _scm = ScHandleGuard(scm);
    let service = match unsafe {
        OpenServiceW(
            scm,
            SERVICE_NAME_W,
            SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_DELETE,
        )
    } {
        Ok(service) => ScHandleGuard(service),
        Err(error) if error.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            bail!("WaystoneHost service is not installed");
        }
        Err(error) => return Err(error).context("opening WaystoneHost service"),
    };
    let _ = stop_handle(service.0);
    unsafe { DeleteService(service.0) }.context("deleting WaystoneHost service")?;
    Ok(())
}

/// Starts the installed `WaystoneHost` service and waits until it is running.
pub fn start() -> Result<()> {
    if !system::input_permitted(false) {
        bail!("starting the Windows service requires an elevated Administrator terminal");
    }
    let scm = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }
        .context("opening Service Control Manager")?;
    let _scm = ScHandleGuard(scm);
    let service =
        unsafe { OpenServiceW(scm, SERVICE_NAME_W, SERVICE_START | SERVICE_QUERY_STATUS) }
            .context("opening WaystoneHost service (is it installed?)")?;
    let _service = ScHandleGuard(service);
    start_handle(service)
}

/// Stops the running `WaystoneHost` service and waits until it is stopped.
pub fn stop() -> Result<()> {
    if !system::input_permitted(false) {
        bail!("stopping the Windows service requires an elevated Administrator terminal");
    }
    let scm = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }
        .context("opening Service Control Manager")?;
    let _scm = ScHandleGuard(scm);
    let service = unsafe { OpenServiceW(scm, SERVICE_NAME_W, SERVICE_STOP | SERVICE_QUERY_STATUS) }
        .context("opening WaystoneHost service (is it installed?)")?;
    let _service = ScHandleGuard(service);
    stop_handle(service)
}

fn configure_metadata_and_recovery(service: SC_HANDLE) {
    let mut desc_buf: Vec<u16> = SERVICE_DESCRIPTION_TEXT
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let desc = SERVICE_DESCRIPTIONW {
        lpDescription: PWSTR(desc_buf.as_mut_ptr()),
    };
    let _ = unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            Some(&desc as *const _ as *const c_void),
        )
    };

    let mut actions = [
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 3_000,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 5_000,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 10_000,
        },
    ];
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        lpRebootMsg: PWSTR::null(),
        lpCommand: PWSTR::null(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    let _ = unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&failure as *const _ as *const c_void),
        )
    };
}

fn start_handle(service: SC_HANDLE) -> Result<()> {
    match unsafe { StartServiceW(service, None) } {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => return Ok(()),
        Err(error) => return Err(error).context("StartServiceW failed"),
    }
    wait_for_state(service, ScmStatus::Running)
}

fn stop_handle(service: SC_HANDLE) -> Result<()> {
    let mut status = SERVICE_STATUS::default();
    match unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) } {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => return Ok(()),
        Err(error) => return Err(error).context("ControlService(STOP) failed"),
    }
    wait_for_state(service, ScmStatus::Stopped)
}

fn wait_for_state(service: SC_HANDLE, target: ScmStatus) -> Result<()> {
    let deadline = Instant::now() + SCM_WAIT_TIMEOUT;
    loop {
        let current = query_handle_status(service)?;
        if current == target {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for WaystoneHost to reach {} (currently {})",
                target.label(),
                current.label()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// MARK: SCM Dispatcher & Session 0 Service Runtime

static SERVICE_STATE_DIR: OnceLock<PathBuf> = OnceLock::new();
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
static SHUTDOWN_TX: OnceLock<Mutex<Option<watch::Sender<bool>>>> = OnceLock::new();

/// Entrypoint for `waystone-host service run`, invoked by Windows SCM.
pub fn run_service(state_dir: PathBuf) -> Result<()> {
    let _ = SERVICE_STATE_DIR.set(state_dir);
    let mut name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::null(),
            lpServiceProc: None,
        },
    ];
    if let Err(error) = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        if error.code() == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT.to_hresult() {
            bail!(
                "`waystone-host service run` is launched by Windows Service Control Manager. \
                 Install the service with `waystone-host service install` or use `waystone-host run` for foreground mode."
            );
        }
        return Err(error).context("StartServiceCtrlDispatcherW failed");
    }
    Ok(())
}

extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(SERVICE_NAME_W, Some(service_ctrl_handler), None)
    } {
        Ok(handle) => handle,
        Err(_) => return,
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::Release);
    set_service_status(handle, SERVICE_START_PENDING, 0, 10_000);

    let state_dir = SERVICE_STATE_DIR
        .get()
        .cloned()
        .unwrap_or_else(super::state_dir);
    let _ = std::fs::create_dir_all(&state_dir);
    init_service_logging(&state_dir);
    std::panic::set_hook(Box::new(|panic| error!("service panic: {panic}")));

    info!(
        "Waystone Session 0 service starting (session={}, state={})",
        system::current_process_session_id(),
        state_dir.display()
    );

    let key_path = state_dir.join("device.key");
    if key_path.exists() {
        let _ = protect_file_for_system_and_admins(&key_path);
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let slot = SHUTDOWN_TX.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = Some(shutdown_tx);

    set_service_status(handle, SERVICE_RUNNING, 0, 0);

    let exit_code = match tokio::runtime::Runtime::new() {
        Ok(runtime) => match runtime.block_on(service_async_main(state_dir, shutdown_rx)) {
            Ok(()) => 0,
            Err(error) => {
                error!("service exited with error: {error:#}");
                1
            }
        },
        Err(error) => {
            error!("cannot start Tokio runtime: {error:#}");
            1
        }
    };

    *slot.lock().unwrap() = None;
    info!("Waystone Session 0 service stopped (exit={exit_code})");
    set_service_status(handle, SERVICE_STOPPED, exit_code, 0);
}

extern "system" fn service_ctrl_handler(
    control: u32,
    event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            let raw = STATUS_HANDLE.load(Ordering::Acquire);
            if raw != 0 {
                set_service_status(
                    SERVICE_STATUS_HANDLE(raw as *mut c_void),
                    SERVICE_STOP_PENDING,
                    0,
                    10_000,
                );
            }
            if let Some(slot) = SHUTDOWN_TX.get() {
                if let Some(tx) = slot.lock().unwrap().as_ref() {
                    let _ = tx.send(true);
                }
            }
            NO_ERROR.0
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            debug!("SCM session change notification: event={event_type}");
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

fn set_service_status(
    handle: SERVICE_STATUS_HANDLE,
    state: SERVICE_STATUS_CURRENT_STATE,
    win32_exit_code: u32,
    wait_hint_ms: u32,
) {
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
    } else {
        0
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: win32_exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_hint_ms,
    };
    let _ = unsafe { SetServiceStatus(handle, &status) };
}

fn init_service_logging(dir: &Path) {
    let path = dir.join("host.log");
    let _ = std::fs::rename(&path, dir.join("host.old.log"));
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystone_host=info,waystone_rtc=info".into()),
        )
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .try_init();
}

// MARK: Service State Hub & Control Pipe Server

struct ServiceHub {
    state_dir: PathBuf,
    inner: Mutex<HubInner>,
    updates: broadcast::Sender<ServiceSnapshot>,
}

struct HubInner {
    config: Option<Config>,
    device_id: Option<String>,
    status: ServiceAgentStatus,
    agent_tx: Option<mpsc::UnboundedSender<Command>>,
    agent_task: Option<tokio::task::JoinHandle<()>>,
}

impl ServiceHub {
    fn new(state_dir: PathBuf) -> Arc<ServiceHub> {
        let config = Config::load(&state_dir.join("config.json")).ok();
        let device_id = config.as_ref().and_then(|cfg| {
            DeviceKey::load_or_create(&state_dir.join("device.key"))
                .ok()
                .map(|key| key.info(&cfg.name).id.to_string())
        });
        let status = if config.is_some() {
            ServiceAgentStatus::Connecting
        } else {
            ServiceAgentStatus::NotPaired
        };
        let (updates, _) = broadcast::channel(32);
        Arc::new(ServiceHub {
            state_dir,
            inner: Mutex::new(HubInner {
                config,
                device_id,
                status,
                agent_tx: None,
                agent_task: None,
            }),
            updates,
        })
    }

    fn snapshot(&self) -> ServiceSnapshot {
        let inner = self.inner.lock().unwrap();
        ServiceSnapshot {
            status: inner.status.clone(),
            paired: inner.config.is_some(),
            device_id: inner.device_id.clone(),
            name: inner.config.as_ref().map(|c| c.name.clone()),
            endpoint: inner.config.as_ref().map(|c| c.endpoint.clone()),
            turn: inner.config.as_ref().map(|c| c.turn.clone()),
            max_bitrate: inner.config.as_ref().map(|c| c.max_bitrate),
            lock_on_disconnect: inner
                .config
                .as_ref()
                .map(|c| c.lock_on_disconnect)
                .unwrap_or(true),
            mute_speakers: inner
                .config
                .as_ref()
                .map(|c| c.mute_speakers)
                .unwrap_or(true),
            state_dir: self.state_dir.display().to_string(),
            active_console_session: active_console_session_id(),
        }
    }

    fn broadcast(&self) {
        let _ = self.updates.send(self.snapshot());
    }

    fn start_agent(self: &Arc<Self>) {
        let (config, key) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(config) = inner.config.clone() else {
                inner.status = ServiceAgentStatus::NotPaired;
                drop(inner);
                self.broadcast();
                return;
            };
            let key_path = self.state_dir.join("device.key");
            let key = match DeviceKey::load_or_create(&key_path) {
                Ok(key) => {
                    let _ = protect_file_for_system_and_admins(&key_path);
                    Arc::new(key)
                }
                Err(error) => {
                    warn!("cannot load device key: {error:#}");
                    inner.status = ServiceAgentStatus::Offline {
                        reason: format!("device key error: {error:#}"),
                    };
                    drop(inner);
                    self.broadcast();
                    return;
                }
            };
            inner.device_id = Some(key.info(&config.name).id.to_string());
            if let Some(tx) = inner.agent_tx.take() {
                let _ = tx.send(Command::Shutdown);
            }
            if let Some(old) = inner.agent_task.take() {
                old.abort();
            }
            (config, key)
        };

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let hub_report = self.clone();
        let report: agent::Reporter = Arc::new(move |status: Status| {
            {
                let mut inner = hub_report.inner.lock().unwrap();
                inner.status = ServiceAgentStatus::from(&status);
            }
            hub_report.broadcast();
        });

        let hub_done = self.clone();
        let mode = SessionMode::ConsoleWorker {
            state_dir: self.state_dir.clone(),
        };
        let task = tokio::spawn(async move {
            if let Err(error) = agent::run_with_mode(config, key, mode, report, cmd_rx).await {
                warn!("service agent stopped: {error:#}");
                {
                    let mut inner = hub_done.inner.lock().unwrap();
                    inner.status = ServiceAgentStatus::Offline {
                        reason: format!("{error:#}"),
                    };
                }
                hub_done.broadcast();
            }
        });

        {
            let mut inner = self.inner.lock().unwrap();
            inner.agent_tx = Some(cmd_tx);
            inner.agent_task = Some(task);
        }
        self.broadcast();
    }

    async fn shutdown_agent(&self) {
        let task = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(tx) = inner.agent_tx.take() {
                let _ = tx.send(Command::Shutdown);
            }
            inner.agent_task.take()
        };
        if let Some(task) = task {
            if tokio::time::timeout(Duration::from_secs(6), task)
                .await
                .is_err()
            {
                warn!("service agent did not stop within 6s");
            }
        }
    }

    async fn handle_request(self: &Arc<Self>, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::GetStatus => ControlResponse::Snapshot(self.snapshot()),
            ControlRequest::Subscribe => ControlResponse::Snapshot(self.snapshot()),
            ControlRequest::EndSession => {
                if let Some(tx) = self.inner.lock().unwrap().agent_tx.clone() {
                    let _ = tx.send(Command::EndSession);
                }
                ControlResponse::Ok
            }
            ControlRequest::UpdateSettings {
                lock_on_disconnect,
                mute_speakers,
            } => {
                let save_result = {
                    let mut inner = self.inner.lock().unwrap();
                    let Some(config) = inner.config.as_mut() else {
                        return ControlResponse::Error {
                            message: "this computer is not paired yet".into(),
                        };
                    };
                    config.lock_on_disconnect = lock_on_disconnect;
                    config.mute_speakers = mute_speakers;
                    let res = config.save(&self.state_dir.join("config.json"));
                    if let Some(tx) = &inner.agent_tx {
                        let _ = tx.send(Command::Settings {
                            lock_on_disconnect,
                            mute_speakers,
                        });
                    }
                    res
                };
                self.broadcast();
                match save_result {
                    Ok(()) => ControlResponse::Ok,
                    Err(error) => ControlResponse::Error {
                        message: format!("{error:#}"),
                    },
                }
            }
            ControlRequest::Pair { uri, name } => {
                let chosen_name = name.unwrap_or_else(|| {
                    self.inner
                        .lock()
                        .unwrap()
                        .config
                        .as_ref()
                        .map(|c| c.name.clone())
                        .unwrap_or_else(Config::default_name)
                });
                match agent::pair(&self.state_dir, &uri, chosen_name).await {
                    Ok(config) => {
                        let key_path = self.state_dir.join("device.key");
                        let _ = protect_file_for_system_and_admins(&key_path);
                        {
                            let mut inner = self.inner.lock().unwrap();
                            inner.config = Some(config);
                        }
                        self.start_agent();
                        ControlResponse::Snapshot(self.snapshot())
                    }
                    Err(error) => ControlResponse::Error {
                        message: format!("{error:#}"),
                    },
                }
            }
        }
    }
}

async fn service_async_main(
    state_dir: PathBuf,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let hub = ServiceHub::new(state_dir);
    hub.start_agent();

    let pipe_hub = hub.clone();
    let mut pipe_shutdown = shutdown_rx.clone();
    let control_server = tokio::spawn(async move {
        if let Err(error) = run_control_pipe_server(pipe_hub, &mut pipe_shutdown).await {
            error!("control pipe server exited: {error:#}");
        }
    });

    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }

    info!("shutdown requested; stopping agent and control pipe");
    control_server.abort();
    hub.shutdown_agent().await;
    Ok(())
}

fn create_control_pipe(first: bool) -> Result<NamedPipeServer> {
    let sd = SecurityDescriptor::from_sddl(CONTROL_PIPE_SDDL)?;
    let mut sa = sd.security_attributes();
    let mut opts = ServerOptions::new();
    opts.pipe_mode(PipeMode::Byte)
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .in_buffer_size(64 * 1024)
        .out_buffer_size(64 * 1024);
    unsafe {
        opts.create_with_security_attributes_raw(
            CONTROL_PIPE_NAME,
            &mut sa as *mut _ as *mut c_void,
        )
    }
    .context("creating control Named Pipe")
}

async fn run_control_pipe_server(
    hub: Arc<ServiceHub>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let mut server = create_control_pipe(true)?;
    loop {
        tokio::select! {
            res = server.connect() => {
                if let Err(error) = res {
                    warn!("control pipe accept error: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                let connected = server;
                // Create the next listening instance immediately so clients never see a gap.
                server = create_control_pipe(false)?;
                let hub_c = hub.clone();
                tokio::spawn(async move {
                    handle_control_client(connected, hub_c).await;
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn handle_control_client(mut pipe: NamedPipeServer, hub: Arc<ServiceHub>) {
    let request: ControlRequest = match read_json_frame(&mut pipe).await {
        Ok(req) => req,
        Err(error) => {
            debug!("control pipe read failed: {error:#}");
            return;
        }
    };
    if matches!(request, ControlRequest::Subscribe) {
        let mut rx = hub.updates.subscribe();
        let initial = ControlResponse::Snapshot(hub.snapshot());
        if write_json_frame(&mut pipe, &initial).await.is_err() {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(snapshot) => {
                    if write_json_frame(&mut pipe, &ControlResponse::Snapshot(snapshot))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let latest = ControlResponse::Snapshot(hub.snapshot());
                    if write_json_frame(&mut pipe, &latest).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        return;
    }

    let response = hub.handle_request(request).await;
    let _ = write_json_frame(&mut pipe, &response).await;
}

// MARK: Control Pipe Client (Tray & CLI)

/// Sends one control request to the running Session 0 service.
pub async fn send_control_request(request: ControlRequest) -> Result<ControlResponse> {
    let mut client = open_control_pipe().await?;
    write_json_frame(&mut client, &request).await?;
    read_json_frame(&mut client).await
}

/// Fetches a live status snapshot from the running Session 0 service.
pub async fn query_live_status() -> Result<ServiceSnapshot> {
    match send_control_request(ControlRequest::GetStatus).await? {
        ControlResponse::Snapshot(snapshot) => Ok(snapshot),
        ControlResponse::Error { message } => Err(anyhow!("{message}")),
        ControlResponse::Ok => Err(anyhow!("unexpected response from service")),
    }
}

/// Opens a persistent subscription stream to the running Session 0 service.
pub async fn subscribe_control_events() -> Result<NamedPipeClient> {
    let mut client = open_control_pipe().await?;
    write_json_frame(&mut client, &ControlRequest::Subscribe).await?;
    Ok(client)
}

async fn open_control_pipe() -> Result<NamedPipeClient> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match ClientOptions::new().open(CONTROL_PIPE_NAME) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(231) && Instant::now() < deadline => {
                // ERROR_PIPE_BUSY: wait briefly for the server to spin up the next instance.
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            Err(error) => {
                return Err(error).context("connecting to WaystoneHost service control pipe")
            }
        }
    }
}
