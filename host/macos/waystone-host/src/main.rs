//! The macOS host agent.
//!
//! Responsibilities, in order of importance:
//!
//! 1. Keep a WSS connection to the signaling server so the phone always sees the
//!    machine as online.
//! 2. On a session, start capture, feed the engine and answer the SDP offer.
//! 3. Inject the controller's input frames into the local desktop.
//! 4. Tear everything down deterministically when the session ends.

mod capture;
mod config;
mod signal;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use waystone_proto::identity::Challenge;
use waystone_proto::pairing::PairingUri;
use waystone_proto::signal::{ByeReason, ClientMessage, RtcSignal, ServerMessage};

use waystone_rtc::engine::{sink, Engine, EngineConfig, Event, SessionOptions};

use crate::capture::{Capture, FrameBridge};
use crate::config::{state_dir, Config, DeviceKey};
use crate::signal::Signal;

/// macOS host agent for Waystone.
#[derive(Parser, Debug)]
#[command(name = "waystone-host", version, about)]
struct Args {
    /// Directory holding the device key and configuration.
    #[arg(long, env = "WAYSTONE_STATE_DIR")]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Enrolls this machine using a `waystone://pair?...` URI or token.
    Pair {
        /// Pairing URI printed as text or scanned from the panel.
        uri: String,
        /// Display name shown on the phone.
        #[arg(long)]
        name: Option<String>,
    },
    /// Connects to the server and waits for sessions.
    Run {
        /// Capture resolution, `WIDTHxHEIGHT`.
        #[arg(long, default_value = "1920x1080")]
        capture: String,
        /// Frame rate requested from the encoder.
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Video bitrate ceiling in bits per second.
        #[arg(long, default_value_t = 8_000_000)]
        bitrate: u32,
    },
    /// Prints the current identity and configuration.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystone_host=info,warn".into()),
        )
        .init();

    let args = Args::parse();
    let dir = args.state_dir.clone().unwrap_or_else(state_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;

    match args.command {
        Command::Pair { uri, name } => pair(dir, uri, name).await,
        Command::Run {
            capture,
            fps,
            bitrate,
        } => run(dir, capture, fps, bitrate).await,
        Command::Status => status(dir),
    }
}

/// Enrolls the machine and writes the configuration.
async fn pair(dir: PathBuf, uri_text: String, name: Option<String>) -> Result<()> {
    let uri: PairingUri = uri_text
        .trim()
        .parse()
        .context("the argument is not a waystone://pair URI")?;

    let name = name.unwrap_or_else(Config::default_name);
    let key = DeviceKey::load_or_create(&dir.join("device.key"))?;

    let (mut signal, challenge) = Signal::connect(&uri.endpoint, &uri.fingerprint).await?;
    let ServerMessage::Challenge {
        nonce, server_id, ..
    } = challenge
    else {
        anyhow::bail!("the server did not open with a challenge");
    };
    let _ = Challenge { nonce, server_id };

    info!("pairing with {}", uri.endpoint);
    signal
        .send(&ClientMessage::PairRequest {
            token: uri.token.clone(),
            device: key.info(&name),
        })
        .await?;

    match signal.next_message().await? {
        ServerMessage::PairAccepted { server_id } => {
            info!("paired with instance {server_id}");
        }
        ServerMessage::PairRejected { reason } => {
            anyhow::bail!("pairing was rejected: {reason}");
        }
        other => anyhow::bail!("unexpected reply to pairing: {other:?}"),
    }

    let config = Config {
        endpoint: uri.endpoint,
        turn: uri.turn,
        fingerprint: uri.fingerprint,
        name,
        max_bitrate: 8_000_000,
        fps: 60,
        capture: (1920, 1080),
    };
    config.save(&dir.join("config.json"))?;
    info!("configuration written to {}", dir.display());
    Ok(())
}

/// Connects and handles sessions until the process is stopped.
async fn run(dir: PathBuf, capture_spec: String, fps: u32, bitrate: u32) -> Result<()> {
    let capture_size = parse_size(&capture_spec)?;
    let mut config = Config::load(&dir.join("config.json"))
        .context("this machine is not paired yet; run `waystone-host pair <uri>` first")?;
    config.capture = capture_size;
    config.fps = fps;
    config.max_bitrate = bitrate;
    config.save(&dir.join("config.json"))?;

    let key = Arc::new(DeviceKey::load_or_create(&dir.join("device.key"))?);

    loop {
        match session_loop(&config, key.clone()).await {
            Ok(()) => info!("session loop ended, reconnecting"),
            Err(error) => warn!("signaling connection lost: {error}"),
        }
        sleep(Duration::from_secs(3)).await;
    }
}

fn status(dir: PathBuf) -> Result<()> {
    let config = Config::load(&dir.join("config.json"))?;
    let key = DeviceKey::load_or_create(&dir.join("device.key"))?;
    let info = key.info(&config.name);
    println!("device id   {}", info.id);
    println!("name        {}", info.name);
    println!("endpoint    {}", config.endpoint);
    println!("turn        {}", config.turn);
    println!("fingerprint {}", config.fingerprint);
    println!("capture     {}x{} @ {} fps", config.capture.0, config.capture.1, config.fps);
    Ok(())
}

/// One signaling connection lifetime, including every session it serves.
///
/// Signaling and engine events are handled in a single task: the engine handle
/// and the WebSocket both mutate shared state, and interleaving them in one place
/// keeps session teardown deterministic.
async fn session_loop(config: &Config, key: Arc<DeviceKey>) -> Result<()> {
    let (mut signal, challenge) = Signal::connect(&config.endpoint, &config.fingerprint).await?;
    let peers = authenticate(&mut signal, &key, &config.name, challenge).await?;
    info!("{} peer(s) in the pool", peers.len());

    let bridge = FrameBridge::new();
    let capture = Capture::new(bridge.clone());
    let mut active: Option<Active> = None;

    loop {
        // While a session is live the engine reports progress through its own
        // channel; otherwise only signaling traffic wakes this loop.
        let message = match active.as_mut() {
            Some(state) => tokio::select! {
                incoming = signal.next_message() => Some(incoming?),
                event = state.events.recv() => {
                    if let Some(event) = event {
                        handle_engine_event(&mut signal, &capture, &mut active, event).await?;
                    }
                    None
                }
            },
            None => Some(signal.next_message().await?),
        };
        let Some(message) = message else { continue };

        match message {
            ServerMessage::SessionAccepted {
                session_id,
                viewport,
                audio,
                peer,
                ..
            } => {
                info!("session {session_id} requested by {}", peer.name);
                if active.is_some() {
                    warn!("a session was already running, ignoring the new request");
                    continue;
                }
                let (width, height) = (config.capture.0, config.capture.1);
                if let Err(error) = capture.start(width, height, config.fps, config.max_bitrate) {
                    error!("capture failed: {error}");
                    signal
                        .send(&ClientMessage::SessionBye {
                            session_id,
                            reason: ByeReason::HostError,
                        })
                        .await?;
                    continue;
                }

                let (event_tx, event_rx) = unbounded_channel();
                let sink = sink(move |event: Event| {
                    let _ = event_tx.send(event);
                });
                // The relay credentials are allocated by the server per session;
                // until that is wired up the host only advertises direct paths.
                let mut engine_config = EngineConfig::new(waystone_rtc::Role::Host, sink);
                if let Some(viewport) = viewport {
                    engine_config = engine_config.with_session(SessionOptions {
                        viewport,
                        audio,
                        max_video_bitrate: config.max_bitrate,
                    });
                }

                match Engine::start(engine_config).await {
                    Ok(engine) => {
                        bridge.attach(engine.clone());
                        active = Some(Active {
                            session_id,
                            engine,
                            events: event_rx,
                        });
                    }
                    Err(error) => {
                        error!("cannot start the transport: {error}");
                        capture.stop();
                        signal
                            .send(&ClientMessage::SessionBye {
                                session_id,
                                reason: ByeReason::HostError,
                            })
                            .await?;
                    }
                }
            }
            ServerMessage::RtcSignal { session_id, signal: rtc } => {
                let Some(state) = active.as_mut() else { continue };
                if state.session_id != session_id {
                    continue;
                }
                match rtc {
                    RtcSignal::Offer { sdp } => state.engine.accept_offer(sdp),
                    RtcSignal::Ice { candidate, .. } => state.engine.add_remote_candidate(candidate),
                    RtcSignal::Answer { .. } | RtcSignal::IceComplete => {
                        debug!("ignoring a message only the controller sends")
                    }
                }
            }
            ServerMessage::SessionControl { control, .. } => match control {
                waystone_proto::signal::SessionControl::RequestKeyframe => capture.request_keyframe(),
                other => debug!("session control {other:?} is not implemented yet"),
            },
            ServerMessage::SessionEnded {
                session_id, reason, ..
            } => {
                info!("session {session_id} ended: {reason:?}");
                stop_active(&bridge, &capture, &mut active);
            }
            ServerMessage::Error { message } => warn!("server error: {message}"),
            ServerMessage::AuthRejected { reason } => {
                anyhow::bail!("authentication rejected: {reason}")
            }
            other => debug!("ignoring {other:?}"),
        }
    }
}

/// State of a live session.
struct Active {
    session_id: String,
    engine: Engine,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
}

async fn handle_engine_event(
    signal: &mut Signal,
    capture: &Capture,
    active: &mut Option<Active>,
    event: Event,
) -> Result<()> {
    let Some(state) = active.as_mut() else { return Ok(()) };
    match event {
        Event::LocalAnswer(sdp) => {
            // The answer goes back through the signaling server.
            signal
                .send(&ClientMessage::RtcSignal {
                    session_id: state.session_id.clone(),
                    signal: RtcSignal::Answer { sdp },
                })
                .await?;
        }
        Event::LocalCandidate(candidate) => {
            signal
                .send(&ClientMessage::RtcSignal {
                    session_id: state.session_id.clone(),
                    signal: RtcSignal::Ice {
                        candidate,
                        sdp_mid: None,
                        sdp_mline_index: None,
                    },
                })
                .await?;
        }
        Event::Input { data, .. } => {
            if !capture::inject_input(&data) {
                debug!("input frame rejected");
            }
        }
        Event::KeyframeRequest => capture.request_keyframe(),
        Event::PathUpdate {
            rtt_ms,
            relayed,
            estimated_bitrate,
        } => {
            if relayed {
                capture.set_bitrate(4_000_000);
            } else {
                capture.set_bitrate(estimated_bitrate.unwrap_or(8_000_000).max(1_000_000));
            }
            debug!("path update rtt={rtt_ms:?} relayed={relayed}");
        }
        Event::Closed(reason) => {
            info!("engine closed: {reason}");
            let session_id = state.session_id.clone();
            let _ = signal
                .send(&ClientMessage::SessionBye {
                    session_id,
                    reason: ByeReason::HostError,
                })
                .await;
            *active = None;
            capture.stop();
        }
        other => debug!("engine event: {other:?}"),
    }
    Ok(())
}

fn stop_active(bridge: &Arc<FrameBridge>, capture: &Capture, active: &mut Option<Active>) {
    capture.stop();
    bridge.detach();
    if let Some(state) = active.take() {
        state.engine.shutdown();
    }
}

async fn authenticate(
    signal: &mut Signal,
    key: &DeviceKey,
    name: &str,
    challenge: ServerMessage,
) -> Result<Vec<waystone_proto::signal::PeerState>> {
    let ServerMessage::Challenge {
        nonce, server_id, ..
    } = challenge
    else {
        anyhow::bail!("the server did not open with a challenge");
    };
    let challenge = Challenge { nonce, server_id };
    let signature = key.sign(&challenge.signing_payload());
    signal
        .send(&ClientMessage::Auth {
            identity: waystone_proto::identity::Identity {
                id: key.info(name).id,
                public_key: key.info_key.clone(),
            },
            signature,
            protocol_version: waystone_proto::PROTOCOL_VERSION,
            name: name.to_string(),
        })
        .await?;

    loop {
        match signal.next_message().await? {
            ServerMessage::AuthOk { peers, .. } => return Ok(peers),
            ServerMessage::AuthRejected { reason } => {
                anyhow::bail!("authentication rejected: {reason}")
            }
            other => debug!("ignoring {other:?} before auth completed"),
        }
    }
}

fn parse_size(value: &str) -> Result<(u32, u32)> {
    let (width, height) = value
        .split_once('x')
        .or_else(|| value.split_once('X'))
        .context("capture size must look like 1920x1080")?;
    Ok((width.trim().parse()?, height.trim().parse()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_capture_sizes() {
        assert_eq!(parse_size("1920x1080").unwrap(), (1920, 1080));
        assert_eq!(parse_size("2560X1440").unwrap(), (2560, 1440));
        assert!(parse_size("1920").is_err());
        assert!(parse_size("axb").is_err());
    }

    #[test]
    fn malformed_input_frames_are_rejected() {
        assert!(!capture::inject_input(&[]));
        assert!(!capture::inject_input(&[0x7f]));
    }
}
