//! Which peers may use this machine's hardware.
//!
//! Chat and files are open to the whole LAN, matching a shared printer. Compute
//! is not: it burns someone's GPU, holds their memory and runs their programs,
//! so it takes a deliberate act to allow. The owner opens their dashboard, sees
//! a six-digit code, and reads it to whoever is asking. Once paired, that
//! device is remembered and never asks again.
//!
//! The code is short because a human types it, and short is safe here only
//! because of the limits enforced below: it expires in three minutes, a wrong
//! guess burns the attempt, and five failures cancel the code entirely. Six
//! digits with five tries is a one-in-two-hundred-thousand chance — and the
//! attacker has to already be on the network.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, RwLock};

use crate::compute::protocol::PAIRING_CODE_TTL_SECS;

/// Wrong guesses before a code is thrown away.
const MAX_ATTEMPTS: u8 = 5;

/// A peer allowed to use this node's resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub node_id: String,
    pub display_name: String,
    /// Presented on every later request instead of the code.
    pub token: String,
    pub paired_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used: Option<DateTime<Utc>>,
    #[serde(default)]
    pub jobs_run: u64,
}

/// A code currently on screen, waiting to be read out.
#[derive(Debug, Clone)]
struct PendingCode {
    code: String,
    expires_at: DateTime<Utc>,
    attempts: u8,
}

/// Who may run work here.
#[derive(Clone)]
pub struct TrustStore {
    trusted: Arc<RwLock<HashMap<String, TrustedPeer>>>,
    pending: Arc<RwLock<Option<PendingCode>>>,
    /// When false, anyone on the network may submit work without pairing.
    require_pairing: Arc<RwLock<bool>>,
    /// Bumped whenever access is granted or withdrawn, so the node can write
    /// the change to disk without this module knowing what a config is.
    changed: Arc<watch::Sender<u64>>,
}

impl Default for TrustStore {
    fn default() -> Self {
        Self::new(true)
    }
}

impl std::fmt::Debug for TrustStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustStore").finish_non_exhaustive()
    }
}

impl TrustStore {
    pub fn new(require_pairing: bool) -> Self {
        Self {
            trusted: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(RwLock::new(None)),
            require_pairing: Arc::new(RwLock::new(require_pairing)),
            changed: Arc::new(watch::channel(0).0),
        }
    }

    /// Fires when a device is granted or loses access.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn announce_change(&self) {
        self.changed.send_modify(|version| *version += 1);
    }

    /// Restore peers paired in an earlier run.
    pub async fn load(&self, peers: Vec<TrustedPeer>) {
        let mut trusted = self.trusted.write().await;
        for peer in peers {
            trusted.insert(peer.node_id.clone(), peer);
        }
    }

    pub async fn list(&self) -> Vec<TrustedPeer> {
        let mut peers: Vec<TrustedPeer> = self.trusted.read().await.values().cloned().collect();
        peers.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        peers
    }

    pub async fn requires_pairing(&self) -> bool {
        *self.require_pairing.read().await
    }

    pub async fn set_require_pairing(&self, required: bool) {
        *self.require_pairing.write().await = required;
    }

    /// Produce a code for the owner to read out. Replaces any previous one.
    pub async fn begin_pairing(&self) -> String {
        // Digits only: this gets read aloud and typed on a phone.
        let code: String = (0..6)
            .map(|_| char::from_digit(fastrand_digit(), 10).unwrap_or('0'))
            .collect();

        *self.pending.write().await = Some(PendingCode {
            code: code.clone(),
            expires_at: Utc::now() + Duration::seconds(PAIRING_CODE_TTL_SECS),
            attempts: 0,
        });

        tracing::info!("pairing code generated; it expires in {PAIRING_CODE_TTL_SECS}s");
        code
    }

    /// The code currently on screen, if one is still valid.
    pub async fn current_code(&self) -> Option<String> {
        let pending = self.pending.read().await;
        pending
            .as_ref()
            .filter(|p| p.expires_at > Utc::now())
            .map(|p| p.code.clone())
    }

    pub async fn cancel_pairing(&self) {
        *self.pending.write().await = None;
    }

    /// Check a code and, if it matches, trust the peer.
    ///
    /// Returns the token the peer should present from now on.
    pub async fn complete_pairing(
        &self,
        node_id: &str,
        display_name: &str,
        code: &str,
    ) -> Result<String, PairingError> {
        let mut pending = self.pending.write().await;
        let Some(entry) = pending.as_mut() else {
            return Err(PairingError::NoCodeActive);
        };

        if entry.expires_at <= Utc::now() {
            *pending = None;
            return Err(PairingError::Expired);
        }

        // Count the attempt before comparing, so a wrong guess always costs
        // one whatever happens next.
        entry.attempts += 1;
        if entry.attempts > MAX_ATTEMPTS {
            *pending = None;
            tracing::warn!(peer = %display_name, "pairing cancelled after too many wrong codes");
            return Err(PairingError::TooManyAttempts);
        }

        if !constant_time_eq(entry.code.as_bytes(), code.trim().as_bytes()) {
            let remaining = MAX_ATTEMPTS.saturating_sub(entry.attempts);
            return Err(PairingError::WrongCode { remaining });
        }

        // Correct: consume the code so it cannot pair a second device.
        *pending = None;
        drop(pending);

        let token = new_token();
        self.trusted.write().await.insert(
            node_id.to_string(),
            TrustedPeer {
                node_id: node_id.to_string(),
                display_name: display_name.to_string(),
                token: token.clone(),
                paired_at: Utc::now(),
                last_used: None,
                jobs_run: 0,
            },
        );

        self.announce_change();
        tracing::info!(peer = %display_name, "paired; it may now use this node's resources");
        Ok(token)
    }

    /// May this peer submit work?
    ///
    /// Records the use, so the dashboard can show who is actually active.
    pub async fn authorise(&self, node_id: &str, token: Option<&str>) -> Result<(), PairingError> {
        if !*self.require_pairing.read().await {
            return Ok(());
        }

        let mut trusted = self.trusted.write().await;
        let Some(peer) = trusted.get_mut(node_id) else {
            return Err(PairingError::NotPaired);
        };

        match token {
            Some(token) if constant_time_eq(peer.token.as_bytes(), token.as_bytes()) => {
                peer.last_used = Some(Utc::now());
                peer.jobs_run += 1;
                Ok(())
            }
            _ => Err(PairingError::BadToken),
        }
    }

    /// Withdraw a peer's access.
    pub async fn revoke(&self, node_id: &str) -> bool {
        let removed = self.trusted.write().await.remove(node_id).is_some();
        if removed {
            self.announce_change();
        }
        removed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    NoCodeActive,
    Expired,
    WrongCode { remaining: u8 },
    TooManyAttempts,
    NotPaired,
    BadToken,
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCodeActive => write!(
                f,
                "that device is not showing a pairing code. Open its dashboard, \
                 go to Compute, and choose Pair a device"
            ),
            Self::Expired => write!(f, "the pairing code has expired; ask for a new one"),
            Self::WrongCode { remaining } => {
                write!(f, "wrong code, {remaining} attempt(s) left")
            }
            Self::TooManyAttempts => {
                write!(f, "too many wrong codes; the pairing has been cancelled")
            }
            Self::NotPaired => write!(
                f,
                "this device has not been paired with that one. Pair them first"
            ),
            Self::BadToken => write!(f, "the stored pairing is no longer valid; pair again"),
        }
    }
}

impl std::error::Error for PairingError {}

/// Compare without leaking where two values first differ.
///
/// A naive comparison returns as soon as it finds a mismatch, which over many
/// attempts reveals a prefix. That matters more for the long token than the
/// six-digit code, but both go through the same path.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A random digit, from the same source the UUIDs come from.
fn fastrand_digit() -> u32 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    u32::from(bytes[0]) % 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_right_code_pairs_and_the_token_then_authorises() {
        let store = TrustStore::new(true);
        let code = store.begin_pairing().await;
        assert_eq!(code.len(), 6);
        assert!(
            code.chars().all(|c| c.is_ascii_digit()),
            "must be typable on a phone"
        );

        let token = store
            .complete_pairing("peer-1", "Alice's laptop", &code)
            .await
            .unwrap();
        store.authorise("peer-1", Some(&token)).await.unwrap();

        let listed = store.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].jobs_run, 1, "authorising counts as a use");
    }

    #[tokio::test]
    async fn a_wrong_code_is_refused_and_the_attempts_run_out() {
        let store = TrustStore::new(true);
        let code = store.begin_pairing().await;
        let wrong = if code == "000000" { "111111" } else { "000000" };

        for expected_remaining in (0..MAX_ATTEMPTS).rev() {
            match store.complete_pairing("peer-1", "attacker", wrong).await {
                Err(PairingError::WrongCode { remaining }) => {
                    assert_eq!(remaining, expected_remaining);
                }
                other => panic!("expected a wrong-code error, got {other:?}"),
            }
        }

        // The next attempt cancels the code, even if it is the right one.
        assert_eq!(
            store.complete_pairing("peer-1", "attacker", &code).await,
            Err(PairingError::TooManyAttempts)
        );
        assert!(
            store.current_code().await.is_none(),
            "the code must be gone"
        );
    }

    #[tokio::test]
    async fn a_code_pairs_exactly_one_device() {
        let store = TrustStore::new(true);
        let code = store.begin_pairing().await;

        store
            .complete_pairing("peer-1", "first", &code)
            .await
            .unwrap();
        // Reusing the same code must not let a second device in.
        assert_eq!(
            store.complete_pairing("peer-2", "second", &code).await,
            Err(PairingError::NoCodeActive)
        );
    }

    #[tokio::test]
    async fn an_unpaired_peer_cannot_submit_work() {
        let store = TrustStore::new(true);
        assert_eq!(
            store.authorise("stranger", None).await,
            Err(PairingError::NotPaired)
        );

        let code = store.begin_pairing().await;
        store
            .complete_pairing("peer-1", "alice", &code)
            .await
            .unwrap();
        // Paired, but presenting the wrong token.
        assert_eq!(
            store.authorise("peer-1", Some("not-the-token")).await,
            Err(PairingError::BadToken)
        );
    }

    #[tokio::test]
    async fn revoking_takes_access_away_immediately() {
        let store = TrustStore::new(true);
        let code = store.begin_pairing().await;
        let token = store
            .complete_pairing("peer-1", "alice", &code)
            .await
            .unwrap();

        assert!(store.revoke("peer-1").await);
        assert_eq!(
            store.authorise("peer-1", Some(&token)).await,
            Err(PairingError::NotPaired)
        );
        assert!(
            !store.revoke("peer-1").await,
            "revoking twice is not an error but changes nothing"
        );
    }

    #[tokio::test]
    async fn pairing_can_be_switched_off_for_a_trusted_network() {
        let store = TrustStore::new(false);
        // With pairing off, anyone on the LAN may submit work.
        store.authorise("anyone-at-all", None).await.unwrap();

        store.set_require_pairing(true).await;
        assert_eq!(
            store.authorise("anyone-at-all", None).await,
            Err(PairingError::NotPaired)
        );
    }

    #[test]
    fn comparison_rejects_differing_lengths_and_contents() {
        assert!(constant_time_eq(b"123456", b"123456"));
        assert!(!constant_time_eq(b"123456", b"123457"));
        assert!(!constant_time_eq(b"123456", b"12345"));
        assert!(constant_time_eq(b"", b""));
    }

    #[tokio::test]
    async fn generated_codes_vary() {
        let store = TrustStore::new(true);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            seen.insert(store.begin_pairing().await);
        }
        // A generator stuck on one value would be a serious weakness.
        assert!(
            seen.len() > 30,
            "codes are not varying: {} distinct",
            seen.len()
        );
    }
}
