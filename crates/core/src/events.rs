//! The node-wide event bus.
//!
//! One `tokio::sync::broadcast` channel carries everything interesting that
//! happens, so the terminal UI, the web dashboard's SSE stream, and the mobile
//! FFI callback all observe the same node through the same events instead of
//! each polling on its own schedule.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::files::transfer::TransferStatus;
use crate::net::peers::Peer;
use crate::net::protocol::{ChatPayload, Telemetry, VoiceSignalKind};

/// Capacity of the event channel.
///
/// A slow subscriber lags rather than blocking the node, and
/// `broadcast::error::RecvError::Lagged` tells it to re-sync from a snapshot.
const EVENT_CHANNEL_CAPACITY: usize = 512;

/// Severity for a [`NodeEvent::Notice`], driving colour in both dashboards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// Something worth telling every attached UI about.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum NodeEvent {
    /// A chat message was sent or received.
    Chat(ChatPayload),
    /// A peer was discovered, or came back online.
    PeerOnline(Peer),
    /// A peer stopped beaconing.
    PeerOffline {
        node_id: String,
        display_name: String,
    },
    /// A file transfer changed state.
    Transfer(TransferStatus),
    /// This node's own metrics were re-sampled.
    Telemetry(Telemetry),
    /// A voice call changed state.
    Voice {
        session_id: String,
        peer_id: String,
        kind: VoiceSignalKind,
    },
    /// Free-form message for the log pane.
    Notice { level: NoticeLevel, text: String },
}

impl NodeEvent {
    pub fn info(text: impl Into<String>) -> Self {
        Self::Notice {
            level: NoticeLevel::Info,
            text: text.into(),
        }
    }
    pub fn success(text: impl Into<String>) -> Self {
        Self::Notice {
            level: NoticeLevel::Success,
            text: text.into(),
        }
    }
    pub fn warning(text: impl Into<String>) -> Self {
        Self::Notice {
            level: NoticeLevel::Warning,
            text: text.into(),
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self::Notice {
            level: NoticeLevel::Error,
            text: text.into(),
        }
    }
}

/// Cloneable publisher for [`NodeEvent`]s.
#[derive(Clone, Debug)]
pub struct EventBus {
    tx: broadcast::Sender<NodeEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Publish an event.
    ///
    /// Having no subscribers is normal (headless mode) and not an error, so
    /// the send result is deliberately discarded.
    pub fn publish(&self, event: NodeEvent) {
        let _ = self.tx.send(event);
    }

    pub fn notice(&self, level: NoticeLevel, text: impl Into<String>) {
        self.publish(NodeEvent::Notice {
            level,
            text: text.into(),
        });
    }

    /// Subscribe. Only events published after this call are delivered.
    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.tx.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_subscriber_sees_each_event() {
        let bus = EventBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        bus.publish(NodeEvent::info("hello"));

        for rx in [&mut a, &mut b] {
            match rx.recv().await.unwrap() {
                NodeEvent::Notice { text, level } => {
                    assert_eq!(text, "hello");
                    assert_eq!(level, NoticeLevel::Info);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_is_fine() {
        let bus = EventBus::new();
        bus.publish(NodeEvent::info("nobody is listening"));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn events_serialise_with_a_tagged_shape_the_browser_can_switch_on() {
        let json = serde_json::to_value(NodeEvent::success("done")).unwrap();
        assert_eq!(json["event"], "notice");
        assert_eq!(json["data"]["level"], "success");
        assert_eq!(json["data"]["text"], "done");
    }
}
