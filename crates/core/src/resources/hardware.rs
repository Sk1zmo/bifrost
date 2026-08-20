//! Graphics hardware detection.
//!
//! What a node can usefully contribute to a shared pool is mostly decided by
//! its GPU, so a machine has to be able to say what it has. There is no
//! portable way to ask, so each platform gets the query that actually works
//! there, and every one of them degrades to "no GPU found" rather than
//! failing — a node with no discrete graphics is still a perfectly good peer.

use serde::{Deserialize, Serialize};

/// A graphics adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    pub name: String,
    /// Video memory in MB, or `None` where the platform will not say.
    pub vram_mb: Option<u64>,
    pub vendor: GpuVendor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
    Other,
}

impl GpuVendor {
    // Only the platforms that enumerate adapters call this. On Android and
    // iOS `platform_gpus` returns nothing, so it would be dead code there —
    // and a warning on those targets is a warning in the app build, where it
    // is noise rather than a finding.
    #[cfg_attr(any(target_os = "android", target_os = "ios"), allow(dead_code))]
    fn detect(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        match () {
            _ if n.contains("nvidia")
                || n.contains("geforce")
                || n.contains("rtx")
                || n.contains("gtx")
                || n.contains("quadro")
                || n.contains("tesla") =>
            {
                Self::Nvidia
            }
            _ if n.contains("amd") || n.contains("radeon") || n.contains("ati") => Self::Amd,
            _ if n.contains("intel") || n.contains("arc") => Self::Intel,
            _ if n.contains("apple")
                || n.contains(" m1")
                || n.contains(" m2")
                || n.contains(" m3")
                || n.contains(" m4") =>
            {
                Self::Apple
            }
            _ => Self::Other,
        }
    }

    /// Is this the sort of GPU worth sending a model to?
    ///
    /// Integrated graphics can technically run inference, but routing work to
    /// one is usually slower than the asking machine's own CPU, so it should
    /// not be advertised as an accelerator.
    pub fn is_accelerator(self) -> bool {
        matches!(self, Self::Nvidia | Self::Amd | Self::Apple)
    }
}

impl Gpu {
    /// e.g. `"NVIDIA GeForce RTX 4060 Laptop GPU · 8 GB"`.
    pub fn summary(&self) -> String {
        match self.vram_mb {
            Some(mb) if mb > 0 => format!("{} · {:.0} GB", self.name, mb as f64 / 1024.0),
            _ => self.name.clone(),
        }
    }
}

/// Every graphics adapter this machine has, best first.
pub fn detect_gpus() -> Vec<Gpu> {
    let mut gpus = platform_gpus();

    // Discrete accelerators first, then by memory: the dashboard and the
    // scheduler both want the most capable one at the front.
    gpus.sort_by(|a, b| {
        b.vendor
            .is_accelerator()
            .cmp(&a.vendor.is_accelerator())
            .then(b.vram_mb.unwrap_or(0).cmp(&a.vram_mb.unwrap_or(0)))
    });
    gpus
}

#[cfg(target_os = "windows")]
fn platform_gpus() -> Vec<Gpu> {
    // WMI through PowerShell rather than a COM crate: one process launch at
    // startup is cheap, and it avoids a dependency that only builds on
    // Windows.
    //
    // AdapterRAM is a 32-bit field, so it silently caps at 4 GB and reports an
    // 8 GB card as 4. nvidia-smi is asked first where it exists, and the WMI
    // figure is only a fallback.
    let mut gpus = nvidia_smi_gpus();

    let script = "Get-CimInstance Win32_VideoController | \
                  ForEach-Object { \"$($_.Name)|$($_.AdapterRAM)\" }";
    if let Some(output) = run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
    ) {
        for line in output.lines() {
            let Some((name, ram)) = line.trim().split_once('|') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || gpus.iter().any(|g| g.name.eq_ignore_ascii_case(name)) {
                continue;
            }

            gpus.push(Gpu {
                vendor: GpuVendor::detect(name),
                name: name.to_string(),
                vram_mb: ram
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .map(|b| b / (1024 * 1024))
                    .filter(|&mb| mb > 0),
            });
        }
    }

    gpus
}

#[cfg(target_os = "linux")]
fn platform_gpus() -> Vec<Gpu> {
    let mut gpus = nvidia_smi_gpus();

    // lspci is the only thing present on essentially every distribution.
    if let Some(output) = run("lspci", &["-mm"]) {
        for line in output.lines() {
            let lower = line.to_ascii_lowercase();
            if !(lower.contains("vga compatible controller")
                || lower.contains("3d controller")
                || lower.contains("display controller"))
            {
                continue;
            }

            // Fields are quoted: slot "class" "vendor" "device" ...
            let fields: Vec<&str> = line.split('"').map(str::trim).collect();
            let vendor = fields.get(3).copied().unwrap_or("");
            let device = fields.get(5).copied().unwrap_or("");
            let name = format!("{vendor} {device}").trim().to_string();

            if name.is_empty() || gpus.iter().any(|g| g.name.eq_ignore_ascii_case(&name)) {
                continue;
            }
            gpus.push(Gpu {
                vendor: GpuVendor::detect(&name),
                name,
                vram_mb: None,
            });
        }
    }

    gpus
}

#[cfg(target_os = "macos")]
fn platform_gpus() -> Vec<Gpu> {
    // Apple silicon shares system memory with the GPU, so there is no separate
    // VRAM figure to report; the chip name is what matters.
    let Some(output) = run("system_profiler", &["SPDisplaysDataType"]) else {
        return Vec::new();
    };

    let mut gpus = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("Chipset Model:") {
            let name = name.trim().to_string();
            if !name.is_empty() {
                gpus.push(Gpu {
                    vendor: GpuVendor::detect(&name),
                    name,
                    vram_mb: None,
                });
            }
        }
    }
    gpus
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn platform_gpus() -> Vec<Gpu> {
    // Android and iOS have GPUs, but no API a plain process may query, and a
    // phone is not somewhere to route inference anyway.
    Vec::new()
}

/// Ask nvidia-smi, which reports true VRAM where WMI and lspci cannot.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn nvidia_smi_gpus() -> Vec<Gpu> {
    let Some(output) = run(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ],
    ) else {
        return Vec::new();
    };

    output
        .lines()
        .filter_map(|line| {
            let (name, memory) = line.split_once(',')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(Gpu {
                vendor: GpuVendor::Nvidia,
                name: name.to_string(),
                vram_mb: memory.trim().parse().ok(),
            })
        })
        .collect()
}

/// Run a command, returning its stdout if it succeeded.
///
/// A missing tool is the normal case, not an error: most machines have no
/// nvidia-smi, and every caller treats absence as "nothing found".
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn run(command: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(command)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendors_are_recognised_from_real_product_names() {
        let cases = [
            ("NVIDIA GeForce RTX 4060 Laptop GPU", GpuVendor::Nvidia),
            ("NVIDIA GeForce GTX 1650", GpuVendor::Nvidia),
            ("AMD Radeon RX 7900 XTX", GpuVendor::Amd),
            ("Intel(R) UHD Graphics", GpuVendor::Intel),
            ("Intel Arc A770", GpuVendor::Intel),
            ("Apple M3 Max", GpuVendor::Apple),
            ("Some Unknown Adapter", GpuVendor::Other),
        ];

        for (name, expected) in cases {
            assert_eq!(GpuVendor::detect(name), expected, "for {name}");
        }
    }

    #[test]
    fn only_discrete_gpus_count_as_accelerators() {
        // Routing a model to integrated graphics is usually slower than the
        // asking machine's own CPU, so it must not be advertised as one.
        assert!(GpuVendor::Nvidia.is_accelerator());
        assert!(GpuVendor::Amd.is_accelerator());
        assert!(GpuVendor::Apple.is_accelerator());
        assert!(!GpuVendor::Intel.is_accelerator());
        assert!(!GpuVendor::Other.is_accelerator());
    }

    #[test]
    fn summaries_read_naturally_with_and_without_vram() {
        let with = Gpu {
            name: "NVIDIA GeForce RTX 4060 Laptop GPU".into(),
            vram_mb: Some(8188),
            vendor: GpuVendor::Nvidia,
        };
        assert_eq!(with.summary(), "NVIDIA GeForce RTX 4060 Laptop GPU · 8 GB");

        let without = Gpu {
            name: "Apple M3 Max".into(),
            vram_mb: None,
            vendor: GpuVendor::Apple,
        };
        assert_eq!(without.summary(), "Apple M3 Max");

        // A zero reading is as good as unknown, and "· 0 GB" reads as broken.
        let zero = Gpu {
            vram_mb: Some(0),
            ..without.clone()
        };
        assert_eq!(zero.summary(), "Apple M3 Max");
    }

    #[test]
    fn detection_puts_the_most_capable_adapter_first() {
        // Ordering is what the scheduler picks by, so it has to be right even
        // on a laptop that reports integrated graphics first.
        let gpus = detect_gpus();
        for pair in gpus.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            if !a.vendor.is_accelerator() {
                assert!(
                    !b.vendor.is_accelerator(),
                    "{} came before accelerator {}",
                    a.name,
                    b.name
                );
            }
        }
    }

    #[test]
    fn detection_never_panics_on_this_machine() {
        // Whatever hardware and tooling is present, this must return.
        let _ = detect_gpus();
    }
}
