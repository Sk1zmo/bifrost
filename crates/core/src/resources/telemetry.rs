//! System metrics collection.
//!
//! One long-lived [`sysinfo::System`] is refreshed on a timer. Keeping the
//! handle alive is what makes CPU and network numbers meaningful: both are
//! deltas between consecutive refreshes, so the `System::new_all()`-per-sample
//! approach in `Bifrost v2` reported a meaningless instantaneous figure every
//! time and re-enumerated every process on each tick.

use std::time::Duration;

use chrono::Utc;
use sysinfo::{Disks, MemoryRefreshKind, Networks, RefreshKind, System};
use tokio::sync::watch;

use crate::events::{EventBus, NodeEvent};
use crate::net::protocol::Telemetry;

/// Sampling interval, matching the documented 2 s cadence.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(2000);

/// Collects metrics and publishes them on a [`watch`] channel.
#[derive(Debug)]
pub struct TelemetryCollector {
    system: System,
    disks: Disks,
    networks: Networks,
    tx: watch::Sender<Telemetry>,
}

impl TelemetryCollector {
    /// Build a collector and its receiver.
    ///
    /// One sample is taken before returning, so the first frame a dashboard
    /// draws already shows real memory and disk figures instead of zeroes.
    /// CPU needs two refreshes to mean anything and stays at 0 until the
    /// second sample, a couple of hundred milliseconds later.
    pub fn new() -> (Self, watch::Receiver<Telemetry>) {
        let (tx, rx) = watch::channel(Telemetry::default());
        let mut collector = Self {
            system: System::new_with_specifics(
                RefreshKind::new()
                    .with_cpu(sysinfo::CpuRefreshKind::everything())
                    .with_memory(MemoryRefreshKind::new().with_ram().with_swap()),
            ),
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            tx,
        };
        collector.sample();
        (collector, rx)
    }

    /// Take one sample.
    pub fn sample(&mut self) -> Telemetry {
        self.system.refresh_cpu();
        self.system.refresh_memory();
        self.disks.refresh();
        self.networks.refresh();

        let cpus = self.system.cpus();
        let cpu_cores = cpus.len();
        let cpu_usage = if cpu_cores == 0 {
            0.0
        } else {
            cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / cpu_cores as f32
        };

        // sysinfo 0.30 reports memory in bytes.
        const MB: u64 = 1024 * 1024;
        let ram_total_mb = self.system.total_memory() / MB;
        let ram_used_mb = self.system.used_memory() / MB;

        // Physical disks can be reported more than once (a device mounted at
        // several points, or bind mounts on Linux), which would multiply the
        // apparent capacity. Count each unique total/available pair once.
        const GB: f64 = 1024.0 * 1024.0 * 1024.0;
        let mut seen = std::collections::HashSet::new();
        let (mut disk_total_bytes, mut disk_used_bytes) = (0u64, 0u64);
        for disk in self.disks.list() {
            let total = disk.total_space();
            let available = disk.available_space();
            if total == 0 || !seen.insert((disk.name().to_owned(), total)) {
                continue;
            }
            disk_total_bytes += total;
            disk_used_bytes += total.saturating_sub(available);
        }

        // `received()` is already the delta since the previous refresh, so it
        // only needs converting to a rate.
        let secs = SAMPLE_INTERVAL.as_secs_f64();
        let (mut rx_bytes, mut tx_bytes) = (0u64, 0u64);
        for (_name, data) in self.networks.iter() {
            rx_bytes += data.received();
            tx_bytes += data.transmitted();
        }

        let telemetry = Telemetry {
            cpu_usage: cpu_usage.clamp(0.0, 100.0),
            cpu_cores,
            ram_used_mb,
            ram_total_mb,
            disk_used_gb: disk_used_bytes as f64 / GB,
            disk_total_gb: disk_total_bytes as f64 / GB,
            net_rx_kbps: rx_bytes as f64 / secs / 1024.0,
            net_tx_kbps: tx_bytes as f64 / secs / 1024.0,
            uptime_secs: System::uptime(),
            collected_at: Utc::now(),
        };

        // A watch send fails only when every receiver is gone, which for a
        // running node means shutdown is already under way.
        let _ = self.tx.send(telemetry.clone());
        telemetry
    }

    /// Sample forever until `shutdown` fires.
    pub async fn run(
        mut self,
        events: EventBus,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) {
        // CPU percentages are the difference between two refreshes, so the
        // very first sample is discarded after a short settling delay rather
        // than published as a bogus reading.
        tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;
        self.sample();

        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let telemetry = self.sample();
                    events.publish(NodeEvent::Telemetry(telemetry));
                }
                _ = shutdown.recv() => {
                    tracing::info!("telemetry shutting down");
                    return;
                }
            }
        }
    }
}

/// Static facts about this machine, read once at startup.
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub os_name: String,
    pub os_version: String,
    pub kernel: String,
    pub arch: String,
}

impl HostInfo {
    pub fn detect() -> Self {
        Self {
            hostname: System::host_name().unwrap_or_else(|| "unknown-host".into()),
            os_name: System::name().unwrap_or_else(|| std::env::consts::OS.to_string()),
            os_version: System::os_version().unwrap_or_else(|| "unknown".into()),
            kernel: System::kernel_version().unwrap_or_else(|| "unknown".into()),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    /// e.g. `"Windows 11 (x86_64)"`.
    pub fn summary(&self) -> String {
        format!("{} {} ({})", self.os_name, self.os_version, self.arch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_reports_plausible_numbers_for_this_machine() {
        let (mut collector, rx) = TelemetryCollector::new();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        collector.sample();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let t = collector.sample();

        assert!(t.cpu_cores >= 1, "should see at least one core");
        assert!(
            (0.0..=100.0).contains(&t.cpu_usage),
            "cpu out of range: {}",
            t.cpu_usage
        );

        // Any machine that can run this test has at least 256 MB and under
        // 100 TB of RAM. A unit slip would land far outside that.
        assert!(
            (256..100_000_000).contains(&t.ram_total_mb),
            "implausible total RAM: {} MB",
            t.ram_total_mb
        );
        assert!(t.ram_used_mb <= t.ram_total_mb, "used RAM exceeds total");
        assert!(t.disk_total_gb >= 0.0);
        assert!(
            t.disk_used_gb <= t.disk_total_gb + 0.001,
            "used disk exceeds total"
        );
        assert!(t.net_rx_kbps >= 0.0 && t.net_tx_kbps >= 0.0);

        // The watch receiver observes the same sample.
        assert_eq!(rx.borrow().ram_total_mb, t.ram_total_mb);
    }

    #[test]
    fn memory_is_populated_before_the_first_tick() {
        // A dashboard rendering immediately after construction must not show
        // a machine with 0 GB of RAM.
        let (_collector, rx) = TelemetryCollector::new();
        assert!(
            rx.borrow().ram_total_mb > 0,
            "the constructor should prime the sample"
        );
        assert!(rx.borrow().cpu_cores > 0);
    }

    #[test]
    fn host_info_is_populated() {
        let info = HostInfo::detect();
        assert!(!info.hostname.is_empty());
        assert!(!info.arch.is_empty());
        assert!(info.summary().contains(&info.arch));
    }

    #[test]
    fn available_helpers_never_go_negative() {
        let t = Telemetry {
            ram_used_mb: 900,
            ram_total_mb: 800,
            ..Telemetry::default()
        };
        assert_eq!(t.ram_available_mb(), 0, "saturating, not wrapping");

        let t = Telemetry {
            disk_used_gb: 9.0,
            disk_total_gb: 8.0,
            ..Telemetry::default()
        };
        assert_eq!(t.disk_available_gb(), 0.0);
    }
}
