//! Page views and sidebar for the native Glide UI.

use super::icons;
use super::theme::{ty, Palette, ThemePref};
use super::widgets::{
    badge, button, card, card_rows, empty_box, kbd, section_head, segmented, setting_row,
    slider_row, text_field, toggle, xs_button, BtnKind,
};
use super::{DeskDrag, DragKind, Role, Tab, Ui};
use glide_backend::InputBackendKind;
use glide_proto::{ClipboardFormat, Edge};
use rune::{
    col, el, row, rule, spacer, text, Align, Color, Cursor, Drag, El, FileDialogOptions, Phase,
    ScrollSpec, Tx,
};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

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
        Tab::System => system_page(s, p),
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
            text(msg.to_owned())
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
                .label("Dismiss error")
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
        .center_y()
        .child(icons::glide_mark(28.0, 7.0, p.accent))
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(1.0)
                .child(text("Glide").font(ty::BRAND_TITLE).color(p.text))
                .child(
                    text(format!("{} · {}", s.hostname, s.os_label))
                        .font(ty::BRAND_SUB)
                        .color(p.muted)
                        .truncate(),
                ),
        );

    type NavIcon = fn(Color) -> El;
    let nav_items: [(Tab, &'static str, NavIcon); 4] = [
        (Tab::Devices, "Devices", icons::nav_devices),
        (Tab::Input, "Input", icons::nav_input),
        (Tab::Clipboard, "Clipboard & Files", icons::nav_clipboard),
        (Tab::System, "System", icons::nav_system),
    ];

    let mut nav = col().key("nav").w_full().gap(2.0);
    for (tab, label, icon_fn) in nav_items {
        let active = s.tab == tab;
        let icon_color = if active { p.text } else { p.muted };
        let mut item = row()
            .key(format!("nav-{label}"))
            .role("tab")
            .label(label)
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
            text(status_label)
                .font(ty::LABEL)
                .color(p.text_2)
                .grow(1.0)
                .shrink()
                .truncate(),
        );

    let mut foot = col().key("side-foot").w_full().gap(10.0).child(status_row);

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
        34.0,
        14.0,
        false,
        |s, tx| s.toggle_start_stop(tx),
    )
    .w_full();

    let locked = s.status.locked;
    let lock_color = if locked { p.amber } else { p.text_2 };
    let lock_btn = row()
        .key("lock-btn")
        .role("button")
        .label(if locked {
            "Unlock cursor"
        } else {
            "Lock cursor"
        })
        .grow(1.0)
        .h(28.0)
        .gap(6.0)
        .center()
        .radius(6.0)
        .fill(if locked {
            p.amber.alpha(0.12)
        } else {
            Color::TRANSPARENT
        })
        .hover(p.hover)
        .press(p.press)
        .cursor(Cursor::Pointer)
        .focusable(true)
        .on_press(|s: &mut Ui| s.toggle_lock())
        .child(icons::lock_icon(locked, lock_color))
        .child(
            text(if locked { "Locked" } else { "Lock" })
                .font(ty::SUB_MED)
                .color(lock_color)
                .a11y_hidden(),
        );

    let quit_btn = row()
        .key("quit-btn")
        .role("button")
        .label("Quit Glide")
        .grow(1.0)
        .h(28.0)
        .gap(6.0)
        .center()
        .radius(6.0)
        .hover(p.hover)
        .press(p.press)
        .cursor(Cursor::Pointer)
        .focusable(true)
        .on_press_tx(|s: &mut Ui, tx| s.quit_app(tx))
        .child(icons::power_icon(p.muted))
        .child(text("Quit").font(ty::SUB_MED).color(p.text_2).a11y_hidden());

    foot = foot.child(action_btn).child(
        row()
            .key("foot-actions")
            .w_full()
            .gap(6.0)
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
        .justify_between()
        .child(text(s.layout_hint()).font(ty::CAPTION).color(p.muted))
        .child(badge(
            p,
            "cursor-badge",
            format!("Cursor: {}", s.friendly_target()),
            false,
            s.is_running(),
        ));

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
            if best.map_or(true, |(_, d)| dist < d) {
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
            &server_label,
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
                let sub = match s.status.rtt_ms {
                    Some(ms) if is_connected_client => fmt_rtt(ms),
                    _ if is_connected_client => "Connected".into(),
                    _ => "This PC".into(),
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
        } else if let Some(peer) = s.status.peers.iter().find(|pr| pr.edge == edge).cloned() {
            if is_drag_origin {
                stage = stage.child(ghost_slot(p, edge, (x, y, w, h), false, is_hover));
            }
            let peer_focused = active_target == peer.name;
            let is_selected =
                s.status.peers.len() > 1 && s.active_peer().is_some_and(|ap| ap.name == peer.name);
            let sub = peer
                .rtt_ms
                .map(fmt_rtt)
                .unwrap_or_else(|| "Connected".into());
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
                            .key(format!("jump-tile-{edge:?}"))
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
            text(name.to_owned())
                .font(ty::BODY_SEMI)
                .color(name_c)
                .truncate(),
        );
        body = body.child(line);
        if !sub.is_empty() {
            body = body.child(text(sub.to_owned()).font(ty::SUB).color(sub_c).truncate());
        }
    } else {
        if let Some(os_k) = os {
            body = body.child(el().mb(2.0).child(icons::os_icon(os_k, 15.0, ico_c)));
        }
        body = body.child(
            text(name.to_owned())
                .font(ty::BODY_SEMI)
                .color(name_c)
                .align(Align::Center)
                .truncate(),
        );
        if !sub.is_empty() {
            body = body.child(
                text(sub.to_owned())
                    .font(ty::SUB)
                    .color(sub_c)
                    .align(Align::Center)
                    .truncate(),
            );
        }
    }

    if focused {
        body = body.child(
            el().absolute(w - 20.0, h - 22.0)
                .child(icons::cursor_glyph()),
        );
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
        .key(format!("ghost-{edge:?}"))
        .role("button")
        .label(format!("Move to {}", edge_word(edge)))
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
        let port = s.current_port();
        let mut r = row()
            .gap(4.0)
            .center_y()
            .child(text(format!("{first}:{port}")).font(ty::MONO).color(p.text));
        if s.local_ips.len() > 1 {
            r = r.child(
                text(format!("+{}", s.local_ips.len() - 1))
                    .font(ty::BADGE)
                    .color(p.muted),
            );
        }
        r
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
        .child(card_rows(
            p,
            "conn-card",
            vec![addr_row, port_row, disc_row],
        ))
}

fn server_clients_col(s: &Ui, p: Palette) -> El {
    let count = s.status.peers.len();
    let head = section_head(
        p,
        "clients-head",
        "Clients",
        Some(badge(
            p,
            "peer-count",
            format!("{count} online"),
            count > 0,
            false,
        )),
    );

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

    col()
        .key("client-conn-col")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .gap(8.0)
        .child(section_head(p, "client-conn-head", "Connection", None))
        .child(card_rows(p, "client-conn-card", vec![server_row, pos_row]))
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
            .clone()
            .unwrap_or_else(|| "Found automatically".into());
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
    let header = col()
        .key("input-header")
        .w_full()
        .gap(2.0)
        .child(text("Input").font(ty::PAGE_TITLE).color(p.text))
        .child(
            text("Pointer, screen edges and keyboard. Changes apply instantly.")
                .font(ty::CAPTION)
                .color(p.muted),
        );

    // Left column: Pointer + Screen edges
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
            Some("Invert wheel direction on the client"),
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
        pointer_rows.push(setting_row(
            p,
            "backend-row",
            "Input method",
            Some("Linux only"),
            segmented(
                p,
                "backend-seg",
                &[
                    (InputBackendKind::Auto, "Auto"),
                    (InputBackendKind::XTest, "XTest"),
                    (InputBackendKind::Uinput, "uinput"),
                ],
                s.config.client.input_backend,
                true,
                |s, kind, _| {
                    s.config.client.input_backend = kind;
                    s.push_config();
                },
            ),
        ));
    }

    let thresh = s.config.server.edge_threshold_px;
    let dead = s.config.server.dead_corner_px;
    let dwell = s.config.server.edge_dwell_ms;

    let edge_rows = vec![
        slider_row(
            p,
            "thresh-row",
            "Switch distance",
            format!("{thresh} px"),
            (thresh as f32 - 1.0) / (24.0 - 1.0),
            |s, n, commit| {
                let v = (1.0 + n * 23.0).round().clamp(1.0, 24.0) as i32;
                s.config.server.edge_threshold_px = v;
                s.config.client.edge_threshold_px = v;
                if commit {
                    s.push_config();
                }
            },
        ),
        slider_row(
            p,
            "corner-row",
            "Corner guard",
            format!("{dead} px"),
            dead as f32 / 80.0,
            |s, n, commit| {
                let v = (((n * 80.0) / 4.0).round() * 4.0).clamp(0.0, 80.0) as i32;
                s.config.server.dead_corner_px = v;
                s.config.client.dead_corner_px = v;
                if commit {
                    s.push_config();
                }
            },
        ),
        slider_row(
            p,
            "dwell-row",
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
            "sync-lock-row",
            "Sync Caps & Num Lock",
            Some("Keyboard lights stay consistent"),
            toggle(
                p,
                "sync-lock-toggle",
                "Sync Caps & Num Lock",
                s.config.server.sync_lock_keys,
                |s, _| {
                    let next = !s.config.server.sync_lock_keys;
                    s.config.server.sync_lock_keys = next;
                    s.config.client.sync_lock_keys = next;
                    s.push_config();
                },
            ),
        ),
    ];

    let left_col = col()
        .key("input-left")
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
                .child(section_head(p, "edges-head", "Screen edges", None))
                .child(card_rows(p, "edges-card", edge_rows)),
        );

    // Right column: Key remapping
    let preset_btn = |key: &'static str,
                      a: &'static str,
                      b: &'static str,
                      from: &'static str,
                      to: &'static str| {
        row()
            .key(key)
            .role("button")
            .label(format!("{a} to {b}"))
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
                    "Caps",
                    "Esc",
                    "CapsLock",
                    "Escape",
                ))
                .child(preset_btn(
                    "preset-ctrl",
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
                        .label("Remove remap")
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
        .key("input-right")
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

    col()
        .key("page-input")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(14.0)
        .child(header)
        .child(
            row()
                .key("input-cols")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(18.0)
                .child(left_col)
                .child(right_col),
        )
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
            text("Copy on one PC, paste on the other. Text and images, both directions.")
                .font(ty::CAPTION)
                .color(p.muted),
        );

    // Left: Shared clipboard toggle + History
    let clip_toggle_card = card_rows(
        p,
        "clip-toggle-card",
        vec![setting_row(
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
        )],
    );

    let history_body = if s.status.clipboard_history.is_empty() {
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
                .child(section_head(p, "hist-head", "History", None))
                .child(
                    card(p, "hist-card")
                        .grow(1.0)
                        .min_h(0.0)
                        .child(history_body),
                ),
        );

    // Right: Send files dropzone + Recent transfers
    let dropzone = col()
        .key("dropzone")
        .role("button")
        .label("Drop files here or browse")
        .w_full()
        .h(132.0)
        .gap(10.0)
        .center()
        .radius(10.0)
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
            el().size(34.0, 34.0)
                .radius(8.0)
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
            let row_el = row()
                .key(format!("file-{i}"))
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
        .gap(14.0)
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(
                    p,
                    "send-head",
                    "Send files",
                    Some(
                        text("Saved to Downloads/Glide")
                            .font(ty::SUB)
                            .color(p.faint),
                    ),
                ))
                .child(dropzone),
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
// Page 4: System
// ---------------------------------------------------------------------------

fn system_page(s: &Ui, p: Palette) -> El {
    let header = col()
        .key("sys-header")
        .w_full()
        .gap(2.0)
        .child(text("System").font(ty::PAGE_TITLE).color(p.text))
        .child(
            text("Security, startup and diagnostics.")
                .font(ty::CAPTION)
                .color(p.muted),
        );

    // Left: Identity + Integration + Version footer
    let fp_text = s
        .status
        .fingerprint
        .as_deref()
        .or(s.fingerprint.as_deref())
        .unwrap_or("Not generated yet");

    let trust_label = match s.config.client.trusted_fingerprint.as_deref() {
        Some(fp) => {
            let short: String = fp.chars().take(18).collect();
            format!("Trusted server: {short}…")
        }
        None => "Trusts the first server it sees".into(),
    };

    let identity_card = card_rows(
        p,
        "identity-card",
        vec![
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
                            text(fp_text.to_owned())
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
                    text(trust_label)
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
        ],
    );

    let autostart_desc = match s.role {
        Role::Server => "Starts minimized as Server",
        Role::Client => "Starts minimized as Client",
    };

    let integration_sub = if s.tray_active {
        "Closing keeps Glide in the tray"
    } else {
        "Startup, firewall & permissions"
    };

    let elevated_row = {
        let mut r = setting_row(
            p,
            "elevated-row",
            "Control admin windows",
            Some(if s.is_windows {
                "Let Glide control UAC / admin apps"
            } else {
                "Only needed on Windows"
            }),
            xs_button(
                p,
                "elevated-btn",
                "Set up",
                BtnKind::Default,
                !s.is_windows,
                |s, tx| s.run_elevated(tx),
            ),
        );
        if !s.is_windows {
            r = r.opacity(0.55);
        }
        r
    };

    let integration_card = card_rows(
        p,
        "integration-card",
        vec![
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
            elevated_row,
        ],
    );

    let version_foot = row()
        .key("version-foot")
        .w_full()
        .pt(2.0)
        .gap(10.0)
        .center_y()
        .child(icons::glide_mark(24.0, 6.0, p.accent))
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(1.0)
                .child(
                    row()
                        .gap(5.0)
                        .center_y()
                        .child(
                            text(format!("Glide v{APP_VERSION}"))
                                .font(ty::SUB_MED)
                                .color(p.text),
                        )
                        .child(text("· Direct PC-to-PC.").font(ty::SUB).color(p.muted)),
                )
                .child(
                    text("No account, no cloud, no internet.")
                        .font(ty::SUB)
                        .color(p.muted),
                ),
        );

    let left_col = col()
        .key("sys-left")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .gap(14.0)
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(
                    p,
                    "id-head",
                    "Identity",
                    Some(
                        text("TLS 1.3 · fingerprint verified")
                            .font(ty::SUB)
                            .color(p.faint),
                    ),
                ))
                .child(identity_card),
        )
        .child(
            col()
                .w_full()
                .gap(7.0)
                .child(section_head(
                    p,
                    "int-head",
                    "Integration",
                    Some(text(integration_sub).font(ty::SUB).color(p.faint)),
                ))
                .child(integration_card),
        )
        .child(version_foot);

    // Right: Appearance + Shortcuts + Diagnostics
    let appearance_card = card_rows(
        p,
        "appearance-card",
        vec![setting_row(
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
        )],
    );

    let plus = || text("+").font(ty::SUB).color(p.faint);
    let shortcuts_card = card_rows(
        p,
        "shortcuts-card",
        vec![
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
            text(s.doctor_text.clone())
                .w_full()
                .font(ty::MONO_SM)
                .color(p.text_2)
                .wrap(),
        );

    let right_col = col()
        .key("sys-right")
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
                .child(section_head(p, "app-head", "Appearance", None))
                .child(appearance_card),
        )
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
        )
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(7.0)
                .child(diag_head)
                .child(diag_card),
        );

    col()
        .key("page-system")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(14.0)
        .child(header)
        .child(
            row()
                .key("sys-cols")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(18.0)
                .child(left_col)
                .child(right_col),
        )
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

pub fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
