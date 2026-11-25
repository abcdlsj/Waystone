//! The long-running agent: stay online, serve one session at a time.
//!
//! A front end (the CLI, or the Windows tray) watches it through
//! [`Status`] reports and steers it with [`Command`]s.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};

use waystone_proto::pairing::PairingUri;

use waystone_proto::identity::{Challenge, Identity};
use waystone_proto::signal::{ByeReason, ClientMessage, ServerMessage, TurnCredentials};

use crate::config::{Config, DeviceKey};
use crate::platform;
use crate::session::{Outcome, Request, Session, SessionMode};
use crate::signal::Signal;

/// What the agent is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Online,
    Streaming {
        controller: String,
    },
    /// Waiting to retry after `reason`.
    Offline {
        reason: String,
    },
}

/// Requests from the front end.
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Command {
    /// Ends the current session, if any, without locking the screen.
    EndSession,
    /// Settings for the sessions that start from now on.
    Settings {
        lock_on_disconnect: bool,
        mute_speakers: bool,
    },
    /// Gracefully ends any active session (running rollback) and exits `run`.
    Shutdown,
}

pub type Reporter = Arc<dyn Fn(Status) + Send + Sync>;

/// Reconnects forever with capped backoff in direct in-process mode; returns
/// only on a fatal error (the server no longer knows this machine) or shutdown.
pub async fn run(
    config: Config,
    key: Arc<DeviceKey>,
    report: Reporter,
    commands: UnboundedReceiver<Command>,
) -> Result<()> {
    run_with_mode(config, key, SessionMode::Direct, report, commands).await
}

/// Reconnects forever with capped backoff using `mode` to access the desktop.
pub async fn run_with_mode(
    mut config: Config,
    key: Arc<DeviceKey>,
    mode: SessionMode,
    report: Reporter,
    mut commands: UnboundedReceiver<Command>,
) -> Result<()> {
    if matches!(mode, SessionMode::Direct) && !platform::input_permitted(true) {
        warn!("{}", platform::INPUT_PERMISSION_HINT);
    }
    let mut failures = 0u32;
    loop {
        report(Status::Connecting);
        match serve(&mut config, &key, &mode, &report, &mut commands).await {
            Ok(ServeExit::Shutdown) => return Ok(()),
            Ok(ServeExit::Reconnect) => failures = 0,
            Err(error) if error.to_string().contains("not paired") => return Err(error),
            Err(error) => {
                failures += 1;
                warn!("signaling: {error:#}");
                report(Status::Offline {
                    reason: format!("{error:#}"),
                });
            }
        }
        let delay = Duration::from_secs(1 << failures.min(5));
        info!("reconnecting in {delay:?}");
        let wake = tokio::time::sleep(delay);
        tokio::pin!(wake);
        loop {
            tokio::select! {
                _ = &mut wake => break,
                Some(command) = commands.recv() => {
                    if matches!(command, Command::Shutdown) {
                        return Ok(());
                    }
                    apply(&mut config, command);
                }
            }
        }
    }
}

enum ServeExit {
    Reconnect,
    Shutdown,
}

/// Settings apply to the next session; ending a session needs one.
fn apply(config: &mut Config, command: Command) {
    if let Command::Settings {
        lock_on_disconnect,
        mute_speakers,
    } = command
    {
        config.lock_on_disconnect = lock_on_disconnect;
        config.mute_speakers = mute_speakers;
    }
}

/// Spends a host pairing link and returns the configuration to save.
pub async fn pair(dir: &Path, link: &str, name: String) -> Result<Config> {
    let uri: PairingUri = link
        .trim()
        .parse()
        .map_err(|error| anyhow::anyhow!("not a pairing link: {error}"))?;
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
    let config = Config::new(uri.endpoint, uri.turn, uri.fingerprint, name);
    config.save(&dir.join("config.json"))?;
    Ok(config)
}

/// One signaling connection, and the session it may carry.
async fn serve(
    config: &mut Config,
    key: &DeviceKey,
    mode: &SessionMode,
    report: &Reporter,
    commands: &mut UnboundedReceiver<Command>,
) -> Result<ServeExit> {
    let (mut signal, challenge) = Signal::connect(&config.endpoint, &config.fingerprint).await?;
    let turn = authenticate(&mut signal, key, &config.name, challenge).await?;
    info!("online as {}", config.name);
    report(Status::Online);

    let mut session: Option<Session> = None;
    let mut shutting_down = false;
    let result = loop {
        tokio::select! {
            message = signal.next_message() => match message {
                Ok(message) => if let Err(error) = handle(message, config, mode, turn.as_ref(), &mut signal, &mut session, report).await {
                    break Err(error);
                },
                Err(error) => break Err(error),
            },
            Some(command) = commands.recv() => match command {
                Command::EndSession => if let Some(current) = session.take() {
                    info!("ending session {} from this computer", current.id);
                    let _ = signal.send(&ClientMessage::SessionBye { session_id: current.id.clone(), reason: ByeReason::UserHangup }).await;
                    finish(Some(current), false);
                    report(Status::Online);
                },
                Command::Shutdown => {
                    shutting_down = true;
                    if let Some(current) = session.take() {
                        info!("ending session {} for service shutdown", current.id);
                        let _ = signal.send(&ClientMessage::SessionBye { session_id: current.id.clone(), reason: ByeReason::HostError }).await;
                        finish(Some(current), config.locks_after(ByeReason::NetworkTimeout));
                    }
                    break Ok(ServeExit::Shutdown);
                }
                other => apply(config, other),
            },
            outcome = async { session.as_mut().unwrap().next().await }, if session.is_some() => {
                let current = session.as_ref().unwrap();
                let id = current.id.clone();
                match outcome {
                    Outcome::Signal(rtc) => {
                        signal.send(&ClientMessage::RtcSignal { session_id: id, signal: rtc }).await?;
                    }
                    Outcome::End(reason) => {
                        info!("ending session {id}: {reason:?}");
                        let _ = signal.send(&ClientMessage::SessionBye { session_id: id, reason }).await;
                        finish(session.take(), config.locks_after(reason));
                        report(Status::Online);
                    }
                }
            }
        }
    };
    if !shutting_down {
        // Without signaling the controller is gone too: the server ends the
        // session on its side, so roll back as for a network timeout.
        finish(
            session.take(),
            config.locks_after(ByeReason::NetworkTimeout),
        );
    }
    result.map(|_| {
        if shutting_down {
            ServeExit::Shutdown
        } else {
            ServeExit::Reconnect
        }
    })
}

async fn handle(
    message: ServerMessage,
    config: &Config,
    mode: &SessionMode,
    turn: Option<&TurnCredentials>,
    signal: &mut Signal,
    session: &mut Option<Session>,
    report: &Reporter,
) -> Result<()> {
    match message {
        ServerMessage::SessionAccepted {
            session_id,
            peer,
            viewport,
            audio,
            video_codec,
            ..
        } => {
            // The server refuses a second controller while one is connected,
            // so a new session here means the old one is dead.
            if let Some(stale) = session.take() {
                warn!("replacing session {}", stale.id);
                finish(Some(stale), false);
            }
            let controller = peer.name;
            let request = Request {
                id: session_id.clone(),
                controller: controller.clone(),
                viewport,
                audio,
                codec: video_codec,
            };
            match Session::start(
                request,
                &config.session_options(),
                mode,
                Some(&config.turn),
                turn,
            )
            .await
            {
                Ok(started) => {
                    *session = Some(started);
                    report(Status::Streaming { controller });
                }
                Err(error) => {
                    warn!("cannot start session: {error:#}");
                    signal
                        .send(&ClientMessage::SessionBye {
                            session_id,
                            reason: ByeReason::HostError,
                        })
                        .await?;
                }
            }
        }
        ServerMessage::RtcSignal {
            session_id,
            signal: rtc,
        } => {
            if let Some(current) = session.as_mut().filter(|s| s.id == session_id) {
                current.on_signal(rtc);
            }
        }
        ServerMessage::SessionControl {
            session_id,
            control,
        } => {
            if let Some(current) = session.as_mut().filter(|s| s.id == session_id) {
                current.on_control(control);
            }
        }
        ServerMessage::SessionEnded {
            session_id, reason, ..
        } => {
            if session.as_ref().is_some_and(|s| s.id == session_id) {
                info!("session {session_id} ended by the controller: {reason:?}");
                finish(session.take(), config.locks_after(reason));
                report(Status::Online);
            }
        }
        ServerMessage::AuthRejected { reason } => {
            bail!("the server closed this connection: {reason}")
        }
        ServerMessage::Error { message } => warn!("server: {message}"),
        _ => {}
    }
    Ok(())
}

/// Rollback blocks briefly while capture stops; keep the runtime moving.
fn finish(session: Option<Session>, lock: bool) {
    if let Some(session) = session {
        tokio::task::block_in_place(|| session.finish(lock));
    }
}

async fn authenticate(
    signal: &mut Signal,
    key: &DeviceKey,
    name: &str,
    challenge: ServerMessage,
) -> Result<Option<TurnCredentials>> {
    let ServerMessage::Challenge {
        nonce, server_id, ..
    } = challenge
    else {
        bail!("the server did not open with a challenge");
    };
    let payload = Challenge { nonce, server_id }.signing_payload();
    let info = key.info(name);
    signal
        .send(&ClientMessage::Auth {
            identity: Identity {
                id: info.id,
                public_key: info.public_key,
            },
            signature: key.sign(&payload),
            protocol_version: waystone_proto::PROTOCOL_VERSION,
            name: name.to_string(),
        })
        .await?;
    loop {
        match signal.next_message().await? {
            ServerMessage::AuthOk { turn, .. } => return Ok(turn),
            ServerMessage::AuthRejected { reason } => bail!("authentication rejected: {reason}"),
            _ => {}
        }
    }
}
