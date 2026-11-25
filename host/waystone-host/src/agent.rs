//! The long-running agent: stay online, serve one session at a time.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use tracing::{info, warn};

use waystone_proto::identity::{Challenge, Identity};
use waystone_proto::signal::{ByeReason, ClientMessage, ServerMessage, TurnCredentials};

use crate::config::{Config, DeviceKey};
use crate::platform;
use crate::session::{Outcome, Request, Session};
use crate::signal::Signal;

/// Reconnects forever with capped backoff; returns only on a fatal error
/// (the server no longer knows this machine).
pub async fn run(config: Config, key: Arc<DeviceKey>) -> Result<()> {
    if !platform::input_permitted(true) {
        warn!("{}", platform::INPUT_PERMISSION_HINT);
    }
    let mut failures = 0u32;
    loop {
        match serve(&config, &key).await {
            Ok(()) => failures = 0,
            Err(error) if error.to_string().contains("not paired") => return Err(error),
            Err(error) => {
                failures += 1;
                warn!("signaling: {error:#}");
            }
        }
        let delay = Duration::from_secs(1 << failures.min(5));
        info!("reconnecting in {delay:?}");
        tokio::time::sleep(delay).await;
    }
}

/// One signaling connection, and the session it may carry.
async fn serve(config: &Config, key: &DeviceKey) -> Result<()> {
    let (mut signal, challenge) = Signal::connect(&config.endpoint, &config.fingerprint).await?;
    let turn = authenticate(&mut signal, key, &config.name, challenge).await?;
    info!("online as {}", config.name);

    let mut session: Option<Session> = None;
    let result = loop {
        tokio::select! {
            message = signal.next_message() => match message {
                Ok(message) => if let Err(error) = handle(message, config, turn.as_ref(), &mut signal, &mut session).await {
                    break Err(error);
                },
                Err(error) => break Err(error),
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
                    }
                }
            }
        }
    };
    // Without signaling the controller is gone too: the server ends the
    // session on its side, so roll back as for a network timeout.
    finish(
        session.take(),
        config.locks_after(ByeReason::NetworkTimeout),
    );
    result
}

async fn handle(
    message: ServerMessage,
    config: &Config,
    turn: Option<&TurnCredentials>,
    signal: &mut Signal,
    session: &mut Option<Session>,
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
            let request = Request {
                id: session_id.clone(),
                controller: peer.name,
                viewport,
                audio,
                codec: video_codec,
            };
            match Session::start(request, &config.session_options(), Some(&config.turn), turn).await
            {
                Ok(started) => *session = Some(started),
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
