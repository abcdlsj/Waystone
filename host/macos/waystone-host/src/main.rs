//! The macOS host agent: keeps this Mac online in the pool and streams it.
//!
//! * `agent` – signaling connection, reconnects, one session at a time.
//! * `session` – one stream: engine, capture, input, rollback.
//! * `capture`, `input`, `system` – the Swift media layer behind `ffi`.
//! * `plan`, `keymap` – pure decisions, unit tested.

mod agent;
mod capture;
mod config;
mod ffi;
mod input;
mod keymap;
mod plan;
mod session;
mod signal;
mod system;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;

use waystone_proto::pairing::PairingUri;
use waystone_proto::signal::{ClientMessage, ServerMessage};
use waystone_proto::VideoCodec;

use crate::config::{state_dir, Config, DeviceKey};
use crate::signal::Signal;

#[derive(Parser, Debug)]
#[command(
    name = "waystone-host",
    version,
    about = "Stream this Mac to your Waystone pool"
)]
struct Args {
    /// Directory holding the device key and configuration.
    #[arg(long, env = "WAYSTONE_STATE_DIR")]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystone_host=info,waystone_rtc=warn".into()),
        )
        .init();

    let args = Args::parse();
    let dir = args.state_dir.unwrap_or_else(state_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    match args.command {
        Command::Pair { uri, name } => pair(&dir, &uri, name).await,
        Command::Run { no_lock, no_mute } => {
            let mut config = load(&dir)?;
            if no_lock {
                config.lock_on_disconnect = false;
            }
            config.mute_speakers &= !no_mute;
            let key = Arc::new(DeviceKey::load_or_create(&dir.join("device.key"))?);
            agent::run(config, key).await
        }
        Command::Status => status(&dir),
        Command::CaptureTest { seconds } => capture_test(seconds).await,
    }
}

fn load(dir: &Path) -> Result<Config> {
    Config::load(&dir.join("config.json"))
        .context("this Mac is not paired yet; run `waystone-host pair <link>` first")
}

/// Spends a host pairing token and writes the configuration.
async fn pair(dir: &Path, link: &str, name: Option<String>) -> Result<()> {
    let uri: PairingUri = link
        .trim()
        .parse()
        .map_err(|error| anyhow::anyhow!("not a pairing link: {error}"))?;
    let name = name.unwrap_or_else(Config::default_name);
    let key = DeviceKey::load_or_create(&dir.join("device.key"))?;

    let (mut signal, _challenge) = Signal::connect(&uri.endpoint, &uri.fingerprint).await?;
    signal
        .send(&ClientMessage::PairRequest {
            token: uri.token.clone(),
            device: key.info(&name),
        })
        .await?;
    loop {
        match signal.next_message().await? {
            ServerMessage::PairAccepted { .. } => break,
            ServerMessage::PairRejected { reason } => bail!("pairing was rejected: {reason}"),
            _ => {}
        }
    }

    Config::new(uri.endpoint, uri.turn, uri.fingerprint, name.clone())
        .save(&dir.join("config.json"))?;
    println!("Paired as \"{name}\". Start streaming with `waystone-host run`.");
    Ok(())
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
        if system::input_permitted(false) {
            "allowed"
        } else {
            "needs Accessibility permission"
        }
    );
    Ok(())
}

/// Starts capture with no session attached and counts what comes out, so a
/// missing Screen Recording permission shows up before pairing a phone.
async fn capture_test(seconds: u64) -> Result<()> {
    let display = system::Display::main().context("no display")?;
    let plan = plan::plan(&display, None, 8_000_000);
    info!("capturing {}x{} for {seconds}s", plan.width, plan.height);
    let sink = capture::FrameSink::new(None);
    let capture = capture::Capture::start(display.id, &plan, VideoCodec::H265, true, sink).await?;
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let (video, audio) = capture.counts();
    drop(capture);
    println!("{video} video frames and {audio} audio packets in {seconds}s");
    if video == 0 {
        bail!("no frames: allow Screen Recording for this binary, then retry");
    }
    Ok(())
}
