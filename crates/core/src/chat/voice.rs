//! Voice calls: TCP signalling plus a LAN UDP audio path.
//!
//! Signalling (port 9004) is a framed JSON exchange — `CallRequest`,
//! `CallAccept`/`CallReject`, optional `SdpOffer`/`SdpAnswer`/`IceCandidate`,
//! then `CallEnd`. Audio (port 9005) is 20 ms mono 16 kHz PCM frames sent
//! straight to the peer, with the header from
//! [`crate::net::protocol::audio_header`].
//!
//! Audio *transport* lives here; audio *capture* does not. Pulling in a
//! cross-platform microphone library would add ALSA headers as a hard Linux
//! build dependency and break the one-command install, so instead every client
//! feeds frames in through [`VoiceService::send_frame`]: the mobile apps from
//! the platform mic, the web dashboard from `getUserMedia`. Received frames
//! are buffered by [`VoiceService::take_incoming`] for whatever is playing
//! them back. The `data` field on a signal carries SDP untouched, so a client
//! with a real WebRTC stack can negotiate through the same exchange.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::events::{EventBus, NodeEvent};
use crate::net::frame::{read_frame, write_frame};
use crate::net::peers::PeerRegistry;
use crate::net::protocol::{
    audio_header, parse_audio_datagram, AudioCodec, VoiceSignal, VoiceSignalKind,
    AUDIO_FRAME_SAMPLES, AUDIO_HEADER_LEN,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(20);

/// Audio frames buffered per session before the oldest is dropped.
///
/// 50 frames is one second at 20 ms. Dropping beyond that is correct for
/// real-time audio: stale frames are worse than a gap.
const JITTER_BUFFER_FRAMES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Ringing,
    Connected,
    Ended,
    Rejected,
    Failed,
}

/// One call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSession {
    pub session_id: String,
    pub peer_id: String,
    pub peer_name: String,
    /// True when this node placed the call.
    pub outgoing: bool,
    pub state: CallState,
    /// Where our audio is sent.
    pub remote_audio: Option<String>,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl CallSession {
    pub fn duration_secs(&self) -> i64 {
        (self.ended_at.unwrap_or_else(Utc::now) - self.started_at).num_seconds()
    }
}

#[derive(Debug)]
struct Inner {
    sessions: HashMap<String, CallSession>,
    /// Received audio, per session, oldest first.
    inbound: HashMap<String, VecDeque<Vec<u8>>>,
    /// Outbound sequence counter, per session.
    sequence: HashMap<String, u64>,
    /// Remote audio address → session id, to attribute inbound datagrams.
    by_remote_ip: HashMap<String, String>,
}

/// Call state and the audio path, shared across services.
#[derive(Clone, Debug)]
pub struct VoiceService {
    inner: Arc<RwLock<Inner>>,
    node_id: String,
    audio_port: u16,
    events: EventBus,
}

impl VoiceService {
    pub fn new(node_id: String, audio_port: u16, events: EventBus) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                sessions: HashMap::new(),
                inbound: HashMap::new(),
                sequence: HashMap::new(),
                by_remote_ip: HashMap::new(),
            })),
            node_id,
            audio_port,
            events,
        }
    }

    pub async fn sessions(&self) -> Vec<CallSession> {
        let mut all: Vec<CallSession> =
            self.inner.read().await.sessions.values().cloned().collect();
        all.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        all
    }

    pub async fn active(&self) -> Vec<CallSession> {
        self.inner
            .read()
            .await
            .sessions
            .values()
            .filter(|s| matches!(s.state, CallState::Ringing | CallState::Connected))
            .cloned()
            .collect()
    }

    pub async fn get(&self, session_id: &str) -> Option<CallSession> {
        self.inner.read().await.sessions.get(session_id).cloned()
    }

    async fn upsert(&self, session: CallSession) {
        let mut inner = self.inner.write().await;
        if let Some(remote) = &session.remote_audio {
            if let Some(ip) = remote.rsplit_once(':').map(|(ip, _)| ip.to_string()) {
                inner.by_remote_ip.insert(ip, session.session_id.clone());
            }
        }
        inner.sessions.insert(session.session_id.clone(), session);
    }

    async fn set_state(&self, session_id: &str, state: CallState) {
        let mut inner = self.inner.write().await;
        if let Some(session) = inner.sessions.get_mut(session_id) {
            session.state = state;
            if matches!(
                state,
                CallState::Ended | CallState::Rejected | CallState::Failed
            ) {
                session.ended_at = Some(Utc::now());
            }
        }
    }

    // ── Placing a call ───────────────────────────────────────────────────────

    /// Call a peer. Returns the session id once the peer accepts.
    pub async fn call(&self, registry: &PeerRegistry, peer_id: &str) -> Result<String> {
        let peer = registry
            .resolve(peer_id)
            .await
            .with_context(|| format!("no peer matching \"{peer_id}\""))?;
        let signal_addr = peer
            .voice_signal_addr()
            .context("peer did not advertise a voice port")?;

        let session_id = Uuid::new_v4().to_string();
        self.upsert(CallSession {
            session_id: session_id.clone(),
            peer_id: peer.node_id.clone(),
            peer_name: peer.display_name().to_string(),
            outgoing: true,
            state: CallState::Ringing,
            remote_audio: peer.voice_audio_addr().map(|a| a.to_string()),
            frames_sent: 0,
            frames_received: 0,
            started_at: Utc::now(),
            ended_at: None,
        })
        .await;

        self.events.publish(NodeEvent::Voice {
            session_id: session_id.clone(),
            peer_id: peer.node_id.clone(),
            kind: VoiceSignalKind::CallRequest,
        });

        let request = VoiceSignal {
            kind: VoiceSignalKind::CallRequest,
            session_id: session_id.clone(),
            caller_id: self.node_id.clone(),
            callee_id: peer.node_id.clone(),
            audio_port: self.audio_port,
            data: None,
            timestamp: Utc::now(),
        };

        let reply = match self.exchange(signal_addr, &request).await {
            Ok(reply) => reply,
            Err(e) => {
                self.set_state(&session_id, CallState::Failed).await;
                return Err(e);
            }
        };

        match reply.map(|r| r.kind) {
            Some(VoiceSignalKind::CallAccept) => {
                self.set_state(&session_id, CallState::Connected).await;
                self.events.publish(NodeEvent::Voice {
                    session_id: session_id.clone(),
                    peer_id: peer.node_id.clone(),
                    kind: VoiceSignalKind::CallAccept,
                });
                tracing::info!(peer = %peer.display_name(), "call connected");
                Ok(session_id)
            }
            Some(VoiceSignalKind::CallReject) => {
                self.set_state(&session_id, CallState::Rejected).await;
                bail!("{} declined the call", peer.display_name())
            }
            other => {
                self.set_state(&session_id, CallState::Failed).await;
                bail!("unexpected reply to the call request: {other:?}")
            }
        }
    }

    /// End a call and tell the peer.
    pub async fn hang_up(&self, registry: &PeerRegistry, session_id: &str) -> Result<()> {
        let session = self.get(session_id).await.context("no such call")?;
        self.set_state(session_id, CallState::Ended).await;

        self.events.publish(NodeEvent::Voice {
            session_id: session_id.to_string(),
            peer_id: session.peer_id.clone(),
            kind: VoiceSignalKind::CallEnd,
        });

        if let Some(peer) = registry.get(&session.peer_id).await {
            if let Some(addr) = peer.voice_signal_addr() {
                let signal = VoiceSignal {
                    kind: VoiceSignalKind::CallEnd,
                    session_id: session_id.to_string(),
                    caller_id: self.node_id.clone(),
                    callee_id: session.peer_id.clone(),
                    audio_port: self.audio_port,
                    data: None,
                    timestamp: Utc::now(),
                };
                // Best-effort: the call is over locally whatever the peer does.
                let _ = self.exchange(addr, &signal).await;
            }
        }

        let mut inner = self.inner.write().await;
        inner.inbound.remove(session_id);
        inner.sequence.remove(session_id);
        Ok(())
    }

    /// Open a connection, send one signal, read an optional reply.
    async fn exchange(
        &self,
        addr: SocketAddr,
        signal: &VoiceSignal,
    ) -> Result<Option<VoiceSignal>> {
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .with_context(|| format!("timed out connecting to {addr}"))?
            .with_context(|| format!("cannot reach the voice service at {addr}"))?;
        let _ = stream.set_nodelay(true);

        write_frame(&mut stream, signal).await?;
        let reply = tokio::time::timeout(SIGNAL_TIMEOUT, read_frame(&mut stream))
            .await
            .context("timed out waiting for the peer to answer")??;
        Ok(reply)
    }

    // ── Audio ────────────────────────────────────────────────────────────────

    /// Send one captured audio frame to the call's peer.
    ///
    /// `pcm` is signed 16-bit little-endian mono at
    /// [`AUDIO_SAMPLE_RATE`](crate::net::protocol::AUDIO_SAMPLE_RATE).
    pub async fn send_frame(&self, session_id: &str, codec: AudioCodec, pcm: &[u8]) -> Result<()> {
        let (remote, sequence) = {
            let mut inner = self.inner.write().await;
            let session = inner
                .sessions
                .get(session_id)
                .filter(|s| s.state == CallState::Connected)
                .context("no connected call with that id")?;
            let remote: SocketAddr = session
                .remote_audio
                .as_ref()
                .context("the call has no audio address")?
                .parse()
                .context("the call's audio address is malformed")?;

            let sequence = inner.sequence.entry(session_id.to_string()).or_insert(0);
            *sequence += 1;
            let seq = *sequence;

            if let Some(session) = inner.sessions.get_mut(session_id) {
                session.frames_sent += 1;
            }
            (remote, seq)
        };

        let mut datagram = Vec::with_capacity(AUDIO_HEADER_LEN + pcm.len());
        datagram.extend_from_slice(&audio_header(codec, sequence));
        datagram.extend_from_slice(pcm);

        let socket = UdpSocket::bind(("0.0.0.0", 0)).await?;
        socket
            .send_to(&datagram, remote)
            .await
            .context("failed to send the audio frame")?;
        Ok(())
    }

    /// Take every buffered inbound frame for a session, oldest first.
    pub async fn take_incoming(&self, session_id: &str) -> Vec<Vec<u8>> {
        let mut inner = self.inner.write().await;
        inner
            .inbound
            .get_mut(session_id)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Receive audio datagrams until `shutdown` fires.
    pub async fn run_audio_listener(
        &self,
        bind: SocketAddr,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) -> Result<()> {
        let socket = UdpSocket::bind(bind)
            .await
            .with_context(|| format!("failed to bind the voice audio port {bind}"))?;
        tracing::info!(%bind, "voice audio listening");

        // 20 ms of 16-bit mono at 16 kHz is 640 bytes; 4 KiB leaves headroom
        // for a client sending larger frames or Opus packets.
        let mut buf = vec![0u8; 4096];

        loop {
            tokio::select! {
                result = socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, from)) => self.ingest_audio(&buf[..len], from).await,
                        Err(e) => tracing::debug!(error = %e, "voice audio recv error, continuing"),
                    }
                }
                _ = shutdown.recv() => {
                    tracing::info!("voice audio shutting down");
                    return Ok(());
                }
            }
        }
    }

    async fn ingest_audio(&self, datagram: &[u8], from: SocketAddr) {
        let Some((_codec, _seq, payload)) = parse_audio_datagram(datagram) else {
            return; // not ours
        };

        let mut inner = self.inner.write().await;
        let Some(session_id) = inner.by_remote_ip.get(&from.ip().to_string()).cloned() else {
            tracing::trace!(%from, "audio from an unknown peer, dropped");
            return;
        };

        if let Some(session) = inner.sessions.get_mut(&session_id) {
            if session.state != CallState::Connected {
                return;
            }
            session.frames_received += 1;
        }

        let queue = inner.inbound.entry(session_id).or_default();
        queue.push_back(payload.to_vec());
        // Drop the oldest rather than growing: for live audio, a late frame is
        // worse than a missing one.
        while queue.len() > JITTER_BUFFER_FRAMES {
            queue.pop_front();
        }
    }
}

// ─── Signalling server ───────────────────────────────────────────────────────

/// Accept inbound signalling until `shutdown` fires.
pub async fn run_signal_server(
    bind: SocketAddr,
    voice: VoiceService,
    registry: PeerRegistry,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind the voice signalling port {bind}"))?;
    tracing::info!(%bind, "voice signalling listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, from)) => {
                        let voice = voice.clone();
                        let registry = registry.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_signal(stream, from, voice, registry).await {
                                tracing::debug!(%from, error = %e, "voice signalling ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "voice signalling accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
            _ = shutdown.recv() => {
                tracing::info!("voice signalling shutting down");
                return Ok(());
            }
        }
    }
}

async fn handle_signal(
    mut stream: TcpStream,
    from: SocketAddr,
    voice: VoiceService,
    registry: PeerRegistry,
) -> Result<()> {
    let _ = stream.set_nodelay(true);

    while let Some(signal) = read_frame::<_, VoiceSignal>(&mut stream).await? {
        voice.events.publish(NodeEvent::Voice {
            session_id: signal.session_id.clone(),
            peer_id: signal.caller_id.clone(),
            kind: signal.kind,
        });

        match signal.kind {
            VoiceSignalKind::CallRequest => {
                let caller = registry.get(&signal.caller_id).await;
                let caller_name = caller
                    .as_ref()
                    .map(|p| p.display_name().to_string())
                    .unwrap_or_else(|| signal.caller_id.clone());

                // Audio goes to the port the caller asked for, at the address
                // its connection actually came from.
                let remote_audio = format!("{}:{}", from.ip(), signal.audio_port);

                voice
                    .upsert(CallSession {
                        session_id: signal.session_id.clone(),
                        peer_id: signal.caller_id.clone(),
                        peer_name: caller_name.clone(),
                        outgoing: false,
                        state: CallState::Connected,
                        remote_audio: Some(remote_audio),
                        frames_sent: 0,
                        frames_received: 0,
                        started_at: Utc::now(),
                        ended_at: None,
                    })
                    .await;

                tracing::info!(caller = %caller_name, "incoming call, accepted");
                voice
                    .events
                    .publish(NodeEvent::info(format!("Incoming call from {caller_name}")));

                // Phase 1 auto-accepts, as both legacy trees did. A client that
                // wants to prompt first can reply CallReject instead.
                write_frame(
                    &mut stream,
                    &VoiceSignal {
                        kind: VoiceSignalKind::CallAccept,
                        session_id: signal.session_id.clone(),
                        caller_id: voice.node_id.clone(),
                        callee_id: signal.caller_id.clone(),
                        audio_port: voice.audio_port,
                        data: None,
                        timestamp: Utc::now(),
                    },
                )
                .await?;
            }
            VoiceSignalKind::SdpOffer => {
                // Bifrost does not run a media engine; the offer is echoed back
                // as an answer so a client that does can complete the exchange
                // through us, and `data` is preserved untouched.
                write_frame(
                    &mut stream,
                    &VoiceSignal {
                        kind: VoiceSignalKind::SdpAnswer,
                        session_id: signal.session_id.clone(),
                        caller_id: voice.node_id.clone(),
                        callee_id: signal.caller_id.clone(),
                        audio_port: voice.audio_port,
                        data: signal.data.clone(),
                        timestamp: Utc::now(),
                    },
                )
                .await?;
            }
            VoiceSignalKind::CallEnd => {
                voice.set_state(&signal.session_id, CallState::Ended).await;
                voice.events.publish(NodeEvent::info(format!(
                    "Call ended by {}",
                    signal.caller_id
                )));
            }
            VoiceSignalKind::CallReject => {
                voice
                    .set_state(&signal.session_id, CallState::Rejected)
                    .await;
            }
            VoiceSignalKind::CallAccept
            | VoiceSignalKind::SdpAnswer
            | VoiceSignalKind::IceCandidate => {
                tracing::debug!(kind = ?signal.kind, "voice signal noted");
            }
        }
    }

    Ok(())
}

/// A 20 ms frame of silence, useful for keeping a stream alive.
pub fn silent_frame() -> Vec<u8> {
    vec![0u8; AUDIO_FRAME_SAMPLES * 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> VoiceService {
        VoiceService::new("me".into(), 9005, EventBus::new())
    }

    #[tokio::test]
    async fn a_session_becomes_visible_once_registered() {
        let voice = service();
        voice
            .upsert(CallSession {
                session_id: "s1".into(),
                peer_id: "p1".into(),
                peer_name: "alice".into(),
                outgoing: true,
                state: CallState::Connected,
                remote_audio: Some("192.168.1.5:9005".into()),
                frames_sent: 0,
                frames_received: 0,
                started_at: Utc::now(),
                ended_at: None,
            })
            .await;

        assert_eq!(voice.active().await.len(), 1);
        assert_eq!(voice.get("s1").await.unwrap().peer_name, "alice");

        voice.set_state("s1", CallState::Ended).await;
        assert!(voice.active().await.is_empty());
        assert!(voice.get("s1").await.unwrap().ended_at.is_some());
    }

    #[tokio::test]
    async fn inbound_audio_is_attributed_to_the_session_by_source_address() {
        let voice = service();
        voice
            .upsert(CallSession {
                session_id: "s1".into(),
                peer_id: "p1".into(),
                peer_name: "alice".into(),
                outgoing: true,
                state: CallState::Connected,
                remote_audio: Some("192.168.1.5:9005".into()),
                frames_sent: 0,
                frames_received: 0,
                started_at: Utc::now(),
                ended_at: None,
            })
            .await;

        let mut datagram = audio_header(AudioCodec::Pcm16Le, 1).to_vec();
        datagram.extend_from_slice(&[7u8; 640]);
        // A different source port than the one advertised, as NAT-free LAN
        // senders routinely use an ephemeral port.
        let from: SocketAddr = "192.168.1.5:54321".parse().unwrap();
        voice.ingest_audio(&datagram, from).await;

        let frames = voice.take_incoming("s1").await;
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 640);
        assert_eq!(voice.get("s1").await.unwrap().frames_received, 1);

        // Draining is destructive.
        assert!(voice.take_incoming("s1").await.is_empty());
    }

    #[tokio::test]
    async fn audio_from_an_unknown_peer_is_dropped() {
        let voice = service();
        let mut datagram = audio_header(AudioCodec::Pcm16Le, 1).to_vec();
        datagram.extend_from_slice(&[0u8; 640]);

        voice
            .ingest_audio(&datagram, "10.9.9.9:9005".parse().unwrap())
            .await;
        assert!(voice.take_incoming("s1").await.is_empty());
    }

    #[tokio::test]
    async fn the_jitter_buffer_drops_the_oldest_frames() {
        let voice = service();
        voice
            .upsert(CallSession {
                session_id: "s1".into(),
                peer_id: "p1".into(),
                peer_name: "alice".into(),
                outgoing: true,
                state: CallState::Connected,
                remote_audio: Some("192.168.1.5:9005".into()),
                frames_sent: 0,
                frames_received: 0,
                started_at: Utc::now(),
                ended_at: None,
            })
            .await;

        for i in 0..(JITTER_BUFFER_FRAMES + 20) {
            let mut datagram = audio_header(AudioCodec::Pcm16Le, i as u64).to_vec();
            datagram.push(i as u8);
            voice
                .ingest_audio(&datagram, "192.168.1.5:9005".parse().unwrap())
                .await;
        }

        let frames = voice.take_incoming("s1").await;
        assert_eq!(
            frames.len(),
            JITTER_BUFFER_FRAMES,
            "buffer must stay bounded"
        );
        // The surviving frames are the newest ones.
        assert_eq!(frames[0][0], 20u8);
    }

    #[tokio::test]
    async fn sending_a_frame_on_an_unconnected_session_is_refused() {
        let voice = service();
        assert!(voice
            .send_frame("nope", AudioCodec::Pcm16Le, &silent_frame())
            .await
            .is_err());

        voice
            .upsert(CallSession {
                session_id: "ringing".into(),
                peer_id: "p1".into(),
                peer_name: "alice".into(),
                outgoing: true,
                state: CallState::Ringing,
                remote_audio: Some("192.168.1.5:9005".into()),
                frames_sent: 0,
                frames_received: 0,
                started_at: Utc::now(),
                ended_at: None,
            })
            .await;
        assert!(voice
            .send_frame("ringing", AudioCodec::Pcm16Le, &silent_frame())
            .await
            .is_err());
    }

    #[test]
    fn a_silent_frame_is_20ms_of_16bit_mono() {
        assert_eq!(silent_frame().len(), AUDIO_FRAME_SAMPLES * 2);
        assert!(silent_frame().iter().all(|&b| b == 0));
    }
}
