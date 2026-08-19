//! Resource advertisement.
//!
//! Phase 1 only advertises what a node is willing to contribute; Phase 2's
//! scheduler will consume the same [`SharedResources`] to place work.

use tokio::sync::watch;

use crate::net::protocol::{SharedResources, Telemetry};

/// Fraction of *idle* capacity offered by default.
///
/// A quarter leaves the machine's owner plainly in charge — the point of
/// sharing is that nobody notices it happening.
pub const DEFAULT_SHARE_FRACTION: f32 = 0.25;

/// User-controlled sharing policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharingPolicy {
    pub enabled: bool,
    /// Fraction of idle capacity to offer, clamped to `0.0..=1.0`.
    pub fraction: f32,
}

impl Default for SharingPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            fraction: DEFAULT_SHARE_FRACTION,
        }
    }
}

impl SharingPolicy {
    /// Compute the current offer from a telemetry sample.
    pub fn apply(&self, telemetry: &Telemetry) -> SharedResources {
        if !self.enabled {
            return SharedResources {
                enabled: false,
                cpu_offered_pct: 0.0,
                ram_offered_mb: 0,
                disk_offered_gb: 0.0,
            };
        }

        let fraction = self.fraction.clamp(0.0, 1.0);
        let idle_cpu = (100.0 - telemetry.cpu_usage).clamp(0.0, 100.0);

        SharedResources {
            enabled: true,
            cpu_offered_pct: idle_cpu * fraction,
            ram_offered_mb: (telemetry.ram_available_mb() as f32 * fraction) as u64,
            disk_offered_gb: telemetry.disk_available_gb() * fraction as f64,
        }
    }

    /// Human-readable summary for both dashboards.
    pub fn describe(&self, shared: &SharedResources) -> String {
        if !shared.enabled {
            return "Sharing off".to_string();
        }
        format!(
            "CPU {:.0}% · RAM {:.1} GB · Disk {:.0} GB",
            shared.cpu_offered_pct,
            shared.ram_offered_mb as f64 / 1024.0,
            shared.disk_offered_gb
        )
    }
}

/// Keeps a [`SharedResources`] watch channel in step with telemetry and policy.
#[derive(Debug)]
pub struct SharingService {
    policy: watch::Receiver<SharingPolicy>,
    telemetry: watch::Receiver<Telemetry>,
    tx: watch::Sender<SharedResources>,
}

impl SharingService {
    pub fn new(
        policy: watch::Receiver<SharingPolicy>,
        telemetry: watch::Receiver<Telemetry>,
    ) -> (Self, watch::Receiver<SharedResources>) {
        let initial = policy.borrow().apply(&telemetry.borrow());
        let (tx, rx) = watch::channel(initial);
        (
            Self {
                policy,
                telemetry,
                tx,
            },
            rx,
        )
    }

    /// Recompute whenever telemetry or the policy changes.
    pub async fn run(mut self, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
        loop {
            tokio::select! {
                changed = self.telemetry.changed() => {
                    if changed.is_err() { return; }
                    self.recompute();
                }
                changed = self.policy.changed() => {
                    if changed.is_err() { return; }
                    self.recompute();
                }
                _ = shutdown.recv() => return,
            }
        }
    }

    fn recompute(&self) {
        let shared = self.policy.borrow().apply(&self.telemetry.borrow());
        let _ = self.tx.send(shared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry() -> Telemetry {
        Telemetry {
            cpu_usage: 20.0,
            cpu_cores: 8,
            ram_used_mb: 8192,
            ram_total_mb: 16384,
            disk_used_gb: 100.0,
            disk_total_gb: 500.0,
            ..Telemetry::default()
        }
    }

    #[test]
    fn a_quarter_of_idle_capacity_is_offered_by_default() {
        let shared = SharingPolicy::default().apply(&telemetry());

        assert!(shared.enabled);
        assert_eq!(shared.cpu_offered_pct, 20.0); // 25% of 80% idle
        assert_eq!(shared.ram_offered_mb, 2048); // 25% of 8192 free
        assert_eq!(shared.disk_offered_gb, 100.0); // 25% of 400 free
    }

    #[test]
    fn disabling_zeroes_the_offer() {
        let policy = SharingPolicy {
            enabled: false,
            ..SharingPolicy::default()
        };
        let shared = policy.apply(&telemetry());

        assert!(!shared.enabled);
        assert_eq!(shared.cpu_offered_pct, 0.0);
        assert_eq!(shared.ram_offered_mb, 0);
        assert_eq!(policy.describe(&shared), "Sharing off");
    }

    #[test]
    fn a_saturated_machine_offers_no_cpu_and_never_goes_negative() {
        let busy = Telemetry {
            cpu_usage: 100.0,
            ..telemetry()
        };
        assert_eq!(SharingPolicy::default().apply(&busy).cpu_offered_pct, 0.0);

        // Some platforms briefly report over 100% across cores.
        let over = Telemetry {
            cpu_usage: 137.0,
            ..telemetry()
        };
        assert_eq!(SharingPolicy::default().apply(&over).cpu_offered_pct, 0.0);
    }

    #[test]
    fn an_out_of_range_fraction_is_clamped() {
        let greedy = SharingPolicy {
            enabled: true,
            fraction: 5.0,
        };
        let shared = greedy.apply(&telemetry());
        assert_eq!(shared.ram_offered_mb, 8192, "capped at all free RAM");

        let negative = SharingPolicy {
            enabled: true,
            fraction: -1.0,
        };
        assert_eq!(negative.apply(&telemetry()).ram_offered_mb, 0);
    }

    #[tokio::test]
    async fn the_service_republishes_when_telemetry_moves() {
        let (policy_tx, policy_rx) = watch::channel(SharingPolicy::default());
        let (telemetry_tx, telemetry_rx) = watch::channel(telemetry());
        let (service, mut shared_rx) = SharingService::new(policy_rx, telemetry_rx);

        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
        tokio::spawn(service.run(shutdown_rx));

        assert_eq!(shared_rx.borrow_and_update().ram_offered_mb, 2048);

        // Free up RAM; the offer should follow.
        telemetry_tx
            .send(Telemetry {
                ram_used_mb: 4096,
                ..telemetry()
            })
            .unwrap();
        shared_rx.changed().await.unwrap();
        assert_eq!(shared_rx.borrow_and_update().ram_offered_mb, 3072);

        // Turning sharing off should also propagate.
        policy_tx
            .send(SharingPolicy {
                enabled: false,
                fraction: 0.25,
            })
            .unwrap();
        shared_rx.changed().await.unwrap();
        assert!(!shared_rx.borrow().enabled);

        let _ = shutdown_tx.send(());
    }
}
