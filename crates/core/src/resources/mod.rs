//! System metrics and resource advertisement.

pub mod hardware;
pub mod sharing;
pub mod telemetry;

pub use hardware::{Gpu, GpuVendor};
pub use sharing::SharingPolicy;
pub use telemetry::{HostInfo, TelemetryCollector};
