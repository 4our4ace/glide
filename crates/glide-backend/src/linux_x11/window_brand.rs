//! Pure-Rust X11 window branding & fixed-size enforcement via `x11rb`.
//!
//! When Glide launches its embedded desktop UI window on Linux X11, Chromium/Chrome
//! in `--app` mode often sets `WM_CLASS` to `"Google-chrome"` / `"chrome-127.0.0.1__..."`,
//! causing Ubuntu GNOME Shell to display the Google Chrome icon and name in the dock,
//! top bar, and Alt+Tab switcher.
//!
//! This module finds Glide's X11 window by title / PID and atomically patches:
//! - `WM_CLASS` -> `"glide\0Glide\0"` (matches `StartupWMClass=Glide` in `glide.desktop`)
//! - `_GTK_APPLICATION_ID` -> `"glide"`
//! - `_NET_WM_ICON` -> crisp 32x32 + 64x64 Glide icon
//! - `WM_NORMAL_HINTS` -> locks min & max size to the exact content dimensions

use crate::icon::render_x11_net_wm_icon;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt, PropMode, Window, CLIENT_MESSAGE_EVENT,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperExt;

fn intern(conn: &RustConnection, name: &[u8]) -> Option<Atom> {
    conn.intern_atom(false, name)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
}

fn read_window_title(conn: &RustConnection, win: Window, net_wm_name: Atom, utf8: Atom) -> String {
    if let Ok(c) = conn.get_property(false, win, net_wm_name, utf8, 0, 128) {
        if let Ok(r) = c.reply() {
            if !r.value.is_empty() {
                return String::from_utf8_lossy(&r.value).into_owned();
            }
        }
    }
    if let Ok(c) = conn.get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 128) {
        if let Ok(r) = c.reply() {
            if !r.value.is_empty() {
                return String::from_utf8_lossy(&r.value).into_owned();
            }
        }
    }
    String::new()
}

fn read_window_class(conn: &RustConnection, win: Window) -> String {
    if let Ok(c) = conn.get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 128) {
        if let Ok(r) = c.reply() {
            return String::from_utf8_lossy(&r.value).into_owned();
        }
    }
    String::new()
}

fn collect_client_windows(
    conn: &RustConnection,
    root: Window,
    net_client_list: Atom,
) -> Vec<Window> {
    // Prefer EWMH `_NET_CLIENT_LIST` (all managed top-level windows), falling back to query_tree.
    if let Ok(c) = conn.get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, 1024) {
        if let Ok(r) = c.reply() {
            if let Some(vals) = r.value32() {
                let list: Vec<Window> = vals.collect();
                if !list.is_empty() {
                    return list;
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Ok(c) = conn.query_tree(root) {
        if let Ok(r) = c.reply() {
            out = r.children;
        }
    }
    out
}

fn find_glide_window(conn: &RustConnection, root: Window) -> Option<Window> {
    let net_client_list = intern(conn, b"_NET_CLIENT_LIST")?;
    let net_wm_name = intern(conn, b"_NET_WM_NAME")?;
    let utf8 = intern(conn, b"UTF8_STRING")?;

    for win in collect_client_windows(conn, root, net_client_list) {
        let title = read_window_title(conn, win, net_wm_name, utf8);
        if title.contains("Glide KVM") || title == "Glide" {
            return Some(win);
        }
        let cls = read_window_class(conn, win);
        if cls.contains("glide\0Glide") || cls.contains("127.0.0.1__24802") {
            return Some(win);
        }
    }
    None
}

/// Applies Glide's `WM_CLASS`, `_GTK_APPLICATION_ID`, `_NET_WM_ICON`, and fixed `WM_NORMAL_HINTS`
/// to the newly opened Glide UI window on X11 so GNOME/KDE/XFCE treat it as native Glide KVM.
pub fn spawn_x11_window_brander(fixed_width: u32, fixed_height: u32) {
    let _ = std::thread::Builder::new()
        .name("glide-x11-brand".into())
        .spawn(move || {
            let Ok((conn, screen_num)) = RustConnection::connect(None) else {
                return;
            };
            let root = conn.setup().roots[screen_num].root;
            let Some(net_wm_icon) = intern(&conn, b"_NET_WM_ICON") else {
                return;
            };
            let Some(gtk_app_id) = intern(&conn, b"_GTK_APPLICATION_ID") else {
                return;
            };
            let Some(utf8_string) = intern(&conn, b"UTF8_STRING") else {
                return;
            };

            // Prebuild 32x32 + 64x64 _NET_WM_ICON payload
            let mut icon_data = render_x11_net_wm_icon(32);
            icon_data.extend_from_slice(&render_x11_net_wm_icon(64));

            // ICCCM WM_SIZE_HINTS (18 x u32):
            // flags: PMinSize (1<<4) | PMaxSize (1<<5) = 0x30
            let mut size_hints = [0u32; 18];
            size_hints[0] = (1 << 4) | (1 << 5);
            size_hints[5] = fixed_width;
            size_hints[6] = fixed_height;
            size_hints[7] = fixed_width;
            size_hints[8] = fixed_height;

            let deadline = Instant::now() + Duration::from_secs(8);
            let mut branded_win: Option<Window> = None;

            while Instant::now() < deadline {
                if let Some(win) = find_glide_window(&conn, root) {
                    let _ = conn.change_property8(
                        PropMode::REPLACE,
                        win,
                        AtomEnum::WM_CLASS,
                        AtomEnum::STRING,
                        b"glide\0Glide\0",
                    );
                    let _ = conn.change_property8(
                        PropMode::REPLACE,
                        win,
                        gtk_app_id,
                        utf8_string,
                        b"glide",
                    );
                    let _ = conn.change_property32(
                        PropMode::REPLACE,
                        win,
                        net_wm_icon,
                        AtomEnum::CARDINAL,
                        &icon_data,
                    );
                    let _ = conn.change_property32(
                        PropMode::REPLACE,
                        win,
                        AtomEnum::WM_NORMAL_HINTS,
                        AtomEnum::WM_SIZE_HINTS,
                        &size_hints,
                    );
                    let _ = conn.flush();

                    if branded_win == Some(win) {
                        // Patched twice (once on first appearance, once after page title/icon loads)
                        break;
                    }
                    branded_win = Some(win);
                    // Wait briefly and re-apply once in case Chromium rewrote WM_CLASS on load
                    std::thread::sleep(Duration::from_millis(650));
                    continue;
                }
                std::thread::sleep(Duration::from_millis(120));
            }
        });
}

/// Searches X11 for an already-open Glide KVM window and raises/focuses it.
/// Returns `true` if an existing window was found and activated.
pub fn focus_existing_glide_window() -> bool {
    let Ok((conn, screen_num)) = RustConnection::connect(None) else {
        return false;
    };
    let root = conn.setup().roots[screen_num].root;
    let Some(win) = find_glide_window(&conn, root) else {
        return false;
    };
    let _ = conn.map_window(win);
    if let Some(net_active) = intern(&conn, b"_NET_ACTIVE_WINDOW") {
        let ev = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: win,
            type_: net_active,
            data: [1u32, 0, 0, 0, 0].into(),
        };
        let _ = conn.send_event(
            false,
            root,
            x11rb::protocol::xproto::EventMask::SUBSTRUCTURE_REDIRECT
                | x11rb::protocol::xproto::EventMask::SUBSTRUCTURE_NOTIFY,
            ev,
        );
    }
    let _ = conn.flush();
    true
}
