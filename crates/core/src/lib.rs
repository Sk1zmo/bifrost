//! # Bifrost core
//!
//! The LAN mesh engine behind every Bifrost client: the terminal dashboard,
//! the web dashboard, the CLI, and the Android and iOS apps.
//!
//! Bifrost connects every device on one local network — laptops, desktops,
//! servers, phones and tablets — with no configuration, no internet, no
//! account and no relay server. Nodes find each other by broadcasting on the
//! LAN, then chat, transfer files, share live system metrics and place voice
//! calls directly.
//!
//! ## Using it
//!
//! ```no_run
//! use bifrost_core::{Node, util::Config};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let path = Config::default_path();
//! let node = Node::start(Config::load_or_create(&path)?, &path).await?;
//!
//! node.broadcast("hello, everyone").await?;
//! for peer in node.registry.list().await {
//!     println!("{} at {}", peer.display_name(), peer.ip);
//! }
//!
//! node.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! ## Layout
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`net`] | Wire protocol, framing, discovery, messaging, peer registry |
//! | [`chat`] | Message history, command parsing, presence, voice |
//! | [`files`] | Chunked, checksummed file transfer |
//! | [`resources`] | System metrics and resource advertisement |
//! | [`store`] | On-disk chat log and key-value state |
//! | [`util`] | Identity, configuration, logging |
//! | [`events`] | The node-wide event bus |
//! | [`node`] | [`Node`], which owns all of the above |
//! | [`ffi`] | C ABI for the iOS client and any C caller |
//! | `ffi_jni` | JNI entry points for the Android client (Android only) |
//!
//! The wire protocol is specified in `docs/PROTOCOL.md`.

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod chat;
pub mod events;
pub mod ffi;
/// JNI entry points for the Android client. Android only; every other target
/// uses the C ABI in [`ffi`].
#[cfg(target_os = "android")]
pub mod ffi_jni;
pub mod files;
pub mod net;
pub mod node;
pub mod resources;
pub mod store;
pub mod util;

pub use events::{EventBus, NodeEvent, NoticeLevel};
pub use net::peers::{Peer, PeerRegistry};
pub use node::{CommandOutcome, Node, NodeSnapshot};
pub use util::config::Config;
pub use util::identity::Identity;

/// This build's version, as reported in beacons and the dashboards.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The wire protocol version this build speaks.
pub const PROTOCOL_VERSION: u8 = net::protocol::PROTOCOL_VERSION;
