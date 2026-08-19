//! In-memory chat history with de-duplication.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::net::protocol::ChatPayload;

/// Messages retained in memory. Older ones live only in the on-disk log.
const MAX_HISTORY: usize = 1000;

/// Message ids remembered for de-duplication.
const DEDUPE_WINDOW: usize = 4096;

/// Shared, cloneable chat log.
#[derive(Clone, Debug, Default)]
pub struct ChatStore {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    messages: VecDeque<ChatPayload>,
    seen_ids: HashSet<String>,
    seen_order: VecDeque<String>,
}

impl ChatStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a message, returning false if it is a duplicate.
    ///
    /// De-duplication matters because a broadcast reaches a peer once per
    /// connection attempt, and a retry after a half-open TCP connection can
    /// deliver the same message twice. `Bifrost v2`'s gossipsub layer handled
    /// this for us; sending directly means doing it here.
    pub async fn ingest(&self, message: ChatPayload) -> bool {
        let mut inner = self.inner.write().await;

        if !inner.seen_ids.insert(message.message_id.clone()) {
            return false;
        }
        inner.seen_order.push_back(message.message_id.clone());
        while inner.seen_order.len() > DEDUPE_WINDOW {
            if let Some(old) = inner.seen_order.pop_front() {
                inner.seen_ids.remove(&old);
            }
        }

        inner.messages.push_back(message);
        while inner.messages.len() > MAX_HISTORY {
            inner.messages.pop_front();
        }
        true
    }

    /// The most recent `limit` messages, oldest first.
    pub async fn recent(&self, limit: usize) -> Vec<ChatPayload> {
        let inner = self.inner.read().await;
        let skip = inner.messages.len().saturating_sub(limit);
        inner.messages.iter().skip(skip).cloned().collect()
    }

    /// Messages exchanged directly with one peer, in either direction.
    pub async fn conversation_with(&self, node_id: &str, limit: usize) -> Vec<ChatPayload> {
        let inner = self.inner.read().await;
        let all: Vec<ChatPayload> = inner
            .messages
            .iter()
            .filter(|m| m.sender_id == node_id || m.recipient_id.as_deref() == Some(node_id))
            .cloned()
            .collect();
        let skip = all.len().saturating_sub(limit);
        all[skip..].to_vec()
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.messages.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    pub async fn clear(&self) {
        let mut inner = self.inner.write().await;
        inner.messages.clear();
        inner.seen_ids.clear();
        inner.seen_order.clear();
    }
}

/// Build an outgoing chat message. `recipient` is `None` to broadcast.
pub fn compose(
    sender_id: &str,
    sender_name: &str,
    content: &str,
    recipient: Option<String>,
) -> ChatPayload {
    ChatPayload {
        message_id: Uuid::new_v4().to_string(),
        sender_id: sender_id.to_string(),
        sender_name: sender_name.to_string(),
        recipient_id: recipient,
        content: content.to_string(),
        timestamp: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn duplicates_are_rejected_by_message_id() {
        let store = ChatStore::new();
        let msg = compose("n1", "alice", "hello", None);

        assert!(store.ingest(msg.clone()).await, "first delivery accepted");
        assert!(!store.ingest(msg).await, "redelivery rejected");
        assert_eq!(store.len().await, 1);
    }

    #[tokio::test]
    async fn history_is_capped_and_keeps_the_newest() {
        let store = ChatStore::new();
        for i in 0..MAX_HISTORY + 50 {
            store
                .ingest(compose("n1", "alice", &format!("msg {i}"), None))
                .await;
        }

        assert_eq!(store.len().await, MAX_HISTORY);
        let recent = store.recent(1).await;
        assert_eq!(recent[0].content, format!("msg {}", MAX_HISTORY + 49));
    }

    #[tokio::test]
    async fn conversation_filters_to_one_peer_in_both_directions() {
        let store = ChatStore::new();
        store
            .ingest(compose("bob", "bob", "from bob", Some("me".into())))
            .await;
        store
            .ingest(compose("me", "me", "to bob", Some("bob".into())))
            .await;
        store
            .ingest(compose("carol", "carol", "from carol", Some("me".into())))
            .await;
        store.ingest(compose("me", "me", "broadcast", None)).await;

        let convo = store.conversation_with("bob", 100).await;
        assert_eq!(convo.len(), 2);
        assert_eq!(convo[0].content, "from bob");
        assert_eq!(convo[1].content, "to bob");
    }

    #[tokio::test]
    async fn recent_returns_oldest_first_within_the_window() {
        let store = ChatStore::new();
        for i in 0..5 {
            store
                .ingest(compose("n1", "alice", &format!("m{i}"), None))
                .await;
        }

        let recent = store.recent(3).await;
        assert_eq!(
            recent
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["m2", "m3", "m4"]
        );
    }
}
