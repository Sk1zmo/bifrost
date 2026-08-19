//! Networking: the wire protocol, framing, discovery, messaging and the peer
//! registry.

pub mod discovery;
pub mod frame;
pub mod iface;
pub mod messaging;
pub mod peers;
pub mod protocol;

pub use peers::{Peer, PeerRegistry};
