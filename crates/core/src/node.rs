//! [`Node`] — the running Bifrost engine.
//!
//! Owns every service and exposes one API that the terminal dashboard, the web
//! dashboard, the CLI and the mobile FFI all drive. Keeping a single
//! [`Node::execute`] entry point is what makes `/sendfile bob report.pdf`
//! behave identically whether it is typed in a terminal, tapped on a phone, or
//! posted to the REST API.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch, RwLock};

use crate::chat::commands::Command;
use crate::chat::history::{compose, ChatStore};
use crate::chat::presence::PresenceTracker;
use crate::chat::voice::{CallSession, VoiceService};
use crate::events::{EventBus, NodeEvent, NoticeLevel};
use crate::files::transfer::{AcceptPolicy, TransferStatus, TransferStore};
use crate::net::discovery::{self, BeaconSource};
use crate::net::messaging::{self, MessagingContext};
use crate::net::peers::{Peer, PeerRegistry, PoolTotals};
use crate::net::protocol::{ChatPayload, PortSet, PresenceStatus, SharedResources, Telemetry};
use crate::net::{iface, protocol};
use crate::resources::sharing::{SharingPolicy, SharingService};
use crate::resources::telemetry::{HostInfo, TelemetryCollector};
use crate::store::DataStore;
use crate::util::config::Config;
use crate::util::identity::Identity;

/// Chat lines restored from disk at startup.
const HISTORY_ON_START: usize = 200;

/// A complete, consistent view of the node for rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSnapshot {
    pub identity: Identity,
    pub host: HostSummary,
    pub ports: PortSet,
    pub ip: String,
    pub dashboard_url: String,
    pub telemetry: Telemetry,
    pub shared: SharedResources,
    pub sharing_enabled: bool,
    pub peers: Vec<Peer>,
    pub online_peers: usize,
    pub pool: PoolTotals,
    pub chats: Vec<ChatPayload>,
    pub transfers: Vec<TransferStatus>,
    pub calls: Vec<CallSession>,
    pub download_dir: String,
    pub uptime_secs: u64,
    pub version: String,
    pub captured_at: DateTime<Utc>,
}

/// Static facts about the host, in a serialisable shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSummary {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub kernel: String,
}

impl From<HostInfo> for HostSummary {
    fn from(h: HostInfo) -> Self {
        Self {
            hostname: h.hostname,
            os: format!("{} {}", h.os_name, h.os_version).trim().to_string(),
            arch: h.arch,
            kernel: h.kernel,
        }
    }
}

/// What running a [`Command`] produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CommandOutcome {
    /// Text for the log pane.
    Message { level: NoticeLevel, text: String },
    /// The peer list, for `/peers`.
    Peers { peers: Vec<Peer> },
    /// A full snapshot, for `/status`.
    Status { snapshot: Box<NodeSnapshot> },
    /// The UI should clear its message pane.
    Clear,
    /// The UI should exit.
    Quit,
}

impl CommandOutcome {
    fn info(text: impl Into<String>) -> Self {
        Self::Message {
            level: NoticeLevel::Info,
            text: text.into(),
        }
    }
    fn success(text: impl Into<String>) -> Self {
        Self::Message {
            level: NoticeLevel::Success,
            text: text.into(),
        }
    }
    fn warning(text: impl Into<String>) -> Self {
        Self::Message {
            level: NoticeLevel::Warning,
            text: text.into(),
        }
    }
    fn error(text: impl Into<String>) -> Self {
        Self::Message {
            level: NoticeLevel::Error,
            text: text.into(),
        }
    }

    /// Plain text, for the CLI and log panes.
    pub fn as_text(&self) -> String {
        match self {
            Self::Message { text, .. } => text.clone(),
            Self::Peers { peers } => {
                if peers.is_empty() {
                    return "No peers discovered yet.".to_string();
                }
                peers
                    .iter()
                    .map(|p| {
                        format!(
                            "{} {:<20} {:<15} {:<14} CPU {:>5.1}%  RAM {:>6.1} GB free",
                            if p.online { "●" } else { "○" },
                            p.display_name(),
                            p.ip,
                            p.device_type,
                            p.telemetry.cpu_usage,
                            p.telemetry.ram_available_mb() as f64 / 1024.0,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::Status { snapshot } => format!(
                "{} · {} · {}\n{} peer(s) online · dashboard {}",
                snapshot.identity.username,
                snapshot.identity.device_type,
                snapshot.ip,
                snapshot.online_peers,
                snapshot.dashboard_url,
            ),
            Self::Clear => String::new(),
            Self::Quit => "Shutting down.".to_string(),
        }
    }
}

/// The running node.
#[derive(Clone, Debug)]
pub struct Node {
    pub identity_rx: watch::Receiver<Identity>,
    pub registry: PeerRegistry,
    pub chat: ChatStore,
    pub transfers: TransferStore,
    pub voice: VoiceService,
    pub presence: PresenceTracker,
    pub events: EventBus,
    pub store: DataStore,
    pub host: HostInfo,
    ports: PortSet,
    telemetry_rx: watch::Receiver<Telemetry>,
    shared_rx: watch::Receiver<SharedResources>,
    identity_tx: Arc<watch::Sender<Identity>>,
    policy_tx: Arc<watch::Sender<SharingPolicy>>,
    config: Arc<RwLock<Config>>,
    config_path: Arc<PathBuf>,
    shutdown_tx: broadcast::Sender<()>,
    started_at: Instant,
}

impl Node {
    /// Start every service and return once they are listening.
    pub async fn start(config: Config, config_path: impl AsRef<Path>) -> Result<Self> {
        let config_path = config_path.as_ref().to_path_buf();
        config.ensure_dirs()?;

        let ports = config.ports;
        let host = HostInfo::detect();
        let events = EventBus::new();
        let registry = PeerRegistry::new();
        let chat = ChatStore::new();
        let transfers = TransferStore::new(events.clone());
        let presence = PresenceTracker::new();
        let store = DataStore::open(&config.data_dir).await?;

        let (identity_tx, identity_rx) = watch::channel(config.identity.clone());
        let (policy_tx, policy_rx) = watch::channel(config.sharing_policy());
        let (shutdown_tx, _) = broadcast::channel(16);

        let (telemetry_collector, telemetry_rx) = TelemetryCollector::new();
        let (sharing_service, shared_rx) = SharingService::new(policy_rx, telemetry_rx.clone());

        let voice = VoiceService::new(
            config.identity.node_id.clone(),
            ports.voice_audio,
            events.clone(),
        );

        // Restore aliases and history before anything can observe the node, so
        // the first frame the dashboard draws is already correct.
        registry.apply_saved_aliases(&config.aliases).await;
        match store.load_recent_chat(HISTORY_ON_START).await {
            Ok(history) => {
                for message in history {
                    chat.ingest(message).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not restore chat history"),
        }

        let node = Self {
            identity_rx,
            registry: registry.clone(),
            chat: chat.clone(),
            transfers: transfers.clone(),
            voice: voice.clone(),
            presence: presence.clone(),
            events: events.clone(),
            store: store.clone(),
            host,
            ports,
            telemetry_rx: telemetry_rx.clone(),
            shared_rx: shared_rx.clone(),
            identity_tx: Arc::new(identity_tx),
            policy_tx: Arc::new(policy_tx),
            config: Arc::new(RwLock::new(config)),
            config_path: Arc::new(config_path),
            shutdown_tx: shutdown_tx.clone(),
            started_at: Instant::now(),
        };

        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

        tokio::spawn(telemetry_collector.run(events.clone(), shutdown_tx.subscribe()));
        tokio::spawn(sharing_service.run(shutdown_tx.subscribe()));

        {
            let source = BeaconSource {
                identity: node.identity_rx.clone(),
                ports,
                telemetry: telemetry_rx,
                shared: shared_rx,
            };
            let registry = registry.clone();
            let shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) = discovery::run(source, registry, shutdown).await {
                    tracing::error!(error = %format!("{e:#}"), "discovery stopped");
                }
            });
        }

        {
            let ctx = MessagingContext {
                node_id: node.node_id(),
                registry: registry.clone(),
                chat: chat.clone(),
                events: events.clone(),
            };
            let bind = SocketAddr::new(any, ports.messaging);
            let shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) = messaging::run_server(bind, ctx, shutdown).await {
                    tracing::error!(error = %format!("{e:#}"), "messaging stopped");
                }
            });
        }

        {
            let policy = {
                let config = node.config.read().await;
                AcceptPolicy {
                    download_dir: config.download_dir.clone(),
                    auto_accept: config.auto_accept_files,
                    max_bytes: config.max_incoming_file_bytes,
                }
            };
            let bind = SocketAddr::new(any, ports.files);
            let transfers = transfers.clone();
            let shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) =
                    crate::files::transfer::run_server(bind, policy, transfers, shutdown).await
                {
                    tracing::error!(error = %format!("{e:#}"), "file transfer stopped");
                }
            });
        }

        {
            let bind = SocketAddr::new(any, ports.voice_signal);
            let voice = voice.clone();
            let registry = registry.clone();
            let shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) =
                    crate::chat::voice::run_signal_server(bind, voice, registry, shutdown).await
                {
                    tracing::error!(error = %format!("{e:#}"), "voice signalling stopped");
                }
            });
        }

        {
            let bind = SocketAddr::new(any, ports.voice_audio);
            let voice = voice.clone();
            let shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) = voice.run_audio_listener(bind, shutdown).await {
                    tracing::error!(error = %format!("{e:#}"), "voice audio stopped");
                }
            });
        }

        // Persist chat as it arrives, and keep presence fresh.
        {
            let store = store.clone();
            let presence = presence.clone();
            let mut rx = events.subscribe();
            let mut shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        received = rx.recv() => match received {
                            Ok(NodeEvent::Chat(message)) => {
                                presence.touch(&message.sender_id).await;
                                if let Err(e) = store.append_chat(&message).await {
                                    tracing::warn!(error = %e, "could not persist a chat message");
                                }
                            }
                            Ok(_) => {}
                            // Lagging only loses log lines, never live state.
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                tracing::debug!(skipped = n, "chat persistence lagged");
                            }
                            Err(broadcast::error::RecvError::Closed) => return,
                        },
                        _ = shutdown.recv() => return,
                    }
                }
            });
        }

        // Periodic housekeeping.
        {
            let presence = presence.clone();
            let transfers = transfers.clone();
            let store = store.clone();
            let mut shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            presence.refresh().await;
                            transfers.prune_finished(50).await;
                            if let Err(e) = store.rotate_chat_log().await {
                                tracing::debug!(error = %e, "chat log rotation skipped");
                            }
                        }
                        _ = shutdown.recv() => return,
                    }
                }
            });
        }

        // Announce immediately rather than waiting out a beacon interval, so
        // peers see this node within a second of it starting.
        let source = BeaconSource {
            identity: node.identity_rx.clone(),
            ports,
            telemetry: node.telemetry_rx.clone(),
            shared: node.shared_rx.clone(),
        };
        if let Err(e) = discovery::announce_once(&source).await {
            tracing::warn!(error = %format!("{e:#}"), "initial announcement failed");
        }

        tracing::info!(
            node_id = %node.node_id(),
            username = %node.identity().username,
            ip = %iface::local_ip_string(),
            "Bifrost node started"
        );
        events.publish(NodeEvent::success("Bifrost node started"));

        Ok(node)
    }

    // ── Accessors ────────────────────────────────────────────────────────────

    pub fn identity(&self) -> Identity {
        self.identity_rx.borrow().clone()
    }

    pub fn node_id(&self) -> String {
        self.identity_rx.borrow().node_id.clone()
    }

    pub fn ports(&self) -> PortSet {
        self.ports
    }

    pub fn telemetry(&self) -> Telemetry {
        self.telemetry_rx.borrow().clone()
    }

    pub fn shared(&self) -> SharedResources {
        self.shared_rx.borrow().clone()
    }

    pub fn dashboard_url(&self) -> String {
        format!("http://{}:{}", iface::local_ip_string(), self.ports.web)
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.events.subscribe()
    }

    pub async fn download_dir(&self) -> PathBuf {
        self.config.read().await.download_dir.clone()
    }

    /// A consistent view of everything, for rendering.
    pub async fn snapshot(&self) -> NodeSnapshot {
        let telemetry = self.telemetry();
        let shared = self.shared();
        let mut pool = self.registry.pool_totals().await;
        pool.add_self(&telemetry, &shared);

        // `watch::Ref` wraps a std RwLock guard and is therefore !Send. Read
        // it into a plain bool up front: held across the `.await`s below it
        // would make this future !Send and reject it as an axum handler.
        let sharing_enabled = self.policy_tx.borrow().enabled;

        let peers_snapshot = self.registry.list().await;
        let online_peers = peers_snapshot.iter().filter(|p| p.online).count();
        let chats = self.chat.recent(200).await;
        let transfers = self.transfers.list().await;
        let calls = self.voice.sessions().await;
        let download_dir = self.config.read().await.download_dir.display().to_string();

        NodeSnapshot {
            identity: self.identity(),
            host: self.host.clone().into(),
            ports: self.ports,
            ip: iface::local_ip_string(),
            dashboard_url: self.dashboard_url(),
            telemetry,
            shared,
            sharing_enabled,
            peers: peers_snapshot,
            online_peers,
            pool,
            chats,
            transfers,
            calls,
            download_dir,
            uptime_secs: self.uptime_secs(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            captured_at: Utc::now(),
        }
    }

    // ── Actions ──────────────────────────────────────────────────────────────

    /// Broadcast a message to every online peer.
    pub async fn broadcast(&self, text: &str) -> Result<CommandOutcome> {
        let identity = self.identity();
        let message = compose(&identity.node_id, &identity.username, text, None);

        // Show our own message locally first: waiting for the network would
        // make typing feel laggy, and the message is ours regardless.
        self.chat.ingest(message.clone()).await;
        self.events.publish(NodeEvent::Chat(message.clone()));
        self.presence.touch(&identity.node_id).await;

        let outcome = messaging::broadcast_chat(&self.registry, &message).await;
        for (peer, error) in &outcome.failed {
            tracing::warn!(peer, error, "chat delivery failed");
        }

        Ok(if outcome.is_total_failure() {
            CommandOutcome::warning(format!("Sent, but {}", outcome.summary()))
        } else {
            CommandOutcome::success(outcome.summary())
        })
    }

    /// Send a private message to one peer.
    pub async fn direct_message(&self, peer_ref: &str, text: &str) -> Result<CommandOutcome> {
        let Some(peer) = self.registry.resolve(peer_ref).await else {
            return Ok(CommandOutcome::error(format!(
                "No peer matching \"{peer_ref}\""
            )));
        };
        let addr = match peer.messaging_addr() {
            Some(addr) => addr,
            None => return Ok(CommandOutcome::error("That peer has no reachable address")),
        };

        let identity = self.identity();
        let message = compose(
            &identity.node_id,
            &identity.username,
            text,
            Some(peer.node_id.clone()),
        );

        self.chat.ingest(message.clone()).await;
        self.events.publish(NodeEvent::Chat(message.clone()));

        match messaging::send_chat_to(addr, &message).await {
            Ok(()) => Ok(CommandOutcome::success(format!(
                "Sent to {}",
                peer.display_name()
            ))),
            Err(e) => Ok(CommandOutcome::error(format!(
                "Could not reach {}: {e:#}",
                peer.display_name()
            ))),
        }
    }

    /// Send a file to a peer. Returns once the transfer starts.
    pub async fn send_file(&self, peer_ref: &str, path: &Path) -> Result<CommandOutcome> {
        let Some(peer) = self.registry.resolve(peer_ref).await else {
            return Ok(CommandOutcome::error(format!(
                "No peer matching \"{peer_ref}\""
            )));
        };
        if !path.exists() {
            return Ok(CommandOutcome::error(format!(
                "No such file: {}",
                path.display()
            )));
        }
        let Some(addr) = peer.file_addr() else {
            return Ok(CommandOutcome::error(
                "That peer has no reachable file service",
            ));
        };

        let identity = self.identity();
        let transfers = self.transfers.clone();
        let events = self.events.clone();
        let path = path.to_path_buf();
        let peer_name = peer.display_name().to_string();
        let peer_id = peer.node_id.clone();
        let filename = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();

        // Transfers can take minutes; run in the background so the caller —
        // a keystroke handler or an HTTP request — is never blocked.
        let announced = filename.clone();
        tokio::spawn(async move {
            match crate::files::transfer::send_file(
                addr,
                &path,
                &identity.node_id,
                &identity.username,
                &peer_id,
                &peer_name,
                transfers,
            )
            .await
            {
                Ok(_) => events.publish(NodeEvent::success(format!(
                    "Sent \"{filename}\" to {peer_name}"
                ))),
                Err(e) => events.publish(NodeEvent::error(format!(
                    "Could not send \"{filename}\" to {peer_name}: {e:#}"
                ))),
            }
        });

        Ok(CommandOutcome::info(format!(
            "Sending \"{announced}\" to {}",
            peer.display_name()
        )))
    }

    /// Nickname a peer, persisting it.
    pub async fn set_alias(&self, peer_ref: &str, alias: Option<String>) -> Result<CommandOutcome> {
        let Some(peer) = self.registry.resolve(peer_ref).await else {
            return Ok(CommandOutcome::error(format!(
                "No peer matching \"{peer_ref}\""
            )));
        };

        self.registry.set_alias(&peer.node_id, alias.clone()).await;
        self.mutate_config(|config| match &alias {
            Some(alias) => {
                config.aliases.insert(peer.node_id.clone(), alias.clone());
            }
            None => {
                config.aliases.remove(&peer.node_id);
            }
        })
        .await?;

        Ok(match alias {
            Some(alias) => CommandOutcome::success(format!("{} is now \"{alias}\"", peer.username)),
            None => CommandOutcome::success(format!("Cleared the alias for {}", peer.username)),
        })
    }

    /// Turn resource sharing on or off.
    pub async fn set_sharing(&self, enabled: bool) -> Result<CommandOutcome> {
        let fraction = self.policy_tx.borrow().fraction;
        let _ = self.policy_tx.send(SharingPolicy { enabled, fraction });
        self.mutate_config(|config| config.sharing_enabled = enabled)
            .await?;

        Ok(CommandOutcome::success(if enabled {
            "Resource sharing on"
        } else {
            "Resource sharing off"
        }))
    }

    /// Change this node's display name.
    pub async fn rename(&self, username: &str) -> Result<CommandOutcome> {
        let username = username.trim();
        if username.is_empty() {
            return Ok(CommandOutcome::error("A name cannot be empty"));
        }

        let mut identity = self.identity();
        identity.username = username.to_string();
        let _ = self.identity_tx.send(identity.clone());
        self.mutate_config(|config| config.identity.username = username.to_string())
            .await?;

        // Beacon at once so peers see the new name immediately.
        let source = BeaconSource {
            identity: self.identity_rx.clone(),
            ports: self.ports,
            telemetry: self.telemetry_rx.clone(),
            shared: self.shared_rx.clone(),
        };
        let _ = discovery::announce_once(&source).await;

        Ok(CommandOutcome::success(format!(
            "You are now \"{username}\""
        )))
    }

    /// Start a voice call.
    pub async fn call(&self, peer_ref: &str) -> Result<CommandOutcome> {
        let Some(peer) = self.registry.resolve(peer_ref).await else {
            return Ok(CommandOutcome::error(format!(
                "No peer matching \"{peer_ref}\""
            )));
        };

        match self.voice.call(&self.registry, &peer.node_id).await {
            Ok(session_id) => Ok(CommandOutcome::success(format!(
                "In a call with {} (session {})",
                peer.display_name(),
                &session_id[..8.min(session_id.len())]
            ))),
            Err(e) => Ok(CommandOutcome::error(format!("Call failed: {e:#}"))),
        }
    }

    /// End a call. With no id, ends the only active one.
    pub async fn hang_up(&self, session: Option<&str>) -> Result<CommandOutcome> {
        let session_id = match session {
            Some(id) => id.to_string(),
            None => {
                let active = self.voice.active().await;
                match active.len() {
                    0 => return Ok(CommandOutcome::warning("No active call")),
                    1 => active[0].session_id.clone(),
                    n => {
                        return Ok(CommandOutcome::warning(format!(
                            "{n} calls are active — name one with /hangup <session>"
                        )))
                    }
                }
            }
        };

        match self.voice.hang_up(&self.registry, &session_id).await {
            Ok(()) => Ok(CommandOutcome::success("Call ended")),
            Err(e) => Ok(CommandOutcome::error(format!("{e:#}"))),
        }
    }

    /// Measure round-trip time to a peer.
    pub async fn ping(&self, peer_ref: &str) -> Result<CommandOutcome> {
        let Some(peer) = self.registry.resolve(peer_ref).await else {
            return Ok(CommandOutcome::error(format!(
                "No peer matching \"{peer_ref}\""
            )));
        };
        let Some(addr) = peer.messaging_addr() else {
            return Ok(CommandOutcome::error("That peer has no reachable address"));
        };

        match messaging::ping(addr).await {
            Ok(ms) => {
                self.registry.record_latency(&peer.node_id, ms).await;
                Ok(CommandOutcome::success(format!(
                    "{} replied in {ms:.1} ms",
                    peer.display_name()
                )))
            }
            Err(e) => Ok(CommandOutcome::error(format!(
                "{} did not reply: {e:#}",
                peer.display_name()
            ))),
        }
    }

    /// Run a parsed command. The single dispatch point for every front end.
    pub async fn execute(&self, command: Command) -> Result<CommandOutcome> {
        match command {
            Command::Broadcast(text) => self.broadcast(&text).await,
            Command::Direct { peer, message } => self.direct_message(&peer, &message).await,
            Command::Peers => Ok(CommandOutcome::Peers {
                peers: self.registry.list().await,
            }),
            Command::Alias { peer, alias } => self.set_alias(&peer, alias).await,
            Command::SendFile { peer, path } => self.send_file(&peer, &path).await,
            Command::Call { peer } => self.call(&peer).await,
            Command::HangUp { session } => self.hang_up(session.as_deref()).await,
            Command::Ping { peer } => self.ping(&peer).await,
            Command::Share(enabled) => self.set_sharing(enabled).await,
            Command::Rename(name) => self.rename(&name).await,
            Command::Clear => Ok(CommandOutcome::Clear),
            Command::Status => Ok(CommandOutcome::Status {
                snapshot: Box::new(self.snapshot().await),
            }),
            Command::Help => Ok(CommandOutcome::info(
                crate::chat::commands::HELP
                    .iter()
                    .map(|(syntax, description)| format!("  {syntax:<26} {description}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            Command::Quit => Ok(CommandOutcome::Quit),
            Command::Invalid(hint) => Ok(CommandOutcome::warning(hint)),
        }
    }

    /// Parse and run a line of input.
    pub async fn execute_line(&self, line: &str) -> Result<Option<CommandOutcome>> {
        match crate::chat::commands::parse(line) {
            Some(command) => self.execute(command).await.map(Some),
            None => Ok(None),
        }
    }

    // ── Lifecycle ────────────────────────────────────────────────────────────

    /// Apply a change to the config and persist it.
    async fn mutate_config(&self, f: impl FnOnce(&mut Config)) -> Result<()> {
        let snapshot = {
            let mut config = self.config.write().await;
            f(&mut config);
            config.clone()
        };
        // Written outside the lock: a slow disk must not stall the node.
        snapshot
            .save(&self.config_path)
            .with_context(|| format!("cannot save {}", self.config_path.display()))
    }

    pub async fn config(&self) -> Config {
        self.config.read().await.clone()
    }

    /// Read-only copy of this node's aliases.
    pub async fn aliases(&self) -> HashMap<String, String> {
        self.config.read().await.aliases.clone()
    }

    /// Stop every service, telling peers we are going.
    pub async fn shutdown(&self) {
        tracing::info!("shutting down");

        let identity = self.identity();
        messaging::broadcast_presence(
            &self.registry,
            &identity.node_id,
            &identity.username,
            &identity.device_type,
            PresenceStatus::Offline,
        )
        .await;

        let _ = self.shutdown_tx.send(());
        // A brief pause lets the goodbye and the shutdown signal reach their
        // destinations before the process exits.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }

    /// Resolve after [`shutdown`](Self::shutdown) is called from anywhere.
    pub async fn wait_for_shutdown(&self) {
        let mut rx = self.shutdown_tx.subscribe();
        let _ = rx.recv().await;
    }

    /// The protocol version this build speaks.
    pub const fn protocol_version() -> u8 {
        protocol::PROTOCOL_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::protocol::PortSet;

    /// A node on ports nothing else is using.
    ///
    /// The discovery port is shifted too, which `offset_by` deliberately does
    /// not do. Tests must not find each other — and must not find a real node
    /// running on the developer's own network.
    async fn test_node(offset: u16) -> (Node, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::generate();
        config.ports = PortSet::default().offset_by(offset);
        config.ports.discovery += offset;
        config.download_dir = dir.path().join("downloads");
        config.data_dir = dir.path().join("data");

        let node = Node::start(config, dir.path().join("config.json"))
            .await
            .unwrap();
        (node, dir)
    }

    #[tokio::test]
    async fn a_node_starts_and_reports_a_coherent_snapshot() {
        let (node, _dir) = test_node(200).await;
        let snapshot = node.snapshot().await;

        assert_eq!(snapshot.identity.node_id, node.node_id());
        assert_eq!(snapshot.ports.messaging, 9202);
        assert_eq!(snapshot.online_peers, 0);
        assert!(snapshot.dashboard_url.starts_with("http://"));
        // Our own resources are always in the pool, even alone.
        assert_eq!(snapshot.pool.nodes, 1);

        node.shutdown().await;
    }

    #[tokio::test]
    async fn a_broadcast_appears_locally_even_with_no_peers() {
        let (node, _dir) = test_node(210).await;

        let outcome = node.broadcast("hello, LAN").await.unwrap();
        assert!(matches!(outcome, CommandOutcome::Message { .. }));

        let chats = node.chat.recent(10).await;
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].content, "hello, LAN");

        node.shutdown().await;
    }

    #[tokio::test]
    async fn renaming_updates_the_identity_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let mut config = Config::generate();
        config.ports = PortSet::default().offset_by(220);
        config.ports.discovery += 220;
        config.download_dir = dir.path().join("downloads");
        config.data_dir = dir.path().join("data");
        let node_id = config.identity.node_id.clone();

        let node = Node::start(config, &config_path).await.unwrap();
        node.rename("workshop-nuc").await.unwrap();
        assert_eq!(node.identity().username, "workshop-nuc");
        node.shutdown().await;
        drop(node);

        let reloaded = Config::load_or_create(&config_path).unwrap();
        assert_eq!(reloaded.identity.username, "workshop-nuc");
        assert_eq!(
            reloaded.identity.node_id, node_id,
            "renaming must not change the id"
        );
    }

    #[tokio::test]
    async fn toggling_sharing_zeroes_the_advertised_offer() {
        let (node, _dir) = test_node(230).await;

        node.set_sharing(false).await.unwrap();
        // Let the sharing service observe the policy change.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert!(!node.shared().enabled);
        assert_eq!(node.shared().ram_offered_mb, 0);

        node.set_sharing(true).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert!(node.shared().enabled);

        node.shutdown().await;
    }

    #[tokio::test]
    async fn commands_against_unknown_peers_report_rather_than_fail() {
        let (node, _dir) = test_node(240).await;

        // Every one of these is a user error, not a node error: they must
        // come back as a message, never as an Err that unwinds a UI.
        for command in [
            Command::Direct {
                peer: "ghost".into(),
                message: "hi".into(),
            },
            Command::SendFile {
                peer: "ghost".into(),
                path: PathBuf::from("nope.txt"),
            },
            Command::Call {
                peer: "ghost".into(),
            },
            Command::Ping {
                peer: "ghost".into(),
            },
            Command::Alias {
                peer: "ghost".into(),
                alias: Some("x".into()),
            },
        ] {
            match node.execute(command.clone()).await.unwrap() {
                CommandOutcome::Message { level, text } => {
                    assert_eq!(level, NoticeLevel::Error, "for {command:?}: {text}");
                }
                other => panic!("expected a message for {command:?}, got {other:?}"),
            }
        }

        node.shutdown().await;
    }

    #[tokio::test]
    async fn help_lists_every_command() {
        let (node, _dir) = test_node(250).await;

        let text = node.execute(Command::Help).await.unwrap().as_text();
        for (syntax, _) in crate::chat::commands::HELP {
            assert!(text.contains(syntax), "help omits {syntax}");
        }

        node.shutdown().await;
    }

    #[tokio::test]
    async fn chat_history_is_restored_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let mut config = Config::generate();
        config.ports = PortSet::default().offset_by(260);
        config.ports.discovery += 260;
        config.download_dir = dir.path().join("downloads");
        config.data_dir = dir.path().join("data");

        let node = Node::start(config.clone(), &config_path).await.unwrap();
        node.broadcast("remember me").await.unwrap();
        // Let the persistence task drain the event.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        node.shutdown().await;
        drop(node);

        let node = Node::start(config, &config_path).await.unwrap();
        let chats = node.chat.recent(10).await;
        assert_eq!(chats.len(), 1, "history should survive a restart");
        assert_eq!(chats[0].content, "remember me");
        node.shutdown().await;
    }
}
