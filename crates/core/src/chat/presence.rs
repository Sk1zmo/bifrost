//! Presence: whether a peer is at the keyboard, not merely reachable.
//!
//! Discovery already answers "is this machine powered on and on the network".
//! Presence answers "is somebody there", which is what a chat UI needs in
//! order to say `alice — away 4m` instead of just showing a green dot.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::net::protocol::PresenceStatus;

/// Idle time after which a node is considered away.
const AWAY_AFTER_SECS: i64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceRecord {
    pub node_id: String,
    pub status: PresenceStatus,
    /// Last deliberate action: a message sent, a call, a keystroke.
    pub last_active: DateTime<Utc>,
}

impl PresenceRecord {
    pub fn idle_secs(&self) -> i64 {
        (Utc::now() - self.last_active).num_seconds().max(0)
    }

    /// e.g. `"away 4m"`, or `"online"`.
    pub fn describe(&self) -> String {
        match self.status {
            PresenceStatus::Online => "online".to_string(),
            PresenceStatus::Busy => "busy".to_string(),
            PresenceStatus::Offline => "offline".to_string(),
            PresenceStatus::Away => {
                let mins = self.idle_secs() / 60;
                if mins < 60 {
                    format!("away {mins}m")
                } else {
                    format!("away {}h", mins / 60)
                }
            }
        }
    }
}

/// Presence for every known node, including this one.
#[derive(Clone, Debug, Default)]
pub struct PresenceTracker {
    records: Arc<RwLock<HashMap<String, PresenceRecord>>>,
}

impl PresenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record activity, resetting the idle timer.
    pub async fn touch(&self, node_id: &str) {
        let mut records = self.records.write().await;
        let entry = records
            .entry(node_id.to_string())
            .or_insert_with(|| PresenceRecord {
                node_id: node_id.to_string(),
                status: PresenceStatus::Online,
                last_active: Utc::now(),
            });
        entry.last_active = Utc::now();
        // Activity clears Away, but not a deliberate Busy.
        if entry.status == PresenceStatus::Away || entry.status == PresenceStatus::Offline {
            entry.status = PresenceStatus::Online;
        }
    }

    /// Set a status explicitly, e.g. the user choosing "busy".
    pub async fn set_status(&self, node_id: &str, status: PresenceStatus) {
        let mut records = self.records.write().await;
        let entry = records
            .entry(node_id.to_string())
            .or_insert_with(|| PresenceRecord {
                node_id: node_id.to_string(),
                status,
                last_active: Utc::now(),
            });
        entry.status = status;
    }

    pub async fn get(&self, node_id: &str) -> Option<PresenceRecord> {
        self.records.read().await.get(node_id).cloned()
    }

    /// Status for a node, defaulting to online for one we have not tracked.
    pub async fn status_of(&self, node_id: &str) -> PresenceStatus {
        self.records
            .read()
            .await
            .get(node_id)
            .map(|r| r.status)
            .unwrap_or(PresenceStatus::Online)
    }

    /// Move idle Online nodes to Away.
    pub async fn refresh(&self) {
        let cutoff = Duration::seconds(AWAY_AFTER_SECS);
        let now = Utc::now();
        let mut records = self.records.write().await;
        for record in records.values_mut() {
            if record.status == PresenceStatus::Online && now - record.last_active > cutoff {
                record.status = PresenceStatus::Away;
            }
        }
    }

    /// Mark offline, keeping the record so history survives.
    pub async fn mark_offline(&self, node_id: &str) {
        if let Some(record) = self.records.write().await.get_mut(node_id) {
            record.status = PresenceStatus::Offline;
        }
    }

    pub async fn all(&self) -> Vec<PresenceRecord> {
        self.records.read().await.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_touched_node_is_online() {
        let tracker = PresenceTracker::new();
        tracker.touch("n1").await;

        assert_eq!(tracker.status_of("n1").await, PresenceStatus::Online);
        assert_eq!(tracker.get("n1").await.unwrap().describe(), "online");
    }

    #[tokio::test]
    async fn an_idle_node_goes_away_and_comes_back() {
        let tracker = PresenceTracker::new();
        tracker.touch("n1").await;

        {
            let mut records = tracker.records.write().await;
            records.get_mut("n1").unwrap().last_active =
                Utc::now() - Duration::seconds(AWAY_AFTER_SECS + 60);
        }
        tracker.refresh().await;
        assert_eq!(tracker.status_of("n1").await, PresenceStatus::Away);
        assert!(tracker
            .get("n1")
            .await
            .unwrap()
            .describe()
            .starts_with("away"));

        tracker.touch("n1").await;
        assert_eq!(tracker.status_of("n1").await, PresenceStatus::Online);
    }

    #[tokio::test]
    async fn a_deliberate_busy_survives_both_activity_and_idling() {
        let tracker = PresenceTracker::new();
        tracker.set_status("n1", PresenceStatus::Busy).await;

        tracker.touch("n1").await;
        assert_eq!(tracker.status_of("n1").await, PresenceStatus::Busy);

        {
            let mut records = tracker.records.write().await;
            records.get_mut("n1").unwrap().last_active =
                Utc::now() - Duration::seconds(AWAY_AFTER_SECS + 60);
        }
        tracker.refresh().await;
        assert_eq!(tracker.status_of("n1").await, PresenceStatus::Busy);
    }

    #[tokio::test]
    async fn an_untracked_node_reads_as_online() {
        // A peer we have only ever seen beacon is present, not absent.
        assert_eq!(
            PresenceTracker::new().status_of("stranger").await,
            PresenceStatus::Online
        );
    }

    #[tokio::test]
    async fn away_durations_read_naturally() {
        let tracker = PresenceTracker::new();
        tracker.set_status("n1", PresenceStatus::Away).await;

        {
            let mut records = tracker.records.write().await;
            records.get_mut("n1").unwrap().last_active = Utc::now() - Duration::minutes(4);
        }
        assert_eq!(tracker.get("n1").await.unwrap().describe(), "away 4m");

        {
            let mut records = tracker.records.write().await;
            records.get_mut("n1").unwrap().last_active = Utc::now() - Duration::minutes(150);
        }
        assert_eq!(tracker.get("n1").await.unwrap().describe(), "away 2h");
    }
}
