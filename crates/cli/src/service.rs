//! `bifrost service` — run a node in the background, starting at login.
//!
//! Each platform gets its native mechanism: a systemd user unit on Linux, a
//! launchd agent on macOS, a Startup shortcut on Windows. All three are
//! per-user rather than system-wide, so none needs administrator rights —
//! a background LAN chat tool has no business asking for root.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Subcommand;

#[derive(Subcommand, Debug, Clone)]
pub enum Action {
    /// Install the service and start it at login.
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Show whether it is installed, and where.
    Status,
}

pub fn run(action: Action) -> Result<()> {
    let executable = std::env::current_exe().context("cannot locate the bifrost executable")?;

    match action {
        Action::Install => install(&executable),
        Action::Uninstall => uninstall(),
        Action::Status => status(),
    }
}

/// Where the platform expects the service definition.
fn definition_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot locate your home directory")?;

    Ok(if cfg!(target_os = "linux") {
        home.join(".config/systemd/user/bifrost.service")
    } else if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/dev.bifrost.node.plist")
    } else if cfg!(windows) {
        home.join("AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Startup/bifrost.cmd")
    } else {
        bail!("no service integration for this platform")
    })
}

fn install(executable: &std::path::Path) -> Result<()> {
    let path = definition_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    let contents = definition(executable);
    std::fs::write(&path, contents).with_context(|| format!("cannot write {}", path.display()))?;

    // Windows Startup entries must be executable scripts; the others are data
    // files the supervisor reads.
    println!("Installed {}", path.display());
    println!();

    if cfg!(target_os = "linux") {
        println!("Enable and start it:");
        println!("    systemctl --user daemon-reload");
        println!("    systemctl --user enable --now bifrost");
        println!();
        println!("To keep it running when you are not logged in:");
        println!("    sudo loginctl enable-linger $USER");
    } else if cfg!(target_os = "macos") {
        println!("Load it:");
        println!("    launchctl load -w {}", path.display());
    } else {
        println!("Bifrost will start the next time you sign in.");
        println!("To start it now without signing out:");
        println!("    start \"\" \"{}\"", path.display());
    }

    Ok(())
}

fn uninstall() -> Result<()> {
    let path = definition_path()?;

    if !path.exists() {
        println!("Not installed — nothing to remove.");
        return Ok(());
    }

    // Tell the user to stop it first rather than silently removing a
    // definition out from under a running supervisor.
    if cfg!(target_os = "linux") {
        println!("Stop it first if it is running:");
        println!("    systemctl --user disable --now bifrost");
    } else if cfg!(target_os = "macos") {
        println!("Stop it first if it is running:");
        println!("    launchctl unload -w {}", path.display());
    }

    std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    println!("Removed {}", path.display());
    Ok(())
}

fn status() -> Result<()> {
    let path = definition_path()?;

    if path.exists() {
        println!("Installed at {}", path.display());
        if cfg!(target_os = "linux") {
            println!("Check it with: systemctl --user status bifrost");
        } else if cfg!(target_os = "macos") {
            println!("Check it with: launchctl list | grep bifrost");
        }
    } else {
        println!("Not installed. Install with: bifrost service install");
    }

    Ok(())
}

/// The platform's service definition, pointing at this executable.
fn definition(executable: &std::path::Path) -> String {
    let exe = executable.display();

    if cfg!(target_os = "linux") {
        format!(
            "[Unit]\n\
             Description=Bifrost LAN node\n\
             Documentation=https://github.com/yourorg/bifrost\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exe} serve\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             Environment=BIFROST_LOG=info\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        )
    } else if cfg!(target_os = "macos") {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.bifrost.node</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardErrorPath</key>
    <string>/tmp/bifrost.err.log</string>
    <key>StandardOutPath</key>
    <string>/tmp/bifrost.out.log</string>
</dict>
</plist>
"#
        )
    } else {
        // A Startup shortcut rather than a Windows service: a real service
        // needs administrator rights and runs in session 0, where it cannot
        // reach the user's network profile the way a LAN tool needs to.
        format!("@echo off\r\nstart \"Bifrost\" /min \"{exe}\" serve\r\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_definition_names_this_executable_and_the_headless_subcommand() {
        let exe = PathBuf::from(if cfg!(windows) {
            r"C:\Program Files\Bifrost\bifrost.exe"
        } else {
            "/usr/local/bin/bifrost"
        });

        let text = definition(&exe);
        assert!(
            text.contains(&exe.display().to_string()),
            "should reference the executable"
        );
        assert!(
            text.contains("serve"),
            "the service must run the headless subcommand"
        );

        if cfg!(target_os = "linux") {
            assert!(text.contains("[Unit]") && text.contains("WantedBy=default.target"));
        } else if cfg!(target_os = "macos") {
            assert!(text.contains("<plist") && text.contains("dev.bifrost.node"));
        } else if cfg!(windows) {
            assert!(text.starts_with("@echo off"));
        }
    }

    #[test]
    fn the_definition_path_is_per_user() {
        // Never a system path: installing must not need administrator rights.
        let path = definition_path().expect("this platform should be supported");
        let home = dirs::home_dir().unwrap();
        assert!(
            path.starts_with(&home),
            "{} should live under {}",
            path.display(),
            home.display()
        );
    }
}
