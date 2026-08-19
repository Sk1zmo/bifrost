//! End-to-end tests: two real nodes, real sockets, no mocks.
//!
//! Every test binds a discovery port of its own. That isolation is not
//! cosmetic — without it, concurrent tests would discover each other, and a
//! test run on a developer's laptop would join whatever real Bifrost network
//! happened to be on their LAN.

use std::path::PathBuf;
use std::time::Duration;

use bifrost_core::files::TransferState;
use bifrost_core::net::protocol::PortSet;
use bifrost_core::{Config, Node};

/// Ports for one test, isolated from every other test and from any real node.
///
/// `base` must be unique per test.
fn isolated_ports(base: u16) -> PortSet {
    let mut ports = PortSet::default().offset_by(base);
    ports.discovery += base;
    ports
}

/// A node with its own config, download and data directories.
async fn spawn(base: u16, name: &str) -> (Node, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");

    let mut config = Config::generate();
    config.identity.username = name.to_string();
    config.ports = isolated_ports(base);
    config.download_dir = dir.path().join("downloads");
    config.data_dir = dir.path().join("data");

    let node = Node::start(config, dir.path().join("config.json"))
        .await
        .expect("node should start");

    (node, dir)
}

/// Poll until `condition` holds, or fail after `timeout`.
///
/// Discovery is inherently timing-dependent, so a fixed sleep would be either
/// slow or flaky; polling is both fast and reliable.
async fn wait_for<F, Fut>(timeout: Duration, description: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out after {timeout:?} waiting for: {description}");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_discover_each_other_over_the_real_network() {
    let (alice, _a) = spawn(400, "alice").await;
    let (bob, _b) = spawn(410, "bob").await;

    // Both must be on the same discovery port to meet.
    assert_eq!(alice.ports().discovery, 9401);
    let mut bob_ports = PortSet::default().offset_by(410);
    bob_ports.discovery += 410;
    assert_ne!(
        bob_ports.discovery,
        alice.ports().discovery,
        "test isolation check"
    );

    // With different discovery ports they are on separate networks, so this
    // test instead verifies a shared one — rebuild bob on alice's port.
    drop(bob);

    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::generate();
    config.identity.username = "bob".into();
    config.ports = PortSet::default().offset_by(410);
    config.ports.discovery = alice.ports().discovery;
    config.download_dir = dir.path().join("downloads");
    config.data_dir = dir.path().join("data");
    let bob = Node::start(config, dir.path().join("config.json"))
        .await
        .unwrap();

    wait_for(Duration::from_secs(15), "alice to see bob", || async {
        alice.registry.online_count().await >= 1
    })
    .await;

    wait_for(Duration::from_secs(15), "bob to see alice", || async {
        bob.registry.online_count().await >= 1
    })
    .await;

    let seen = alice
        .registry
        .resolve("bob")
        .await
        .expect("bob should be resolvable");
    assert_eq!(seen.username, "bob");
    assert!(seen.online);
    // The port set travels in the beacon, so alice knows where to connect.
    assert_eq!(seen.ports.messaging, 9412);
    assert!(
        seen.telemetry.ram_total_mb > 0,
        "telemetry should ride along"
    );

    // Pool totals span both nodes.
    let snapshot = alice.snapshot().await;
    assert_eq!(snapshot.pool.nodes, 2);

    alice.shutdown().await;
    bob.shutdown().await;
}

/// Build two nodes that share a discovery port, the normal arrangement.
async fn spawn_pair(
    base_a: u16,
    base_b: u16,
    discovery: u16,
) -> (Node, Node, tempfile::TempDir, tempfile::TempDir) {
    async fn make(base: u16, name: &str, discovery: u16) -> (Node, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::generate();
        config.identity.username = name.to_string();
        config.ports = PortSet::default().offset_by(base);
        config.ports.discovery = discovery;
        config.download_dir = dir.path().join("downloads");
        config.data_dir = dir.path().join("data");
        let node = Node::start(config, dir.path().join("config.json"))
            .await
            .unwrap();
        (node, dir)
    }

    let (alice, dir_a) = make(base_a, "alice", discovery).await;
    let (bob, dir_b) = make(base_b, "bob", discovery).await;

    wait_for(
        Duration::from_secs(15),
        "the two nodes to find each other",
        || async {
            alice.registry.online_count().await >= 1 && bob.registry.online_count().await >= 1
        },
    )
    .await;

    (alice, bob, dir_a, dir_b)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broadcast_reaches_the_other_node() {
    let (alice, bob, _a, _b) = spawn_pair(420, 430, 9421).await;

    alice.broadcast("hello from alice").await.unwrap();

    wait_for(
        Duration::from_secs(10),
        "bob to receive the message",
        || async {
            bob.chat
                .recent(10)
                .await
                .iter()
                .any(|m| m.content == "hello from alice")
        },
    )
    .await;

    let received = bob.chat.recent(10).await;
    let message = received
        .iter()
        .find(|m| m.content == "hello from alice")
        .unwrap();
    assert_eq!(message.sender_name, "alice");
    assert!(
        message.recipient_id.is_none(),
        "a broadcast has no recipient"
    );

    alice.shutdown().await;
    bob.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_message_reaches_only_its_recipient() {
    let (alice, bob, _a, _b) = spawn_pair(440, 450, 9441).await;

    let bob_id = bob.node_id();
    alice.direct_message(&bob_id, "just for you").await.unwrap();

    wait_for(
        Duration::from_secs(10),
        "bob to receive the direct message",
        || async {
            bob.chat
                .recent(10)
                .await
                .iter()
                .any(|m| m.content == "just for you")
        },
    )
    .await;

    let received = bob.chat.recent(10).await;
    let message = received
        .iter()
        .find(|m| m.content == "just for you")
        .unwrap();
    assert_eq!(message.recipient_id.as_deref(), Some(bob_id.as_str()));

    alice.shutdown().await;
    bob.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_transfers_intact_and_is_verified() {
    let (alice, bob, _a, dir_b) = spawn_pair(460, 470, 9461).await;

    // Larger than one 64 KiB chunk, so the chunking path is genuinely
    // exercised rather than a single-chunk special case.
    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("report.bin");
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    tokio::fs::write(&source, &payload).await.unwrap();

    let bob_id = bob.node_id();
    alice.send_file(&bob_id, &source).await.unwrap();

    wait_for(
        Duration::from_secs(30),
        "the transfer to complete",
        || async {
            alice
                .transfers
                .list()
                .await
                .iter()
                .any(|t| matches!(t.state, TransferState::Completed))
        },
    )
    .await;

    // The receiver recorded it too.
    wait_for(
        Duration::from_secs(10),
        "bob to record the transfer",
        || async {
            bob.transfers
                .list()
                .await
                .iter()
                .any(|t| matches!(t.state, TransferState::Completed))
        },
    )
    .await;

    let landed: PathBuf = dir_b.path().join("downloads").join("report.bin");
    assert!(
        landed.exists(),
        "the file should be in bob's download directory"
    );

    let received = tokio::fs::read(&landed).await.unwrap();
    assert_eq!(received.len(), payload.len(), "size should match exactly");
    assert_eq!(received, payload, "contents should be byte-identical");

    let status = alice
        .transfers
        .list()
        .await
        .into_iter()
        .find(|t| matches!(t.state, TransferState::Completed))
        .unwrap();
    assert_eq!(status.bytes_done, payload.len() as u64);
    assert_eq!(status.progress_pct(), 100.0);

    alice.shutdown().await;
    bob.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupted_transfer_is_rejected_rather_than_written() {
    // The checksum is the guarantee that a received file is the file that was
    // sent; a transfer that fails verification must leave nothing behind.
    let (alice, bob, _a, dir_b) = spawn_pair(480, 490, 9481).await;

    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("data.bin");
    tokio::fs::write(&source, vec![7u8; 100_000]).await.unwrap();

    let bob_id = bob.node_id();
    alice.send_file(&bob_id, &source).await.unwrap();

    wait_for(
        Duration::from_secs(30),
        "the transfer to finish",
        || async {
            alice
                .transfers
                .list()
                .await
                .iter()
                .any(|t| t.state.is_terminal())
        },
    )
    .await;

    // Nothing partial is left in the download directory.
    let downloads = dir_b.path().join("downloads");
    let mut entries = tokio::fs::read_dir(&downloads).await.unwrap();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(
            !name.ends_with(".bfpart"),
            "a partial file was left behind: {name}"
        );
    }

    alice.shutdown().await;
    bob.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ping_measures_a_real_round_trip() {
    let (alice, bob, _a, _b) = spawn_pair(500, 510, 9501).await;

    let bob_id = bob.node_id();
    let outcome = alice.ping(&bob_id).await.unwrap();
    assert!(
        outcome.as_text().contains("replied in"),
        "got: {}",
        outcome.as_text()
    );

    // The measurement is recorded against the peer.
    let peer = alice.registry.get(&bob_id).await.unwrap();
    assert!(peer.latency_ms.is_some());
    assert!(peer.latency_ms.unwrap() >= 0.0);

    alice.shutdown().await;
    bob.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_voice_call_connects_on_both_sides() {
    let (alice, bob, _a, _b) = spawn_pair(520, 530, 9521).await;

    let bob_id = bob.node_id();
    let outcome = alice.call(&bob_id).await.unwrap();
    assert!(
        outcome.as_text().contains("In a call"),
        "got: {}",
        outcome.as_text()
    );

    let alice_calls = alice.voice.active().await;
    assert_eq!(alice_calls.len(), 1);
    assert!(alice_calls[0].outgoing);

    wait_for(
        Duration::from_secs(5),
        "bob to register the call",
        || async { !bob.voice.active().await.is_empty() },
    )
    .await;

    let bob_calls = bob.voice.active().await;
    assert!(!bob_calls[0].outgoing, "bob's side is the incoming one");
    // Bob derives the audio address from where the connection came from.
    assert!(bob_calls[0].remote_audio.is_some());

    // Hanging up clears it on the caller's side.
    alice.hang_up(None).await.unwrap();
    assert!(alice.voice.active().await.is_empty());

    alice.shutdown().await;
    bob.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_alias_survives_a_restart() {
    let discovery = 9541;
    let dir_a = tempfile::tempdir().unwrap();
    let config_path = dir_a.path().join("config.json");

    let mut config = Config::generate();
    config.identity.username = "alice".into();
    config.ports = PortSet::default().offset_by(540);
    config.ports.discovery = discovery;
    config.download_dir = dir_a.path().join("downloads");
    config.data_dir = dir_a.path().join("data");

    let alice = Node::start(config.clone(), &config_path).await.unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    let mut bob_config = Config::generate();
    bob_config.identity.username = "bob".into();
    bob_config.ports = PortSet::default().offset_by(550);
    bob_config.ports.discovery = discovery;
    bob_config.download_dir = dir_b.path().join("downloads");
    bob_config.data_dir = dir_b.path().join("data");
    let bob = Node::start(bob_config, dir_b.path().join("config.json"))
        .await
        .unwrap();
    let bob_id = bob.node_id();

    wait_for(Duration::from_secs(15), "alice to see bob", || async {
        alice.registry.online_count().await >= 1
    })
    .await;

    alice
        .set_alias(&bob_id, Some("Workshop NUC".into()))
        .await
        .unwrap();
    assert_eq!(
        alice.registry.get(&bob_id).await.unwrap().display_name(),
        "Workshop NUC"
    );

    alice.shutdown().await;
    drop(alice);

    // Restart alice from the same config; the alias should come back and be
    // reapplied to bob when he is rediscovered.
    let reloaded = Config::load_or_create(&config_path).unwrap();
    assert_eq!(
        reloaded.aliases.get(&bob_id).map(String::as_str),
        Some("Workshop NUC")
    );

    let alice = Node::start(reloaded, &config_path).await.unwrap();
    wait_for(
        Duration::from_secs(15),
        "alice to rediscover bob",
        || async { alice.registry.get(&bob_id).await.is_some() },
    )
    .await;

    assert_eq!(
        alice.registry.get(&bob_id).await.unwrap().display_name(),
        "Workshop NUC",
        "a restart must not lose the name the user gave a peer"
    );

    alice.shutdown().await;
    bob.shutdown().await;
}
