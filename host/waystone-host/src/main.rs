//! The host agent: keeps this computer online in the pool and streams it.
//!
//! On Windows the binary is a GUI program: started without arguments (from
//! Explorer, or at login) it lives in the notification area; with a
//! subcommand it attaches to the terminal it was started from. For full
//! support of elevated windows, UAC prompts and the lock screen, install it
//! as a Session 0 Windows service with `waystone-host service install`.
//!
//! * `agent` – signaling connection, reconnects, one session at a time.
//! * `session` – one stream: engine, capture, input, rollback.
//! * `ipc` – local Named Pipe framing for the Windows dual-process service.
//! * `input` – decoding and held-state rules; `plan` – stream size and rate.
//! * `platform` – capture, injection, system calls, service and tray per OS.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod agent;
mod capture;
mod config;
mod display;
mod input;
mod ipc;
mod plan;
mod platform;
mod session;
mod signal;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;

use waystone_proto::VideoCodec;

use crate::config::{state_dir, Config, DeviceKey};

#[derive(Parser, Debug)]
#[command(
    name = "waystone-host",
    version,
    about = "Stream this computer to your Waystone pool"
)]
struct Args {
    /// Directory holding the device key and configuration.
    #[arg(long, env = "WAYSTONE_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// Without one, Windows starts the tray and macOS prints this help.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Joins the pool with a host pairing link from the server panel.
    Pair {
        /// The `waystone://pair?...` link.
        uri: String,
        /// Name shown on the phone; the computer name by default.
        #[arg(long)]
        name: Option<String>,
    },
    /// Stays online and serves sessions until stopped.
    Run {
        /// Never lock the screen when a session ends, whatever the reason
        /// (useful while developing on the host itself).
        #[arg(long)]
        no_lock: bool,
        /// Leave the speakers alone during sessions.
        #[arg(long)]
        no_mute: bool,
    },
    /// Prints the identity, configuration and service state.
    Status,
    /// Captures for a few seconds without a session, to check permissions.
    CaptureTest {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Lives in the notification area: status, pairing, settings, quit.
    #[cfg(windows)]
    Tray,
    /// Manages the privileged Windows Session 0 service (`WaystoneHost`).
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Internal console-session worker spawned by the Session 0 service.
    #[cfg(windows)]
    #[command(hide = true)]
    Worker {
        #[arg(long)]
        pipe: String,
    },
    /// Internal fallback helper to lock the active console workstation.
    #[cfg(windows)]
    #[command(hide = true)]
    LockWorkstation,
}

#[cfg(windows)]
#[derive(Subcommand, Debug)]
enum ServiceAction {
    /// Installs (or updates) and starts the LocalSystem Windows service.
    Install {
        /// Register the service without starting it immediately.
        #[arg(long)]
        no_start: bool,
    },
    /// Stops and removes the Windows service.
    Uninstall,
    /// Starts the installed Windows service.
    Start,
    /// Stops the running Windows service.
    Stop,
    /// Prints the SCM and live status of the Windows service.
    Status,
    /// Internal entrypoint invoked by Windows Service Control Manager.
    Run,
}

fn main() -> Result<()> {
    platform::init();
    let args = Args::parse();
    let explicit_state_dir = args.state_dir.clone();
    let dir = args.state_dir.unwrap_or_else(state_dir);

    let command = match args.command {
        Some(command) => command,
        #[cfg(windows)]
        None => Command::Tray,
        #[cfg(not(windows))]
        None => {
            use clap::CommandFactory;
            Args::command().print_help()?;
            return Ok(());
        }
    };

    #[cfg(windows)]
    {
        if matches!(command, Command::LockWorkstation) {
            platform::lock_screen();
            return Ok(());
        }
        if let Command::Worker { pipe } = command {
            return platform::worker::run_worker(&pipe, &dir);
        }
        if let Command::Service {
            action: ServiceAction::Run,
        } = command
        {
            return platform::service::run_service(dir);
        }
        if explicit_state_dir.is_none() {
            platform::migrate_legacy_state(&dir);
        }
        if matches!(command, Command::Tray) {
            let _ = std::fs::create_dir_all(&dir);
            return platform::tray::run(&dir);
        }
    }

    #[cfg(not(windows))]
    let _ = explicit_state_dir;

    if !matches!(command, Command::Status) {
        let _ = std::fs::create_dir_all(&dir);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystone_host=info,waystone_rtc=warn".into()),
        )
        .init();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(cli(command, &dir, explicit_state_dir.as_deref()))
}

async fn cli(command: Command, dir: &Path, explicit_state_dir: Option<&Path>) -> Result<()> {
    #[cfg(not(windows))]
    let _ = explicit_state_dir;

    match command {
        Command::Pair { uri, name } => {
            #[cfg(windows)]
            if platform::service::query_scm_status().is_running() {
                let req = crate::ipc::ControlRequest::Pair {
                    uri: uri.clone(),
                    name: name.clone(),
                };
                match platform::service::send_control_request(req).await {
                    Ok(crate::ipc::ControlResponse::Snapshot(snap)) => {
                        let shown = snap.name.as_deref().unwrap_or("Waystone Host");
                        println!("Paired as \"{shown}\" through the running WaystoneHost service.");
                        return Ok(());
                    }
                    Ok(crate::ipc::ControlResponse::Error { message }) => bail!("{message}"),
                    Ok(crate::ipc::ControlResponse::Ok) => {
                        println!("Paired through the running WaystoneHost service.");
                        return Ok(());
                    }
                    Err(error) => {
                        bail!(
                            "WaystoneHost service is running, but pairing over the control pipe \
                             failed ({error:#}). Stop the service or run from an elevated prompt."
                        );
                    }
                }
            }
            let name = name.unwrap_or_else(Config::default_name);
            agent::pair(dir, &uri, name.clone()).await?;
            println!("Paired as \"{name}\". Start streaming with `waystone-host run`.");
            Ok(())
        }
        Command::Run { no_lock, no_mute } => {
            #[cfg(windows)]
            if platform::service::query_scm_status().is_running() {
                bail!(
                    "WaystoneHost Windows service is already running. Stop it first \
                     (`waystone-host service stop`) before starting foreground mode."
                );
            }
            let mut config = load(dir)?;
            if no_lock {
                config.lock_on_disconnect = false;
            }
            config.mute_speakers &= !no_mute;
            let key = Arc::new(DeviceKey::load_or_create(&dir.join("device.key"))?);
            // Nothing steers the CLI; holding the sender keeps the channel open.
            let (_commands, receiver) = tokio::sync::mpsc::unbounded_channel();
            agent::run(config, key, Arc::new(|_| {}), receiver).await
        }
        Command::Status => status(dir).await,
        Command::CaptureTest { seconds } => capture_test(seconds).await,
        #[cfg(windows)]
        Command::Service { action } => handle_service_cli(action, dir, explicit_state_dir).await,
        #[cfg(windows)]
        Command::Tray | Command::Worker { .. } | Command::LockWorkstation => {
            unreachable!("handled before the runtime starts")
        }
    }
}

#[cfg(windows)]
async fn handle_service_cli(
    action: ServiceAction,
    dir: &Path,
    explicit_state_dir: Option<&Path>,
) -> Result<()> {
    match action {
        ServiceAction::Install { no_start } => {
            platform::service::install(dir, explicit_state_dir, !no_start)?;
            println!(
                "Installed Windows service `{}` (state dir: {}).",
                platform::service::SERVICE_NAME,
                dir.display()
            );
            if !no_start {
                println!(
                    "Service is running in Session 0 with elevated desktop/UAC/lock-screen access."
                );
            }
            Ok(())
        }
        ServiceAction::Uninstall => {
            platform::service::uninstall()?;
            println!(
                "Uninstalled Windows service `{}`.",
                platform::service::SERVICE_NAME
            );
            Ok(())
        }
        ServiceAction::Start => {
            platform::service::start()?;
            println!(
                "Started Windows service `{}`.",
                platform::service::SERVICE_NAME
            );
            Ok(())
        }
        ServiceAction::Stop => {
            platform::service::stop()?;
            println!(
                "Stopped Windows service `{}`.",
                platform::service::SERVICE_NAME
            );
            Ok(())
        }
        ServiceAction::Status => print_windows_service_status().await,
        ServiceAction::Run => unreachable!("handled before the runtime starts"),
    }
}

#[cfg(windows)]
async fn print_windows_service_status() -> Result<()> {
    let scm = platform::service::query_scm_status();
    println!(
        "service      {} ({})",
        platform::service::SERVICE_NAME,
        scm.label()
    );
    if scm.is_running() {
        match platform::service::query_live_status().await {
            Ok(snap) => {
                println!("agent        {:?}", snap.status);
                println!("state dir    {}", snap.state_dir);
                if let Some(session) = snap.active_console_session {
                    println!("console      session {session}");
                }
                if let Some(id) = snap.device_id {
                    println!("device id    {id}");
                }
                if let Some(name) = snap.name {
                    println!("name         {name}");
                }
                if let Some(endpoint) = snap.endpoint {
                    println!("server       {endpoint}");
                }
            }
            Err(error) => {
                println!("control pipe unavailable ({error:#})");
            }
        }
    }
    Ok(())
}

fn load(dir: &Path) -> Result<Config> {
    Config::load(&dir.join("config.json"))
        .context("this computer is not paired yet; run `waystone-host pair <link>` first")
}

async fn status(dir: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let scm = platform::service::query_scm_status();
        println!(
            "service      {} ({})",
            platform::service::SERVICE_NAME,
            scm.label()
        );
        if scm.is_running() {
            if let Ok(snap) = platform::service::query_live_status().await {
                println!("agent        {:?}", snap.status);
                println!("state dir    {}", snap.state_dir);
                if let Some(session) = snap.active_console_session {
                    println!("console      session {session}");
                }
                if snap.paired {
                    if let Some(id) = snap.device_id {
                        println!("device id    {id}");
                    }
                    if let Some(name) = snap.name {
                        println!("name         {name}");
                    }
                    if let Some(endpoint) = snap.endpoint {
                        println!("server       {endpoint}");
                    }
                    if let Some(turn) = snap.turn {
                        println!("stun/turn    {turn}");
                    }
                    if let Some(bitrate) = snap.max_bitrate {
                        println!("bitrate      {} kbit/s at 1080p60", bitrate / 1000);
                    }
                    println!("auto lock    {}", snap.lock_on_disconnect);
                    println!("mute         {}", snap.mute_speakers);
                } else {
                    println!("paired       no (run `waystone-host pair <link>`)");
                }
                println!("input        allowed (Session 0 privileged service)");
                return Ok(());
            }
        }
    }

    let config = load(dir)?;
    let device_id = match DeviceKey::load_or_create(&dir.join("device.key")) {
        Ok(key) => key.info(&config.name).id.to_string(),
        Err(_) => "<requires administrator or running service>".to_string(),
    };
    println!("state dir    {}", dir.display());
    println!("device id    {device_id}");
    println!("name         {}", config.name);
    println!("server       {}", config.endpoint);
    println!("stun/turn    {}", config.turn);
    println!(
        "bitrate      {} kbit/s at 1080p60",
        config.max_bitrate / 1000
    );
    println!("auto lock    {}", config.lock_on_disconnect);
    println!("mute         {}", config.mute_speakers);
    println!(
        "input        {}",
        if platform::input_permitted(false) {
            "allowed"
        } else {
            platform::INPUT_PERMISSION_HINT
        }
    );
    Ok(())
}

/// Starts capture with no session attached and counts what comes out, so a
/// missing Screen Recording permission shows up before pairing a phone.
async fn capture_test(seconds: u64) -> Result<()> {
    let display = platform::main_display().context("no display")?;
    let plan = plan::plan(&display, None, 8_000_000);
    info!("capturing {}x{} for {seconds}s", plan.width, plan.height);
    let sink = capture::FrameSink::new(None);
    let capture =
        platform::Capture::start(&display, &plan, VideoCodec::H265, true, sink.clone()).await?;
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    tokio::task::block_in_place(|| drop(capture));
    let (video, audio) = sink.counts();
    println!("{video} video frames and {audio} audio packets in {seconds}s");
    if video == 0 {
        bail!("no frames: check the screen capture permission for this binary, then retry");
    }
    Ok(())
}
