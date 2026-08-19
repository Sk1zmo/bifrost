//! Chat and control transport over TCP.
//!
//! Direct peer-to-peer connections, no gossip layer and no relay. `Bifrost v2`
//! carried chat over libp2p gossipsub, which worked but pulled in a large
//! dependency tree, needed a second peer-identity scheme alongside our node
//! ids, and made Android/iOS builds fragile. On a single LAN segment every
//! peer is directly reachable, so a plain framed TCP connection does the same
//! job with a fraction of the moving parts.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::net::{TcpListener, TcpStream};

use crate::chat::history::ChatStore;
use crate::events::{EventBus, NodeEvent};
use crate::net::frame::{read_frame, write_frame};
use crate::net::peers::PeerRegistry;
use crate::net::protocol::{ChatPayload, Envelope, PresenceStatus};

/// Connect timeout. Deliberately short: a peer on the same LAN either answers
/// almost immediately or is gone, and a long timeout would stall a broadcast
/// behind one sleeping laptop.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(2500);

/// Read/write timeout once connected.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared context for the messaging server.
#[derive(Clone, Debug)]
pub struct MessagingContext {
    pub node_id: String,
    pub registry: PeerRegistry,
    pub chat: ChatStore,
    pub events: EventBus,
}

/// Accept inbound messaging connections until `shutdown` fires.
pub async fn run_server(
    bind: SocketAddr,
    ctx: MessagingContext,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind the messaging port {bind}"))?;
    tracing::info!(%bind, "messaging listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, from)) => {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, from, ctx).await {
                                tracing::debug!(%from, error = %e, "messaging connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "messaging accept failed");
                        // Back off so a persistently failing accept (e.g. the
                        // fd table being full) cannot spin the CPU.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("messaging shutting down");
                return Ok(());
            }
        }
    }
}

/// Handle one connection, which may carry any number of framed messages.
async fn serve_connection(
    mut stream: TcpStream,
    from: SocketAddr,
    ctx: MessagingContext,
) -> Result<()> {
    let _ = stream.set_nodelay(true);

    while let Some(envelope) =
        tokio::time::timeout(IO_TIMEOUT, read_frame::<_, Envelope>(&mut stream))
            .await
            .context("timed out waiting for a message")??
    {
        match envelope {
            Envelope::Chat(payload) => {
                handle_chat(payload, &ctx).await;
            }
            Envelope::Ping => {
                write_frame(&mut stream, &Envelope::Pong).await?;
            }
            Envelope::Pong => {}
            Envelope::Telemetry(payload) => {
                // Beacons already carry telemetry; this path exists so a peer
                // can push an update out of band, e.g. right after a job.
                tracing::trace!(node_id = %payload.node_id, "telemetry push received");
            }
            Envelope::Presence(payload) => {
                if payload.status == PresenceStatus::Offline {
                    ctx.events.publish(NodeEvent::PeerOffline {
                        node_id: payload.node_id.clone(),
                        display_name: payload.username.clone(),
                    });
                }
            }
            Envelope::FileOffer(offer) => {
                ctx.events.publish(NodeEvent::info(format!(
                    "{} is offering \"{}\"",
                    offer.sender_name, offer.filename
                )));
            }
        }
    }

    tracing::trace!(%from, "messaging connection closed");
    Ok(())
}

async fn handle_chat(payload: ChatPayload, ctx: &MessagingContext) {
    // A direct message addressed to somebody else is not ours to display.
    if let Some(to) = &payload.recipient_id {
        if to != &ctx.node_id {
            return;
        }
    }

    if ctx.chat.ingest(payload.clone()).await {
        tracing::info!(from = %payload.sender_name, "{}", payload.content);
        ctx.events.publish(NodeEvent::Chat(payload));
    }
}

/// Open a connection and send one envelope.
async fn send_envelope(addr: SocketAddr, envelope: &Envelope) -> Result<TcpStream> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("timed out connecting to {addr}"))?
        .with_context(|| format!("could not connect to {addr}"))?;

    let _ = stream.set_nodelay(true);
    tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, envelope))
        .await
        .with_context(|| format!("timed out sending to {addr}"))??;

    Ok(stream)
}

/// Deliver one chat message to one peer.
pub async fn send_chat_to(addr: SocketAddr, message: &ChatPayload) -> Result<()> {
    send_envelope(addr, &Envelope::Chat(message.clone())).await?;
    Ok(())
}

/// Outcome of a broadcast: who got it, and who did not.
#[derive(Debug, Clone, Default)]
pub struct BroadcastOutcome {
    pub delivered: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl BroadcastOutcome {
    pub fn is_total_failure(&self) -> bool {
        self.delivered.is_empty() && !self.failed.is_empty()
    }

    /// One-line summary for the UI.
    pub fn summary(&self) -> String {
        match (self.delivered.len(), self.failed.len()) {
            (0, 0) => "no peers online".to_string(),
            (n, 0) => format!("delivered to {n} peer{}", if n == 1 { "" } else { "s" }),
            (0, f) => format!("delivery failed to all {f} peer(s)"),
            (n, f) => format!("delivered to {n}, failed for {f}"),
        }
    }
}

/// Send a chat message to every online peer, concurrently.
///
/// Peers are contacted in parallel so one unresponsive machine cannot hold up
/// delivery to the rest — `Bifrost v3`'s `broadcast_chat` looped sequentially
/// and blocked the whole broadcast for the full connect timeout of each dead
/// peer in turn.
pub async fn broadcast_chat(registry: &PeerRegistry, message: &ChatPayload) -> BroadcastOutcome {
    let peers = registry.online().await;
    let mut tasks = Vec::with_capacity(peers.len());

    for peer in peers {
        let Some(addr) = peer.messaging_addr() else {
            continue;
        };
        let message = message.clone();
        let name = peer.display_name().to_string();
        tasks.push(tokio::spawn(async move {
            match send_chat_to(addr, &message).await {
                Ok(()) => (name, None),
                Err(e) => (name, Some(e.to_string())),
            }
        }));
    }

    let mut outcome = BroadcastOutcome::default();
    for task in tasks {
        match task.await {
            Ok((name, None)) => outcome.delivered.push(name),
            Ok((name, Some(err))) => outcome.failed.push((name, err)),
            Err(e) => outcome.failed.push(("<unknown>".into(), e.to_string())),
        }
    }

    outcome
}

/// Announce a presence change to every online peer.
pub async fn broadcast_presence(
    registry: &PeerRegistry,
    node_id: &str,
    username: &str,
    device_type: &str,
    status: PresenceStatus,
) {
    let envelope = Envelope::Presence(crate::net::protocol::PresencePayload {
        node_id: node_id.to_string(),
        username: username.to_string(),
        device_type: device_type.to_string(),
        status,
        timestamp: Utc::now(),
    });

    for peer in registry.online().await {
        if let Some(addr) = peer.messaging_addr() {
            let envelope = envelope.clone();
            tokio::spawn(async move {
                let _ = send_envelope(addr, &envelope).await;
            });
        }
    }
}

/// Ping a peer and return the round-trip time in milliseconds.
pub async fn ping(addr: SocketAddr) -> Result<f64> {
    let started = Instant::now();
    let mut stream = send_envelope(addr, &Envelope::Ping).await?;

    let reply: Option<Envelope> = tokio::time::timeout(IO_TIMEOUT, read_frame(&mut stream))
        .await
        .context("timed out waiting for a pong")??;

    match reply {
        Some(Envelope::Pong) => Ok(started.elapsed().as_secs_f64() * 1000.0),
        Some(other) => anyhow::bail!("expected a Pong, got {other:?}"),
        None => anyhow::bail!("peer closed the connection without replying"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::history::compose;

    /// Start a messaging server on an ephemeral port for one test.
    async fn spawn_server(
        node_id: &str,
    ) -> (
        SocketAddr,
        MessagingContext,
        tokio::sync::broadcast::Sender<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // release the port; run_server rebinds it

        let ctx = MessagingContext {
            node_id: node_id.to_string(),
            registry: PeerRegistry::new(),
            chat: ChatStore::new(),
            events: EventBus::new(),
        };
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);

        let server_ctx = ctx.clone();
        tokio::spawn(async move {
            let _ = run_server(addr, server_ctx, rx).await;
        });

        // Give the listener a moment to bind before the test connects.
        tokio::time::sleep(Duration::from_millis(120)).await;
        (addr, ctx, shutdown)
    }

    #[tokio::test]
    async fn a_chat_message_reaches_the_receiver_history() {
        let (addr, ctx, _shutdown) = spawn_server("receiver").await;

        let msg = compose("sender", "alice", "hello over TCP", None);
        send_chat_to(addr, &msg).await.expect("send should succeed");

        tokio::time::sleep(Duration::from_millis(150)).await;
        let history = ctx.chat.recent(10).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "hello over TCP");
    }

    #[tokio::test]
    async fn ping_returns_a_round_trip_time() {
        let (addr, _ctx, _shutdown) = spawn_server("receiver").await;

        let rtt = ping(addr).await.expect("ping should succeed");
        assert!((0.0..5000.0).contains(&rtt), "implausible rtt: {rtt}");
    }

    #[tokio::test]
    async fn a_direct_message_for_someone_else_is_not_stored() {
        let (addr, ctx, _shutdown) = spawn_server("me").await;

        let for_other = compose("sender", "alice", "private", Some("someone-else".into()));
        send_chat_to(addr, &for_other).await.unwrap();

        let for_us = compose("sender", "alice", "for you", Some("me".into()));
        send_chat_to(addr, &for_us).await.unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;
        let history = ctx.chat.recent(10).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "for you");
    }

    #[tokio::test]
    async fn several_messages_survive_one_connection() {
        let (addr, ctx, _shutdown) = spawn_server("receiver").await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        for i in 0..5 {
            let msg = compose("sender", "alice", &format!("msg {i}"), None);
            write_frame(&mut stream, &Envelope::Chat(msg))
                .await
                .unwrap();
        }
        drop(stream);

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(ctx.chat.len().await, 5);
    }

    #[tokio::test]
    async fn broadcasting_with_no_peers_reports_cleanly() {
        let registry = PeerRegistry::new();
        let outcome = broadcast_chat(&registry, &compose("me", "me", "hi", None)).await;

        assert!(outcome.delivered.is_empty());
        assert!(outcome.failed.is_empty());
        assert!(!outcome.is_total_failure());
        assert_eq!(outcome.summary(), "no peers online");
    }

    #[tokio::test]
    async fn connecting_to_a_dead_port_fails_fast() {
        // Bind then drop, so the port is almost certainly closed.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let started = Instant::now();
        let result = send_chat_to(addr, &compose("me", "me", "hi", None)).await;
        assert!(result.is_err());
        assert!(
            started.elapsed() < CONNECT_TIMEOUT + Duration::from_secs(2),
            "failure should be bounded by the connect timeout"
        );
    }
}
