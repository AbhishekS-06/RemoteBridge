//! The host page: a small web UI on the Mac for approving new devices and
//! managing paired ones. It replaces typing y/N into the host's terminal.
//!
//! Only the Mac itself may use it, because approving a device grants control
//! of the Mac:
//! - It listens on 127.0.0.1 only, so nothing on the network can reach it.
//! - Every API call must carry a random key made fresh on each run. The key
//!   travels in the URL's #fragment, which browsers never send to servers
//!   or put in Referer headers, and the page sends it back in a header.
//!   A website open in another tab can't learn the key, and can't set a
//!   custom header on a cross-origin request without a CORS preflight,
//!   which this server never approves. That also covers DNS rebinding.
//!
//! The page polls /api/state once a second. Approve and reject go to the
//! session loop in webrtc_host.rs, which holds the waiting answer; rename
//! and remove change the paired list directly.

use std::io::Read;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

use crate::pairing::{self, PairedDevices};
use crate::signaling::BoxError;

/// What the session loop and the page both see.
pub struct Shared {
    pub host_fingerprint: String,
    pub paired: PairedDevices,
    /// A new device waiting for approval.
    pub pending: Option<Pending>,
    /// The paired device of the current session and its connection.
    pub active: Option<(String, Arc<RTCPeerConnection>)>,
}

pub struct Pending {
    pub fingerprint: String,
    pub code: String,
}

/// Requests from the page that the session loop carries out. Each names the
/// device it is about, so a click meant for one device can't act on another
/// that replaced it in the meantime.
pub enum Command {
    Approve {
        fingerprint: String,
        name: String,
    },
    Reject {
        fingerprint: String,
    },
    /// Already removed from the list; close its session if it is connected.
    Disconnect {
        fingerprint: String,
    },
}

#[derive(Clone)]
struct AppState {
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::UnboundedSender<Command>,
    key: Arc<String>,
}

/// Starts the page server and returns its URL, key included.
pub async fn start(
    shared: Arc<Mutex<Shared>>,
    commands: mpsc::UnboundedSender<Command>,
) -> Result<String, BoxError> {
    let key = random_key()?;
    let state = AppState {
        shared,
        commands,
        key: Arc::new(key.clone()),
    };
    let app = Router::new()
        .route("/", get(page))
        .route("/api/state", get(get_state))
        .route("/api/approve", post(approve))
        .route("/api/reject", post(reject))
        .route("/api/rename", post(rename))
        .route("/api/remove", post(remove))
        .with_state(state);
    let port = std::env::var("REMOTEBRIDGE_ADMIN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8091u16);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            println!("host page stopped: {e}");
        }
    });
    Ok(format!("http://127.0.0.1:{port}/#key={key}"))
}

fn random_key() -> Result<String, BoxError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == state.key.as_str())
}

async fn page() -> Html<&'static str> {
    Html(include_str!("../admin/index.html"))
}

#[derive(Serialize)]
struct StateView {
    host_fingerprint: String,
    pending: Option<PendingView>,
    devices: Vec<DeviceView>,
}

#[derive(Serialize)]
struct PendingView {
    fingerprint: String,
    code: String,
}

#[derive(Serialize)]
struct DeviceView {
    fingerprint: String,
    name: String,
    paired_at: u64,
    last_seen: u64,
    connected: bool,
}

async fn get_state(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let shared = state.shared.lock().unwrap();
    let connected_fp = shared
        .active
        .as_ref()
        .filter(|(_, pc)| pc.connection_state() == RTCPeerConnectionState::Connected)
        .map(|(fp, _)| fp.as_str());
    Json(StateView {
        host_fingerprint: shared.host_fingerprint.clone(),
        pending: shared.pending.as_ref().map(|p| PendingView {
            fingerprint: p.fingerprint.clone(),
            code: p.code.clone(),
        }),
        devices: shared
            .paired
            .list()
            .iter()
            .map(|d| DeviceView {
                fingerprint: d.fingerprint.clone(),
                name: d.name.clone(),
                paired_at: d.paired_at,
                last_seen: d.last_seen,
                connected: connected_fp == Some(d.fingerprint.as_str()),
            })
            .collect(),
    })
    .into_response()
}

#[derive(Deserialize)]
struct DeviceRequest {
    fingerprint: String,
    #[serde(default)]
    name: String,
}

async fn approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DeviceRequest>,
) -> StatusCode {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    let _ = state.commands.send(Command::Approve {
        fingerprint: req.fingerprint,
        name: pairing::clean_name(&req.name),
    });
    StatusCode::NO_CONTENT
}

async fn reject(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DeviceRequest>,
) -> StatusCode {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    let _ = state.commands.send(Command::Reject {
        fingerprint: req.fingerprint,
    });
    StatusCode::NO_CONTENT
}

async fn rename(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DeviceRequest>,
) -> StatusCode {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    let name = pairing::clean_name(&req.name);
    match state
        .shared
        .lock()
        .unwrap()
        .paired
        .rename(&req.fingerprint, &name)
    {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            println!("rename failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DeviceRequest>,
) -> StatusCode {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED;
    }
    let removed = state.shared.lock().unwrap().paired.remove(&req.fingerprint);
    match removed {
        Ok(true) => {
            println!("device removed");
            let _ = state.commands.send(Command::Disconnect {
                fingerprint: req.fingerprint,
            });
            StatusCode::NO_CONTENT
        }
        Ok(false) => StatusCode::NOT_FOUND,
        Err(e) => {
            println!("remove failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
