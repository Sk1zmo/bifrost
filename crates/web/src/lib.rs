//! The web dashboard every Bifrost node serves.
//!
//! This is what makes Bifrost work on phones without an app store. A laptop
//! running `bifrost` serves a dashboard on port 9000; any device on the LAN
//! with a browser — an iPhone, an Android tablet, another laptop, a Linux box
//! with no desktop environment — opens it and gets the full interface. No
//! install, no pairing, no account.
//!
//! The page is one self-contained HTML file compiled into the binary, so the
//! node has no assets to lose and works with no internet connection.
//!
//! Live updates go over Server-Sent Events rather than WebSockets: the traffic
//! is almost entirely server-to-client, `EventSource` reconnects on its own
//! when a phone's Wi-Fi drops, and it needs no extra dependencies.
//!
//! ## Trust model
//!
//! Anyone who can reach the port can drive this node. That is deliberate and
//! matches the rest of Bifrost — a LAN-local tool for a network you already
//! trust, with no accounts anywhere. Two consequences are handled here rather
//! than assumed away: the download route refuses to serve anything outside the
//! download directory, and binding to `0.0.0.0` is a config setting
//! (`web_lan_accessible`) that can be turned off to restrict the dashboard to
//! localhost.

pub mod companion;
pub mod compute_api;

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::FromRef;
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::HeaderMap as RequestHeaders;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bifrost_core::events::NodeEvent;
use bifrost_core::Node;

use crate::companion::CompanionManager;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// What every handler is given: this machine's own node, plus the companion
/// devices it is hosting on behalf of phones.
#[derive(Clone, Debug)]
pub struct AppState {
    pub host: Node,
    pub companions: CompanionManager,
}

impl FromRef<AppState> for Node {
    fn from_ref(state: &AppState) -> Self {
        state.host.clone()
    }
}

impl AppState {
    pub fn new(host: Node) -> Self {
        Self {
            host,
            companions: CompanionManager::new(),
        }
    }

    /// Resolve which node a request acts on.
    ///
    /// `?as=<companion-id>` selects a phone's own device; anything else falls
    /// back to this machine's node. One place decides, so every endpoint
    /// behaves identically whether it is driven from the laptop or the phone.
    async fn node_for(&self, as_device: Option<&str>) -> Result<Node, ApiError> {
        match as_device.filter(|id| !id.is_empty()) {
            Some(id) => self.companions.node(id).await.ok_or_else(|| {
                // The browser holds an id we no longer have — it was pruned,
                // or the host restarted. Say so precisely so the page can
                // re-claim rather than silently acting as the laptop.
                ApiError {
                    status: StatusCode::GONE,
                    message: "this device is no longer registered; reload to rejoin".into(),
                }
            }),
            None => Ok(self.host.clone()),
        }
    }
}

/// `?as=<companion-id>` on any endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct AsDevice {
    #[serde(default, rename = "as")]
    pub as_device: Option<String>,
}

/// The dashboard, compiled in so the binary is self-contained.
const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");

/// Web app manifest, so a phone can add the dashboard to its home screen.
const MANIFEST_JSON: &str = include_str!("../assets/manifest.webmanifest");

/// Largest upload accepted (2 GiB), bounding memory per request.
const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Build the router.
pub fn router(node: Node) -> Router {
    router_with_state(AppState::new(node))
}

/// Build the router over an existing state, so the caller can keep hold of the
/// companion manager (to prune it, or shut it down cleanly).
pub fn router_with_state(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/manifest.webmanifest", get(manifest))
        .route("/health", get(health))
        .route("/api/snapshot", get(snapshot))
        .route("/api/scan", post(network_scan))
        .route("/api/events", get(events))
        .route("/api/command", post(command))
        .route("/api/chat", post(chat))
        .route("/api/alias", post(alias))
        .route("/api/sharing", post(sharing))
        .route("/api/rename", post(rename))
        .route("/api/call", post(call))
        .route("/api/hangup", post(hangup))
        .route("/api/ping", post(ping))
        .route("/api/upload", post(upload))
        .route("/api/downloads", get(list_downloads))
        .route("/api/download/:filename", get(serve_download))
        .route("/api/voice/frame", post(voice_send))
        .route("/api/voice/frames", get(voice_receive))
        // Companion devices: a phone claiming its own place on the network.
        .route("/api/companion", get(list_companions).post(claim_companion))
        .route("/api/companion/leave", post(release_companion))
        .merge(compute_api::routes())
        .with_state(state)
}

/// Serve the dashboard until `shutdown` fires.
pub async fn serve(
    bind: SocketAddr,
    node: Node,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind the dashboard to {bind}"))?;

    tracing::info!(%bind, "web dashboard listening");

    let state = AppState::new(node);

    // Retire companions whose browser has stopped polling, so a phone that
    // was closed days ago is not still advertised to the network.
    {
        let companions = state.companions.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(300));
            loop {
                ticker.tick().await;
                companions.prune_idle().await;
            }
        });
    }

    let companions = state.companions.clone();
    let result = axum::serve(listener, router_with_state(state))
        .with_graceful_shutdown(async move {
            let _ = shutdown.recv().await;
            tracing::info!("web dashboard shutting down");
        })
        .await
        .context("the dashboard server stopped");

    // Take every companion off the network rather than leaving peers to time
    // out devices that are definitely gone.
    companions.shutdown_all().await;
    result
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// An API error, rendered as JSON so the browser can show it verbatim.
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

// ─── Pages ───────────────────────────────────────────────────────────────────

async fn dashboard() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // The page is compiled in and changes only with the binary, but a
            // stale cached copy after an upgrade is confusing, so revalidate.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        DASHBOARD_HTML,
    )
}

async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        MANIFEST_JSON,
    )
}

/// Liveness probe, also used by `bifrost doctor` to find running nodes.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let node = state.host;
    Json(serde_json::json!({
        "status": "ok",
        "node_id": node.node_id(),
        "version": bifrost_core::VERSION,
        "protocol": bifrost_core::PROTOCOL_VERSION,
        "uptime_secs": node.uptime_secs(),
    }))
}

// ─── Reading state ───────────────────────────────────────────────────────────

/// Look for devices on this network, including ones not running Bifrost.
///
/// A POST because it is an action with a cost — it probes a subnet and shells
/// out to the platform — not a value that can be fetched twice for free.
async fn network_scan(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<ScanRequest>,
) -> ApiResult<impl IntoResponse> {
    use bifrost_core::net::scan::{scan, ScanOptions, ThisDevice};

    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let snapshot = node.snapshot().await;

    let this = ThisDevice {
        address: snapshot.ip.clone(),
        name: snapshot.identity.username.clone(),
        sharing_compute: node.compute.is_enabled().await,
    };

    let options = ScanOptions {
        sweep_subnet: !body.quick,
        bifrost_port: snapshot.ports.messaging,
        ..Default::default()
    };

    Ok(Json(scan(&snapshot.peers, Some(&this), &options).await))
}

#[derive(Debug, Default, Deserialize)]
struct ScanRequest {
    /// Skip the subnet probe: seconds rather than a moment, and finds less.
    #[serde(default)]
    quick: bool,
}

async fn snapshot(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    Ok(Json(node.snapshot().await))
}

/// Live event stream.
async fn events(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures_util::StreamExt;

    // An unknown id falls back to the host rather than failing: SSE has no
    // good way to report an error, and the next API call will say so clearly.
    let node = match state.node_for(as_device.as_device.as_deref()).await {
        Ok(node) => node,
        Err(_) => state.host.clone(),
    };

    let stream = tokio_stream::wrappers::BroadcastStream::new(node.subscribe()).filter_map(
        |event| async move {
            match event {
                Ok(event) => Some(Ok(serialise_event(&event))),
                // A lagging browser tab missed some events; tell it to re-sync
                // from a snapshot rather than dropping the connection.
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                    Some(Ok(Event::default()
                        .event("lagged")
                        .data(serde_json::json!({ "skipped": n }).to_string())))
                }
            }
        },
    );

    // A comment every 15s keeps the connection alive through the aggressive
    // idle timeouts phones and proxies apply.
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn serialise_event(event: &NodeEvent) -> Event {
    match serde_json::to_string(event) {
        Ok(json) => Event::default().event("node").data(json),
        Err(e) => Event::default()
            .event("error")
            .data(serde_json::json!({ "error": e.to_string() }).to_string()),
    }
}

// ─── Commands ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CommandBody {
    line: String,
}

/// Run a command line, exactly as the terminal dashboard would.
async fn command(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<CommandBody>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    match node.execute_line(&body.line).await {
        Ok(Some(outcome)) => Ok(Json(serde_json::json!({
            "outcome": outcome,
            "text": outcome.as_text(),
        }))),
        Ok(None) => Ok(Json(serde_json::json!({ "outcome": null, "text": "" }))),
        Err(e) => Err(ApiError::internal(format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
struct ChatBody {
    content: String,
    #[serde(default)]
    recipient: Option<String>,
}

async fn chat(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<ChatBody>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    if body.content.trim().is_empty() {
        return Err(ApiError::bad_request("a message cannot be empty"));
    }

    let outcome = match &body.recipient {
        Some(peer) => node.direct_message(peer, &body.content).await,
        None => node.broadcast(&body.content).await,
    }
    .map_err(|e| ApiError::internal(format!("{e:#}")))?;

    Ok(Json(serde_json::json!({ "text": outcome.as_text() })))
}

#[derive(Deserialize)]
struct AliasBody {
    peer: String,
    #[serde(default)]
    alias: Option<String>,
}

async fn alias(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<AliasBody>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let outcome = node
        .set_alias(&body.peer, body.alias)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "text": outcome.as_text() })))
}

#[derive(Deserialize)]
struct SharingBody {
    enabled: bool,
}

async fn sharing(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<SharingBody>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let outcome = node
        .set_sharing(body.enabled)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "text": outcome.as_text() })))
}

#[derive(Deserialize)]
struct RenameBody {
    username: String,
}

async fn rename(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<RenameBody>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let outcome = node
        .rename(&body.username)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "text": outcome.as_text() })))
}

#[derive(Deserialize)]
struct PeerBody {
    peer: String,
}

async fn call(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<PeerBody>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let outcome = node
        .call(&body.peer)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({
        "text": outcome.as_text(),
        "calls": node.voice.active().await,
    })))
}

#[derive(Deserialize)]
struct HangupBody {
    #[serde(default)]
    session: Option<String>,
}

async fn hangup(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<HangupBody>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let outcome = node
        .hang_up(body.session.as_deref())
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "text": outcome.as_text() })))
}

async fn ping(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
    Json(body): Json<PeerBody>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let outcome = node
        .ping(&body.peer)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "text": outcome.as_text() })))
}

// ─── Companion devices ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ClaimBody {
    /// What the device should be called on the network.
    #[serde(default)]
    name: Option<String>,
    /// An id from a previous visit, so a returning phone keeps its identity —
    /// and the aliases other machines gave it.
    #[serde(default)]
    id: Option<String>,
}

/// Register the caller's browser as a device on the network.
///
/// This is what turns "my phone shows my laptop's dashboard" into "my phone is
/// on the network": the host starts a real node for it, and from then on the
/// page passes `?as=<id>` and every action is that device's own.
async fn claim_companion(
    State(state): State<AppState>,
    headers: RequestHeaders,
    Json(body): Json<ClaimBody>,
) -> ApiResult<impl IntoResponse> {
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let name = body
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| companion::default_name_from_user_agent(user_agent));

    if name.chars().count() > 64 {
        return Err(ApiError::bad_request("that name is too long"));
    }

    let device_type = companion::device_type_from_user_agent(user_agent);

    let info = state
        .companions
        .claim(&state.host, body.id, &name, device_type)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;

    Ok(Json(info))
}

/// Every companion this host is running, for the laptop's settings page.
async fn list_companions(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.companions.list().await)
}

#[derive(Deserialize)]
struct LeaveBody {
    id: String,
}

/// Take a companion off the network.
async fn release_companion(
    State(state): State<AppState>,
    Json(body): Json<LeaveBody>,
) -> ApiResult<impl IntoResponse> {
    if state.companions.release(&body.id).await {
        Ok(Json(
            serde_json::json!({ "text": "Device removed from the network" }),
        ))
    } else {
        Err(ApiError::not_found("no such device"))
    }
}

// ─── Files ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct UploadQuery {
    peer: String,
    name: String,
    #[serde(default, rename = "as")]
    as_device: Option<String>,
}

/// Accept a file from the browser and forward it to a peer.
///
/// The body is the raw file rather than a multipart form: `fetch` can send a
/// `File` object directly, and it avoids a multipart parser dependency for a
/// form that only ever has one field.
async fn upload(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(query.as_device.as_deref()).await?;
    if body.is_empty() {
        return Err(ApiError::bad_request("the uploaded file was empty"));
    }
    if body.len() > MAX_UPLOAD_BYTES {
        return Err(ApiError::bad_request(format!(
            "the file is larger than the {} MiB upload limit",
            MAX_UPLOAD_BYTES / (1 << 20)
        )));
    }

    let peer = node
        .registry
        .resolve(&query.peer)
        .await
        .ok_or_else(|| ApiError::not_found(format!("no peer matching \"{}\"", query.peer)))?;

    // Stage under the node's own directory rather than the OS temp dir, so
    // the file lands on the same filesystem as everything else Bifrost writes
    // and a container with no writable /tmp still works.
    let filename = bifrost_core::files::transfer::sanitize_filename(&query.name);
    let staging = node.download_dir().await.join(".outbox");
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|e| ApiError::internal(format!("cannot create the staging directory: {e}")))?;

    let staged = staging.join(format!("{}-{filename}", uuid_like()));
    tokio::fs::write(&staged, &body)
        .await
        .map_err(|e| ApiError::internal(format!("cannot stage the upload: {e}")))?;

    let outcome = node
        .send_file(&peer.node_id, &staged)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;

    // The transfer runs in the background; remove the staged copy once it can
    // no longer be needed rather than leaving the directory to grow.
    let cleanup = staged.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(600)).await;
        let _ = tokio::fs::remove_file(cleanup).await;
    });

    Ok(Json(serde_json::json!({
        "text": outcome.as_text(),
        "filename": filename,
        "bytes": body.len(),
    })))
}

/// A short unique-ish prefix for staged uploads.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

#[derive(Serialize)]
struct DownloadEntry {
    name: String,
    size: u64,
    modified_secs: u64,
}

/// List files this node has received, so a phone can pull them down.
async fn list_downloads(
    State(state): State<AppState>,
    Query(as_device): Query<AsDevice>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let dir = node.download_dir().await;
    let mut entries = Vec::new();

    let mut read_dir = match tokio::fs::read_dir(&dir).await {
        Ok(read_dir) => read_dir,
        // No directory yet simply means nothing has been received.
        Err(_) => return Ok(Json(entries)),
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // Hide staging files and in-progress transfers.
        if name.starts_with('.') || name.ends_with(".bfpart") {
            continue;
        }

        entries.push(DownloadEntry {
            name,
            size: metadata.len(),
            modified_secs: metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
    }

    entries.sort_by_key(|e| std::cmp::Reverse(e.modified_secs));
    Ok(Json(entries))
}

/// Serve one received file.
async fn serve_download(
    State(state): State<AppState>,
    RoutePath(filename): RoutePath<String>,
    Query(as_device): Query<AsDevice>,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(as_device.as_device.as_deref()).await?;
    let dir = node.download_dir().await;
    let path =
        resolve_within(&dir, &filename).ok_or_else(|| ApiError::bad_request("invalid filename"))?;

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found(format!("no such file: {filename}")))?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    // Quote and strip the name so a filename with a comma or quote cannot
    // forge extra header parameters.
    let safe = filename.replace(['"', '\\', '\r', '\n'], "_");
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }

    Ok((headers, bytes))
}

/// Resolve `filename` strictly inside `dir`, or `None`.
///
/// The check is structural — a name with any separator, parent reference or
/// root is refused outright — rather than relying on canonicalisation, which
/// on Windows also has to contend with drive-relative paths and short names.
fn resolve_within(dir: &Path, filename: &str) -> Option<PathBuf> {
    if filename.is_empty() || filename.contains('\0') {
        return None;
    }

    let candidate = Path::new(filename);
    let mut components = candidate.components();
    let only = match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => name,
        _ => return None,
    };

    // Reject a Windows drive-relative name like `C:file.txt`, which parses as
    // one Normal component on other platforms.
    let text = only.to_str()?;
    if text.contains(':') || text.contains('/') || text.contains('\\') || text == ".." {
        return None;
    }

    Some(dir.join(only))
}

// ─── Voice ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct VoiceQuery {
    session: String,
    #[serde(default, rename = "as")]
    as_device: Option<String>,
}

/// Forward one captured audio frame from the browser to the call's peer.
///
/// The body is raw signed 16-bit little-endian mono PCM at 16 kHz, which is
/// what `getUserMedia` plus an `AudioWorklet` produces after downsampling.
async fn voice_send(
    State(state): State<AppState>,
    Query(query): Query<VoiceQuery>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let node = state.node_for(query.as_device.as_deref()).await?;
    if body.is_empty() {
        return Err(ApiError::bad_request("no audio data"));
    }

    node.voice
        .send_frame(
            &query.session,
            bifrost_core::net::protocol::AudioCodec::Pcm16Le,
            &body,
        )
        .await
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Drain received audio for a call, as base64 frames the browser can play.
async fn voice_receive(
    State(state): State<AppState>,
    Query(query): Query<VoiceQuery>,
) -> ApiResult<impl IntoResponse> {
    use base64::Engine as _;

    let node = state.node_for(query.as_device.as_deref()).await?;

    let frames: Vec<String> = node
        .voice
        .take_incoming(&query.session)
        .await
        .iter()
        .map(|f| base64::engine::general_purpose::STANDARD.encode(f))
        .collect();

    Ok(Json(serde_json::json!({
        "sample_rate": bifrost_core::net::protocol::AUDIO_SAMPLE_RATE,
        "frames": frames,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_paths_cannot_escape_the_download_directory() {
        let dir = Path::new("/home/me/Downloads/Bifrost");

        for hostile in [
            "../../../etc/passwd",
            r"..\..\Windows\System32\config\SAM",
            "/etc/passwd",
            r"C:\Windows\win.ini",
            "C:file.txt",
            "sub/dir/file.txt",
            "..",
            ".",
            "",
        ] {
            assert!(
                resolve_within(dir, hostile).is_none(),
                "{hostile:?} should have been refused"
            );
        }
    }

    #[test]
    fn ordinary_filenames_resolve_inside_the_directory() {
        let dir = Path::new("/home/me/Downloads/Bifrost");

        for name in ["report.pdf", "holiday photo.jpg", "archive.tar.gz"] {
            let resolved = resolve_within(dir, name).expect("should resolve");
            assert_eq!(resolved.parent(), Some(dir));
            assert_eq!(resolved.file_name().unwrap().to_str().unwrap(), name);
        }
    }

    #[test]
    fn the_dashboard_is_compiled_in_and_self_contained() {
        assert!(
            DASHBOARD_HTML.contains("<!doctype html>"),
            "should be a complete document"
        );

        // A node may have no internet access at all, so nothing may be fetched
        // from a third-party host.
        for offender in [
            "http://cdn",
            "https://cdn",
            "https://unpkg",
            "https://fonts.g",
        ] {
            assert!(
                !DASHBOARD_HTML.contains(offender),
                "the dashboard must not reference {offender}"
            );
        }
        assert!(serde_json::from_str::<serde_json::Value>(MANIFEST_JSON).is_ok());
    }
}
