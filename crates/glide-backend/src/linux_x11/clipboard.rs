//! Event-driven X11 CLIPBOARD backend using `x11rb` + `XFixes` (zero polling).
//!
//! Supports `UTF8_STRING` / `text/plain;charset=utf-8` and `image/png`, including
//! ICCCM `INCR` chunked transfers for large payloads, with SHA-256 anti-loop protection.

use crate::{ClipboardBackend, ClipboardHandle};
use anyhow::{Context, Result};
use bytes::Bytes;
use glide_net::sha256_bytes;
use glide_proto::{ClipboardFormat, ClipboardPayload, MAX_CLIPBOARD_BYTES};
use parking_lot::Mutex;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{ConnectionExt as XFixesExt, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as XProtoExt,
    CreateWindowAux, EventMask, PropMode, Property, SelectionNotifyEvent, Window, WindowClass,
    CLIENT_MESSAGE_EVENT, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperExt;

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8_string: Atom,
    text_plain_utf8: Atom,
    string: Atom,
    image_png: Atom,
    incr: Atom,
    prop: Atom,
    wake: Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self> {
        let clipboard = conn.intern_atom(false, b"CLIPBOARD")?;
        let targets = conn.intern_atom(false, b"TARGETS")?;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?;
        let text_plain_utf8 = conn.intern_atom(false, b"text/plain;charset=utf-8")?;
        let image_png = conn.intern_atom(false, b"image/png")?;
        let incr = conn.intern_atom(false, b"INCR")?;
        let prop = conn.intern_atom(false, b"GLIDE_CLIP_DATA")?;
        let wake = conn.intern_atom(false, b"GLIDE_CLIP_WAKE")?;
        Ok(Self {
            clipboard: clipboard.reply()?.atom,
            targets: targets.reply()?.atom,
            utf8_string: utf8_string.reply()?.atom,
            text_plain_utf8: text_plain_utf8.reply()?.atom,
            string: AtomEnum::STRING.into(),
            image_png: image_png.reply()?.atom,
            incr: incr.reply()?.atom,
            prop: prop.reply()?.atom,
            wake: wake.reply()?.atom,
        })
    }
}

struct SharedClip {
    owned: Mutex<Option<ClipboardPayload>>,
    last_remote_hash: Mutex<Option<[u8; 32]>>,
}

struct X11ClipboardBackend {
    conn: Arc<RustConnection>,
    win: Window,
    wake_atom: Atom,
    shared: Arc<SharedClip>,
}

impl ClipboardBackend for X11ClipboardBackend {
    fn set(&self, payload: ClipboardPayload) -> Result<()> {
        *self.shared.last_remote_hash.lock() = Some(payload.sha256);
        *self.shared.owned.lock() = Some(payload);
        let ev = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: self.win,
            type_: self.wake_atom,
            data: ClientMessageData::from([1u32, 0, 0, 0, 0]),
        };
        self.conn
            .send_event(false, self.win, EventMask::NO_EVENT, ev)
            .context("failed to wake X11 clipboard thread")?;
        self.conn
            .flush()
            .context("failed to flush X11 clipboard wake")?;
        Ok(())
    }
}

pub fn start_x11_clipboard() -> Result<ClipboardHandle> {
    let (conn, screen_num) =
        RustConnection::connect(None).context("failed to connect to X11 display for clipboard")?;
    let conn = Arc::new(conn);
    let root = conn.setup().roots[screen_num].root;

    conn.xfixes_query_version(5, 0)?
        .reply()
        .context("XFixes extension unavailable for X11 clipboard")?;

    let atoms = Atoms::intern(&conn)?;

    let win = conn.generate_id()?;
    let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &aux,
    )?;

    conn.xfixes_select_selection_input(
        root,
        atoms.clipboard,
        SelectionEventMask::SET_SELECTION_OWNER,
    )?;
    conn.flush()?;

    let shared = Arc::new(SharedClip {
        owned: Mutex::new(None),
        last_remote_hash: Mutex::new(None),
    });

    let (tx, rx) = mpsc::unbounded_channel();
    let wake_atom = atoms.wake;

    let thread_conn = Arc::clone(&conn);
    let thread_shared = Arc::clone(&shared);
    thread::Builder::new()
        .name("glide-x11-clipboard".into())
        .spawn(move || {
            run_clipboard_loop(thread_conn, win, atoms, thread_shared, tx);
        })
        .context("failed to spawn glide-x11-clipboard thread")?;

    Ok(ClipboardHandle {
        backend: Box::new(X11ClipboardBackend {
            conn,
            win,
            wake_atom,
            shared,
        }),
        local_changes: rx,
    })
}

enum ReadState {
    Idle,
    AwaitingTargets,
    AwaitingData(ClipboardFormat),
    ReceivingIncr {
        format: ClipboardFormat,
        buf: Vec<u8>,
    },
}

fn run_clipboard_loop(
    conn: Arc<RustConnection>,
    win: Window,
    atoms: Atoms,
    shared: Arc<SharedClip>,
    tx: mpsc::UnboundedSender<ClipboardPayload>,
) {
    let mut state = ReadState::Idle;

    let emit_payload = |format: ClipboardFormat, data: Vec<u8>| {
        if data.is_empty() || data.len() > MAX_CLIPBOARD_BYTES {
            return;
        }
        let hash = sha256_bytes(&data);
        if *shared.last_remote_hash.lock() == Some(hash) {
            trace!("Ignoring X11 clipboard echo matching last remote hash");
            return;
        }
        *shared.last_remote_hash.lock() = Some(hash);
        debug!(
            ?format,
            len = data.len(),
            "Captured local X11 clipboard update"
        );
        let _ = tx.send(ClipboardPayload {
            format,
            sha256: hash,
            data: Bytes::from(data),
        });
    };

    loop {
        let ev = match conn.wait_for_event() {
            Ok(ev) => ev,
            Err(e) => {
                warn!(error = %e, "X11 clipboard connection closed");
                break;
            }
        };

        match ev {
            Event::ClientMessage(cm) if cm.type_ == atoms.wake => {
                // Claim CLIPBOARD ownership so X11 apps paste our remote payload.
                if let Err(e) = conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME)
                {
                    warn!(error = %e, "Failed to claim X11 CLIPBOARD ownership");
                }
                let _ = conn.flush();
            }
            Event::XfixesSelectionNotify(sn) if sn.selection == atoms.clipboard => {
                if sn.owner == win || sn.owner == x11rb::NONE {
                    continue;
                }
                // Another local app copied something — ask for its TARGETS first.
                state = ReadState::AwaitingTargets;
                let _ = conn.delete_property(win, atoms.prop);
                let _ = conn.convert_selection(
                    win,
                    atoms.clipboard,
                    atoms.targets,
                    atoms.prop,
                    x11rb::CURRENT_TIME,
                );
                let _ = conn.flush();
            }
            Event::SelectionNotify(sn)
                if sn.requestor == win && sn.selection == atoms.clipboard =>
            {
                if sn.property == x11rb::NONE {
                    state = ReadState::Idle;
                    continue;
                }
                let Ok(cookie) = conn.get_property(
                    true,
                    win,
                    sn.property,
                    AtomEnum::ANY,
                    0,
                    (MAX_CLIPBOARD_BYTES / 4) as u32,
                ) else {
                    state = ReadState::Idle;
                    continue;
                };
                let Ok(prop) = cookie.reply() else {
                    state = ReadState::Idle;
                    continue;
                };

                match state {
                    ReadState::AwaitingTargets => {
                        let mut has_utf8 = false;
                        let mut has_plain_utf8 = false;
                        let mut has_png = false;
                        if let Some(iter) = prop.value32() {
                            for atom in iter {
                                if atom == atoms.utf8_string {
                                    has_utf8 = true;
                                    break;
                                } else if atom == atoms.text_plain_utf8 {
                                    has_plain_utf8 = true;
                                } else if atom == atoms.image_png {
                                    has_png = true;
                                }
                            }
                        }
                        let chosen = if has_utf8 {
                            Some((atoms.utf8_string, ClipboardFormat::TextUtf8))
                        } else if has_plain_utf8 {
                            Some((atoms.text_plain_utf8, ClipboardFormat::TextUtf8))
                        } else if has_png {
                            Some((atoms.image_png, ClipboardFormat::ImagePng))
                        } else {
                            None
                        };
                        if let Some((target_atom, fmt)) = chosen {
                            state = ReadState::AwaitingData(fmt);
                            let _ = conn.convert_selection(
                                win,
                                atoms.clipboard,
                                target_atom,
                                atoms.prop,
                                x11rb::CURRENT_TIME,
                            );
                            let _ = conn.flush();
                        } else {
                            state = ReadState::Idle;
                        }
                    }
                    ReadState::AwaitingData(fmt) => {
                        if prop.type_ == atoms.incr {
                            state = ReadState::ReceivingIncr {
                                format: fmt,
                                buf: Vec::new(),
                            };
                        } else {
                            emit_payload(fmt, prop.value);
                            state = ReadState::Idle;
                        }
                    }
                    _ => {
                        state = ReadState::Idle;
                    }
                }
            }
            Event::PropertyNotify(pn)
                if pn.window == win && pn.atom == atoms.prop && pn.state == Property::NEW_VALUE =>
            {
                if let ReadState::ReceivingIncr {
                    format,
                    ref mut buf,
                } = state
                {
                    let Ok(cookie) = conn.get_property(
                        true,
                        win,
                        atoms.prop,
                        AtomEnum::ANY,
                        0,
                        (MAX_CLIPBOARD_BYTES / 4) as u32,
                    ) else {
                        state = ReadState::Idle;
                        continue;
                    };
                    let Ok(prop) = cookie.reply() else {
                        state = ReadState::Idle;
                        continue;
                    };
                    if prop.value.is_empty() {
                        let data = std::mem::take(buf);
                        let fmt = format;
                        state = ReadState::Idle;
                        emit_payload(fmt, data);
                    } else if buf.len() + prop.value.len() <= MAX_CLIPBOARD_BYTES {
                        buf.extend_from_slice(&prop.value);
                    } else {
                        state = ReadState::Idle;
                    }
                }
            }
            Event::SelectionRequest(req) if req.selection == atoms.clipboard => {
                let owned = shared.owned.lock().clone();
                let property = if req.property == x11rb::NONE {
                    req.target
                } else {
                    req.property
                };
                let mut reply_prop = x11rb::NONE;

                if let Some(payload) = owned {
                    if req.target == atoms.targets {
                        let text_targets = [
                            atoms.targets,
                            atoms.utf8_string,
                            atoms.text_plain_utf8,
                            atoms.string,
                        ];
                        let png_targets = [atoms.targets, atoms.image_png];
                        let targets: &[Atom] = match payload.format {
                            ClipboardFormat::TextUtf8 => &text_targets,
                            ClipboardFormat::ImagePng => &png_targets,
                        };
                        if conn
                            .change_property32(
                                PropMode::REPLACE,
                                req.requestor,
                                property,
                                AtomEnum::ATOM,
                                targets,
                            )
                            .is_ok()
                        {
                            reply_prop = property;
                        }
                    } else {
                        let ok_target = match payload.format {
                            ClipboardFormat::TextUtf8 => {
                                req.target == atoms.utf8_string
                                    || req.target == atoms.text_plain_utf8
                                    || req.target == atoms.string
                            }
                            ClipboardFormat::ImagePng => req.target == atoms.image_png,
                        };
                        if ok_target
                            && conn
                                .change_property8(
                                    PropMode::REPLACE,
                                    req.requestor,
                                    property,
                                    req.target,
                                    &payload.data,
                                )
                                .is_ok()
                        {
                            reply_prop = property;
                        }
                    }
                }

                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property: reply_prop,
                };
                let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
                let _ = conn.flush();
            }
            _ => {}
        }
    }
}
