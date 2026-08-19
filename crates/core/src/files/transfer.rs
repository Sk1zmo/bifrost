//! Chunked file transfer with SHA-256 verification.
//!
//! Session, on TCP port 9003:
//!
//! 1. sender → receiver: [`FileOffer`] as a length-prefixed JSON frame
//! 2. receiver → sender: [`FileAck`]
//! 3. sender → receiver: the binary chunk stream — repeated
//!    `u64` index, `u32` length, `length` bytes — terminated by index
//!    [`CHUNK_STREAM_END`] with length 0
//! 4. receiver → sender: [`FileResult`] after checksum validation
//!
//! Chunks are raw bytes rather than JSON. `Bifrost v3` serialised each chunk
//! as a JSON array of numbers, which inflates every byte to roughly four on
//! the wire and forces the receiver to parse a 64 KiB JSON array per chunk;
//! `Bifrost v2` streamed binary, which is what this keeps.
//!
//! All integers are little-endian. Both legacy trees documented little-endian
//! and then called tokio's big-endian `write_u32`/`write_u64`; here the code
//! and the spec agree.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::events::{EventBus, NodeEvent};
use crate::net::frame::{read_frame, read_frame_required, write_frame};
use crate::net::protocol::{
    FileAck, FileOffer, FileResult, CHUNK_SIZE, CHUNK_STREAM_END, MAX_FRAME_BYTES,
};

/// Connect timeout for the data connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle timeout while streaming. Generous: a large file over slow Wi-Fi is
/// still making progress between chunks.
const STREAM_TIMEOUT: Duration = Duration::from_secs(120);

/// Refuse a transfer larger than this (16 GiB), so a hostile offer cannot
/// fill the receiver's disk before the size is even known.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum TransferState {
    Offered,
    Accepted,
    Streaming,
    Verifying,
    Completed,
    Rejected(String),
    Failed(String),
}

impl TransferState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Rejected(_) | Self::Failed(_))
    }
}

/// Live view of one transfer, surfaced in both dashboards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferStatus {
    pub transfer_id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub filename: String,
    pub direction: Direction,
    pub state: TransferState,
    pub bytes_total: u64,
    pub bytes_done: u64,
    /// Throughput in bytes per second, averaged over the transfer so far.
    pub rate_bps: f64,
    pub saved_path: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TransferStatus {
    pub fn progress_pct(&self) -> f64 {
        if self.bytes_total == 0 {
            return if matches!(self.state, TransferState::Completed) {
                100.0
            } else {
                0.0
            };
        }
        (self.bytes_done as f64 / self.bytes_total as f64 * 100.0).min(100.0)
    }

    /// Estimated seconds remaining, or `None` when it cannot be judged yet.
    pub fn eta_secs(&self) -> Option<f64> {
        if self.rate_bps <= 0.0 || self.bytes_done >= self.bytes_total {
            return None;
        }
        Some((self.bytes_total - self.bytes_done) as f64 / self.rate_bps)
    }
}

/// Shared table of active and recent transfers.
#[derive(Clone, Debug, Default)]
pub struct TransferStore {
    inner: Arc<RwLock<HashMap<String, TransferStatus>>>,
    events: Option<EventBus>,
}

impl TransferStore {
    pub fn new(events: EventBus) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            events: Some(events),
        }
    }

    async fn put(&self, status: TransferStatus) {
        self.inner
            .write()
            .await
            .insert(status.transfer_id.clone(), status.clone());
        if let Some(events) = &self.events {
            events.publish(NodeEvent::Transfer(status));
        }
    }

    /// Apply a mutation and republish.
    async fn update(&self, transfer_id: &str, f: impl FnOnce(&mut TransferStatus)) {
        let updated = {
            let mut map = self.inner.write().await;
            match map.get_mut(transfer_id) {
                Some(status) => {
                    f(status);
                    status.updated_at = Utc::now();
                    Some(status.clone())
                }
                None => None,
            }
        };
        if let (Some(status), Some(events)) = (updated, &self.events) {
            events.publish(NodeEvent::Transfer(status));
        }
    }

    pub async fn get(&self, transfer_id: &str) -> Option<TransferStatus> {
        self.inner.read().await.get(transfer_id).cloned()
    }

    /// All transfers, newest first.
    pub async fn list(&self) -> Vec<TransferStatus> {
        let mut all: Vec<TransferStatus> = self.inner.read().await.values().cloned().collect();
        all.sort_by_key(|t| std::cmp::Reverse(t.started_at));
        all
    }

    pub async fn active(&self) -> Vec<TransferStatus> {
        self.inner
            .read()
            .await
            .values()
            .filter(|t| !t.state.is_terminal())
            .cloned()
            .collect()
    }

    /// Drop finished transfers, keeping the `keep` most recent.
    pub async fn prune_finished(&self, keep: usize) {
        let mut map = self.inner.write().await;
        let mut finished: Vec<(String, DateTime<Utc>)> = map
            .iter()
            .filter(|(_, t)| t.state.is_terminal())
            .map(|(id, t)| (id.clone(), t.updated_at))
            .collect();
        finished.sort_by_key(|(_, updated)| std::cmp::Reverse(*updated));
        for (id, _) in finished.into_iter().skip(keep) {
            map.remove(&id);
        }
    }
}

// ─── Filenames ───────────────────────────────────────────────────────────────

/// Reduce a peer-supplied filename to something safe to join onto a directory.
///
/// A remote peer names the file, so this is a trust boundary: without it,
/// `../../.ssh/authorized_keys` would be written wherever the sender chose.
/// `Bifrost v2` only prefixed the transfer id, which does not neutralise the
/// `..` components that follow it.
pub fn sanitize_filename(raw: &str) -> String {
    // Take the last component under either separator, so both `a/b/c.txt` and
    // `a\b\c.txt` reduce to `c.txt` regardless of the host platform.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);

    let cleaned: String = base
        .chars()
        .map(|c| match c {
            // Reserved on Windows, plus control characters everywhere.
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    // `.` and `..` are directory references, not names.
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        return format!("bifrost-file-{}", &Uuid::new_v4().to_string()[..8]);
    }

    // Windows refuses these names even with an extension.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = cleaned.split('.').next().unwrap_or(cleaned);
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return format!("_{cleaned}");
    }

    // Leave room for a de-duplication suffix within the usual 255-byte limit.
    cleaned.chars().take(200).collect()
}

/// Pick a path in `dir` that does not exist, appending ` (1)`, ` (2)`, … .
///
/// Incoming files never silently overwrite existing ones.
async fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let ext = path.extension().and_then(|s| s.to_str());

    for n in 1..10_000 {
        let name = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }

    dir.join(format!("{stem}-{}", Uuid::new_v4()))
}

/// SHA-256 of a file, streamed so memory use stays flat regardless of size.
pub async fn checksum_file(path: &Path) -> Result<String> {
    let file = File::open(path)
        .await
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(CHUNK_SIZE, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hex::encode(hasher.finalize()))
}

// ─── Sending ─────────────────────────────────────────────────────────────────

/// Send `path` to the peer listening at `addr`. Returns the transfer id.
pub async fn send_file(
    addr: SocketAddr,
    path: &Path,
    sender_id: &str,
    sender_name: &str,
    peer_id: &str,
    peer_name: &str,
    store: TransferStore,
) -> Result<String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }

    let file_size = metadata.len();
    if file_size > MAX_FILE_BYTES {
        bail!(
            "{} is larger than the {} GiB limit",
            path.display(),
            MAX_FILE_BYTES / (1 << 30)
        );
    }

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitize_filename)
        .context("the path has no usable filename")?;

    let transfer_id = Uuid::new_v4().to_string();
    let now = Utc::now();

    store
        .put(TransferStatus {
            transfer_id: transfer_id.clone(),
            peer_id: peer_id.to_string(),
            peer_name: peer_name.to_string(),
            filename: filename.clone(),
            direction: Direction::Outgoing,
            state: TransferState::Offered,
            bytes_total: file_size,
            bytes_done: 0,
            rate_bps: 0.0,
            saved_path: None,
            started_at: now,
            updated_at: now,
        })
        .await;

    // Any failure past this point must leave the transfer marked Failed rather
    // than stuck at Offered forever, so the whole exchange runs in a helper
    // whose error is recorded before it propagates.
    match stream_to_peer(
        addr,
        path,
        &transfer_id,
        &filename,
        file_size,
        sender_id,
        sender_name,
        &store,
    )
    .await
    {
        Ok(()) => Ok(transfer_id),
        Err(e) => {
            let message = format!("{e:#}");
            store
                .update(&transfer_id, |s| {
                    s.state = TransferState::Failed(message.clone())
                })
                .await;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_to_peer(
    addr: SocketAddr,
    path: &Path,
    transfer_id: &str,
    filename: &str,
    file_size: u64,
    sender_id: &str,
    sender_name: &str,
    store: &TransferStore,
) -> Result<()> {
    let checksum = checksum_file(path).await?;
    let total_chunks = file_size.div_ceil(CHUNK_SIZE as u64).max(1);

    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("timed out connecting to {addr}"))?
        .with_context(|| format!("cannot reach the file service at {addr}"))?;
    let _ = stream.set_nodelay(true);

    let offer = FileOffer {
        transfer_id: transfer_id.to_string(),
        sender_id: sender_id.to_string(),
        sender_name: sender_name.to_string(),
        filename: filename.to_string(),
        file_size,
        total_chunks,
        chunk_size: CHUNK_SIZE as u32,
        checksum,
        timestamp: Utc::now(),
    };
    write_frame(&mut stream, &offer).await?;

    let ack: FileAck = read_frame_required(&mut stream)
        .await
        .context("no reply to the offer")?;
    if !ack.accepted {
        let reason = ack.reason.unwrap_or_else(|| "peer declined".into());
        store
            .update(transfer_id, |s| {
                s.state = TransferState::Rejected(reason.clone())
            })
            .await;
        bail!("transfer declined: {reason}");
    }
    store
        .update(transfer_id, |s| s.state = TransferState::Streaming)
        .await;

    let file = File::open(path).await?;
    let mut reader = BufReader::with_capacity(CHUNK_SIZE, file);
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut index: u64 = 0;
    let mut sent: u64 = 0;
    let started = Instant::now();

    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }

        // index, length, payload — all little-endian.
        stream.write_u64_le(index).await?;
        stream.write_u32_le(n as u32).await?;
        stream.write_all(&buf[..n]).await?;

        index += 1;
        sent += n as u64;

        let elapsed = started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            sent as f64 / elapsed
        } else {
            0.0
        };
        store
            .update(transfer_id, |s| {
                s.bytes_done = sent;
                s.rate_bps = rate;
            })
            .await;
    }

    // Terminator.
    stream.write_u64_le(CHUNK_STREAM_END).await?;
    stream.write_u32_le(0).await?;
    stream.flush().await?;

    store
        .update(transfer_id, |s| s.state = TransferState::Verifying)
        .await;

    let result: FileResult = tokio::time::timeout(STREAM_TIMEOUT, read_frame_required(&mut stream))
        .await
        .context("timed out waiting for the receiver to verify")??;

    if result.success {
        store
            .update(transfer_id, |s| {
                s.state = TransferState::Completed;
                s.bytes_done = s.bytes_total;
                s.saved_path = result.saved_as.clone();
            })
            .await;
        tracing::info!(filename, bytes = sent, "file sent");
        Ok(())
    } else {
        let error = result
            .error
            .unwrap_or_else(|| "receiver reported failure".into());
        bail!("{error}")
    }
}

// ─── Receiving ───────────────────────────────────────────────────────────────

/// Decides whether to accept an incoming offer.
#[derive(Clone, Debug)]
pub struct AcceptPolicy {
    /// Where accepted files are written.
    pub download_dir: PathBuf,
    /// Accept without asking. Phase 1 default, matching both legacy trees.
    pub auto_accept: bool,
    /// Reject offers larger than this, if set.
    pub max_bytes: Option<u64>,
}

impl AcceptPolicy {
    fn evaluate(&self, offer: &FileOffer) -> Result<(), String> {
        if !self.auto_accept {
            return Err("this node is not accepting files".into());
        }
        if offer.file_size > MAX_FILE_BYTES {
            return Err(format!("{} bytes exceeds the hard limit", offer.file_size));
        }
        if let Some(max) = self.max_bytes {
            if offer.file_size > max {
                return Err(format!(
                    "{} bytes exceeds this node's limit of {max}",
                    offer.file_size
                ));
            }
        }
        Ok(())
    }
}

/// Accept inbound transfers until `shutdown` fires.
pub async fn run_server(
    bind: SocketAddr,
    policy: AcceptPolicy,
    store: TransferStore,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<()> {
    tokio::fs::create_dir_all(&policy.download_dir)
        .await
        .with_context(|| {
            format!(
                "cannot create the download directory {}",
                policy.download_dir.display()
            )
        })?;

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind the file transfer port {bind}"))?;
    tracing::info!(%bind, dir = %policy.download_dir.display(), "file transfer listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, from)) => {
                        let policy = policy.clone();
                        let store = store.clone();
                        tokio::spawn(async move {
                            if let Err(e) = receive(stream, from, policy, store).await {
                                tracing::warn!(%from, error = %format!("{e:#}"), "inbound transfer failed");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "file transfer accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("file transfer shutting down");
                return Ok(());
            }
        }
    }
}

/// Handle one inbound transfer session.
async fn receive(
    mut stream: TcpStream,
    from: SocketAddr,
    policy: AcceptPolicy,
    store: TransferStore,
) -> Result<()> {
    let _ = stream.set_nodelay(true);

    let offer: FileOffer = match read_frame(&mut stream).await? {
        Some(offer) => offer,
        None => return Ok(()), // connected then hung up; nothing to do
    };

    let safe_name = sanitize_filename(&offer.filename);
    let now = Utc::now();

    store
        .put(TransferStatus {
            transfer_id: offer.transfer_id.clone(),
            peer_id: offer.sender_id.clone(),
            peer_name: offer.sender_name.clone(),
            filename: safe_name.clone(),
            direction: Direction::Incoming,
            state: TransferState::Offered,
            bytes_total: offer.file_size,
            bytes_done: 0,
            rate_bps: 0.0,
            saved_path: None,
            started_at: now,
            updated_at: now,
        })
        .await;

    if let Err(reason) = policy.evaluate(&offer) {
        write_frame(
            &mut stream,
            &FileAck {
                transfer_id: offer.transfer_id.clone(),
                accepted: false,
                reason: Some(reason.clone()),
            },
        )
        .await?;
        store
            .update(&offer.transfer_id, |s| {
                s.state = TransferState::Rejected(reason.clone())
            })
            .await;
        return Ok(());
    }

    write_frame(
        &mut stream,
        &FileAck {
            transfer_id: offer.transfer_id.clone(),
            accepted: true,
            reason: None,
        },
    )
    .await?;
    store
        .update(&offer.transfer_id, |s| s.state = TransferState::Streaming)
        .await;

    tracing::info!(%from, filename = %safe_name, bytes = offer.file_size, "receiving file");

    // Write to a temporary file first, so an interrupted transfer never leaves
    // a truncated file wearing the real name.
    let destination = unique_path(&policy.download_dir, &safe_name).await;
    let temp_path = destination.with_extension(format!(
        "{}.bfpart",
        destination
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
    ));

    let outcome = write_stream(&mut stream, &temp_path, &offer, &store).await;

    match outcome {
        Ok(bytes) => {
            tokio::fs::rename(&temp_path, &destination)
                .await
                .with_context(|| {
                    format!(
                        "cannot move the completed file into {}",
                        destination.display()
                    )
                })?;

            let saved = destination.display().to_string();
            store
                .update(&offer.transfer_id, |s| {
                    s.state = TransferState::Completed;
                    s.bytes_done = bytes;
                    s.saved_path = Some(saved.clone());
                })
                .await;

            write_frame(
                &mut stream,
                &FileResult {
                    transfer_id: offer.transfer_id.clone(),
                    success: true,
                    bytes_received: bytes,
                    saved_as: Some(saved.clone()),
                    error: None,
                },
            )
            .await?;

            tracing::info!(path = %saved, "file received and verified");
            Ok(())
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            let message = format!("{e:#}");
            store
                .update(&offer.transfer_id, |s| {
                    s.state = TransferState::Failed(message.clone())
                })
                .await;

            let _ = write_frame(
                &mut stream,
                &FileResult {
                    transfer_id: offer.transfer_id.clone(),
                    success: false,
                    bytes_received: 0,
                    saved_as: None,
                    error: Some(message),
                },
            )
            .await;
            Err(e)
        }
    }
}

/// Read the chunk stream to `temp_path`, verifying the checksum.
async fn write_stream(
    stream: &mut TcpStream,
    temp_path: &Path,
    offer: &FileOffer,
    store: &TransferStore,
) -> Result<u64> {
    let file = File::create(temp_path)
        .await
        .with_context(|| format!("cannot create {}", temp_path.display()))?;
    let mut writer = BufWriter::with_capacity(CHUNK_SIZE, file);

    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let started = Instant::now();

    loop {
        let index = tokio::time::timeout(STREAM_TIMEOUT, stream.read_u64_le())
            .await
            .context("timed out waiting for the next chunk")??;
        let length = stream.read_u32_le().await?;

        if index == CHUNK_STREAM_END {
            break;
        }
        if length > MAX_FRAME_BYTES {
            bail!("chunk {index} announced {length} bytes, over the limit");
        }

        // Stop as soon as the sender exceeds the size it declared, rather than
        // writing an unbounded amount to disk.
        if received + length as u64 > offer.file_size {
            bail!(
                "sender exceeded the declared size of {} bytes",
                offer.file_size
            );
        }

        let mut chunk = vec![0u8; length as usize];
        stream.read_exact(&mut chunk).await?;

        writer.write_all(&chunk).await?;
        hasher.update(&chunk);
        received += length as u64;

        let elapsed = started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            received as f64 / elapsed
        } else {
            0.0
        };
        store
            .update(&offer.transfer_id, |s| {
                s.bytes_done = received;
                s.rate_bps = rate;
            })
            .await;
    }

    writer.flush().await?;
    // Reach the OS before renaming, so a crash cannot leave a correctly-named
    // file whose contents were never written back.
    writer.get_ref().sync_all().await?;
    drop(writer);

    store
        .update(&offer.transfer_id, |s| s.state = TransferState::Verifying)
        .await;

    if received != offer.file_size {
        bail!("expected {} bytes but received {received}", offer.file_size);
    }

    let actual = hex::encode(hasher.finalize());
    if actual != offer.checksum {
        bail!(
            "checksum mismatch: expected {}, computed {actual}",
            offer.checksum
        );
    }

    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn traversal_attempts_are_reduced_to_a_bare_filename() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_filename(r"..\..\Windows\System32\evil.dll"),
            "evil.dll"
        );
        assert_eq!(sanitize_filename("/absolute/path/report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename(r"C:\Users\me\notes.txt"), "notes.txt");

        for name in ["..", ".", "...", "   "] {
            let safe = sanitize_filename(name);
            assert!(
                safe.starts_with("bifrost-file-"),
                "{name:?} became {safe:?}"
            );
        }
    }

    #[test]
    fn ordinary_filenames_pass_through_unchanged() {
        for name in [
            "report.pdf",
            "holiday photo.jpg",
            "archive.tar.gz",
            "données.csv",
        ] {
            assert_eq!(sanitize_filename(name), name);
        }
    }

    #[test]
    fn windows_hostile_names_are_neutralised() {
        assert_eq!(sanitize_filename("a<b>c:d.txt"), "a_b_c_d.txt");
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul.txt"), "_nul.txt");
        assert!(!sanitize_filename("bad\u{7}name.txt").contains('\u{7}'));
    }

    #[tokio::test]
    async fn an_existing_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_path(dir.path(), "notes.txt").await;
        assert_eq!(first.file_name().unwrap(), "notes.txt");

        std::fs::File::create(&first)
            .unwrap()
            .write_all(b"original")
            .unwrap();
        let second = unique_path(dir.path(), "notes.txt").await;
        assert_eq!(second.file_name().unwrap(), "notes (1).txt");

        std::fs::File::create(&second).unwrap();
        let third = unique_path(dir.path(), "notes.txt").await;
        assert_eq!(third.file_name().unwrap(), "notes (2).txt");

        // The original is untouched.
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "original");
    }

    #[tokio::test]
    async fn checksum_matches_a_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.txt");
        tokio::fs::write(&path, b"abc").await.unwrap();

        // The published SHA-256 of "abc".
        assert_eq!(
            checksum_file(&path).await.unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn checksum_of_an_empty_file_is_the_empty_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        tokio::fs::write(&path, b"").await.unwrap();

        assert_eq!(
            checksum_file(&path).await.unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn progress_and_eta_are_sane() {
        let mut status = TransferStatus {
            transfer_id: "t".into(),
            peer_id: "p".into(),
            peer_name: "peer".into(),
            filename: "f".into(),
            direction: Direction::Outgoing,
            state: TransferState::Streaming,
            bytes_total: 1000,
            bytes_done: 250,
            rate_bps: 250.0,
            saved_path: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
        };

        assert_eq!(status.progress_pct(), 25.0);
        assert_eq!(status.eta_secs(), Some(3.0));

        status.bytes_done = 1000;
        assert_eq!(status.progress_pct(), 100.0);
        assert_eq!(status.eta_secs(), None, "no ETA once complete");

        // A zero-byte file that completed still reads as 100%.
        status.bytes_total = 0;
        status.bytes_done = 0;
        status.state = TransferState::Completed;
        assert_eq!(status.progress_pct(), 100.0);
    }

    #[tokio::test]
    async fn the_store_prunes_finished_transfers_but_keeps_active_ones() {
        let store = TransferStore::new(EventBus::new());
        let now = Utc::now();

        for i in 0..5 {
            store
                .put(TransferStatus {
                    transfer_id: format!("done-{i}"),
                    peer_id: "p".into(),
                    peer_name: "peer".into(),
                    filename: "f".into(),
                    direction: Direction::Incoming,
                    state: TransferState::Completed,
                    bytes_total: 1,
                    bytes_done: 1,
                    rate_bps: 0.0,
                    saved_path: None,
                    started_at: now + chrono::Duration::seconds(i),
                    updated_at: now + chrono::Duration::seconds(i),
                })
                .await;
        }
        store
            .put(TransferStatus {
                transfer_id: "live".into(),
                peer_id: "p".into(),
                peer_name: "peer".into(),
                filename: "f".into(),
                direction: Direction::Incoming,
                state: TransferState::Streaming,
                bytes_total: 100,
                bytes_done: 10,
                rate_bps: 0.0,
                saved_path: None,
                started_at: now,
                updated_at: now,
            })
            .await;

        store.prune_finished(2).await;

        let remaining = store.list().await;
        assert_eq!(remaining.len(), 3, "2 finished + 1 active");
        assert_eq!(store.active().await.len(), 1);
        assert!(store.get("live").await.is_some());
    }
}
