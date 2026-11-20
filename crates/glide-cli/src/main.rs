//! Glide CLI — Ultra-low-latency, native-feeling cross-platform software KVM.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use glide_core::client::run_native_client;
use glide_core::config::{default_config_dir, ClientConfig, GlideConfigFile, ServerConfig};
use glide_core::server::run_native_server;
use glide_hid::HidUsage;
use glide_net::discovery::{discover_servers, DEFAULT_DISCOVERY_PORT};
use glide_net::tls::ServerIdentity;
use glide_proto::Edge;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliEdge {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<CliEdge> for Edge {
    fn from(value: CliEdge) -> Self {
        match value {
            CliEdge::Left => Edge::Left,
            CliEdge::Right => Edge::Right,
            CliEdge::Top => Edge::Top,
            CliEdge::Bottom => Edge::Bottom,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "glide",
    version,
    about = "Ultra-low-latency, native-feeling cross-platform software KVM over QUIC"
)]
struct Cli {
    /// Optional path to a glide.toml configuration file
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Enable verbose debug logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run as the primary machine (shares its physical mouse, keyboard, and clipboard)
    Server {
        /// UDP bind address for QUIC (default: 0.0.0.0:24800)
        #[arg(long)]
        bind: Option<SocketAddr>,

        /// Override server hostname shown during LAN discovery
        #[arg(long)]
        name: Option<String>,

        /// Disable LAN auto-discovery beacons
        #[arg(long)]
        no_discovery: bool,

        /// Disable bidirectional clipboard sync
        #[arg(long)]
        no_clipboard: bool,

        /// Local escape key to immediately return focus (default: ScrollLock)
        #[arg(long)]
        escape_key: Option<String>,

        /// Outward push threshold in pixels at screen edges (default: 4)
        #[arg(long)]
        edge_threshold: Option<i32>,

        /// Mouse delta coalescing interval in microseconds (default: 1000 = 1000Hz; 0 = immediate)
        #[arg(long)]
        coalesce_us: Option<u64>,
    },

    /// Run as a secondary machine (receives mouse, keyboard, and clipboard from a Glide server)
    Client {
        /// Explicit server address (host:port). Omit to auto-discover on LAN!
        #[arg(short, long)]
        server: Option<String>,

        /// Filter LAN auto-discovery by server hostname
        #[arg(long)]
        server_name: Option<String>,

        /// Which edge of the server this machine sits on (left, right, top, bottom)
        #[arg(short, long, value_enum)]
        edge: Option<CliEdge>,

        /// Override client hostname
        #[arg(long)]
        name: Option<String>,

        /// Pin an exact server SHA-256 fingerprint (otherwise uses automatic TOFU)
        #[arg(long)]
        fingerprint: Option<String>,

        /// Skip TLS fingerprint verification (local debugging only)
        #[arg(long)]
        insecure: bool,

        /// Mouse speed multiplier (e.g. 0.8 or 1.25; default: 1.0)
        #[arg(long)]
        sensitivity: Option<f32>,

        /// Outward push threshold in pixels to return to server (default: 4)
        #[arg(long)]
        edge_threshold: Option<i32>,

        /// Disable bidirectional clipboard sync
        #[arg(long)]
        no_clipboard: bool,
    },

    /// Scan the local network for running Glide servers
    Discover {
        /// Scan duration in seconds (default: 3)
        #[arg(short, long, default_value_t = 3)]
        timeout: u64,

        /// Discovery UDP port (default: 24801)
        #[arg(long, default_value_t = DEFAULT_DISCOVERY_PORT)]
        port: u16,
    },

    /// Initialize default glide.toml and persistent Ed25519 TLS identity
    Init {
        /// Output path for config file (default: OS config dir / glide.toml)
        #[arg(short, long)]
        out: Option<PathBuf>,
    },

    /// Inspect monitors, DPI scaling, OS extensions, and German ISO HID key mappings
    Doctor,
}

fn load_config(path: Option<&PathBuf>) -> Result<GlideConfigFile> {
    if let Some(p) = path {
        return GlideConfigFile::load(p);
    }
    let default_path = default_config_dir().join("glide.toml");
    if default_path.exists() {
        return GlideConfigFile::load(&default_path);
    }
    Ok(GlideConfigFile::default())
}

fn init_logging(verbose: bool) {
    let default_level = if verbose { "debug" } else { "info" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    glide_backend::init_platform();
    let cli = Cli::parse();
    init_logging(cli.verbose);

    match cli.command {
        Command::Server {
            bind,
            name,
            no_discovery,
            no_clipboard,
            escape_key,
            edge_threshold,
            coalesce_us,
        } => {
            let mut cfg: ServerConfig = load_config(cli.config.as_ref())?.server;
            if let Some(b) = bind {
                cfg.bind = b;
            }
            if let Some(n) = name {
                cfg.name = n;
            }
            if no_discovery {
                cfg.discovery = false;
            }
            if no_clipboard {
                cfg.clipboard = false;
            }
            if let Some(ek) = escape_key {
                cfg.escape_key = ek;
            }
            if let Some(th) = edge_threshold {
                cfg.edge_threshold_px = th;
            }
            if let Some(cu) = coalesce_us {
                cfg.motion_coalesce_us = cu;
            }

            tokio::select! {
                res = run_native_server(cfg) => res?,
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Shutting down Glide server (Ctrl+C)");
                }
            }
        }

        Command::Client {
            server,
            server_name,
            edge,
            name,
            fingerprint,
            insecure,
            sensitivity,
            edge_threshold,
            no_clipboard,
        } => {
            let mut cfg: ClientConfig = load_config(cli.config.as_ref())?.client;
            if let Some(s) = server {
                cfg.server = Some(s);
            }
            if let Some(sn) = server_name {
                cfg.server_name = Some(sn);
            }
            if let Some(e) = edge {
                cfg.edge = e.into();
            }
            if let Some(n) = name {
                cfg.name = n;
            }
            if let Some(fp) = fingerprint {
                cfg.trusted_fingerprint = Some(fp);
            }
            if insecure {
                cfg.insecure_skip_verify = true;
            }
            if let Some(sens) = sensitivity {
                cfg.sensitivity = sens;
            }
            if let Some(th) = edge_threshold {
                cfg.edge_threshold_px = th;
            }
            if no_clipboard {
                cfg.clipboard = false;
            }

            tokio::select! {
                res = run_native_client(cfg) => res?,
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Shutting down Glide client (Ctrl+C)");
                }
            }
        }

        Command::Discover { timeout, port } => {
            println!("Scanning LAN on UDP port {port} for {timeout}s...");
            let servers = discover_servers(port, Duration::from_secs(timeout), None, false).await?;
            if servers.is_empty() {
                println!("No running Glide servers found on the local network.");
                println!("Make sure `glide server` is running and UDP port {port} is allowed.");
            } else {
                println!(
                    "{:<24} {:<12} {:<22} FINGERPRINT",
                    "HOSTNAME", "OS", "ADDRESS"
                );
                println!("{}", "-".repeat(92));
                for s in servers {
                    println!(
                        "{:<24} {:<12} {:<22} {}",
                        s.name, s.os, s.addr, s.fingerprint
                    );
                }
            }
        }

        Command::Init { out } => {
            let cfg_dir = default_config_dir();
            let cfg_path = out.unwrap_or_else(|| cfg_dir.join("glide.toml"));
            let cfg = GlideConfigFile::default();
            cfg.save(&cfg_path)?;
            let id = ServerIdentity::load_or_generate(&cfg.server.resolved_identity_dir())?;
            println!("Initialized Glide configuration at: {}", cfg_path.display());
            println!("Server hostname:    {}", cfg.server.name);
            println!("TLS fingerprint:    {}", id.fingerprint);
            println!();
            println!("Quick start:");
            println!("  Primary machine:   glide server");
            println!("  Secondary machine: glide client --edge right");
        }

        Command::Doctor => {
            let report = glide_backend::run_doctor()?;
            println!("Glide Doctor");
            println!("============");
            println!("OS Backend:   {:?}", report.os_kind);
            println!("Display:      {}", report.display_info);
            println!();
            println!("Monitors ({}):", report.geometry.monitors.len());
            for (i, m) in report.geometry.monitors.iter().enumerate() {
                println!(
                    "  [{i}] {} — {}x{} at ({}, {}) | scale {:.0}%{}",
                    m.name,
                    m.width,
                    m.height,
                    m.x,
                    m.y,
                    m.scale * 100.0,
                    if m.primary { " [PRIMARY]" } else { "" }
                );
            }
            println!();
            println!("Checks:");
            for note in &report.notes {
                println!("  • {note}");
            }
            println!();
            println!("German ISO / AltGr HID Key Mapping Verification:");
            for (label, key) in [
                ("AltGr (Right Alt)", HidUsage::ALT_RIGHT),
                ("ISO < > | (102nd)", HidUsage::ISO_BACKSLASH),
                ("DE # / ' (Backslash)", HidUsage::keyboard(0x31)),
                ("DE ß / ? / \\ (Minus)", HidUsage::keyboard(0x2D)),
                ("DE ^ / ° (Backquote)", HidUsage::keyboard(0x35)),
                ("ScrollLock (Escape)", HidUsage::SCROLL_LOCK),
            ] {
                let win = key.to_win_scan();
                let x11 = key.to_x11_keycode();
                println!(
                    "  • {label:<22} -> HID 0x{:02X} | Win {:?} | X11 {:?}",
                    key.id(),
                    win,
                    x11
                );
            }
        }
    }

    Ok(())
}
