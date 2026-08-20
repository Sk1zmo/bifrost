//! `bifrost scan` — what is on this network, and what could join it.
//!
//! Deliberately usable before anything is running: on a machine where Bifrost
//! has just been installed, the first question is "what else is here?", and
//! answering it should not require starting a node first. If one *is* running,
//! its peer list is folded in so nodes already discovered are marked as such.

use anyhow::Result;
use bifrost_core::net::peers::Peer;
use bifrost_core::net::scan::{scan, ScanOptions, ScanReport, ThisDevice};

/// Run a scan and print it.
pub async fn run(
    peers: Vec<Peer>,
    this: Option<ThisDevice>,
    options: ScanOptions,
) -> Result<()> {
    println!();
    println!("  Looking for devices…");
    if options.sweep_subnet {
        println!("  (probing the local subnet; this takes a few seconds)");
    }
    println!();

    let report = scan(&peers, this.as_ref(), &options).await;
    print_report(&report);
    Ok(())
}

fn print_report(report: &ScanReport) {
    if report.devices.is_empty() {
        println!("  Nothing found.");
        println!();
        println!("  That is unusual on a real network. Check that this machine");
        println!("  is connected, and try `bifrost doctor`.");
        return;
    }

    let width = report
        .devices
        .iter()
        .map(|d| d.address.len())
        .max()
        .unwrap_or(15)
        .max(7);

    let name_width = report
        .devices
        .iter()
        .filter_map(|d| d.name.as_ref().map(|n| n.chars().count()))
        .max()
        .unwrap_or(4)
        .clamp(4, 28);

    for device in &report.devices {
        // A filled marker for a Bifrost node, hollow for everything else: the
        // distinction the whole list exists to draw.
        let marker = if device.sharing_compute {
            "◆"
        } else if device.running_bifrost {
            "●"
        } else {
            "○"
        };

        let name = device.name.clone().unwrap_or_else(|| "—".to_string());
        let name: String = name.chars().take(name_width).collect();

        println!(
            "  {marker} {:<width$}  {:<name_width$}  {}",
            device.address,
            name,
            device.summary(),
        );

        if !device.details.is_empty() {
            let detail = device.details.join(", ");
            let detail: String = detail.chars().take(70).collect();
            println!("    {:<width$}  {detail}", "");
        }
    }

    println!();
    let bifrost = report.running_bifrost();
    let candidates = report.candidates();
    println!(
        "  {} device{} found in {:.1}s · {bifrost} running Bifrost · {candidates} not",
        report.devices.len(),
        if report.devices.len() == 1 { "" } else { "s" },
        report.scanned_for_secs,
    );

    for swept in &report.swept {
        println!("  Swept {swept}");
    }

    // Never let a partial scan look like a complete one.
    for skipped in &report.skipped {
        println!("  Skipped {skipped}");
    }

    if candidates > 0 {
        println!();
        println!("  To bring one of those in, install Bifrost on it:");
        println!("    curl -fsSL https://raw.githubusercontent.com/Sk1zmo/bifrost/main/install.sh | sh");
        println!("    irm https://raw.githubusercontent.com/Sk1zmo/bifrost/main/install.ps1 | iex");
    }
    println!();
}
