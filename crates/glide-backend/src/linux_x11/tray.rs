//! Native Linux System Tray (Ubuntu GNOME top-right AppIndicator / KDE / XFCE / Sway)
//! powered by pure-Rust `ksni` (StatusNotifierItem over `zbus` D-Bus — zero C libraries).

use crate::icon::render_symbolic_argb32_be;
use crate::TrayAction;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, Icon, Status, ToolTip, Tray, TrayMethods};
use tokio::sync::mpsc;
use tracing::{info, warn};

struct GlideLinuxTray {
    tx: mpsc::UnboundedSender<TrayAction>,
    icons: Vec<Icon>,
}

impl Tray for GlideLinuxTray {
    fn id(&self) -> String {
        "glide-kvm".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn title(&self) -> String {
        "Glide KVM".into()
    }

    fn icon_name(&self) -> String {
        // Also provide an empty icon_name when using icon_pixmap so Ubuntu AppIndicators
        // always render our crisp embedded ARGB32 icon even before icon-cache refresh.
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Glide Software KVM".into(),
            description: "Click to open Glide KVM".into(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TrayAction::OpenUi);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            MenuItem::Standard(StandardItem {
                label: "Open Glide KVM".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayAction::OpenUi);
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: "Toggle Cursor Lock (Ctrl+Alt+L)".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayAction::ToggleLock);
                }),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "Quit Glide".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayAction::Quit);
                }),
                ..Default::default()
            }),
        ]
    }
}

/// Handle keeping the native Linux StatusNotifierItem alive until dropped or shut down.
pub struct LinuxTrayHandle {
    handle: Option<ksni::Handle<GlideLinuxTray>>,
}

impl LinuxTrayHandle {
    pub fn is_active(&self) -> bool {
        self.handle.is_some()
    }

    pub async fn shutdown(mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown().await;
        }
    }
}

/// Registers a native Linux StatusNotifierItem system tray icon on the user's D-Bus session.
///
/// Works out of the box on Ubuntu 22.04 GNOME (top-right bar via Ubuntu AppIndicators),
/// KDE Plasma, XFCE, Cinnamon, and Waybar. If no D-Bus session or tray host is available
/// (e.g. headless Xvfb), returns `handle.is_active() == false` cleanly without failing.
pub async fn spawn_linux_tray() -> (LinuxTrayHandle, mpsc::UnboundedReceiver<TrayAction>) {
    let (tx, rx) = mpsc::unbounded_channel();

    // White symbolic glyph: matches Ubuntu's top bar / other monochrome panel icons.
    let icons = [16u32, 22, 24, 32, 48]
        .into_iter()
        .map(|size| Icon {
            width: size as i32,
            height: size as i32,
            data: render_symbolic_argb32_be(size),
        })
        .collect();

    let tray = GlideLinuxTray { tx, icons };
    match tray.spawn().await {
        Ok(handle) => {
            info!("Registered native Linux StatusNotifierItem system tray icon");
            (
                LinuxTrayHandle {
                    handle: Some(handle),
                },
                rx,
            )
        }
        Err(e) => {
            warn!(error = %e, "Linux system tray unavailable on this session (D-Bus / StatusNotifierWatcher missing)");
            (LinuxTrayHandle { handle: None }, rx)
        }
    }
}
