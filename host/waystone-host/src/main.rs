//! The host agent: keeps this computer online in the pool and streams it.
//!
//! On Windows the binary is a GUI program: started without arguments (from
//! Explorer, or at login) it lives in the notification area; with a
//! subcommand it attaches to the terminal it was started from.
//!
//! * `agent` – signaling connection, reconnects, one session at a time.
//! * `session` – one stream: engine, capture, input, rollback.
//! * `input` – decoding and held-state rules; `plan` – stream size and rate.
//! * `platform` – capture, injection, system calls and the tray per
//!   operating system.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod agent;
mod capture;
mod config;
mod display;
mod input;
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
    /// Prints the identity and configuration.
    Status,
    /// Captures for a few seconds without a session, to check permissions.
    CaptureTest {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Lives in the notification area: status, pairing, settings, quit.
    #[cfg(windows)]
    Tray,
}

fn main() -> Result<()> {
    platform::init();
    let args = Args::parse();
    let dir = args.state_dir.unwrap_or_else(state_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

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
    if matches!(command, Command::Tray) {
        return platform::tray::run(&dir);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystone_host=info,waystone_rtc=warn".into()),
        )
        .init();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(cli(command, &dir))
}

async fn cli(command: Command, dir: &Path) -> Result<()> {
    match command {
        Command::Pair { uri, name } => {
            let name = name.unwrap_or_else(Config::default_name);
            agent::pair(dir, &uri, name.clone()).await?;
            println!("Paired as \"{name}\". Start streaming with `waystone-host run`.");
            Ok(())
        }
        Command::Run { no_lock, no_mute } => {
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
        Command::Status => status(dir),
        Command::CaptureTest { seconds } => capture_test(seconds).await,
        #[cfg(windows)]
        Command::Tray => unreachable!("handled before the runtime starts"),
    }
}

fn load(dir: &Path) -> Result<Config> {
    Config::load(&dir.join("config.json"))
        .context("this computer is not paired yet; run `waystone-host pair <link>` first")
}

fn status(dir: &Path) -> Result<()> {
    let config = load(dir)?;
    let key = DeviceKey::load_or_create(&dir.join("device.key"))?;
    println!("device id    {}", key.info(&config.name).id);
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
