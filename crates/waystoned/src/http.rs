//! HTTPS surface: the admin panel, the admin API and the WSS signaling socket.
//!
//! Access rules:
//!
//! * `/`, `/claim`, `/login`, `/logout` and `/panel.js` are public; the panel
//!   only shows the claim or login form until the visitor holds a session.
//! * Everything under `/api` and the `/pair/{token}` QR page require an admin
//!   session cookie (see [`crate::auth`]).
//! * Every state-changing request (any method but GET/HEAD/OPTIONS) must come
//!   from the panel's own origin: browsers send `Origin`, which has to match the
//!   request's host. Requests without `Origin` (curl, scripts) carry no
//!   ambient browser credentials, so they are let through to the normal auth.
//! * `/ws` is authenticated by device signatures, not cookies; browsers may only
//!   open it from the panel's own origin, native clients send no `Origin`.
//!
//! The panel is intentionally tiny and dependency free (inline HTML and CSS, one
//! script route) so the whole product remains one binary with no asset pipeline.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use waystone_proto::device::{DeviceId, Role};
use waystone_proto::signal::{ClientMessage, ServerMessage};

use crate::auth::{self, AdminAuth, ClaimError};
use crate::hub::{Connection, ConnectionState, Hub, OUTBOX_CAPACITY};
use crate::pairing;
use crate::store::Store;

/// Time a new socket has to authenticate (or pair, which extends it once).
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// Interval between WebSocket pings, which also measure the RTT.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);
/// A socket that sent nothing (not even a pong) for this long is dropped.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// Largest signaling message accepted; SDP offers are a few KiB.
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024;
/// A socket write that takes longer than this means the peer stopped reading.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything the HTTP layer needs.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub hub: Hub,
    pub auth: Arc<AdminAuth>,
    pub fingerprint: String,
    pub endpoint: String,
    pub turn_endpoint: String,
}

/// Builds the router served on `TCP 8443`.
pub fn router(state: AppState) -> Router {
    let admin = Router::new()
        .route("/api/status", get(status))
        .route("/api/pair", post(create_pairing))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{id}", delete(remove_device))
        .route("/pair/{token}", get(pair_code))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));

    Router::new()
        .route("/", get(panel))
        .route("/panel.js", get(panel_script))
        .route("/claim", post(claim))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/ws", get(signaling))
        .merge(admin)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            same_origin_writes,
        ))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

// ================================ middleware ================================

/// Rejects requests to admin routes that carry no valid session cookie.
async fn require_admin(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let authorized =
        auth::session_token(request.headers()).is_some_and(|token| state.auth.validate(&token));
    if !authorized {
        return json_error(StatusCode::UNAUTHORIZED, "admin login required");
    }
    next.run(request).await
}

/// CSRF guard: state-changing requests must not come from another site.
async fn same_origin_writes(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if !safe && !origin_allowed(request.headers(), request.uri(), &state.endpoint) {
        warn!(
            "refused cross-origin {} {}",
            request.method(),
            request.uri()
        );
        return json_error(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    next.run(request).await
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; \
             img-src 'self' data:; connect-src 'self'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CACHE_CONTROL, "no-store"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// True when a request may act on this server from a browser's point of view.
///
/// Without `Origin` the request did not come from a web page (native app,
/// curl), unless `Sec-Fetch-Site` says otherwise. With `Origin`, it must be the
/// server's own origin: either the host the request was sent to or the
/// configured public endpoint.
pub fn origin_allowed(headers: &HeaderMap, uri: &Uri, endpoint: &str) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return headers
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|site| site == "same-origin" || site == "none");
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let authority = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| uri.authority().map(|authority| authority.as_str()));
    let matches = |host: &str| origin.eq_ignore_ascii_case(&format!("https://{host}"));
    authority.is_some_and(matches) || matches(endpoint)
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

// ================================ panel ================================

#[derive(Deserialize)]
struct PasswordForm {
    password: String,
}

async fn panel(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.is_claimed() {
        return Html(page("Claim", CLAIM_FORM)).into_response();
    }
    let logged_in = auth::session_token(&headers).is_some_and(|token| state.auth.validate(&token));
    if !logged_in {
        return Html(page("Login", LOGIN_FORM)).into_response();
    }
    Html(page("Waystone", &render_dashboard(&state))).into_response()
}

async fn panel_script() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        PANEL_SCRIPT,
    )
        .into_response()
}

/// Claims the instance by setting the admin password, then logs in.
async fn claim(State(state): State<AppState>, Form(form): Form<PasswordForm>) -> Response {
    match state.auth.claim(form.password).await {
        Ok(token) => {
            info!("instance claimed");
            logged_in_redirect(&token)
        }
        Err(ClaimError::AlreadyClaimed) => message_page(
            StatusCode::CONFLICT,
            "This instance is already claimed. Log in instead.",
        ),
        Err(ClaimError::InvalidPassword(reason)) => message_page(StatusCode::BAD_REQUEST, reason),
        Err(ClaimError::Internal(error)) => {
            warn!("claim failed: {error:#}");
            message_page(StatusCode::INTERNAL_SERVER_ERROR, "Claiming failed.")
        }
    }
}

async fn login(State(state): State<AppState>, Form(form): Form<PasswordForm>) -> Response {
    match state.auth.login(form.password).await {
        Some(token) => logged_in_redirect(&token),
        None => {
            warn!("failed admin login");
            message_page(StatusCode::UNAUTHORIZED, "Wrong password.")
        }
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = auth::session_token(&headers) {
        state.auth.logout(&token);
    }
    (
        [(header::SET_COOKIE, auth::clear_session_cookie())],
        Redirect::to("/"),
    )
        .into_response()
}

fn logged_in_redirect(token: &str) -> Response {
    (
        [(header::SET_COOKIE, auth::session_cookie(token))],
        Redirect::to("/"),
    )
        .into_response()
}

fn message_page(status: StatusCode, message: &str) -> Response {
    let body = format!(
        r#"<p class="hint">{}</p><p><a href="/">Back</a></p>"#,
        escape(message)
    );
    (status, Html(page("Waystone", &body))).into_response()
}

// ================================ admin API ================================

async fn status(State(state): State<AppState>) -> Response {
    let devices = match device_rows(&state) {
        Ok(devices) => devices,
        Err(response) => return *response,
    };
    Json(json!({
        "fingerprint": state.fingerprint,
        "endpoint": state.endpoint,
        "turn": state.turn_endpoint,
        "devices": devices,
        "connected": state.hub.connected_count(),
    }))
    .into_response()
}

async fn list_devices(State(state): State<AppState>) -> Response {
    match device_rows(&state) {
        Ok(devices) => Json(json!({ "devices": devices })).into_response(),
        Err(response) => *response,
    }
}

fn device_rows(state: &AppState) -> Result<Vec<serde_json::Value>, Box<Response>> {
    let devices = state.store.devices().map_err(|error| {
        warn!("listing devices failed: {error:#}");
        Box::new(json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "listing devices failed",
        ))
    })?;
    let presence = state.hub.presence();
    Ok(devices
        .iter()
        .map(|device| {
            let live = presence.get(&device.info.id);
            json!({
                "id": device.info.id,
                "name": device.info.name,
                "platform": device.info.platform.to_string(),
                "role": device.info.role.to_string(),
                "lastSeenAt": device.last_seen_at,
                "online": live.is_some(),
                "busy": live.is_some_and(|live| live.busy),
                "rttMs": live.and_then(|live| live.rtt_ms),
            })
        })
        .collect())
}

#[derive(Deserialize)]
struct PairBody {
    role: Role,
}

async fn create_pairing(State(state): State<AppState>, Json(body): Json<PairBody>) -> Response {
    match pairing::create(
        &state.store,
        &state.endpoint,
        &state.turn_endpoint,
        &state.fingerprint,
        body.role,
    ) {
        Ok(uri) => {
            let uri = uri.to_uri();
            let svg = pairing::qr_svg(&uri).unwrap_or_default();
            Json(json!({ "uri": uri, "svg": svg })).into_response()
        }
        Err(error) => {
            warn!("creating a pairing token failed: {error:#}");
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "creating a pairing token failed",
            )
        }
    }
}

/// Standalone QR page for a token, for printing or a second screen.
async fn pair_code(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let well_formed = token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !well_formed {
        return message_page(StatusCode::BAD_REQUEST, "Malformed pairing token.");
    }
    let uri = waystone_proto::pairing::PairingUri {
        endpoint: state.endpoint.clone(),
        turn: state.turn_endpoint.clone(),
        fingerprint: state.fingerprint.clone(),
        token,
    }
    .to_uri();
    let svg = pairing::qr_svg(&uri).unwrap_or_default();
    let body = format!(
        r#"<div class="qr">{svg}</div><p><code>{}</code></p>"#,
        escape(&uri)
    );
    Html(page("Pair", &body)).into_response()
}

async fn remove_device(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.hub.revoke(&DeviceId::new(id)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            warn!("revoking a device failed: {error:#}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "revoking failed")
        }
    }
}

// ================================ signaling ================================

/// Upgrades a connection to the WSS signaling channel.
async fn signaling(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // Origin first, so a foreign page learns nothing beyond "forbidden".
    if !origin_allowed(&headers, &uri, &state.endpoint) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade
        .max_message_size(MAX_MESSAGE_SIZE)
        .max_frame_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, state.hub))
}

async fn handle_socket(mut socket: WebSocket, hub: Hub) {
    let (outbox, mut queue) = mpsc::channel::<ServerMessage>(OUTBOX_CAPACITY);
    let kick = CancellationToken::new();
    let connection_id = hub.connect(Connection {
        outbox,
        kick: kick.clone(),
    });
    let reason = run_socket(&mut socket, &hub, connection_id, &kick, &mut queue).await;
    hub.disconnect(connection_id);
    debug!("signaling connection {connection_id} closed: {reason}");
    let _ = tokio::time::timeout(
        SEND_TIMEOUT,
        socket.send(Message::Close(Some(CloseFrame {
            code: close_code::POLICY,
            reason: reason.into(),
        }))),
    )
    .await;
}

/// Drives one socket until it should close; returns the close reason.
async fn run_socket(
    socket: &mut WebSocket,
    hub: &Hub,
    connection_id: u64,
    kick: &CancellationToken,
    queue: &mut mpsc::Receiver<ServerMessage>,
) -> &'static str {
    if send(socket, &hub.challenge(connection_id)).await.is_err() {
        return "send failed";
    }

    let auth_deadline = tokio::time::sleep(AUTH_TIMEOUT);
    tokio::pin!(auth_deadline);
    let mut authenticated = false;
    let mut extended = false;

    let mut ping = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    let mut last_received = Instant::now();
    let mut ping_sequence: u64 = 0;
    let mut outstanding: Option<(u64, Instant)> = None;

    loop {
        tokio::select! {
            _ = kick.cancelled() => {
                // Flush what the hub queued before kicking (e.g. AuthRejected).
                while let Ok(message) = queue.try_recv() {
                    if send(socket, &message).await.is_err() {
                        break;
                    }
                }
                return "closed by the server";
            }
            _ = &mut auth_deadline, if !authenticated => {
                match hub.connection_state(connection_id) {
                    ConnectionState::Authenticated => authenticated = true,
                    ConnectionState::Pending { paired: true } if !extended => {
                        // Pairing succeeded; give the same socket time to log in.
                        extended = true;
                        auth_deadline.as_mut().reset(Instant::now() + AUTH_TIMEOUT);
                    }
                    _ => {
                        let _ = send(socket, &ServerMessage::Error {
                            message: "authentication timed out".into(),
                        }).await;
                        return "authentication timed out";
                    }
                }
            }
            _ = ping.tick() => {
                if last_received.elapsed() > IDLE_TIMEOUT {
                    return "idle timeout";
                }
                ping_sequence += 1;
                outstanding = Some((ping_sequence, Instant::now()));
                let payload = ping_sequence.to_be_bytes().to_vec();
                if send_frame(socket, Message::Ping(payload.into())).await.is_err() {
                    return "send failed";
                }
            }
            incoming = socket.recv() => {
                last_received = Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let reply = match ClientMessage::from_json(&text) {
                            Ok(message) => dispatch(hub, connection_id, message).await,
                            Err(error) => {
                                debug!("bad client message: {error}");
                                Some(format!("malformed message: {error}"))
                            }
                        };
                        if let Some(message) = reply {
                            if send(socket, &ServerMessage::Error { message }).await.is_err() {
                                return "send failed";
                            }
                        }
                    }
                    Some(Ok(Message::Pong(payload))) => {
                        if let Some((sequence, sent_at)) = outstanding {
                            if payload.as_ref() == sequence.to_be_bytes() {
                                outstanding = None;
                                hub.record_rtt(connection_id, sent_at.elapsed());
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let _ = send(socket, &ServerMessage::Error {
                            message: "binary frames are not supported".into(),
                        }).await;
                    }
                    Some(Ok(Message::Ping(_))) => {}
                    Some(Ok(Message::Close(_))) | None => return "closed by the peer",
                    Some(Err(error)) => {
                        debug!("socket error: {error}");
                        return "socket error";
                    }
                }
            }
            outgoing = queue.recv() => {
                let Some(message) = outgoing else {
                    return "hub dropped the connection";
                };
                if send(socket, &message).await.is_err() {
                    return "send failed";
                }
            }
        }
    }
}

/// Runs one client message through the hub; returns an error text for the client.
async fn dispatch(hub: &Hub, connection_id: u64, message: ClientMessage) -> Option<String> {
    // Pairing and authentication touch SQLite; keep them off the async workers.
    let touches_disk = matches!(
        message,
        ClientMessage::PairRequest { .. } | ClientMessage::Auth { .. }
    );
    let result = if touches_disk {
        let hub = hub.clone();
        tokio::task::spawn_blocking(move || hub.handle(connection_id, message))
            .await
            .unwrap_or_else(|error| Err(anyhow::anyhow!("hub task failed: {error}")))
    } else {
        hub.handle(connection_id, message)
    };
    result.err().map(|error| {
        warn!("hub error on connection {connection_id}: {error:#}");
        "internal server error".to_string()
    })
}

async fn send(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), ()> {
    send_frame(socket, Message::Text(message.to_json().into())).await
}

async fn send_frame(socket: &mut WebSocket, frame: Message) -> Result<(), ()> {
    match tokio::time::timeout(SEND_TIMEOUT, socket.send(frame)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

// ================================ HTML ================================

const CLAIM_FORM: &str = r#"<p class="hint">This instance is not claimed yet. Choose an admin
password (at least 8 characters) to protect the panel, then add your devices.</p>
<form method="post" action="/claim"><input type="password" name="password" minlength="8"
autocomplete="new-password" placeholder="admin password" required>
<button>Claim instance</button></form>"#;

const LOGIN_FORM: &str = r#"<p class="hint">Enter the admin password to manage this instance.</p>
<form method="post" action="/login"><input type="password" name="password"
autocomplete="current-password" placeholder="admin password" required>
<button>Log in</button></form>"#;

const PANEL_SCRIPT: &str = r#"const qr = document.getElementById('qr');
document.querySelectorAll('[data-role]').forEach((button) => {
  button.addEventListener('click', async () => {
    const response = await fetch('/api/pair', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ role: button.dataset.role }),
    });
    if (!response.ok) { qr.textContent = 'Failed: HTTP ' + response.status; return; }
    const data = await response.json();
    qr.innerHTML = data.svg; // rendered by the server, contains no user input
    const line = document.createElement('p');
    const code = document.createElement('code');
    code.textContent = data.uri;
    line.append(code);
    qr.append(line);
  });
});
document.querySelectorAll('[data-revoke]').forEach((button) => {
  button.addEventListener('click', async () => {
    if (!confirm('Remove ' + button.dataset.name + ' from this instance?')) return;
    const id = encodeURIComponent(button.dataset.revoke);
    const response = await fetch('/api/devices/' + id, { method: 'DELETE' });
    if (response.ok || response.status === 404) location.reload();
    else alert('Failed: HTTP ' + response.status);
  });
});
"#;

fn render_dashboard(state: &AppState) -> String {
    let presence = state.hub.presence();
    let rows = state
        .store
        .devices()
        .unwrap_or_default()
        .into_iter()
        .map(|device| {
            let last_seen = device
                .last_seen_at
                .map(|value| value.to_string())
                .unwrap_or_else(|| "never".to_string());
            let live = match presence.get(&device.info.id) {
                Some(live) if live.busy => "in session",
                Some(_) => "online",
                None => "offline",
            };
            format!(
                "<tr><td>{name}</td><td>{platform}</td><td>{role}</td><td>{live}</td>\
                 <td><code>{id}</code></td><td>{last}</td>\
                 <td><button class=\"danger\" data-revoke=\"{id}\" data-name=\"{name}\">Remove</button></td></tr>",
                name = escape(&device.info.name),
                platform = escape(device.info.platform.label()),
                role = escape(&device.info.role.to_string()),
                id = escape(device.info.id.as_str()),
                last = escape(&last_seen),
            )
        })
        .collect::<String>();

    format!(
        r#"<p class="hint">Fingerprint <code>{fingerprint}</code><br>
Signaling <code>{endpoint}</code> · Relay <code>{turn}</code></p>
<div class="actions">
<button data-role="controller">Add a controller</button>
<button data-role="host">Add a host</button>
<form method="post" action="/logout"><button class="secondary">Log out</button></form>
</div>
<div id="qr" class="qr"></div>
<table><thead><tr><th>Name</th><th>Platform</th><th>Role</th><th>State</th><th>Id</th>
<th>Last seen</th><th></th></tr></thead>
<tbody>{rows}</tbody></table>
<script src="/panel.js"></script>"#,
        fingerprint = escape(&state.fingerprint),
        endpoint = escape(&state.endpoint),
        turn = escape(&state.turn_endpoint),
    )
}

/// Wraps `body` (already escaped HTML) in the panel chrome.
fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
:root{{color-scheme:dark light}}
body{{font-family:system-ui,sans-serif;margin:0;padding:32px;background:#111;color:#eee}}
h1{{font-size:20px;margin:0 0 4px}}
code{{font-family:ui-monospace,monospace;font-size:12px;color:#9ad;word-break:break-all}}
table{{border-collapse:collapse;margin-top:16px;width:100%;font-size:13px}}
td,th{{text-align:left;padding:6px 8px;border-bottom:1px solid #2a2a2a}}
button{{background:#2b6cb0;color:#fff;border:0;border-radius:6px;padding:8px 14px;font-size:13px;cursor:pointer}}
button.secondary{{background:#444}}
button.danger{{background:#9b2c2c;padding:4px 10px}}
input{{background:#1b1b1b;border:1px solid #333;border-radius:6px;padding:8px;color:#eee}}
.hint{{color:#aaa;font-size:13px;max-width:52ch;line-height:1.5}}
.actions{{display:flex;gap:8px;margin-top:16px}}
.actions form{{margin:0}}
.qr:not(:empty){{margin-top:16px;background:#fff;display:inline-block;padding:12px;border-radius:8px;max-width:420px}}
.qr code{{color:#123}}
</style></head><body>
<h1>Waystone</h1>
{body}
</body></html>"#,
        title = escape(title),
    )
}

/// Escapes text for HTML element content and quoted attribute values.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn origin_must_be_the_servers_own() {
        let uri: Uri = "/ws".parse().unwrap();
        let endpoint = "192.168.1.10:8443";
        // Native clients send no Origin.
        assert!(origin_allowed(&headers(&[]), &uri, endpoint));
        // Same origin as the Host header, or the advertised endpoint.
        assert!(origin_allowed(
            &headers(&[("origin", "https://nas.lan:8443"), ("host", "nas.lan:8443")]),
            &uri,
            endpoint
        ));
        assert!(origin_allowed(
            &headers(&[("origin", "https://192.168.1.10:8443"), ("host", "other")]),
            &uri,
            endpoint
        ));
        // Any other https page is refused, as are http and opaque origins.
        for origin in [
            "https://evil.example",
            "http://nas.lan:8443",
            "null",
            "https://nas.lan:8443.evil.example",
        ] {
            assert!(
                !origin_allowed(
                    &headers(&[("origin", origin), ("host", "nas.lan:8443")]),
                    &uri,
                    endpoint
                ),
                "{origin} must be refused"
            );
        }
        // A cross-site fetch that stripped Origin is still refused.
        assert!(!origin_allowed(
            &headers(&[("sec-fetch-site", "cross-site")]),
            &uri,
            endpoint
        ));
    }

    #[test]
    fn escape_covers_attribute_breakers() {
        assert_eq!(
            escape(r#"<a href='x'>"&"#),
            "&lt;a href=&#39;x&#39;&gt;&quot;&amp;"
        );
    }
}
