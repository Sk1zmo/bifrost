//! System metrics and resource advertisement.

pub mod sharing;
pub mod telemetry;

pub use sharing::SharingPolicy;
pub use telemetry::{HostInfo, TelemetryCollector};
