#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
//! Glide CLI & Desktop App Launcher — Ultra-low-latency cross-platform software KVM.
//!
//! Running `glide` with no arguments (e.g. double-clicking `glide.exe` on Windows or
//! clicking "Glide KVM" in Ubuntu's application launcher) opens the modern desktop GUI
//! with zero background console window.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use glide_backend::os_integration::{configure_elevated_task, configure_firewall, set_autostart};
use glide_backend::InputBackendKind;
use glide_core::client::run_native_client;
use glide_core::config::{default_config_dir, ClientConfig, GlideConfigFile, ServerConfig};
use glide_core::server::run_native_server;
use glide_gui::{run_gui_blocking, GuiOptions, DEFAULT_GUI_PORT};
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliBackend {
    Auto,
    Xtest,
    Portal,
    Uinput,
}

impl From<CliBackend> for InputBackendKind {
    fn from(value: CliBackend) -> Self {
        match value {
            CliBackend::Auto => InputBackendKind::Auto,
            CliBackend::Xtest => InputBackendKind::XTest,
            CliBackend::Portal => InputBackendKind::Portal,
            CliBackend::Uinput => InputBackendKind::Uinput,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SetupAction {
    /// Add Windows Defender Firewall rule (UDP 24800 & 24801) or show UFW command on Ubuntu
    Firewall,
    /// Enable Glide autostart at login (client mode)
    AutostartClient,
    /// Enable Glide autostart at login (server mode)
    AutostartServer,
    /// Disable Glide autostart
    AutostartOff,
    /// Register Windows elevated Scheduled Task (so Glide works over UAC/Admin windows)
    ElevatedTask,
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

    /// Subcommand (defaults to launching the desktop GUI if omitted)
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Launch the modern desktop GUI (also the default when running with no arguments)
    Gui {
        /// Localhost port for the embedded UI (default: 24802)
        #[arg(long, default_value_t = DEFAULT_GUI_PORT)]
        port: u16,

        /// Start minimized in background / tray without opening the window immediately
        #[arg(long)]
        minimized: bool,

        /// Automatically start `server` or `client` on GUI launch
        #[arg(long)]
        auto_start: Option<String>,
    },

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

        /// Corner dead-zone in pixels to protect close X / Show Desktop (default: 24)
        #[arg(long)]
        dead_corner: Option<i32>,

        /// Optional edge dwell delay in milliseconds before crossing (default: 0)
        #[arg(long)]
        edge_dwell_ms: Option<u64>,

        /// Allow edge switching even while a fullscreen game/app is focused
        #[arg(long)]
        no_fullscreen_lock: bool,

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

        /// Automatically scale pointer deltas by client_dpi / server_dpi
        #[arg(long)]
        auto_dpi: bool,

        /// Scroll speed multiplier (default: 1.0)
        #[arg(long)]
        scroll_speed: Option<f32>,

        /// Invert scroll wheel direction ("natural scrolling")
        #[arg(long)]
        invert_scroll: bool,

        /// Outward push threshold in pixels to return to server (default: 4)
        #[arg(long)]
        edge_threshold: Option<i32>,

        /// Corner dead-zone in pixels on the return edge (default: 24)
        #[arg(long)]
        dead_corner: Option<i32>,

        /// Input backend on Linux: `auto`, `xtest`, or `uinput` (hardware-native)
        #[arg(long, value_enum)]
        backend: Option<CliBackend>,

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

    /// One-click OS integration helpers (firewall, autostart, Windows elevated task)
    Setup {
        #[arg(value_enum)]
        action: SetupAction,
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

#[cfg(target_os = "windows")]
fn attach_parent_console_if_cli() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && !args[1].eq_ignore_ascii_case("gui") {
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(
                windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
            );
        }
    }
}

fn main() -> Result<()> {
    #[cfg(target_os = "windows")]
    attach_parent_console_if_cli();

    glide_backend::init_platform();
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let command = cli.command.unwrap_or(Command::Gui {
        port: DEFAULT_GUI_PORT,
        minimized: false,
        auto_start: None,
    });

    // The native GUI owns the main thread (its window event loop) and builds its own runtime.
    if let Command::Gui {
        port,
        minimized,
        auto_start,
    } = command
    {
        return run_gui_blocking(GuiOptions {
            config_path: cli.config,
            port,
            minimized,
            auto_start_role: auto_start,
        });
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_command(command, cli.config))
}

async fn run_command(command: Command, config: Option<PathBuf>) -> Result<()> {
    let cli_config = config;
    match command {
        Command::Gui { .. } => unreachable!("handled in main"),

        Command::Server {
            bind,
            name,
            no_discovery,
            no_clipboard,
            escape_key,
            edge_threshold,
            dead_corner,
            edge_dwell_ms,
            no_fullscreen_lock,
            coalesce_us,
        } => {
            let mut cfg: ServerConfig = load_config(cli_config.as_ref())?.server;
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
            if let Some(dc) = dead_corner {
                cfg.dead_corner_px = dc;
            }
            if let Some(dw) = edge_dwell_ms {
                cfg.edge_dwell_ms = dw;
            }
            if no_fullscreen_lock {
                cfg.auto_lock_fullscreen = false;
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
            auto_dpi,
            scroll_speed,
            invert_scroll,
            edge_threshold,
            dead_corner,
            backend,
            no_clipboard,
        } => {
            let mut cfg: ClientConfig = load_config(cli_config.as_ref())?.client;
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
            if auto_dpi {
                cfg.auto_dpi_scale = true;
            }
            if let Some(ss) = scroll_speed {
                cfg.scroll_speed = ss;
            }
            if invert_scroll {
                cfg.invert_scroll = true;
            }
            if let Some(th) = edge_threshold {
                cfg.edge_threshold_px = th;
            }
            if let Some(dc) = dead_corner {
                cfg.dead_corner_px = dc;
            }
            if let Some(b) = backend {
                cfg.input_backend = b.into();
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
            println!("  Desktop GUI:       glide");
            println!("  Primary machine:   glide server");
            println!("  Secondary machine: glide client --edge right");
        }

        Command::Setup { action } => match action {
            SetupAction::Firewall => println!("{}", configure_firewall()?),
            SetupAction::AutostartClient => {
                set_autostart(true, "gui --minimized --auto-start client")?;
                println!("Enabled autostart (client mode).");
            }
            SetupAction::AutostartServer => {
                set_autostart(true, "gui --minimized --auto-start server")?;
                println!("Enabled autostart (server mode).");
            }
            SetupAction::AutostartOff => {
                set_autostart(false, "")?;
                println!("Disabled autostart.");
            }
            SetupAction::ElevatedTask => println!("{}", configure_elevated_task()?),
        },

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
