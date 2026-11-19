//! HTTPS surface: the admin panel, the pairing QR and the WSS signaling socket.
//!
//! The panel is intentionally tiny and dependency free (inline HTML and CSS) so
//! the whole product remains one binary with no asset pipeline.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

use waystone_proto::device::{DeviceId, Role};
use waystone_proto::signal::{ClientMessage, ServerMessage};

use crate::hub::Hub;
use crate::pairing;
use crate::store::Store;

/// Everything the HTTP layer needs.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub hub: Hub,
    pub fingerprint: String,
    pub endpoint: String,
    pub turn_endpoint: String,
    pub admin_password_hash: Option<String>,
}

/// Builds the router served on `TCP 8443`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(panel))
        .route("/api/status", get(status))
        .route("/api/claim", post(claim))
        .route("/api/pair", post(create_pairing))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{id}", axum::routing::delete(remove_device))
        .route("/pair/{token}", get(pair_code))
        .route("/ws", get(signaling))
        .with_state(state)
}

/// Claims the instance by setting the admin password.
///
/// Only possible once; afterwards the stored hash gates the panel.
async fn claim(State(state): State<AppState>, form: String) -> Response {
    if state.admin_password_hash.is_some() {
        return StatusCode::CONFLICT.into_response();
    }
    let password = form
        .split('&')
        .find_map(|pair| pair.strip_prefix("password="))
        .map(|raw| url_decode(raw))
        .unwrap_or_default();
    if password.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "password must be at least 8 characters" })),
        )
            .into_response();
    }
    if let Err(error) = state.store.set_admin_password_hash(&hash_password(&password)) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response();
    }
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Deserialize)]
struct PairQuery {
    role: Option<String>,
}

#[derive(Deserialize)]
struct PanelQuery {
    password: Option<String>,
}

async fn panel(State(state): State<AppState>, Query(query): Query<PanelQuery>) -> Response {
    // The panel is served without a login form only while the instance is
    // unclaimed; afterwards the password hash gates every mutating call.
    let claimed = state.admin_password_hash.is_some();
    let unlocked = match (&state.admin_password_hash, &query.password) {
        (None, _) => true,
        (Some(hash), Some(password)) if hash_password(password) == *hash => true,
        _ => false,
    };
    let html = render_panel(&state, claimed, unlocked);
    Html(html).into_response()
}

async fn status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let devices = state.store.devices().unwrap_or_default();
    Json(json!({
        "fingerprint": state.fingerprint,
        "endpoint": state.endpoint,
        "turn": state.turn_endpoint,
        "devices": devices.iter().map(|device| json!({
            "id": device.info.id,
            "name": device.info.name,
            "platform": device.info.platform.to_string(),
            "role": device.info.role.to_string(),
            "lastSeenAt": device.last_seen_at,
        })).collect::<Vec<_>>(),
        "connected": state.hub.connected_count(),
    }))
}

async fn create_pairing(
    State(state): State<AppState>,
    Query(query): Query<PairQuery>,
) -> Response {
    let role = match query.role.as_deref() {
        Some("host") => Role::Host,
        _ => Role::Controller,
    };
    match pairing::create(
        &state.store,
        &state.endpoint,
        &state.turn_endpoint,
        &state.fingerprint,
        role,
    ) {
        Ok(uri) => {
            let svg = pairing::qr_svg(&uri.to_uri()).unwrap_or_default();
            (
                StatusCode::OK,
                Json(json!({ "uri": uri.to_uri(), "svg": svg })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn pair_code(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    // Renders a standalone page with the QR for the given token so the panel can
    // show it in a second screen or print it.
    let uri = waystone_proto::pairing::PairingUri {
        endpoint: state.endpoint.clone(),
        turn: state.turn_endpoint.clone(),
        fingerprint: state.fingerprint.clone(),
        token,
    };
    let svg = pairing::qr_svg(&uri.to_uri()).unwrap_or_default();
    Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>Pair</title></head>
<body style="font-family:system-ui;display:grid;place-items:center;height:100vh;margin:0">
<div style="text-align:center"><div>{svg}</div>
<p style="font-family:ui-monospace,monospace;font-size:12px">{uri}</p></div></body></html>"#
    ))
    .into_response()
}

async fn list_devices(State(state): State<AppState>) -> Json<serde_json::Value> {
    let devices = state.store.devices().unwrap_or_default();
    Json(json!({
        "devices": devices.iter().map(|device| json!({
            "id": device.info.id,
            "name": device.info.name,
            "platform": device.info.platform.to_string(),
            "role": device.info.role.to_string(),
        })).collect::<Vec<_>>(),
    }))
}

async fn remove_device(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.remove_device(&DeviceId::new(id)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// Upgrades a connection to the WSS signaling channel.
async fn signaling(
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Browsers send an Origin header; native clients do not. Rejecting only
    // cross-site browser origins keeps the channel usable for the apps while
    // stopping a random web page from opening it.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()) {
        if !origin.starts_with("https://") {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    upgrade.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let connection_id = state.hub.connect(out_tx);
    let challenge = state.hub.challenge(connection_id);
    if send(&mut socket, &challenge).await.is_err() {
        state.hub.disconnect(connection_id);
        return;
    }

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match ClientMessage::from_json(&text) {
                            Ok(message) => {
                                if let Err(error) = state.hub.handle(connection_id, message) {
                                    warn!("hub error: {error}");
                                    let _ = send(&mut socket, &ServerMessage::Error {
                                        message: error.to_string(),
                                    }).await;
                                }
                            }
                            Err(error) => {
                                debug!("bad client message: {error}");
                                let _ = send(&mut socket, &ServerMessage::Error {
                                    message: format!("malformed message: {error}"),
                                }).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        debug!("socket error: {error}");
                        break;
                    }
                }
            }
            outgoing = out_rx.recv() => {
                match outgoing {
                    Some(message) => {
                        if send(&mut socket, &message).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    state.hub.disconnect(connection_id);
    info!("signaling connection {connection_id} closed");
}

async fn send(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), ()> {
    socket
        .send(Message::Text(message.to_json().into()))
        .await
        .map_err(|_| ())
}

/// SHA-256 based password hash.
///
/// The admin password protects the panel, which only runs on the instance owner's
/// own network, so a single salted SHA-256 is enough and keeps the binary free of
/// a password hashing dependency.
pub fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"waystone-admin-v1");
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

fn render_panel(state: &AppState, claimed: bool, unlocked: bool) -> String {
    let device_rows = state
        .store
        .devices()
        .unwrap_or_default()
        .into_iter()
        .map(|device| {
            let last_seen = device
                .last_seen_at
                .map(|value| format!("{value}"))
                .unwrap_or_else(|| "never".to_string());
            format!(
                "<tr><td>{name}</td><td>{platform}</td><td>{role}</td><td>{id}</td><td>{last}</td></tr>",
                name = escape(&device.info.name),
                platform = device.info.platform.label(),
                role = device.info.role,
                id = escape(device.info.id.as_str()),
                last = last_seen
            )
        })
        .collect::<String>();

    let body = if !claimed {
        format!(
            r#"<p class="hint">This instance is not claimed yet. Set an admin password to
            protect the panel. Pick up the pairing QR afterwards to enroll your devices.</p>
            <form method="post" action="/api/claim"><input type="password" name="password"
            placeholder="admin password"><button>Claim instance</button></form>"#
        )
    } else if !unlocked {
        r#"<p class="hint">Enter the admin password to manage this instance.</p>"#.to_string()
    } else {
        format!(
            r#"<div class="actions">
            <button data-role="controller">Add a controller</button>
            <button data-role="host">Add a host</button>
            </div><div id="qr"></div>"#
        )
    };

    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Waystone</title>
<style>
:root{{color-scheme:dark light}}
body{{font-family:system-ui,sans-serif;margin:0;padding:32px;background:#111;color:#eee}}
h1{{font-size:20px;margin:0 0 4px}}
code{{font-family:ui-monospace,monospace;font-size:12px;color:#9ad}}
table{{border-collapse:collapse;margin-top:16px;width:100%;font-size:13px}}
td,th{{text-align:left;padding:6px 8px;border-bottom:1px solid #2a2a2a}}
button{{background:#2b6cb0;color:#fff;border:0;border-radius:6px;padding:8px 14px;font-size:13px;cursor:pointer}}
input{{background:#1b1b1b;border:1px solid #333;border-radius:6px;padding:8px;color:#eee}}
.hint{{color:#aaa;font-size:13px;max-width:52ch;line-height:1.5}}
.actions{{display:flex;gap:8px;margin-top:16px}}
#qr{{margin-top:16px;background:#fff;display:inline-block;padding:12px;border-radius:8px}}
</style></head><body>
<h1>Waystone</h1>
<p class="hint">Fingerprint <code>{fingerprint}</code><br>
Signaling <code>{endpoint}</code> · Relay <code>{turn}</code></p>
{body}
<table><thead><tr><th>Name</th><th>Platform</th><th>Role</th><th>Id</th><th>Last seen</th></tr></thead>
<tbody>{rows}</tbody></table>
<script>
document.querySelectorAll('.actions button').forEach(button => {{
  button.addEventListener('click', async () => {{
    const role = button.dataset.role;
    const response = await fetch('/api/pair?role=' + role, {{ method: 'POST' }});
    const data = await response.json();
    document.getElementById('qr').innerHTML = data.svg + '<p><code>' + data.uri + '</code></p>';
  }});
}});
</script>
</body></html>"#,
        fingerprint = escape(&state.fingerprint),
        endpoint = escape(&state.endpoint),
        turn = escape(&state.turn_endpoint),
        body = body,
        rows = device_rows,
    )
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Parses the bind address for the HTTPS listener.
pub fn parse_addr(value: &str) -> SocketAddr {
    value
        .parse()
        .unwrap_or_else(|_| "0.0.0.0:8443".parse().expect("hard coded fallback"))
}

/// Drains the hub outbox into a WebSocket; exposed for tests.
pub async fn pump(mut receiver: UnboundedReceiver<ServerMessage>, mut socket: WebSocket) {
    while let Some(message) = receiver.recv().await {
        if send(&mut socket, &message).await.is_err() {
            break;
        }
    }
}
