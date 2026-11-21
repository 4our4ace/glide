//! Pure-Rust Linux Wayland cross-edge file drag-and-drop backend.
//!
//! ## Wayland Compositor Security Model
//! Unlike X11 and Windows, Wayland compositors intentionally enforce two security boundaries
//! that prevent synthetic cross-process drag-and-drop:
//! 1. **No synthetic external drag initiation (Drag-in):** `wl_data_device.start_drag` can
//!    only be called by a client that owns the currently focused `wl_surface` and presents a
//!    fresh compositor-issued pointer-down serial from an input event delivered to that
//!    surface. An external KVM daemon cannot inject a native drag onto another Wayland app.
//! 2. **No global invisible edge overlay windows (Drag-out):** Standard Wayland compositors
//!    (GNOME Mutter, KDE KWin) do not allow unprivileged clients to position invisible topmost
//!    1px edge-strip windows across arbitrary desktop coordinates to intercept drags from
//!    other applications. (Dragging files from Wayland -> Peer works via Glide's in-app dropzone.)
//!
//! ## Glide's Seamless Wayland Handoff
//! When cross-edge dragged files arrive on a Wayland client, Glide saves them into
//! `~/Downloads/Glide/` and publishes both `text/uri-list` (`file:///...`) and
//! `x-special/gnome-copied-files` onto the Wayland clipboard via `wl-clipboard-rs`
//! (`ext-data-control` / `wlr-data-control`), with automatic fallback to XWayland's X11
//! `CLIPBOARD` bridge when `DISPLAY` is available (e.g. GNOME < 49). The user can press
//! `Ctrl+V` immediately inside Files/Nautilus, VS Code, Chrome, Discord, or any Wayland app!

use crate::dnd::{
    format_gnome_copied_files, format_uri_list, DndBackend, DropDeliveryOutcome,
    FALLBACK_HINT_SINGLE_LINE,
};
use anyhow::{anyhow, Result};
use glide_proto::Edge;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info};
use wl_clipboard_rs::copy::{
    ClipboardType, MimeSource, MimeType, Options, Seat, Source,
};
use zbus::blocking::{Connection as DbusConnection, Proxy};
use zbus::zvariant::Value;

pub struct WaylandDndBackend {
    enabled: AtomicBool,
}

impl DndBackend for WaylandDndBackend {
    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    fn set_active_edges(&self, _edges: &[Edge], _dead_corner_px: i32) {}

    fn set_drag_armed(&self, _armed: bool) {}

    fn take_staged_files(&self, _edge: Edge) -> Option<Vec<PathBuf>> {
        // Wayland compositors prohibit global hidden edge-strip drop targets.
        None
    }

    fn cancel_local_drag(&self) {}

    fn deliver_drop(&self, paths: Vec<PathBuf>) -> Result<DropDeliveryOutcome> {
        if paths.is_empty() {
            return Ok(DropDeliveryOutcome::ClipboardFallback);
        }

        let uri_list = format_uri_list(&paths).into_bytes();
        let gnome_files = format_gnome_copied_files(&paths).into_bytes();
        if uri_list.is_empty() {
            return Ok(DropDeliveryOutcome::ClipboardFallback);
        }

        match publish_wayland_clipboard_uris(uri_list, gnome_files) {
            Ok(()) => {
                debug!(
                    count = paths.len(),
                    "Published text/uri-list to native Wayland clipboard (Ctrl+V ready)"
                );
                spawn_wayland_fallback_notification();
                Ok(DropDeliveryOutcome::ClipboardFallback)
            }
            Err(wl_err) => {
                if std::env::var_os("DISPLAY").is_some() {
                    debug!(
                        error = %wl_err,
                        "Wayland data-control unavailable; falling back to XWayland CLIPBOARD URI publish"
                    );
                    crate::linux_x11::dnd::publish_x11_clipboard_uris(&paths)?;
                    spawn_wayland_fallback_notification();
                    Ok(DropDeliveryOutcome::ClipboardFallback)
                } else {
                    Err(wl_err)
                }
            }
        }
    }
}

/// Sends a non-blocking desktop notification via `org.freedesktop.Notifications::Notify`
/// on a dedicated non-Tokio thread so Wayland users immediately know their files were saved
/// to `Downloads/Glide` and copied for `Ctrl+V`.
fn spawn_wayland_fallback_notification() {
    let _ = std::thread::Builder::new()
        .name("glide-wl-dnd-hint".into())
        .spawn(|| {
            if let Err(e) = send_freedesktop_notification("Glide", FALLBACK_HINT_SINGLE_LINE, 4000) {
                debug!(error = %e, "Could not send Wayland desktop notification for DnD fallback");
            }
        });
}

fn send_freedesktop_notification(summary: &str, body: &str, expire_timeout_ms: i32) -> Result<()> {
    let conn = DbusConnection::session()?;
    let proxy = Proxy::new(
        &conn,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )?;
    let actions: Vec<&str> = Vec::new();
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    let _id: u32 = proxy.call(
        "Notify",
        &(
            "Glide",
            0u32,
            "dialog-information",
            summary,
            body,
            actions,
            hints,
            expire_timeout_ms,
        ),
    )?;
    Ok(())
}

fn publish_wayland_clipboard_uris(uri_list: Vec<u8>, gnome_files: Vec<u8>) -> Result<()> {
    let mut opts = Options::new();
    opts.clipboard(ClipboardType::Regular)
        .seat(Seat::All)
        .foreground(true);

    let sources = vec![
        MimeSource {
            source: Source::Bytes(uri_list.into()),
            mime_type: MimeType::Specific("text/uri-list".into()),
        },
        MimeSource {
            source: Source::Bytes(gnome_files.into()),
            mime_type: MimeType::Specific("x-special/gnome-copied-files".into()),
        },
    ];

    let prepared = opts
        .prepare_copy_multi(sources)
        .map_err(|e| anyhow!("Wayland text/uri-list clipboard publish failed: {e}"))?;

    // Serve selection requests in-process on a background thread (never fork!).
    std::thread::Builder::new()
        .name("glide-wl-dnd-clip".into())
        .spawn(move || {
            if let Err(e) = prepared.serve() {
                debug!(error = %e, "Wayland DnD URI clipboard source ended");
            }
        })?;

    Ok(())
}

/// Starts the Linux Wayland drag-and-drop handoff backend.
pub fn start_wayland_dnd() -> Arc<dyn DndBackend> {
    info!("Wayland DnD backend active (drag-in saves to Downloads/Glide + publishes text/uri-list for Ctrl+V)");
    Arc::new(WaylandDndBackend {
        enabled: AtomicBool::new(true),
    })
}
