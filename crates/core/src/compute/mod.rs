//! Shared compute: running work on other machines' hardware.
//!
//! Chat and files move bytes. This moves work — a prompt to whichever machine
//! has the model and the GPU, a render to whichever one is idle — so that a
//! network of ordinary devices behaves like one larger machine.
//!
//! # The pieces
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`protocol`] | Wire types, and what a node advertises it can do |
//! | [`pairing`] | Which peers may use this machine, and how they earn that |
//! | [`jobs`] | Declared tasks: the commands an owner has agreed to run |
//! | [`ollama`] | Bridge to a local model runtime |
//! | [`orchestrator`] | Pooled capacity, priorities, and placement |
//! | [`service`] | The TCP service, and the client that drives it |
//!
//! # The two rules
//!
//! **Nothing runs that the owner did not declare.** A peer can ask for a model
//! by name or a task by name; it cannot supply a command line. The alternative
//! — accepting commands over the network — is a remote code execution service
//! on your LAN, where any compromised device owns every machine that trusted
//! it.
//!
//! **Nothing runs for a peer the owner did not approve.** Compute costs
//! someone real electricity, memory and time, so unlike chat it is closed by
//! default and opened one device at a time with a pairing code.

pub mod jobs;
pub mod ollama;
pub mod orchestrator;
pub mod pairing;
pub mod protocol;
pub mod service;

pub use jobs::{TaskDefinition, TaskRegistry};
pub use ollama::Ollama;
pub use orchestrator::{ClusterCapacity, Orchestrator, Priority, ScheduledJob, Work};
pub use pairing::{PairingError, TrustStore, TrustedPeer};
pub use protocol::{Capabilities, ModelInfo, TaskInfo};
pub use service::ComputeService;
