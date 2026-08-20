//! The `bifrost` command.
//!
//! Running it bare is the whole product: it starts the node, serves the web
//! dashboard, prints a QR code for phones, and opens the terminal dashboard.
//! Everything else is a subcommand for a narrower job.

mod doctor;
mod qr;
mod service;
mod tui;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
use bifrost_core::net::iface;
use bifrost_core::net::protocol::PortSet;
use bifrost_core::util::logger;
use bifrost_core::{Config, Node};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bifrost",
    version,
    about = "Connect every device on your network — chat, files, telemetry and voice, with no setup",
    long_about = "Bifrost links the laptops, desktops, servers, phones and tablets on one \
                  local network. There is no account, no internet dependency and no relay \
                  server: nodes find each other by broadcasting on the LAN and talk directly.\n\n\
                  Run `bifrost` on two machines on the same Wi-Fi and they will see each \
                  other within a few seconds.",
    after_help = "Run `bifrost qr` to print a QR code your phone's camera can scan."
)]
struct Cli {
    /// Path to config.json (defaults to ~/.bifrost/config.json).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Shift every port except discovery, so several nodes can share one
    /// machine and still find each other.
    #[arg(long, global = true, value_name = "N")]
    port_offset: Option<u16>,

    /// Discovery port. Changing it creates a separate Bifrost network:
    /// nodes only ever see others on the same discovery port.
    #[arg(long, global = true, value_name = "PORT")]
    discovery_port: Option<u16>,

    /// Override this node's display name for this run.
    #[arg(long, global = true, value_name = "NAME")]
    name: Option<String>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, global = true, default_value = "info")]
    log: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start a node with the web dashboard and the terminal dashboard (default).
    Run {
        /// Do not serve the web dashboard.
        #[arg(long)]
        no_web: bool,
        /// Port for the web dashboard.
        #[arg(long, value_name = "PORT")]
        web_port: Option<u16>,
    },
    /// Start a node and the web dashboard with no terminal UI, for a service.
    Serve {
        #[arg(long, value_name = "PORT")]
        web_port: Option<u16>,
        /// Restrict the dashboard to this machine.
        #[arg(long)]
        localhost_only: bool,
    },
    /// Start a node with only the terminal dashboard.
    Tui,
    /// Print the dashboard address and a QR code to scan with a phone.
    Qr,
    /// Check the network, ports and configuration, and report what is wrong.
    Doctor,
    /// List devices seen by the node running on this machine.
    Peers,
    /// Broadcast a message through the node running on this machine.
    Chat {
        /// The message.
        #[arg(required = true, num_args = 1..)]
        words: Vec<String>,
    },
    /// Send a file through the node running on this machine.
    Send {
        /// Peer: node id, id prefix, username or alias.
        peer: String,
        /// File to send.
        path: PathBuf,
    },
    /// Show where the configuration lives and what it contains.
    Config,
    /// Install or remove Bifrost as a background service.
    Service {
        #[command(subcommand)]
        action: service::Action,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // A dedicated runtime rather than #[tokio::main], so the terminal
    // dashboard can own the main thread and restore it on the way out.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;

    runtime.block_on(run(cli))
}

async fn run(mut cli: Cli) -> Result<()> {
    // `take` rather than moving the field out: a partial move would make the
    // whole `cli` unborrowable for the helpers below.
    let command = cli.command.take().unwrap_or(Command::Run {
        no_web: false,
        web_port: None,
    });

    // These need no node, so handle them before starting one.
    if let Command::Doctor = &command {
        logger::init_with_default(&cli.log);
        return doctor::run(&resolve_config_path(&cli)).await;
    }
    if let Command::Config = &command {
        return show_config(&cli);
    }
    if let Command::Qr = &command {
        return show_qr(&cli);
    }
    if let Command::Service { action } = &command {
        return service::run(action.clone());
    }

    // One-shot commands drive an already-running node over its HTTP API,
    // rather than starting a second node that would bind the same ports.
    match &command {
        Command::Peers => return client_command(&cli, "/peers").await,
        Command::Chat { words } => return client_command(&cli, &words.join(" ")).await,
        Command::Send { peer, path } => {
            let absolute = path
                .canonicalize()
                .with_context(|| format!("no such file: {}", path.display()))?;
            return client_command(&cli, &format!("/sendfile {peer} {}", absolute.display())).await;
        }
        _ => {}
    }

    let config = load_config(&cli)?;
    let config_path = resolve_config_path(&cli);

    match command {
        Command::Run { no_web, web_port } => {
            let with_tui = true;
            start(cli, config, config_path, !no_web, web_port, false, with_tui).await
        }
        Command::Serve {
            web_port,
            localhost_only,
        } => {
            start(
                cli,
                config,
                config_path,
                true,
                web_port,
                localhost_only,
                false,
            )
            .await
        }
        Command::Tui => start(cli, config, config_path, false, None, false, true).await,
        _ => unreachable!("handled above"),
    }
}

/// Boot a node, optionally the dashboard, optionally the terminal UI.
async fn start(
    cli: Cli,
    mut config: Config,
    config_path: PathBuf,
    web: bool,
    web_port: Option<u16>,
    localhost_only: bool,
    tui: bool,
) -> Result<()> {
    if let Some(port) = web_port {
        config.ports.web = port;
    }

    // The terminal dashboard owns the screen, so logs go to a file instead of
    // being drawn over the UI.
    if tui {
        let log_path = Config::base_dir().join("bifrost.log");
        let _ = logger::init_for_tui(&log_path);
    } else {
        logger::init_with_default(&cli.log);
    }

    let ports = config.ports;
    let node = Node::start(config, &config_path)
        .await
        .context("could not start the node — run `bifrost doctor` to find out why")?;

    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(4);

    if web {
        let bind_ip = if localhost_only {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        };
        let bind = SocketAddr::new(bind_ip, ports.web);
        let web_node = node.clone();
        let web_shutdown = shutdown_tx.subscribe();

        tokio::spawn(async move {
            if let Err(e) = bifrost_web::serve(bind, web_node, web_shutdown).await {
                tracing::error!(error = %format!("{e:#}"), "the web dashboard stopped");
            }
        });

        if !tui {
            print_banner(&node, ports, localhost_only);
        }
    }

    if tui {
        // Blocks until the user quits.
        tui::run(node.clone(), web.then(|| node.dashboard_url())).await?;
    } else {
        tokio::signal::ctrl_c()
            .await
            .context("could not listen for Ctrl-C")?;
        println!("\nShutting down…");
    }

    let _ = shutdown_tx.send(());
    node.shutdown().await;
    Ok(())
}

/// The startup banner: the address to open, and a QR code for phones.
fn print_banner(node: &Node, ports: PortSet, localhost_only: bool) {
    let identity = node.identity();
    let url = if localhost_only {
        format!("http://127.0.0.1:{}", ports.web)
    } else {
        node.dashboard_url()
    };

    println!();
    println!("  ┌──────────────────────────────────────────────────────┐");
    println!("  │  ⬡  B I F R O S T                                    │");
    println!("  └──────────────────────────────────────────────────────┘");
    println!();
    println!(
        "  Node      {} ({})",
        identity.username, identity.device_type
    );
    println!("  Address   {}", iface::local_ip_string());
    println!("  Dashboard {url}");
    println!();

    if !localhost_only {
        match qr::render(&url) {
            Ok(code) => {
                println!("  Scan with a phone camera to open the dashboard:\n");
                for line in code.lines() {
                    println!("  {line}");
                }
                println!();
            }
            // A QR code is a convenience; the URL above is the real answer.
            Err(e) => tracing::debug!(error = %e, "could not render a QR code"),
        }
    }

    println!(
        "  Ports     discovery {} · chat {} · files {} · voice {}/{}",
        ports.discovery, ports.messaging, ports.files, ports.voice_signal, ports.voice_audio
    );
    println!("  Press Ctrl-C to stop.");
    println!();
}

/// Send a command line to the node running on this machine.
async fn client_command(cli: &Cli, line: &str) -> Result<()> {
    let config = load_config(cli)?;
    let url = format!("http://127.0.0.1:{}/api/command", config.ports.web);

    let response = http_post_json(&url, &serde_json::json!({ "line": line }))
        .await
        .with_context(|| {
            format!(
                "no node is answering on port {}.\n\
                 Start one with `bifrost` (or `bifrost serve`) first.",
                config.ports.web
            )
        })?;

    let text = response.get("text").and_then(|t| t.as_str()).unwrap_or("");
    if !text.is_empty() {
        println!("{text}");
    }
    Ok(())
}

/// A minimal HTTP POST, so the CLI needs no HTTP client dependency.
///
/// It only ever talks to `127.0.0.1` and only ever posts small JSON bodies,
/// so a full client would be a large dependency for one request shape.
async fn http_post_json(url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let rest = url
        .strip_prefix("http://")
        .context("only http:// is supported")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let payload = serde_json::to_vec(body)?;

    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect(authority),
    )
    .await
    .context("timed out connecting")?
    .context("could not connect")?;

    let request = format!(
        "POST /{path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;

    let text = String::from_utf8_lossy(&raw);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .context("the reply had no body")?;

    serde_json::from_str(body.trim()).context("the reply was not JSON")
}

fn resolve_config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(Config::default_path)
}

fn load_config(cli: &Cli) -> Result<Config> {
    let path = resolve_config_path(cli);
    let mut config = Config::load_or_create(&path)?;

    if let Some(offset) = cli.port_offset {
        config.ports = PortSet::default().offset_by(offset);
    }
    if let Some(port) = cli.discovery_port {
        config.ports.discovery = port;
    }
    if let Some(name) = &cli.name {
        config.identity.username = name.clone();
    }

    Ok(config)
}

fn show_config(cli: &Cli) -> Result<()> {
    let path = resolve_config_path(cli);
    let config = Config::load_or_create(&path)?;

    println!("Config file   {}", path.display());
    println!("Downloads     {}", config.download_dir.display());
    println!("Data          {}", config.data_dir.display());
    println!();
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

/// Print the dashboard address and a QR code for it.
///
/// Deliberately does not need a running node: it reads the port from the
/// config and the address from the network, so it can be run in a second
/// terminal beside a node that is already busy drawing its dashboard. The
/// terminal dashboard takes over the screen, which is exactly why the startup
/// banner cannot be the only place the QR code appears.
fn show_qr(cli: &Cli) -> Result<()> {
    let config = load_config(cli)?;
    let ip = iface::local_ipv4();
    let url = format!("http://{}:{}", ip, config.ports.web);

    println!();
    if ip.is_loopback() {
        println!("  This machine has no LAN address, so no other device can reach it.");
        println!("  Connect to Wi-Fi or plug in a cable, then try again.");
        println!("  Run `bifrost doctor` for details.");
        println!();
        return Ok(());
    }

    println!("  Dashboard  {url}");
    println!();

    match qr::render(&url) {
        Ok(code) => {
            for line in code.lines() {
                println!("  {line}");
            }
        }
        Err(e) => println!("  (could not draw a QR code: {e})"),
    }

    println!("  Scan with your phone's camera, on the same Wi-Fi.");
    println!();

    // The address is useless if nothing is serving it, and a QR code that
    // leads to a connection error is worse than none.
    if !is_node_running(config.ports.web) {
        println!(
            "  Note: no node is answering on port {} yet.",
            config.ports.web
        );
        println!("  Start one first, in another terminal:  bifrost");
        println!();
    }

    Ok(())
}

/// Is something serving the dashboard on this port?
fn is_node_running(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}
