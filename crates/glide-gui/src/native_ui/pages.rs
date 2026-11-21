//! Page views and sidebar for the native Glide UI.

use super::icons;
use super::theme::{ty, Palette, ThemePref};
use super::widgets::{
    badge, button, card, card_rows, empty_box, kbd, section_head, segmented, setting_row,
    slider_row, text_field, toggle, xs_button, BtnKind,
};
use super::{DeskDrag, DragKind, Role, Tab, Ui};
use crate::updater::UpdateCheckState;
use glide_backend::InputBackendKind;
use glide_core::config::UpdateChannel;
use glide_proto::{ClipboardFormat, Edge, OsKind};
use rune::{
    col, el, row, rule, spacer, text, Align, Color, Cursor, Drag, El, FileDialogOptions, Phase,
    ScrollSpec, Tx,
};
use std::borrow::Cow;

/// Root window layout (900x640): 208px sidebar + main content + toast overlay.
pub fn root_view(s: &Ui, p: Palette) -> El {
    let mut main_col = col()
        .key("main")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .fill(p.bg)
        .px(26.0)
        .pt(22.0)
        .pb(20.0)
        .gap(14.0);

    if let Some(err) = s.visible_error() {
        main_col = main_col.child(error_banner(p, err));
    }

    let page = match s.tab {
        Tab::Devices => devices_page(s, p),
        Tab::Input => input_page(s, p),
        Tab::Clipboard => clipboard_page(s, p),
        Tab::Settings => settings_page(s, p),
    };
    main_col = main_col.child(page);

    let mut root = row()
        .key("root")
        .role("window")
        .w_full()
        .h_full()
        .fill(p.bg)
        .child(sidebar(s, p))
        .child(main_col);

    if let Some(msg) = s.active_toast() {
        root = root.child(
            row()
                .key("toast")
                .pin(None, None, Some(22.0), Some(18.0))
                .px(13.0)
                .py(8.0)
                .radius(8.0)
                .fill(p.toast_bg)
                .shadow(4.0, 14.0, Color::BLACK.alpha(0.22))
                .child(text(msg).font(ty::LABEL).color(p.toast_text)),
        );
    }

    root
}

fn error_banner(p: Palette, msg: &str) -> El {
    row()
        .key("err-banner")
        .w_full()
        .min_h(34.0)
        .px(12.0)
        .py(6.0)
        .gap(10.0)
        .center_y()
        .justify_between()
        .radius(8.0)
        .fill(p.red_soft)
        .border(1.0, p.red_line)
        .child(
            text(msg)
                .font(ty::SUB_MED)
                .color(p.red)
                .grow(1.0)
                .shrink()
                .truncate(),
        )
        .child(
            row()
                .key("err-dismiss")
                .role("button")
                .label_static("Dismiss error")
                .size(22.0, 22.0)
                .center()
                .radius(5.0)
                .hover(p.red_line)
                .cursor(Cursor::Pointer)
                .on_press(|s: &mut Ui| {
                    s.dismissed_error = s.status.last_error.clone();
                })
                .child(icons::close_icon(12.0, p.red)),
        )
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

fn sidebar(s: &Ui, p: Palette) -> El {
    let brand = row()
        .key("brand")
        .w_full()
        .px(6.0)
        .pb(18.0)
        .gap(10.0)
        .items_start()
        .child(el().pt(2.0).child(icons::glide_mark(30.0, 7.5, p.accent)))
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(1.0)
                .child(text("Glide").font(ty::BRAND_TITLE).color(p.text))
                .child(
                    text(s.hostname.as_str())
                        .font(ty::BRAND_SUB)
                        .color(p.text_2)
                        .truncate(),
                )
                .child(
                    text(s.os_label)
                        .font(ty::BRAND_SUB)
                        .color(p.muted)
                        .truncate(),
                ),
        );

    type NavIcon = fn(Color) -> El;
    const NAV_ITEMS: [(Tab, &str, &str, NavIcon); 4] = [
        (Tab::Devices, "nav-Devices", "Devices", icons::nav_devices),
        (Tab::Input, "nav-Input", "Input", icons::nav_input),
        (
            Tab::Clipboard,
            "nav-Clipboard & Files",
            "Clipboard & Files",
            icons::nav_clipboard,
        ),
        (Tab::Settings, "nav-Settings", "Settings", icons::nav_system),
    ];

    let mut nav = col().key("nav").w_full().gap(2.0);
    for (tab, key, label, icon_fn) in NAV_ITEMS {
        let active = s.tab == tab;
        let icon_color = if active { p.text } else { p.muted };
        let mut item = row()
            .key(key)
            .role("tab")
            .label_static(label)
            .checked(active)
            .w_full()
            .h(32.0)
            .px(10.0)
            .gap(10.0)
            .center_y()
            .radius(6.0)
            .cursor(Cursor::Pointer)
            .focusable(true)
            .on_press(move |s: &mut Ui| s.tab = tab)
            .child(icon_fn(icon_color))
            .child(
                text(label)
                    .font(ty::BODY_MED)
                    .color(if active { p.text } else { p.text_2 })
                    .a11y_hidden(),
            );
        if active {
            item = item.fill(p.nav_active);
        } else {
            item = item.hover(p.hover);
        }
        nav = nav.child(item);
    }

    let running = s.is_running();
    let status_label = s.sidebar_status_text();
    let dot_color = if s.status.status.starts_with("Connected") {
        p.green
    } else if s.status.status.starts_with("Error") {
        p.red
    } else if running {
        p.amber
    } else {
        p.faint
    };

    let status_row = row()
        .key("status-pill")
        .w_full()
        .px(4.0)
        .gap(8.0)
        .center_y()
        .child(
            el().size(7.0, 7.0)
                .radius(3.5)
                .fill(dot_color)
                .a11y_hidden(),
        )
        .child(
            text(status_label.into_owned())
                .font(ty::LABEL)
                .color(p.text_2)
                .grow(1.0)
                .shrink()
                .truncate(),
        );

    let mut foot = col().key("side-foot").w_full().gap(10.0);

    if let UpdateCheckState::Available(avail) = &s.update_state {
        let banner_label = if avail.is_channel_switch {
            format!("Switch · {}", avail.short_label)
        } else {
            format!("Update · {}", avail.short_label)
        };
        let update_pill = row()
            .key("side-update-pill")
            .role("button")
            .label(format!("Update available: {}", avail.version))
            .w_full()
            .h(30.0)
            .px(10.0)
            .gap(7.0)
            .center_y()
            .radius(7.0)
            .fill(p.accent.alpha(0.11))
            .border(1.0, p.accent.alpha(0.32))
            .hover(p.accent.alpha(0.18))
            .press(p.accent.alpha(0.24))
            .cursor(Cursor::Pointer)
            .focusable(true)
            .on_press(|s: &mut Ui| s.tab = Tab::Settings)
            .child(el().size(6.0, 6.0).radius(3.0).fill(p.accent).a11y_hidden())
            .child(
                text(banner_label)
                    .font(ty::SUB_MED)
                    .color(p.accent)
                    .grow(1.0)
                    .shrink()
                    .truncate(),
            );
        foot = foot.child(update_pill);
    }

    foot = foot.child(status_row);

    if running {
        if let Some(rtt) = s.status.rtt_ms {
            let rtt_row = row()
                .key("rtt-row")
                .w_full()
                .px(4.0)
                .gap(8.0)
                .center_y()
                .justify_between()
                .child(
                    row()
                        .gap(6.0)
                        .center_y()
                        .child(text("Latency").font(ty::SUB).color(p.muted))
                        .child(text(fmt_rtt(rtt)).font(ty::SUB_MED).color(p.text_2)),
                )
                .child(icons::sparkline(&s.status.rtt_history, p.accent));
            foot = foot.child(rtt_row);
        }
    }

    let action_label = if running {
        "Stop"
    } else if s.role == Role::Server {
        "Start Server"
    } else {
        "Connect"
    };
    let action_btn = button(
        p,
        "start-stop-btn",
        action_label,
        if running {
            BtnKind::Danger
        } else {
            BtnKind::Primary
        },
        36.0,
        14.0,
        false,
        |s, tx| s.toggle_start_stop(tx),
    )
    .w_full();

    let locked = s.status.locked;
    let lock_color = if locked { p.amber } else { p.text };
    let lock_btn = row()
        .key("lock-btn")
        .role("button")
        .label_static(if locked {
            "Unlock cursor"
        } else {
            "Lock cursor"
        })
        .grow(1.0)
        .h(36.0)
        .gap(6.0)
        .center()
        .radius(7.0)
        .fill(if locked {
            p.amber.alpha(0.14)
        } else {
            p.surface
        })
        .border(1.0, if locked { p.amber } else { p.line_2 })
        .hover(p.hover)
        .press(p.press)
        .cursor(Cursor::Pointer)
        .focusable(true)
        .on_press(|s: &mut Ui| s.toggle_lock())
        .child(icons::lock_icon(locked, 14.5, lock_color))
        .child(
            text(if locked { "Locked" } else { "Lock" })
                .font(ty::LABEL)
                .color(lock_color)
                .a11y_hidden(),
        );

    let quit_btn = row()
        .key("quit-btn")
        .role("button")
        .label_static("Quit Glide")
        .grow(1.0)
        .h(36.0)
        .gap(6.0)
        .center()
        .radius(7.0)
        .fill(p.surface)
        .border(1.0, p.line_2)
        .hover(p.hover)
        .press(p.press)
        .cursor(Cursor::Pointer)
        .focusable(true)
        .on_press_tx(|s: &mut Ui, tx| s.quit_app(tx))
        .child(icons::power_icon(14.5, p.text_2))
        .child(text("Quit").font(ty::LABEL).color(p.text).a11y_hidden());

    foot = foot.child(action_btn).child(
        row()
            .key("foot-actions")
            .w_full()
            .gap(8.0)
            .child(lock_btn)
            .child(quit_btn),
    );

    col()
        .key("sidebar")
        .w(208.0)
        .h_full()
        .fill(p.sidebar)
        .border_sides([0.0, 1.0, 0.0, 0.0], p.line)
        .pt(16.0)
        .pb(14.0)
        .px(12.0)
        .child(brand)
        .child(nav)
        .child(spacer())
        .child(foot)
}

// ---------------------------------------------------------------------------
// Page 1: Devices
// ---------------------------------------------------------------------------

fn devices_page(s: &Ui, p: Palette) -> El {
    let caption = match s.role {
        Role::Server => "Share this PC’s mouse & keyboard with your other PCs",
        Role::Client => "Let another PC control this one",
    };

    let header = row()
        .key("devices-header")
        .w_full()
        .center_y()
        .justify_between()
        .child(
            col()
                .gap(2.0)
                .child(text("Devices").font(ty::PAGE_TITLE).color(p.text))
                .child(text(caption).font(ty::CAPTION).color(p.muted)),
        )
        .child(segmented(
            p,
            "role-seg",
            &[(Role::Server, "Server"), (Role::Client, "Client")],
            s.role,
            false,
            |s, role, tx| s.guard_role(role, tx),
        ));

    let desk = desk_stage(s, p);

    let hint_bar = row()
        .key("hint-bar")
        .w_full()
        .center_y()
        .child(text(s.layout_hint()).font(ty::CAPTION).color(p.muted));

    let bottom = match s.role {
        Role::Server => row()
            .key("server-bottom")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .gap(16.0)
            .child(server_connection_col(s, p))
            .child(server_clients_col(s, p)),
        Role::Client => row()
            .key("client-bottom")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .gap(16.0)
            .child(client_connection_col(s, p))
            .child(client_servers_col(s, p)),
    };

    col()
        .key("page-devices")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(12.0)
        .child(header)
        .child(desk)
        .child(hint_bar)
        .child(bottom)
}

/// Slot geometry inside the 640x286 desk stage.
pub fn slot_rect(edge: Edge) -> (f32, f32, f32, f32) {
    match edge {
        Edge::Left => (56.0, 91.0, 156.0, 104.0),
        Edge::Right => (428.0, 91.0, 156.0, 104.0),
        Edge::Top => (226.0, 19.0, 188.0, 52.0),
        Edge::Bottom => (226.0, 215.0, 188.0, 52.0),
    }
}

/// Find the closest edge slot when dragging a tile's center to `(cx, cy)`.
fn hit_test_slot(cx: f32, cy: f32) -> Option<Edge> {
    let edges = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];
    let mut best: Option<(Edge, f32)> = None;
    for edge in edges {
        let (x, y, w, h) = slot_rect(edge);
        let pad = 28.0;
        if cx >= x - pad && cx <= x + w + pad && cy >= y - pad && cy <= y + h + pad {
            let sx = x + w * 0.5;
            let sy = y + h * 0.5;
            let dist = (cx - sx).hypot(cy - sy);
            if best.is_none_or(|(_, d)| dist < d) {
                best = Some((edge, dist));
            }
        }
    }
    best.map(|(e, _)| e)
}

fn desk_stage(s: &Ui, p: Palette) -> El {
    let running = s.is_running();
    let active_target = s.status.active_target.as_str();
    let is_connected_client = s.client_connected();
    let client_controlling = active_target == "Local (Controlling)";
    let dragging_edge = s.desk_drag.as_ref().filter(|d| d.moved).map(|d| d.origin);
    let hover_edge = s
        .desk_drag
        .as_ref()
        .filter(|d| d.moved)
        .and_then(|d| d.hover);

    let mut stage = el()
        .key("desk-stage")
        .size(640.0, 286.0)
        .radius(12.0)
        .fill(p.canvas)
        .border(1.0, p.line);

    // Center monitor (226, 81, 188x124)
    let center = if s.role == Role::Server {
        let local_focused = !running || active_target == "Local";
        let can_return = running && !local_focused;
        let sub = if can_return {
            "Click to bring cursor back"
        } else {
            "This PC"
        };
        let mut tile = monitor_tile(
            p,
            "center-tile",
            (226.0, 81.0, 188.0, 124.0),
            Some(s.os_kind),
            &s.hostname,
            sub,
            local_focused,
            false,
            false,
            false,
        );
        if can_return {
            tile = tile
                .cursor(Cursor::Pointer)
                .on_press(|s: &mut Ui| s.switch_to_edge(None));
        }
        tile
    } else {
        let primary_focused = is_connected_client && !client_controlling;
        let server_label = s.client_server_label();
        let sub = if is_connected_client {
            "Server"
        } else if running {
            "Searching…"
        } else {
            "Not connected"
        };
        monitor_tile(
            p,
            "center-tile",
            (226.0, 81.0, 188.0, 124.0),
            None,
            server_label,
            sub,
            primary_focused,
            !is_connected_client,
            false,
            false,
        )
    };
    stage = stage.child(center);

    // 4 edge slots (keep any actively dragged tile to append last so it floats on top).
    let mut floating: Option<El> = None;
    let can_place_server = running && !s.status.peers.is_empty();
    let client_edge = s.config.client.edge;

    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        let (x, y, w, h) = slot_rect(edge);
        let is_hover = hover_edge == Some(edge);
        let is_drag_origin = dragging_edge == Some(edge);

        if s.role == Role::Client {
            if client_edge == edge {
                if is_drag_origin {
                    stage = stage.child(ghost_slot(p, edge, (x, y, w, h), false, is_hover));
                }
                let self_focused = is_connected_client && client_controlling;
                let sub: Cow<'static, str> = match s.status.rtt_ms {
                    Some(ms) if is_connected_client => Cow::Owned(fmt_rtt(ms)),
                    _ if is_connected_client => Cow::Borrowed("Connected"),
                    _ => Cow::Borrowed("This PC"),
                };
                let (tx_x, tx_y) =
                    if let Some(d) = s.desk_drag.as_ref().filter(|d| d.moved && d.origin == edge) {
                        (x + d.dx, y + d.dy)
                    } else {
                        (x, y)
                    };
                let tile = monitor_tile(
                    p,
                    "self-tile",
                    (tx_x, tx_y, w, h),
                    Some(s.os_kind),
                    &s.hostname,
                    &sub,
                    self_focused,
                    false,
                    false,
                    is_drag_origin,
                )
                .cursor(if is_drag_origin {
                    Cursor::Grabbing
                } else {
                    Cursor::Grab
                })
                .on_drag(move |s: &mut Ui, d: &mut Drag, tx| {
                    handle_tile_drag(s, DragKind::SelfClient, edge, d, tx);
                });

                if is_drag_origin {
                    floating = Some(tile);
                } else {
                    stage = stage.child(tile);
                }
            } else {
                stage = stage.child(ghost_slot(p, edge, (x, y, w, h), false, is_hover));
            }
        } else if let Some(peer) = s.status.peers.iter().find(|pr| pr.edge == edge) {
            if is_drag_origin {
                stage = stage.child(ghost_slot(p, edge, (x, y, w, h), false, is_hover));
            }
            let peer_focused = active_target == peer.name;
            let is_selected =
                s.status.peers.len() > 1 && s.active_peer().is_some_and(|ap| ap.name == peer.name);
            let sub: Cow<'static, str> = peer
                .rtt_ms
                .map(|ms| Cow::Owned(fmt_rtt(ms)))
                .unwrap_or(Cow::Borrowed("Connected"));
            let peer_name = peer.name.clone();
            let (tx_x, tx_y) =
                if let Some(d) = s.desk_drag.as_ref().filter(|d| d.moved && d.origin == edge) {
                    (x + d.dx, y + d.dy)
                } else {
                    (x, y)
                };

            let tile = monitor_tile(
                p,
                format!("peer-tile-{}", peer.name),
                (tx_x, tx_y, w, h),
                Some(peer.os),
                &peer.name,
                &sub,
                peer_focused,
                false,
                is_selected,
                is_drag_origin,
            )
            .cursor(if is_drag_origin {
                Cursor::Grabbing
            } else {
                Cursor::Grab
            })
            .on_drag(move |s: &mut Ui, d: &mut Drag, tx| {
                handle_tile_drag(s, DragKind::Peer(peer_name.clone()), edge, d, tx);
            });

            if is_drag_origin {
                floating = Some(tile);
            } else {
                stage = stage.child(tile);
                // Place Jump button as a separate sibling overlay so clicking it never starts a tile drag.
                if !peer_focused {
                    let jump_w = 46.0_f32;
                    let jump_h = 22.0_f32;
                    let jx = x + w - jump_w - 8.0;
                    let jy = y + if h < 70.0 { (h - jump_h) * 0.5 } else { 8.0 };
                    stage = stage.child(
                        row()
                            .key(jump_tile_key(edge))
                            .role("button")
                            .label(format!("Jump to {}", peer.name))
                            .size(jump_w, jump_h)
                            .absolute(jx, jy)
                            .center()
                            .radius(6.0)
                            .fill(p.surface)
                            .border(1.0, p.line_2)
                            .hover(p.hover)
                            .press(p.press)
                            .cursor(Cursor::Pointer)
                            .on_press(move |s: &mut Ui| s.switch_to_edge(Some(edge)))
                            .child(text("Jump").font(ty::BADGE).color(p.text).a11y_hidden()),
                    );
                }
            }
        } else {
            stage = stage.child(ghost_slot(
                p,
                edge,
                (x, y, w, h),
                !can_place_server,
                is_hover,
            ));
        }
    }

    if let Some(float_tile) = floating {
        stage = stage.child(float_tile);
    }

    stage
}

fn handle_tile_drag(s: &mut Ui, kind: DragKind, origin: Edge, d: &mut Drag, tx: &mut Tx) {
    let (ox, oy, ow, oh) = slot_rect(origin);
    match d.phase {
        Phase::Down => {
            s.desk_drag = Some(DeskDrag {
                kind,
                origin,
                dx: 0.0,
                dy: 0.0,
                moved: false,
                hover: None,
            });
        }
        Phase::Move => {
            let moved = d.dx.hypot(d.dy) > 4.0;
            let hover = if moved {
                let cx = ox + ow * 0.5 + d.dx;
                let cy = oy + oh * 0.5 + d.dy;
                hit_test_slot(cx, cy)
            } else {
                None
            };
            s.desk_drag = Some(DeskDrag {
                kind,
                origin,
                dx: d.dx,
                dy: d.dy,
                moved,
                hover,
            });
            tx.instant();
        }
        Phase::Up => {
            let prev = s.desk_drag.take();
            if d.cancelled() {
                return;
            }
            if let Some(drag) = prev {
                if drag.moved {
                    if let Some(target) = drag.hover {
                        if target != origin {
                            let peer_name = match drag.kind {
                                DragKind::Peer(n) => Some(n),
                                DragKind::SelfClient => None,
                            };
                            s.move_peer_live(target, peer_name, tx);
                        }
                    }
                } else if let DragKind::Peer(name) = drag.kind {
                    s.selected_peer_name = Some(name);
                    if d.clicks >= 2 {
                        s.switch_to_edge(Some(origin));
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn monitor_tile(
    p: Palette,
    key: impl Into<String>,
    rect: (f32, f32, f32, f32),
    os: Option<glide_proto::OsKind>,
    name: &str,
    sub: &str,
    focused: bool,
    dimmed: bool,
    selected: bool,
    elevated: bool,
) -> El {
    let (x, y, w, h) = rect;
    let compact = h < 70.0;

    let (fill, border_c, name_c, sub_c, ico_c) = if focused {
        (
            p.accent,
            p.accent,
            Color::WHITE,
            Color::WHITE.alpha(0.84),
            Color::WHITE.alpha(0.92),
        )
    } else if dimmed {
        (p.canvas, p.line_2, p.muted, p.muted, p.muted)
    } else if selected {
        (p.surface, p.accent, p.text, p.muted, p.text_2)
    } else {
        (p.surface, p.line_2, p.text, p.muted, p.text_2)
    };

    let mut body = col()
        .key(key)
        .size(w, h)
        .absolute(x, y)
        .px(12.0)
        .py(if compact { 6.0 } else { 10.0 })
        .gap(if compact { 1.0 } else { 3.0 })
        .center()
        .radius(10.0)
        .fill(fill);

    if dimmed {
        body = body.border(1.0, border_c).border_dash(4.0, 4.0);
    } else {
        body = body.border(if selected { 1.5 } else { 1.0 }, border_c);
    }
    if elevated {
        body = body.shadow(
            8.0,
            22.0,
            Color::BLACK.alpha(if p.dark { 0.45 } else { 0.18 }),
        );
    } else if !dimmed {
        body = body.shadow(1.0, 2.0, p.shadow_sm);
    }

    if compact {
        let mut line = row().gap(6.0).center_y();
        if let Some(os_k) = os {
            line = line.child(icons::os_icon(os_k, 13.0, ico_c));
        }
        line = line.child(
            text(name)
                .font(ty::BODY_SEMI)
                .color(name_c)
                .truncate(),
        );
        body = body.child(line);
        if !sub.is_empty() {
            body = body.child(text(sub).font(ty::SUB).color(sub_c).truncate());
        }
    } else {
        if let Some(os_k) = os {
            body = body.child(el().mb(2.0).child(icons::os_icon(os_k, 15.0, ico_c)));
        }
        body = body.child(
            text(name)
                .font(ty::BODY_SEMI)
                .color(name_c)
                .align(Align::Center)
                .truncate(),
        );
        if !sub.is_empty() {
            body = body.child(
                text(sub)
                    .font(ty::SUB)
                    .color(sub_c)
                    .align(Align::Center)
                    .truncate(),
            );
        }
    }

    if focused {
        let (cx, cy) = if compact {
            (w - 22.0, h - 20.0)
        } else {
            (w - 24.0, h - 23.0)
        };
        body = body.child(el().absolute(cx, cy).child(icons::cursor_glyph()));
    }
    body
}

fn ghost_slot(
    p: Palette,
    edge: Edge,
    rect: (f32, f32, f32, f32),
    inert: bool,
    drop_over: bool,
) -> El {
    let (x, y, w, h) = rect;
    let fill = if drop_over {
        p.accent_soft
    } else {
        Color::TRANSPARENT
    };
    let border = if drop_over { p.accent } else { p.line_2 };
    let arrow_c = if drop_over {
        p.accent
    } else if inert {
        p.faint.alpha(0.75)
    } else {
        p.muted
    };

    let mut slot = col()
        .key(ghost_key(edge))
        .role("button")
        .label_static(ghost_label(edge))
        .size(w, h)
        .absolute(x, y)
        .center()
        .gap(3.0)
        .radius(10.0)
        .fill(fill)
        .border(if drop_over { 1.5 } else { 1.0 }, border)
        .border_dash(4.0, 4.0)
        .child(icons::arrow_icon(edge, 14.0, arrow_c));

    if drop_over {
        slot = slot.child(text("Drop here").font(ty::BADGE).color(p.accent));
    }

    if !inert {
        slot = slot
            .hover(p.hover)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut Ui, tx| s.move_peer_live(edge, None, tx));
    } else {
        slot = slot.on_press_tx(|s: &mut Ui, tx| {
            s.toast("Connect a client first, then drag it into place", tx);
        });
    }
    slot
}

fn server_connection_col(s: &Ui, p: Palette) -> El {
    let addr_display = if let Some(first) = s.local_ips.first() {
        row()
            .center_y()
            .child(text(first.as_str()).font(ty::MONO).color(p.text))
    } else {
        row().child(
            text("No network connection")
                .font(ty::CAPTION)
                .color(p.muted),
        )
    };

    let addr_row = setting_row(
        p,
        "row-addr",
        "Address",
        None,
        row()
            .gap(8.0)
            .center_y()
            .child(addr_display)
            .child(xs_button(
                p,
                "copy-ip-btn",
                "Copy",
                BtnKind::Default,
                false,
                |s, tx| {
                    s.copy_local_ip(tx);
                },
            )),
    );

    let port_row = setting_row(
        p,
        "row-port",
        "Port",
        None,
        text_field(
            p,
            "port-input",
            &s.port_edit,
            Some(88.0),
            "24800",
            true,
            |s| &mut s.port_edit,
            Some(|s, _| s.commit_text_fields_and_push()),
        ),
    );

    let disc_row = setting_row(
        p,
        "row-discovery",
        "Discoverable",
        Some("Clients on this network find it automatically"),
        toggle(
            p,
            "disc-toggle",
            "Discoverable",
            s.config.server.discovery,
            |s, _| {
                s.config.server.discovery = !s.config.server.discovery;
                s.push_config();
            },
        ),
    );

    col()
        .key("conn-col")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(8.0)
        .child(section_head(p, "conn-head", "Connection", None))
        .child(card_rows(p, "conn-card", [addr_row, port_row, disc_row]))
}

fn server_clients_col(s: &Ui, p: Palette) -> El {
    let count = s.status.peers.len();
    let head = section_head(p, "clients-head", "Clients", None);

    let body = if s.status.peers.is_empty() {
        if s.is_running() {
            empty_box(
                p,
                "peers-wait",
                Some(icons::waiting_ring(p.line_2, p.accent)),
                "Waiting for clients",
                "Open Glide on the other PC and press Connect",
            )
        } else {
            empty_box(
                p,
                "peers-off",
                None,
                "Server is off",
                "Press Start Server to accept clients",
            )
        }
    } else {
        let mut list = col().key("peers-list").w_full();
        for (i, peer) in s.status.peers.iter().enumerate() {
            let is_focused = s.status.active_target == peer.name;
            let edge = peer.edge;
            let rtt = peer
                .rtt_ms
                .map(fmt_rtt)
                .unwrap_or_else(|| "connected".into());

            let item = row()
                .key(format!("peer-row-{i}"))
                .w_full()
                .min_h(46.0)
                .px(12.0)
                .py(8.0)
                .gap(10.0)
                .center_y()
                .child(
                    el().size(28.0, 28.0)
                        .radius(7.0)
                        .fill(p.canvas)
                        .center()
                        .child(icons::os_icon(peer.os, 14.0, p.text_2)),
                )
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(1.0)
                        .child(
                            text(peer.name.clone())
                                .font(ty::BODY_MED)
                                .color(p.text)
                                .truncate(),
                        )
                        .child(
                            row()
                                .gap(4.0)
                                .center_y()
                                .child(icons::arrow_icon(edge, 11.0, p.muted))
                                .child(
                                    text(format!("{} · {rtt}", edge_word(edge)))
                                        .font(ty::SUB)
                                        .color(p.muted)
                                        .truncate(),
                                ),
                        ),
                )
                .child(xs_button(
                    p,
                    format!("peer-jump-{i}"),
                    if is_focused { "Return" } else { "Jump" },
                    if is_focused {
                        BtnKind::Default
                    } else {
                        BtnKind::Primary
                    },
                    false,
                    move |s, _| s.switch_to_edge(if is_focused { None } else { Some(edge) }),
                ));
            list = list.child(item);
            if i + 1 < count {
                list = list.child(rule(1.0, p.line).a11y_hidden());
            }
        }
        col()
            .key("peers-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(list)
    };

    col()
        .key("clients-col")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(8.0)
        .child(head)
        .child(card(p, "clients-card").grow(1.0).min_h(0.0).child(body))
}

fn client_connection_col(s: &Ui, p: Palette) -> El {
    let server_row = setting_row(
        p,
        "row-server",
        "Server",
        None,
        text_field(
            p,
            "server-input",
            &s.server_edit,
            Some(176.0),
            "Find automatically",
            true,
            |s| &mut s.server_edit,
            Some(|s, _| s.commit_text_fields_and_push()),
        ),
    );

    let pos_row = setting_row(
        p,
        "row-pos",
        "Position",
        None,
        segmented(
            p,
            "client-edge-seg",
            &[
                (Edge::Left, "Left"),
                (Edge::Top, "Top"),
                (Edge::Bottom, "Bottom"),
                (Edge::Right, "Right"),
            ],
            s.config.client.edge,
            true,
            |s, edge, tx| s.move_peer_live(edge, None, tx),
        ),
    );

    let mut col_el = col()
        .key("client-conn-col")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(10.0)
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(p, "client-conn-head", "Connection", None))
                .child(card_rows(p, "client-conn-card", [server_row, pos_row])),
        );

    if !s.known_servers.is_empty() {
        let connected = s.client_connected();
        let active_addr = s.status.local_addr.as_deref();
        let active_fp = s
            .status
            .remote_fingerprint
            .as_deref()
            .or(s.config.client.trusted_fingerprint.as_deref())
            .map(glide_core::config::normalize_known_fingerprint)
            .filter(|fp| !fp.is_empty());
        let count = s.known_servers.len();
        let mut list = col().key("known-servers-list").w_full();
        for (i, srv) in s.known_servers.iter().enumerate() {
            let srv_fp = srv
                .fingerprint
                .as_deref()
                .map(glide_core::config::normalize_known_fingerprint)
                .filter(|fp| !fp.is_empty());
            let is_active = connected
                && match (&active_fp, &srv_fp) {
                    (Some(cur), Some(saved)) => cur == saved,
                    _ => active_addr == Some(srv.address.as_str()),
                };
            let srv_connect = srv.clone();
            let rm_key = srv
                .fingerprint
                .as_deref()
                .filter(|fp| !fp.trim().is_empty())
                .unwrap_or(&srv.address)
                .to_owned();
            let action = if is_active {
                badge(p, format!("known-active-{i}"), "Active", true, false)
            } else {
                button(
                    p,
                    format!("known-conn-{i}"),
                    if connected { "Switch" } else { "Connect" },
                    BtnKind::Primary,
                    22.0,
                    8.0,
                    false,
                    move |s, tx| s.connect_known_server(srv_connect.clone(), tx),
                )
            };
            let rm_btn = row()
                .key(format!("known-rm-{i}"))
                .role("button")
                .label(format!("Forget {}", srv.name))
                .size(22.0, 22.0)
                .center()
                .radius(5.0)
                .hover(p.hover)
                .press(p.press)
                .cursor(Cursor::Pointer)
                .focusable(true)
                .on_press_tx(move |s: &mut Ui, tx| s.forget_known_server(rm_key.clone(), tx))
                .child(icons::close_icon(10.5, p.muted));

            let display_addr = srv
                .address
                .strip_suffix(":24800")
                .unwrap_or(&srv.address)
                .to_owned();
            let item = row()
                .key(format!("known-row-{i}"))
                .w_full()
                .min_h(32.0)
                .px(10.0)
                .py(4.0)
                .gap(8.0)
                .center_y()
                .child(
                    row()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(6.0)
                        .center_y()
                        .child(
                            text(srv.name.clone())
                                .font(ty::SUB_MED)
                                .color(p.text)
                                .truncate(),
                        )
                        .child(
                            text(display_addr)
                                .font(ty::SUB)
                                .color(p.muted)
                                .truncate(),
                        ),
                )
                .child(row().gap(4.0).center_y().child(action).child(rm_btn));

            list = list.child(item);
            if i + 1 < count {
                list = list.child(rule(1.0, p.line).a11y_hidden());
            }
        }

        let scroll = col()
            .key("known-servers-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(list);

        col_el = col_el.child(
            col()
                .key("known-servers-sec")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(5.0)
                .child(section_head(p, "known-servers-head", "Saved servers", None))
                .child(
                    card(p, "known-servers-card")
                        .grow(1.0)
                        .min_h(0.0)
                        .child(scroll),
                ),
        );
    }

    col_el
}

fn client_servers_col(s: &Ui, p: Palette) -> El {
    let connected = s.client_connected();
    let title = if connected {
        "Connected to"
    } else {
        "Nearby servers"
    };
    let right = if connected {
        None
    } else {
        Some(xs_button(
            p,
            "rescan-btn",
            if s.scanning { "Scanning…" } else { "Rescan" },
            BtnKind::Ghost,
            s.scanning,
            |s, _| s.trigger_scan(false),
        ))
    };

    let body = if connected {
        let addr = s
            .config
            .client
            .server
            .as_deref()
            .unwrap_or("Found automatically");
        col()
            .key("connected-box")
            .w_full()
            .child(
                row()
                    .w_full()
                    .min_h(48.0)
                    .px(12.0)
                    .py(8.0)
                    .gap(10.0)
                    .center_y()
                    .child(
                        el().size(28.0, 28.0)
                            .radius(7.0)
                            .fill(p.green_soft)
                            .center()
                            .child(icons::check_icon(p.green)),
                    )
                    .child(
                        col()
                            .grow(1.0)
                            .shrink()
                            .min_w(0.0)
                            .gap(1.0)
                            .child(
                                text(s.client_server_label())
                                    .font(ty::BODY_MED)
                                    .color(p.text)
                                    .truncate(),
                            )
                            .child(text(addr).font(ty::SUB).color(p.muted).truncate()),
                    )
                    .child(badge(p, "conn-live", "Connected", true, false)),
            )
            .child(rule(1.0, p.line).a11y_hidden())
            .child(
                col().w_full().px(14.0).py(12.0).center().child(
                    text("Move the mouse past the server’s screen edge to control this PC.")
                        .w_full()
                        .font(ty::SUB)
                        .color(p.muted)
                        .wrap()
                        .align(Align::Center),
                ),
            )
    } else if s.discovered.is_empty() {
        empty_box(
            p,
            "servers-empty",
            Some(icons::waiting_ring(p.line_2, p.accent)),
            "Looking for servers…",
            "Start Glide as Server on your main PC",
        )
    } else {
        let n = s.discovered.len();
        let mut list = col().key("discovered-list").w_full();
        for (i, d) in s.discovered.iter().enumerate() {
            let addr = d.addr.clone();
            let fp = d.fingerprint.clone();
            let item = row()
                .key(format!("disc-{i}"))
                .w_full()
                .min_h(46.0)
                .px(12.0)
                .py(8.0)
                .gap(10.0)
                .center_y()
                .child(
                    el().size(28.0, 28.0)
                        .radius(7.0)
                        .fill(p.canvas)
                        .center()
                        .child(icons::os_icon(d.os, 14.0, p.text_2)),
                )
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(1.0)
                        .child(
                            text(d.name.clone())
                                .font(ty::BODY_MED)
                                .color(p.text)
                                .truncate(),
                        )
                        .child(text(d.addr.clone()).font(ty::SUB).color(p.muted).truncate()),
                )
                .child(xs_button(
                    p,
                    format!("disc-conn-{i}"),
                    "Connect",
                    BtnKind::Primary,
                    false,
                    move |s, tx| s.connect_discovered(addr.clone(), fp.clone(), tx),
                ));
            list = list.child(item);
            if i + 1 < n {
                list = list.child(rule(1.0, p.line).a11y_hidden());
            }
        }
        col()
            .key("discovered-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(list)
    };

    col()
        .key("client-servers-col")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(8.0)
        .child(section_head(p, "client-servers-head", title, right))
        .child(
            card(p, "client-servers-card")
                .grow(1.0)
                .min_h(0.0)
                .child(body),
        )
}

// ---------------------------------------------------------------------------
// Page 2: Input
// ---------------------------------------------------------------------------

fn input_page(s: &Ui, p: Palette) -> El {
    let caption = match s.role {
        Role::Server => {
            "Screen edges and cursor controls for this server. Changes apply instantly."
        }
        Role::Client => {
            "Pointer speed, return edge and key remapping for this PC. Changes apply instantly."
        }
    };

    let header = col()
        .key("input-header")
        .w_full()
        .gap(2.0)
        .child(text("Input").font(ty::PAGE_TITLE).color(p.text))
        .child(text(caption).font(ty::CAPTION).color(p.muted));

    let body = match s.role {
        Role::Server => input_server_cols(s, p),
        Role::Client => input_client_cols(s, p),
    };

    col()
        .key("page-input")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(14.0)
        .child(header)
        .child(body)
}

fn input_server_cols(s: &Ui, p: Palette) -> El {
    let thresh = s.config.server.edge_threshold_px;
    let dead = s.config.server.dead_corner_px;
    let dwell = s.config.server.edge_dwell_ms;

    let edge_rows = [
        slider_row(
            p,
            "srv-thresh-row",
            "Switch distance",
            format!("{thresh} px"),
            (thresh as f32 - 1.0) / (24.0 - 1.0),
            |s, n, commit| {
                let v = (1.0 + n * 23.0).round().clamp(1.0, 24.0) as i32;
                s.config.server.edge_threshold_px = v;
                if commit {
                    s.push_config();
                }
            },
        ),
        slider_row(
            p,
            "srv-corner-row",
            "Corner guard",
            format!("{dead} px"),
            dead as f32 / 80.0,
            |s, n, commit| {
                let v = (((n * 80.0) / 4.0).round() * 4.0).clamp(0.0, 80.0) as i32;
                s.config.server.dead_corner_px = v;
                if commit {
                    s.push_config();
                }
            },
        ),
        slider_row(
            p,
            "srv-dwell-row",
            "Edge delay",
            format!("{dwell} ms"),
            dwell as f32 / 200.0,
            |s, n, commit| {
                let v = (((n * 200.0) / 10.0).round() * 10.0).clamp(0.0, 200.0) as u64;
                s.config.server.edge_dwell_ms = v;
                if commit {
                    s.push_config();
                }
            },
        ),
        setting_row(
            p,
            "fs-lock-row",
            "Pause in fullscreen apps",
            Some("Games keep the cursor"),
            toggle(
                p,
                "fs-lock-toggle",
                "Pause in fullscreen apps",
                s.config.server.auto_lock_fullscreen,
                |s, _| {
                    s.config.server.auto_lock_fullscreen = !s.config.server.auto_lock_fullscreen;
                    s.push_config();
                },
            ),
        ),
        setting_row(
            p,
            "srv-sync-lock-row",
            "Sync Caps & Num Lock",
            Some("Send keyboard light state to clients"),
            toggle(
                p,
                "srv-sync-lock-toggle",
                "Sync Caps & Num Lock",
                s.config.server.sync_lock_keys,
                |s, _| {
                    s.config.server.sync_lock_keys = !s.config.server.sync_lock_keys;
                    s.push_config();
                },
            ),
        ),
        setting_row(
            p,
            "srv-pin-media-row",
            "Keep media & volume keys on this PC",
            Some("Volume & playback keys stay local"),
            toggle(
                p,
                "srv-pin-media-toggle",
                "Keep media & volume keys on this PC",
                s.config.server.pin_media_keys_local,
                |s, _| {
                    s.config.server.pin_media_keys_local = !s.config.server.pin_media_keys_local;
                    s.push_config();
                },
            ),
        ),
    ];

    let left_col = col()
        .key("input-server-left")
        .w(360.0)
        .gap(7.0)
        .child(section_head(p, "edges-head", "Screen edges", None))
        .child(card_rows(p, "edges-card", edge_rows));

    let note_card = card(p, "client-settings-note")
        .child(
            col()
                .w_full()
                .padding(14.0)
                .gap(10.0)
                .child(
                    row()
                        .w_full()
                        .gap(10.0)
                        .center_y()
                        .child(
                            el().size(28.0, 28.0)
                                .radius(7.0)
                                .fill(p.canvas)
                                .border(1.0, p.line)
                                .center()
                                .child(icons::nav_input(p.text_2)),
                        )
                        .child(
                            col()
                                .grow(1.0)
                                .shrink()
                                .min_w(0.0)
                                .gap(1.0)
                                .child(
                                    text("Set on each Client PC")
                                        .w_full()
                                        .font(ty::BODY_SEMI)
                                        .color(p.text),
                                )
                                .child(
                                    text("Per-machine tuning")
                                        .w_full()
                                        .font(ty::SUB)
                                        .color(p.muted),
                                ),
                        ),
                )
                .child(
                    text("Mouse speed, scroll speed, natural scrolling and key remapping are configured per client so each PC can feel right on its own.")
                        .w_full()
                        .font(ty::CAPTION)
                        .color(p.muted)
                        .wrap(),
                ),
        )
        .child(rule(1.0, p.line).a11y_hidden())
        .child(
            col().w_full().px(14.0).py(10.0).child(
                text("Open Glide’s Input tab on the client PC to adjust them.")
                    .w_full()
                    .font(ty::SUB)
                    .color(p.text_2)
                    .wrap(),
            ),
        );

    let right_col = col()
        .key("input-server-right")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(7.0)
        .child(section_head(
            p,
            "client-note-head",
            "Pointer & key remapping",
            None,
        ))
        .child(note_card);

    row()
        .key("input-server-cols")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .items_start()
        .gap(18.0)
        .child(left_col)
        .child(right_col)
}

fn input_client_cols(s: &Ui, p: Palette) -> El {
    let sens = s.config.client.sensitivity;
    let scroll_sp = s.config.client.scroll_speed;
    let mut pointer_rows = vec![
        slider_row(
            p,
            "sens-row",
            "Mouse speed",
            format!("{sens:.2}x"),
            (sens - 0.25) / (3.0 - 0.25),
            |s, n, commit| {
                let raw = 0.25 + n * (3.0 - 0.25);
                s.config.client.sensitivity = ((raw * 20.0).round() / 20.0).clamp(0.25, 3.0);
                if commit {
                    s.push_config();
                }
            },
        ),
        slider_row(
            p,
            "scroll-row",
            "Scroll speed",
            format!("{scroll_sp:.2}x"),
            (scroll_sp - 0.25) / (4.0 - 0.25),
            |s, n, commit| {
                let raw = 0.25 + n * (4.0 - 0.25);
                s.config.client.scroll_speed = ((raw * 20.0).round() / 20.0).clamp(0.25, 4.0);
                if commit {
                    s.push_config();
                }
            },
        ),
        setting_row(
            p,
            "dpi-row",
            "Match display scaling",
            Some("Same pointer feel across HiDPI screens"),
            toggle(
                p,
                "dpi-toggle",
                "Match display scaling",
                s.config.client.auto_dpi_scale,
                |s, _| {
                    s.config.client.auto_dpi_scale = !s.config.client.auto_dpi_scale;
                    s.push_config();
                },
            ),
        ),
        setting_row(
            p,
            "invert-row",
            "Natural scrolling",
            Some("Invert scroll wheel direction"),
            toggle(
                p,
                "invert-toggle",
                "Natural scrolling",
                s.config.client.invert_scroll,
                |s, _| {
                    s.config.client.invert_scroll = !s.config.client.invert_scroll;
                    s.push_config();
                },
            ),
        ),
    ];

    if !s.is_windows {
        let is_wayland = matches!(s.os_kind, OsKind::LinuxWayland);
        let backend_options: &[(InputBackendKind, &str)] = if is_wayland {
            &[
                (InputBackendKind::Auto, "Auto"),
                (InputBackendKind::Portal, "Portal"),
                (InputBackendKind::Uinput, "uinput"),
            ]
        } else {
            &[
                (InputBackendKind::Auto, "Auto"),
                (InputBackendKind::XTest, "XTest"),
                (InputBackendKind::Uinput, "uinput"),
            ]
        };
        let backend_sub = if is_wayland {
            "Wayland: Portal (EIS) or uinput"
        } else {
            "Linux only"
        };
        pointer_rows.push(setting_row(
            p,
            "backend-row",
            "Input method",
            Some(backend_sub),
            segmented(
                p,
                "backend-seg",
                backend_options,
                s.config.client.input_backend,
                true,
                |s, kind, _| {
                    s.config.client.input_backend = kind;
                    s.push_config();
                },
            ),
        ));
    }

    let thresh = s.config.client.edge_threshold_px;
    let dead = s.config.client.dead_corner_px;

    let return_rows = [
        slider_row(
            p,
            "cli-thresh-row",
            "Switch distance",
            format!("{thresh} px"),
            (thresh as f32 - 1.0) / (24.0 - 1.0),
            |s, n, commit| {
                let v = (1.0 + n * 23.0).round().clamp(1.0, 24.0) as i32;
                s.config.client.edge_threshold_px = v;
                if commit {
                    s.push_config();
                }
            },
        ),
        slider_row(
            p,
            "cli-corner-row",
            "Corner guard",
            format!("{dead} px"),
            dead as f32 / 80.0,
            |s, n, commit| {
                let v = (((n * 80.0) / 4.0).round() * 4.0).clamp(0.0, 80.0) as i32;
                s.config.client.dead_corner_px = v;
                if commit {
                    s.push_config();
                }
            },
        ),
        setting_row(
            p,
            "cli-sync-lock-row",
            "Sync Caps & Num Lock",
            Some("Follow the server’s keyboard lights"),
            toggle(
                p,
                "cli-sync-lock-toggle",
                "Sync Caps & Num Lock",
                s.config.client.sync_lock_keys,
                |s, _| {
                    s.config.client.sync_lock_keys = !s.config.client.sync_lock_keys;
                    s.push_config();
                },
            ),
        ),
    ];

    let left_col = col()
        .key("input-client-left")
        .w(360.0)
        .h_full()
        .min_h(0.0)
        .gap(12.0)
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(p, "pointer-head", "Pointer", None))
                .child(card_rows(p, "pointer-card", pointer_rows)),
        )
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(p, "return-head", "Return edge", None))
                .child(card_rows(p, "return-card", return_rows)),
        );

    // Right column: Key remapping
    let preset_btn = |key: &'static str,
                      a11y_label: &'static str,
                      a: &'static str,
                      b: &'static str,
                      from: &'static str,
                      to: &'static str| {
        row()
            .key(key)
            .role("button")
            .label_static(a11y_label)
            .h(24.0)
            .px(8.0)
            .gap(4.0)
            .center()
            .radius(6.0)
            .fill(p.surface)
            .border(1.0, p.line_2)
            .hover(p.hover)
            .press(p.press)
            .cursor(Cursor::Pointer)
            .focusable(true)
            .on_press_tx(move |s: &mut Ui, tx| s.add_remap_rule(from, to, tx))
            .child(text(a).font(ty::BADGE).color(p.text).a11y_hidden())
            .child(icons::arrow_icon(Edge::Right, 10.0, p.muted))
            .child(text(b).font(ty::BADGE).color(p.text).a11y_hidden())
    };

    let remap_head = section_head(
        p,
        "remap-head",
        "Key remapping",
        Some(
            row()
                .gap(6.0)
                .child(preset_btn(
                    "preset-esc",
                    "Caps to Esc",
                    "Caps",
                    "Esc",
                    "CapsLock",
                    "Escape",
                ))
                .child(preset_btn(
                    "preset-ctrl",
                    "Caps to Ctrl",
                    "Caps",
                    "Ctrl",
                    "CapsLock",
                    "ControlLeft",
                )),
        ),
    );

    let rules = &s.config.client.key_remap;
    let remap_list = if rules.is_empty() {
        empty_box(
            p,
            "remap-empty",
            None,
            "No remaps",
            "Every key passes through as pressed.\nGerman / ISO layouts work as-is.",
        )
    } else {
        let mut items = col().key("remap-items").w_full();
        for (i, r) in rules.iter().enumerate() {
            let item = row()
                .key(format!("remap-{i}"))
                .w_full()
                .min_h(40.0)
                .px(12.0)
                .py(6.0)
                .gap(8.0)
                .center_y()
                .justify_between()
                .child(
                    row()
                        .gap(6.0)
                        .center_y()
                        .child(kbd(p, &r.from))
                        .child(icons::arrow_icon(Edge::Right, 12.0, p.faint))
                        .child(kbd(p, &r.to)),
                )
                .child(
                    row()
                        .key(format!("remap-rm-{i}"))
                        .role("button")
                        .label_static("Remove remap")
                        .size(24.0, 24.0)
                        .center()
                        .radius(6.0)
                        .hover(p.hover)
                        .press(p.press)
                        .cursor(Cursor::Pointer)
                        .on_press(move |s: &mut Ui| s.remove_remap(i))
                        .child(icons::close_icon(11.0, p.muted)),
                );
            items = items.child(item);
            if i + 1 < rules.len() {
                items = items.child(rule(1.0, p.line).a11y_hidden());
            }
        }
        col()
            .key("remap-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(items)
    };

    let remap_add = row()
        .key("remap-add")
        .w_full()
        .px(10.0)
        .py(9.0)
        .gap(6.0)
        .center_y()
        .child(text_field(
            p,
            "remap-from",
            &s.remap_from,
            None,
            "From",
            false,
            |s| &mut s.remap_from,
            Some(|s, tx| s.submit_remap_inputs(tx)),
        ))
        .child(icons::arrow_icon(Edge::Right, 12.0, p.faint))
        .child(text_field(
            p,
            "remap-to",
            &s.remap_to,
            None,
            "To",
            false,
            |s| &mut s.remap_to,
            Some(|s, tx| s.submit_remap_inputs(tx)),
        ))
        .child(xs_button(
            p,
            "remap-add-btn",
            "Add",
            BtnKind::Primary,
            false,
            |s, tx| s.submit_remap_inputs(tx),
        ));

    let right_col = col()
        .key("input-client-right")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(7.0)
        .child(remap_head)
        .child(
            card(p, "remap-card")
                .grow(1.0)
                .min_h(0.0)
                .child(remap_list)
                .child(rule(1.0, p.line).a11y_hidden())
                .child(remap_add),
        );

    row()
        .key("input-client-cols")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(18.0)
        .child(left_col)
        .child(right_col)
}

// ---------------------------------------------------------------------------
// Page 3: Clipboard & Files
// ---------------------------------------------------------------------------

fn clipboard_page(s: &Ui, p: Palette) -> El {
    let header = col()
        .key("clip-header")
        .w_full()
        .gap(2.0)
        .child(text("Clipboard & Files").font(ty::PAGE_TITLE).color(p.text))
        .child(
            text("Sync clipboard text & images, or drag files across screen edges.")
                .font(ty::CAPTION)
                .color(p.muted),
        );

    // Left: Shared clipboard + Save history toggles + History
    let history_on = s.config.clipboard_history;
    let history_hidden = s.config.hide_clipboard_history;

    let clip_toggle_card = card_rows(
        p,
        "clip-toggle-card",
        [
            setting_row(
                p,
                "clip-toggle-row",
                "Shared clipboard",
                Some("Sync between both PCs"),
                toggle(
                    p,
                    "clip-toggle",
                    "Shared clipboard",
                    s.config.server.clipboard,
                    |s, _| {
                        let next = !s.config.server.clipboard;
                        s.config.server.clipboard = next;
                        s.config.client.clipboard = next;
                        s.push_config();
                    },
                ),
            ),
            setting_row(
                p,
                "clip-hist-toggle-row",
                "Save history",
                Some("Keep recent items to re-send"),
                toggle(
                    p,
                    "clip-hist-toggle",
                    "Save clipboard history",
                    history_on,
                    |s, tx| s.toggle_clipboard_history(tx),
                ),
            ),
        ],
    );

    let history_actions = if history_on && !history_hidden {
        let mut actions = row().gap(6.0).center_y();
        if !s.status.clipboard_history.is_empty() {
            actions = actions.child(xs_button(
                p,
                "clip-clear-btn",
                "Clear",
                BtnKind::Ghost,
                false,
                |s, tx| s.clear_clipboard_history(tx),
            ));
        }
        actions = actions.child(xs_button(
            p,
            "clip-hide-btn",
            "Hide",
            BtnKind::Default,
            false,
            |s, _| s.toggle_hide_clipboard_history(),
        ));
        Some(actions)
    } else {
        None
    };

    let history_body = if !history_on {
        empty_box(
            p,
            "clip-hist-off",
            None,
            "History is off",
            "Clipboard sync still works without keeping a list.",
        )
    } else if history_hidden {
        col()
            .key("clip-hidden")
            .w_full()
            .grow(1.0)
            .px(16.0)
            .py(18.0)
            .gap(10.0)
            .center()
            .child(
                el().size(34.0, 34.0)
                    .radius(8.0)
                    .fill(p.canvas)
                    .border(1.0, p.line)
                    .center()
                    .child(icons::eye_off_icon(17.0, p.muted)),
            )
            .child(
                col()
                    .w_full()
                    .gap(3.0)
                    .center()
                    .child(
                        text("History hidden")
                            .font(ty::BODY_MED)
                            .color(p.text_2)
                            .align(Align::Center),
                    )
                    .child(
                        text("Click Show to view recent clipboard items")
                            .w_full()
                            .font(ty::SUB)
                            .color(p.muted)
                            .wrap()
                            .align(Align::Center),
                    ),
            )
            .child(xs_button(
                p,
                "clip-reveal-btn",
                "Show",
                BtnKind::Default,
                false,
                |s, _| s.toggle_hide_clipboard_history(),
            ))
    } else if s.status.clipboard_history.is_empty() {
        empty_box(
            p,
            "clip-empty",
            None,
            "Nothing copied yet",
            "Recent clipboard items show up here",
        )
    } else {
        let len = s.status.clipboard_history.len();
        let mut items = col().key("clip-items").w_full();
        for (i, item) in s.status.clipboard_history.iter().enumerate() {
            let is_img = matches!(item.format, ClipboardFormat::ImagePng);
            let dir = if item.direction == "sent" {
                "Sent"
            } else {
                "Received"
            };
            let id = item.id;
            let row_el = row()
                .key(format!("clip-{i}"))
                .w_full()
                .min_h(46.0)
                .px(12.0)
                .py(8.0)
                .gap(10.0)
                .center_y()
                .child(
                    el().size(28.0, 28.0)
                        .radius(7.0)
                        .fill(p.canvas)
                        .center()
                        .child(if is_img {
                            icons::image_clip_icon(p.text_2)
                        } else {
                            icons::text_clip_icon(p.text_2)
                        }),
                )
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(1.0)
                        .child(
                            text(item.preview.clone())
                                .font(ty::BODY_MED)
                                .color(p.text)
                                .truncate(),
                        )
                        .child(
                            text(format!(
                                "{dir} · {} · {}",
                                if is_img { "Image" } else { "Text" },
                                format_bytes(item.bytes)
                            ))
                            .font(ty::SUB)
                            .color(p.muted)
                            .truncate(),
                        ),
                )
                .child(xs_button(
                    p,
                    format!("clip-resend-{i}"),
                    "Re-send",
                    BtnKind::Default,
                    false,
                    move |s, tx| s.resend_clipboard(id, tx),
                ));
            items = items.child(row_el);
            if i + 1 < len {
                items = items.child(rule(1.0, p.line).a11y_hidden());
            }
        }
        col()
            .key("clip-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(items)
    };

    let left_col = col()
        .key("clip-left")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(14.0)
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(p, "clip-sec-head", "Clipboard", None))
                .child(clip_toggle_card),
        )
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(7.0)
                .child(section_head(p, "hist-head", "History", history_actions))
                .child(
                    card(p, "hist-card")
                        .grow(1.0)
                        .min_h(0.0)
                        .child(history_body),
                ),
        );

    // Right: Edge drag-and-drop toggle + Send files dropzone + Save folder + Recent transfers
    let dnd_row = setting_row(
        p,
        "dnd-toggle-row",
        "Drag files across edges",
        Some("Pull files past a screen edge"),
        toggle(
            p,
            "dnd-toggle",
            "Drag files across edges",
            s.config.drag_drop_files,
            |s, tx| s.toggle_drag_drop_files(tx),
        ),
    );

    let dropzone = col()
        .key("dropzone")
        .role("button")
        .label_static("Drop files here or browse")
        .w_full()
        .h(78.0)
        .gap(6.0)
        .center()
        .radius(8.0)
        .fill(if s.drop_hover {
            p.accent_soft
        } else {
            p.canvas
        })
        .border(
            if s.drop_hover { 1.5 } else { 1.0 },
            if s.drop_hover { p.accent } else { p.line_2 },
        )
        .border_dash(4.0, 4.0)
        .hover(p.hover)
        .cursor(Cursor::Pointer)
        .focusable(true)
        .on_drop_hover(|s: &mut Ui, over, _| s.drop_hover = over)
        .on_drop_files(|s: &mut Ui, paths, tx| {
            s.drop_hover = false;
            s.send_file_paths(paths.to_vec(), tx);
        })
        .on_press_tx(|s: &mut Ui, tx| {
            if !s.is_running() {
                s.toast("Connect first, then send files", tx);
                return;
            }
            tx.open_file(
                FileDialogOptions {
                    title: "Send file to other PC".into(),
                    directory: None,
                    file_name: None,
                    filters: Vec::new(),
                },
                |s: &mut Ui, res, tx| match res {
                    Ok(Some(path)) => s.send_file_paths(vec![path], tx),
                    Ok(None) => {}
                    Err(e) => s.toast(format!("File picker unavailable: {e:?}"), tx),
                },
            );
        })
        .child(
            el().size(28.0, 28.0)
                .radius(7.0)
                .fill(p.surface)
                .border(1.0, p.line)
                .center()
                .child(icons::upload_icon(if s.drop_hover {
                    p.accent
                } else {
                    p.text_2
                })),
        )
        .child(
            row()
                .gap(4.0)
                .center_y()
                .child(text("Drop files here or").font(ty::CAPTION).color(p.muted))
                .child(text("browse").font(ty::LABEL).color(p.accent_text)),
        );

    let has_custom_receive_dir = s.config.receive_dir.is_some();
    let mut folder_btns = row().gap(5.0).center_y();
    if has_custom_receive_dir {
        folder_btns = folder_btns.child(xs_button(
            p,
            "receive-dir-reset-btn",
            "Reset",
            BtnKind::Ghost,
            false,
            |s, tx| s.reset_receive_dir(tx),
        ));
    }
    folder_btns = folder_btns.child(xs_button(
        p,
        "receive-dir-open-btn",
        "Open folder",
        BtnKind::Default,
        false,
        |s, tx| s.open_receive_folder(tx),
    ));

    let save_folder_row = col()
        .key("save-folder-row")
        .w_full()
        .px(10.0)
        .py(8.0)
        .gap(6.0)
        .child(
            row()
                .w_full()
                .center_y()
                .justify_between()
                .gap(8.0)
                .child(text("Save folder").font(ty::SUB_MED).color(p.text_2))
                .child(folder_btns),
        )
        .child(text_field(
            p,
            "receive-dir-input",
            &s.receive_dir_edit,
            None,
            "Downloads/Glide (default)",
            true,
            |s| &mut s.receive_dir_edit,
            Some(|s, _| s.commit_text_fields_and_push()),
        ));

    let transfer_card = card(p, "send-files-card")
        .child(dnd_row)
        .child(rule(1.0, p.line).a11y_hidden())
        .child(el().w_full().padding(8.0).child(dropzone))
        .child(rule(1.0, p.line).a11y_hidden())
        .child(save_folder_row);

    let files_body = if s.status.recent_files.is_empty() {
        empty_box(
            p,
            "files-empty",
            None,
            "No transfers yet",
            "Files you send or receive show up here",
        )
    } else {
        let show = s.status.recent_files.len().min(8);
        let mut items = col().key("files-items").w_full();
        for (i, f) in s.status.recent_files.iter().take(show).enumerate() {
            let sent = f.direction == "sent";
            let mut row_el = row()
                .key(format!("file-{i}"))
                .w_full()
                .min_h(46.0)
                .px(12.0)
                .py(8.0)
                .gap(8.0)
                .center_y()
                .child(
                    el().size(28.0, 28.0)
                        .radius(7.0)
                        .fill(p.canvas)
                        .center()
                        .child(icons::transfer_icon(sent, p.text_2)),
                )
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(1.0)
                        .child(
                            text(f.filename.clone())
                                .font(ty::BODY_MED)
                                .color(p.text)
                                .truncate(),
                        )
                        .child(
                            text(format!(
                                "{} · {}",
                                if sent { "Sent" } else { "Received" },
                                format_bytes(f.bytes)
                            ))
                            .font(ty::SUB)
                            .color(p.muted)
                            .truncate(),
                        ),
                );
            if let Some(saved_path) = f.saved_path.clone() {
                let open_path = saved_path.clone();
                let folder_path = saved_path;
                row_el = row_el.child(
                    row()
                        .gap(4.0)
                        .center_y()
                        .child(button(
                            p,
                            format!("file-open-{i}"),
                            "Open",
                            BtnKind::Default,
                            24.0,
                            7.0,
                            false,
                            move |s, tx| s.open_received_file(open_path.clone(), tx),
                        ))
                        .child(button(
                            p,
                            format!("file-folder-{i}"),
                            "Folder",
                            BtnKind::Default,
                            24.0,
                            7.0,
                            false,
                            move |s, tx| s.reveal_received_file(folder_path.clone(), tx),
                        )),
                );
            }
            items = items.child(row_el);
            if i + 1 < show {
                items = items.child(rule(1.0, p.line).a11y_hidden());
            }
        }
        col()
            .key("files-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(items)
    };

    let right_col = col()
        .key("clip-right")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(12.0)
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(p, "send-head", "File transfer", None))
                .child(transfer_card),
        )
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(7.0)
                .child(section_head(p, "transfers-head", "Recent transfers", None))
                .child(
                    card(p, "transfers-card")
                        .grow(1.0)
                        .min_h(0.0)
                        .child(files_body),
                ),
        );

    col()
        .key("page-clipboard")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(14.0)
        .child(header)
        .child(
            row()
                .key("clip-cols")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(18.0)
                .child(left_col)
                .child(right_col),
        )
}

// ---------------------------------------------------------------------------
// Page 4: Settings
// ---------------------------------------------------------------------------

fn settings_page(s: &Ui, p: Palette) -> El {
    let header = col()
        .key("settings-header")
        .w_full()
        .gap(2.0)
        .child(text("Settings").font(ty::PAGE_TITLE).color(p.text))
        .child(
            text("Appearance, startup and shortcuts.")
                .font(ty::CAPTION)
                .color(p.muted),
        );

    let autostart_desc = match s.role {
        Role::Server => "Starts minimized as Server",
        Role::Client => "Starts minimized as Client",
    };

    let general_sub = if s.tray_active {
        Some(
            text("Closing keeps Glide in the tray")
                .font(ty::SUB)
                .color(p.faint),
        )
    } else {
        None
    };

    let mut general_rows = vec![
        setting_row(
            p,
            "theme-row",
            "Theme",
            None,
            segmented(
                p,
                "theme-seg",
                &[
                    (ThemePref::System, "System"),
                    (ThemePref::Light, "Light"),
                    (ThemePref::Dark, "Dark"),
                ],
                s.theme,
                true,
                |s, pref, _| {
                    s.theme = pref;
                    pref.save();
                },
            ),
        ),
        setting_row(
            p,
            "autostart-row",
            "Start on login",
            Some(autostart_desc),
            toggle(
                p,
                "autostart-toggle",
                "Start on login",
                s.autostart,
                |s, tx| {
                    s.toggle_autostart(tx);
                },
            ),
        ),
    ];

    if s.is_windows {
        general_rows.push(setting_row(
            p,
            "elevated-row",
            "Control admin windows",
            Some("Let Glide control UAC / admin apps"),
            xs_button(
                p,
                "elevated-btn",
                "Set up",
                BtnKind::Default,
                false,
                |s, tx| s.run_elevated(tx),
            ),
        ));
    }

    let channel_desc = match s.config.update_channel {
        UpdateChannel::Stable => "Official tested releases",
        UpdateChannel::Nightly => "Dev builds · auto-checks",
    };
    general_rows.push(setting_row(
        p,
        "update-channel-row",
        "Update channel",
        Some(channel_desc),
        segmented(
            p,
            "update-channel-seg",
            &[
                (UpdateChannel::Stable, "Stable"),
                (UpdateChannel::Nightly, "Nightly"),
            ],
            s.config.update_channel,
            true,
            |s, ch, tx| s.set_update_channel(ch, tx),
        ),
    ));

    let (update_desc, update_btn): (Cow<'_, str>, El) = match &s.update_state {
        UpdateCheckState::Idle => (
            Cow::Borrowed(match s.config.update_channel {
                UpdateChannel::Stable => "Check GitHub for new releases",
                UpdateChannel::Nightly => "Check GitHub for the latest nightly build",
            }),
            xs_button(
                p,
                "update-check-btn",
                "Check",
                BtnKind::Default,
                false,
                |s, tx| s.check_for_updates(tx),
            ),
        ),
        UpdateCheckState::Checking => (
            Cow::Borrowed("Checking GitHub…"),
            xs_button(
                p,
                "update-check-btn",
                "Checking…",
                BtnKind::Default,
                true,
                |_, _| {},
            ),
        ),
        UpdateCheckState::UpToDate { latest } => (
            Cow::Owned(format!("You're up to date ({latest})")),
            xs_button(
                p,
                "update-check-btn",
                "Check",
                BtnKind::Default,
                false,
                |s, tx| s.check_for_updates(tx),
            ),
        ),
        UpdateCheckState::Available(avail) => {
            let desc = if avail.is_channel_switch {
                format!("Switch to {}", avail.version)
            } else {
                format!("{} is available", avail.version)
            };
            let btn = if avail.can_self_update() {
                xs_button(
                    p,
                    "update-install-btn",
                    if avail.is_channel_switch {
                        "Switch & Restart"
                    } else {
                        "Update & Restart"
                    },
                    BtnKind::Primary,
                    false,
                    |s, tx| s.install_update(tx),
                )
            } else {
                xs_button(
                    p,
                    "update-download-btn",
                    "Download",
                    BtnKind::Primary,
                    false,
                    |s, tx| s.open_update_download(tx),
                )
            };
            (Cow::Owned(desc), btn)
        }
        UpdateCheckState::Downloading {
            version,
            progress_pct,
        } => (
            Cow::Owned(format!("Downloading {version}… {progress_pct}%")),
            xs_button(
                p,
                "update-progress-btn",
                "Updating…",
                BtnKind::Default,
                true,
                |_, _| {},
            ),
        ),
        UpdateCheckState::Installing { version } => (
            Cow::Owned(format!("Verifying & installing {version}…")),
            xs_button(
                p,
                "update-install-progress-btn",
                "Installing…",
                BtnKind::Default,
                true,
                |_, _| {},
            ),
        ),
        UpdateCheckState::Error(msg) => (
            Cow::Borrowed(msg.as_str()),
            xs_button(
                p,
                "update-retry-btn",
                "Retry",
                BtnKind::Default,
                false,
                |s, tx| s.check_for_updates(tx),
            ),
        ),
    };

    general_rows.push(setting_row(
        p,
        "updates-row",
        "Updates",
        Some(&update_desc),
        update_btn,
    ));

    let general_card = card_rows(p, "general-card", general_rows);

    let advanced_toggle_card = card_rows(
        p,
        "advanced-toggle-card",
        [setting_row(
            p,
            "advanced-row",
            "Show advanced settings",
            Some("Security fingerprint, firewall & diagnostics"),
            toggle(
                p,
                "advanced-toggle",
                "Show advanced settings",
                s.show_advanced,
                |s, _| {
                    s.show_advanced = !s.show_advanced;
                },
            ),
        )],
    );

    let version_label = UpdateChannel::build_display_label(env!("CARGO_PKG_VERSION"));
    let version_foot = row()
        .key("version-foot")
        .w_full()
        .pt(12.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], p.line)
        .gap(8.0)
        .center_y()
        .justify_between()
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(icons::glide_mark(18.0, 4.5, p.accent))
                .child(text(version_label).font(ty::SUB_MED).color(p.text_2)),
        )
        .child(
            text("No account, no cloud, no internet.")
                .font(ty::SUB)
                .color(p.faint),
        );

    let mut left_inner = col()
        .key("settings-left-inner")
        .w_full()
        .gap(14.0)
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(p, "gen-head", "General", general_sub))
                .child(general_card),
        )
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(p, "adv-head", "Advanced", None))
                .child(advanced_toggle_card),
        );

    if s.show_advanced {
        let fp_text = s
            .status
            .fingerprint
            .as_deref()
            .or(s.fingerprint.as_deref())
            .unwrap_or("Not generated yet");

        let trust_label: Cow<'static, str> = match s.config.client.trusted_fingerprint.as_deref() {
            Some(fp) => {
                let end = fp
                    .char_indices()
                    .nth(18)
                    .map_or(fp.len(), |(idx, _)| idx);
                Cow::Owned(format!("Trusted server: {}…", &fp[..end]))
            }
            None => Cow::Borrowed("Trusts the first server it sees"),
        };

        let security_card = card_rows(
            p,
            "security-card",
            [
                row()
                    .key("fp-row")
                    .w_full()
                    .padding(12.0)
                    .gap(10.0)
                    .items_start()
                    .child(
                        col()
                            .key("fp-box")
                            .grow(1.0)
                            .shrink()
                            .min_w(0.0)
                            .px(10.0)
                            .py(8.0)
                            .radius(8.0)
                            .fill(p.canvas)
                            .border(1.0, p.line)
                            .child(
                                text(fp_text)
                                    .w_full()
                                    .shrink()
                                    .min_w(0.0)
                                    .font(ty::MONO_SM)
                                    .color(p.text_2)
                                    .wrap_anywhere(),
                            ),
                    )
                    .child(xs_button(
                        p,
                        "copy-fp-btn",
                        "Copy",
                        BtnKind::Default,
                        false,
                        |s, tx| {
                            s.copy_fingerprint(tx);
                        },
                    )),
                row()
                    .key("trust-row")
                    .w_full()
                    .min_h(40.0)
                    .px(12.0)
                    .py(7.0)
                    .gap(8.0)
                    .center_y()
                    .justify_between()
                    .child(
                        text(trust_label.into_owned())
                            .font(ty::SUB)
                            .color(p.muted)
                            .grow(1.0)
                            .shrink()
                            .truncate(),
                    )
                    .child(xs_button(
                        p,
                        "forget-btn",
                        "Forget server",
                        BtnKind::Default,
                        false,
                        |s, tx| s.reset_trust(tx),
                    )),
                setting_row(
                    p,
                    "firewall-row",
                    "Allow through firewall",
                    Some("Local network only · UDP 24800–24801"),
                    xs_button(
                        p,
                        "firewall-btn",
                        "Allow",
                        BtnKind::Default,
                        false,
                        |s, tx| {
                            s.run_firewall(tx);
                        },
                    ),
                ),
            ],
        );

        let transfers_card = card_rows(
            p,
            "adv-transfers-card",
            vec![setting_row(
                p,
                "max-file-mb-row",
                "Max file size",
                Some("Single file transfer ceiling (1–4096 MB)"),
                row()
                    .gap(6.0)
                    .center_y()
                    .child(text_field(
                        p,
                        "max-file-mb-input",
                        &s.max_file_mb_edit,
                        Some(76.0),
                        "256",
                        true,
                        |s| &mut s.max_file_mb_edit,
                        Some(|s, _| s.commit_text_fields_and_push()),
                    ))
                    .child(text("MB").font(ty::SUB).color(p.muted)),
            )],
        );

        left_inner = left_inner
            .child(
                col()
                    .w_full()
                    .gap(7.0)
                    .child(section_head(
                        p,
                        "sec-head",
                        "Security & firewall",
                        Some(text("TLS 1.3 · fingerprint").font(ty::SUB).color(p.faint)),
                    ))
                    .child(security_card),
            )
            .child(
                col()
                    .w_full()
                    .gap(7.0)
                    .child(section_head(p, "adv-transfers-head", "File transfers", None))
                    .child(transfers_card),
            );
    }

    let left_col = col()
        .key("settings-left")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(left_inner);

    // Right: Shortcuts (+ Diagnostics when Advanced is enabled)
    let plus = || text("+").font(ty::SUB).color(p.faint);
    let shortcuts_card = card_rows(
        p,
        "shortcuts-card",
        [
            setting_row(
                p,
                "sc-jump",
                "Jump to a side",
                None,
                row()
                    .gap(4.0)
                    .center_y()
                    .child(kbd(p, "Ctrl"))
                    .child(plus())
                    .child(kbd(p, "Alt"))
                    .child(plus())
                    .child(kbd(p, "Arrow")),
            ),
            setting_row(
                p,
                "sc-lock",
                "Lock to current screen",
                None,
                row()
                    .gap(4.0)
                    .center_y()
                    .child(kbd(p, "Ctrl"))
                    .child(plus())
                    .child(kbd(p, "Alt"))
                    .child(plus())
                    .child(kbd(p, "L")),
            ),
            setting_row(
                p,
                "sc-home",
                "Bring cursor home",
                None,
                kbd(p, "Scroll Lock"),
            ),
        ],
    );

    let mut right_col = col()
        .key("settings-right")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .gap(14.0)
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(
                    p,
                    "sc-head",
                    "Shortcuts",
                    Some(text("On the server").font(ty::SUB).color(p.faint)),
                ))
                .child(shortcuts_card),
        );

    if s.show_advanced {
        let diag_title = format!("Diagnostics · {}", s.doctor_display);
        let diag_head = row()
            .key("diag-head")
            .w_full()
            .h(24.0)
            .gap(8.0)
            .center_y()
            .justify_between()
            .child(
                text(diag_title)
                    .font(ty::SECTION)
                    .color(p.muted)
                    .grow(1.0)
                    .shrink()
                    .truncate(),
            )
            .child(badge(p, "diag-os", s.os_label, false, false));

        let diag_card = col()
            .key("diag-card")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .radius(10.0)
            .fill(p.canvas)
            .border(1.0, p.line)
            .padding(11.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(
                text(s.doctor_text.as_str())
                    .w_full()
                    .font(ty::MONO_SM)
                    .color(p.text_2)
                    .wrap(),
            );

        right_col = right_col.child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(7.0)
                .child(diag_head)
                .child(diag_card),
        );
    }

    col()
        .key("page-settings")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(14.0)
        .child(header)
        .child(
            row()
                .key("settings-cols")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(18.0)
                .child(left_col)
                .child(right_col),
        )
        .child(version_foot)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

pub fn fmt_rtt(v: f32) -> String {
    if v < 10.0 {
        format!("{v:.1} ms")
    } else {
        format!("{} ms", v.round() as i64)
    }
}

pub fn edge_word(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "left",
        Edge::Right => "right",
        Edge::Top => "top",
        Edge::Bottom => "bottom",
    }
}

fn ghost_key(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "ghost-Left",
        Edge::Right => "ghost-Right",
        Edge::Top => "ghost-Top",
        Edge::Bottom => "ghost-Bottom",
    }
}

fn ghost_label(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "Move to left",
        Edge::Right => "Move to right",
        Edge::Top => "Move to top",
        Edge::Bottom => "Move to bottom",
    }
}

fn jump_tile_key(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "jump-tile-Left",
        Edge::Right => "jump-tile-Right",
        Edge::Top => "jump-tile-Top",
        Edge::Bottom => "jump-tile-Bottom",
    }
}

pub fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
