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

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bifrost_core::events::NodeEvent;
use bifrost_core::Node;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// The dashboard, compiled in so the binary is self-contained.
const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");

/// Web app manifest, so a phone can add the dashboard to its home screen.
const MANIFEST_JSON: &str = include_str!("../assets/manifest.webmanifest");

/// Largest upload accepted (2 GiB), bounding memory per request.
const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// Build the router.
pub fn router(node: Node) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/manifest.webmanifest", get(manifest))
        .route("/health", get(health))
        .route("/api/snapshot", get(snapshot))
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
        .with_state(node)
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

    axum::serve(listener, router(node))
        .with_graceful_shutdown(async move {
            let _ = shutdown.recv().await;
            tracing::info!("web dashboard shutting down");
        })
        .await
        .context("the dashboard server stopped")
}

// ─── Errors ──────────────────────────────────────────────────────────────────

/// An API error, rendered as JSON so the browser can show it verbatim.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
    fn internal(message: impl Into<String>) -> Self {
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

type ApiResult<T> = std::result::Result<T, ApiError>;

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
async fn health(State(node): State<Node>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "node_id": node.node_id(),
        "version": bifrost_core::VERSION,
        "protocol": bifrost_core::PROTOCOL_VERSION,
        "uptime_secs": node.uptime_secs(),
    }))
}

// ─── Reading state ───────────────────────────────────────────────────────────

async fn snapshot(State(node): State<Node>) -> impl IntoResponse {
    Json(node.snapshot().await)
}

/// Live event stream.
async fn events(
    State(node): State<Node>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures_util::StreamExt;

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
    State(node): State<Node>,
    Json(body): Json<CommandBody>,
) -> ApiResult<impl IntoResponse> {
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
    State(node): State<Node>,
    Json(body): Json<ChatBody>,
) -> ApiResult<impl IntoResponse> {
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
    State(node): State<Node>,
    Json(body): Json<AliasBody>,
) -> ApiResult<impl IntoResponse> {
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
    State(node): State<Node>,
    Json(body): Json<SharingBody>,
) -> ApiResult<impl IntoResponse> {
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
    State(node): State<Node>,
    Json(body): Json<RenameBody>,
) -> ApiResult<impl IntoResponse> {
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
    State(node): State<Node>,
    Json(body): Json<PeerBody>,
) -> ApiResult<impl IntoResponse> {
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
    State(node): State<Node>,
    Json(body): Json<HangupBody>,
) -> ApiResult<impl IntoResponse> {
    let outcome = node
        .hang_up(body.session.as_deref())
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "text": outcome.as_text() })))
}

async fn ping(
    State(node): State<Node>,
    Json(body): Json<PeerBody>,
) -> ApiResult<impl IntoResponse> {
    let outcome = node
        .ping(&body.peer)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({ "text": outcome.as_text() })))
}

// ─── Files ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct UploadQuery {
    peer: String,
    name: String,
}

/// Accept a file from the browser and forward it to a peer.
///
/// The body is the raw file rather than a multipart form: `fetch` can send a
/// `File` object directly, and it avoids a multipart parser dependency for a
/// form that only ever has one field.
async fn upload(
    State(node): State<Node>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
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
async fn list_downloads(State(node): State<Node>) -> ApiResult<impl IntoResponse> {
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
    State(node): State<Node>,
    RoutePath(filename): RoutePath<String>,
) -> ApiResult<impl IntoResponse> {
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
}

/// Forward one captured audio frame from the browser to the call's peer.
///
/// The body is raw signed 16-bit little-endian mono PCM at 16 kHz, which is
/// what `getUserMedia` plus an `AudioWorklet` produces after downsampling.
async fn voice_send(
    State(node): State<Node>,
    Query(query): Query<VoiceQuery>,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
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
    State(node): State<Node>,
    Query(query): Query<VoiceQuery>,
) -> impl IntoResponse {
    use base64::Engine as _;

    let frames: Vec<String> = node
        .voice
        .take_incoming(&query.session)
        .await
        .iter()
        .map(|f| base64::engine::general_purpose::STANDARD.encode(f))
        .collect();

    Json(serde_json::json!({
        "sample_rate": bifrost_core::net::protocol::AUDIO_SAMPLE_RATE,
        "frames": frames,
    }))
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
