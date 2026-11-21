//! Compact, versioned binary wire protocol for Glide (v2).
//!
//! Traffic is cleanly separated across three QUIC transport lanes:
//! 1. [`DatagramMessage`] — Unreliable, unordered QUIC datagrams for mouse deltas and
//!    high-resolution scroll (zero head-of-line blocking).
//! 2. [`ControlMessage`] — Reliable, ordered QUIC bidirectional stream for keystrokes,
//!    buttons, screen transitions, lock-key sync, lazy clipboard offers, and heartbeats.
//! 3. [`BulkTransfer`] — Separate unidirectional bulk QUIC streams for clipboard
//!    payloads (UTF-8 text / PNG images) and drag-and-drop file transfers so large
//!    transfers never delay mouse or keyboard input.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use glide_hid::{HidUsage, ModifierMask, MouseButton};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_MAGIC: [u8; 4] = *b"GLID";
pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_CONTROL_FRAME_LEN: usize = 64 * 1024;
pub const MAX_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
/// Maximum number of files accepted in a single cross-edge drag-and-drop operation.
pub const MAX_DRAG_FILES: usize = 64;
/// Payloads above this threshold use lazy `ClipboardOffer` / `ClipboardRequest`.
pub const LAZY_CLIPBOARD_THRESHOLD: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("frame too short: expected at least {expected} bytes, got {actual}")]
    Truncated { expected: usize, actual: usize },
    #[error("frame exceeds maximum size ({len} > {max})")]
    FrameTooLarge { len: usize, max: usize },
    #[error("unknown message tag: 0x{0:02X}")]
    UnknownTag(u8),
    #[error("invalid enum value {value} for {kind}")]
    InvalidEnum { kind: &'static str, value: u8 },
    #[error("invalid utf-8 in protocol string")]
    InvalidUtf8,
    #[error("invalid protocol magic")]
    InvalidMagic,
}

/// Which physical screen edge connects two machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Edge {
    Left = 1,
    Right = 2,
    Top = 3,
    Bottom = 4,
}

impl Edge {
    pub const fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    pub const fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::Left),
            2 => Ok(Self::Right),
            3 => Ok(Self::Top),
            4 => Ok(Self::Bottom),
            _ => Err(ProtoError::InvalidEnum {
                kind: "Edge",
                value: v,
            }),
        }
    }
}

/// Operating system / display server kind reported during handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum OsKind {
    Windows = 1,
    LinuxX11 = 2,
    LinuxWayland = 3,
    MacOs = 4,
    Unknown = 255,
}

impl OsKind {
    pub const fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::Windows),
            2 => Ok(Self::LinuxX11),
            3 => Ok(Self::LinuxWayland),
            4 => Ok(Self::MacOs),
            255 => Ok(Self::Unknown),
            _ => Err(ProtoError::InvalidEnum {
                kind: "OsKind",
                value: v,
            }),
        }
    }
}

/// Keyboard toggle-key lock state synchronized across screen transitions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockState {
    pub caps_lock: bool,
    pub num_lock: bool,
}

impl LockState {
    pub const fn to_u8(self) -> u8 {
        (if self.caps_lock { 1 } else { 0 }) | (if self.num_lock { 2 } else { 0 })
    }

    pub const fn from_u8(bits: u8) -> Self {
        Self {
            caps_lock: (bits & 1) != 0,
            num_lock: (bits & 2) != 0,
        }
    }
}

/// Supported clipboard MIME payload formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ClipboardFormat {
    TextUtf8 = 1,
    ImagePng = 2,
}

impl ClipboardFormat {
    pub const fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::TextUtf8),
            2 => Ok(Self::ImagePng),
            _ => Err(ProtoError::InvalidEnum {
                kind: "ClipboardFormat",
                value: v,
            }),
        }
    }
}

/// Lane A: Unreliable QUIC datagrams for latency-critical relative pointer motion and scroll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DatagramMessage {
    /// Raw relative pointer delta (`f32` preserves sub-pixel precision).
    MouseMove { seq: u32, dx: f32, dy: f32 },
    /// High-resolution scroll delta where `120` = 1 standard notch.
    MouseScroll { seq: u32, dx_120: i16, dy_120: i16 },
}

impl DatagramMessage {
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(13);
        match *self {
            Self::MouseMove { seq, dx, dy } => {
                buf.put_u8(0x01);
                buf.put_u32(seq);
                buf.put_f32(dx);
                buf.put_f32(dy);
            }
            Self::MouseScroll {
                seq,
                dx_120,
                dy_120,
            } => {
                buf.put_u8(0x02);
                buf.put_u32(seq);
                buf.put_i16(dx_120);
                buf.put_i16(dy_120);
            }
        }
        buf.freeze()
    }

    pub fn decode(mut data: &[u8]) -> Result<Self, ProtoError> {
        if data.is_empty() {
            return Err(ProtoError::Truncated {
                expected: 1,
                actual: 0,
            });
        }
        let tag = data.get_u8();
        match tag {
            0x01 => {
                if data.len() < 12 {
                    return Err(ProtoError::Truncated {
                        expected: 13,
                        actual: data.len() + 1,
                    });
                }
                let seq = data.get_u32();
                let dx = data.get_f32();
                let dy = data.get_f32();
                Ok(Self::MouseMove { seq, dx, dy })
            }
            0x02 => {
                if data.len() < 8 {
                    return Err(ProtoError::Truncated {
                        expected: 9,
                        actual: data.len() + 1,
                    });
                }
                let seq = data.get_u32();
                let dx_120 = data.get_i16();
                let dy_120 = data.get_i16();
                Ok(Self::MouseScroll {
                    seq,
                    dx_120,
                    dy_120,
                })
            }
            other => Err(ProtoError::UnknownTag(other)),
        }
    }
}

/// Lane B: Reliable ordered QUIC control stream messages.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlMessage {
    /// Client -> Server initial handshake.
    Hello {
        version: u16,
        peer_name: String,
        edge_on_server: Edge,
        os: OsKind,
    },
    /// Server -> Client handshake reply (includes server primary DPI scale for auto-DPI matching).
    Welcome {
        version: u16,
        server_name: String,
        accepted: bool,
        reason: String,
        server_scale: f32,
    },
    /// Physical USB HID key press/release.
    Key { usage: HidUsage, pressed: bool },
    /// Mouse button press/release.
    Button { button: MouseButton, pressed: bool },
    /// Periodic / transition modifier state sync to prevent stuck modifiers.
    ModifierSync { mask: ModifierMask },
    /// Server -> Client: take control, warp cursor to `edge` at `normalized_pos ∈ [0.0, 1.0]`.
    Enter {
        edge: Edge,
        normalized_pos: f32,
        lock_state: LockState,
    },
    /// Server -> Client (hotkey switch): take control and restore cursor at `(norm_x, norm_y)`.
    EnterAt {
        norm_x: f32,
        norm_y: f32,
        lock_state: LockState,
    },
    /// Client -> Server (user exited back to server) OR Server -> Client (focus revoked).
    Leave {
        edge: Edge,
        normalized_pos: f32,
        cursor_norm_x: f32,
        cursor_norm_y: f32,
    },
    /// Force release of every currently held key and mouse button.
    ReleaseAll,
    /// Synchronize CapsLock / NumLock state.
    LockSync { lock_state: LockState },
    /// Live edge repositioning (either Server -> Client or Client -> Server) without reconnecting.
    SetEdge { edge_on_server: Edge },
    /// Lazy clipboard offer for large payloads (> 256 KiB).
    ClipboardOffer {
        format: ClipboardFormat,
        sha256: [u8; 32],
        byte_len: u32,
    },
    /// Request peer to stream an offered clipboard payload over Lane C.
    ClipboardRequest { sha256: [u8; 32] },
    /// Heartbeat request with sender timestamp in microseconds.
    Ping { ts_us: u64 },
    /// Heartbeat response echoing `ts_us`.
    Pong { ts_us: u64 },
    /// Peer started dragging one or more local files across an active screen edge.
    /// Followed by parallel `BulkTransfer::DragFile` streams and concluded by
    /// `DragDrop` or `DragCancel`.
    DragStart {
        drag_id: u64,
        files: Vec<DragFileInfo>,
    },
    /// User released the drag on the receiving screen; commit OS drop handoff for `drag_id`.
    DragDrop { drag_id: u64 },
    /// User cancelled the drag (e.g. returned across edge, pressed Escape, or disconnected).
    DragCancel { drag_id: u64 },
}

/// Metadata for a single file participating in a cross-edge drag-and-drop transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragFileInfo {
    pub filename: String,
    pub byte_len: u64,
}

fn put_string(buf: &mut BytesMut, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u16::MAX as usize);
    buf.put_u16(len as u16);
    buf.put_slice(&bytes[..len]);
}

fn get_string(data: &mut &[u8]) -> Result<String, ProtoError> {
    if data.len() < 2 {
        return Err(ProtoError::Truncated {
            expected: 2,
            actual: data.len(),
        });
    }
    let len = data.get_u16() as usize;
    if data.len() < len {
        return Err(ProtoError::Truncated {
            expected: len,
            actual: data.len(),
        });
    }
    let raw = &data[..len];
    data.advance(len);
    String::from_utf8(raw.to_vec()).map_err(|_| ProtoError::InvalidUtf8)
}

impl ControlMessage {
    pub fn encode_body(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(40);
        match self {
            Self::Hello {
                version,
                peer_name,
                edge_on_server,
                os,
            } => {
                buf.put_u8(0x10);
                buf.put_slice(&PROTOCOL_MAGIC);
                buf.put_u16(*version);
                buf.put_u8(*edge_on_server as u8);
                buf.put_u8(*os as u8);
                put_string(&mut buf, peer_name);
            }
            Self::Welcome {
                version,
                server_name,
                accepted,
                reason,
                server_scale,
            } => {
                buf.put_u8(0x11);
                buf.put_slice(&PROTOCOL_MAGIC);
                buf.put_u16(*version);
                buf.put_u8(u8::from(*accepted));
                buf.put_f32(*server_scale);
                put_string(&mut buf, server_name);
                put_string(&mut buf, reason);
            }
            Self::Key { usage, pressed } => {
                buf.put_u8(0x20);
                buf.put_u32(usage.raw());
                buf.put_u8(u8::from(*pressed));
            }
            Self::Button { button, pressed } => {
                buf.put_u8(0x21);
                buf.put_u8(*button as u8);
                buf.put_u8(u8::from(*pressed));
            }
            Self::ModifierSync { mask } => {
                buf.put_u8(0x22);
                buf.put_u8(mask.0);
            }
            Self::Enter {
                edge,
                normalized_pos,
                lock_state,
            } => {
                buf.put_u8(0x30);
                buf.put_u8(*edge as u8);
                buf.put_f32(normalized_pos.clamp(0.0, 1.0));
                buf.put_u8(lock_state.to_u8());
            }
            Self::Leave {
                edge,
                normalized_pos,
                cursor_norm_x,
                cursor_norm_y,
            } => {
                buf.put_u8(0x31);
                buf.put_u8(*edge as u8);
                buf.put_f32(normalized_pos.clamp(0.0, 1.0));
                buf.put_f32(cursor_norm_x.clamp(0.0, 1.0));
                buf.put_f32(cursor_norm_y.clamp(0.0, 1.0));
            }
            Self::ReleaseAll => {
                buf.put_u8(0x32);
            }
            Self::EnterAt {
                norm_x,
                norm_y,
                lock_state,
            } => {
                buf.put_u8(0x33);
                buf.put_f32(norm_x.clamp(0.0, 1.0));
                buf.put_f32(norm_y.clamp(0.0, 1.0));
                buf.put_u8(lock_state.to_u8());
            }
            Self::LockSync { lock_state } => {
                buf.put_u8(0x34);
                buf.put_u8(lock_state.to_u8());
            }
            Self::SetEdge { edge_on_server } => {
                buf.put_u8(0x35);
                buf.put_u8(*edge_on_server as u8);
            }
            Self::ClipboardOffer {
                format,
                sha256,
                byte_len,
            } => {
                buf.put_u8(0x38);
                buf.put_u8(*format as u8);
                buf.put_slice(sha256);
                buf.put_u32(*byte_len);
            }
            Self::ClipboardRequest { sha256 } => {
                buf.put_u8(0x39);
                buf.put_slice(sha256);
            }
            Self::Ping { ts_us } => {
                buf.put_u8(0x40);
                buf.put_u64(*ts_us);
            }
            Self::Pong { ts_us } => {
                buf.put_u8(0x41);
                buf.put_u64(*ts_us);
            }
            Self::DragStart { drag_id, files } => {
                buf.put_u8(0x50);
                buf.put_u64(*drag_id);
                let count = files.len().min(MAX_DRAG_FILES);
                buf.put_u16(count as u16);
                for f in files.iter().take(count) {
                    let safe = sanitize_filename(&f.filename);
                    put_string(&mut buf, &safe);
                    buf.put_u64(f.byte_len);
                }
            }
            Self::DragDrop { drag_id } => {
                buf.put_u8(0x51);
                buf.put_u64(*drag_id);
            }
            Self::DragCancel { drag_id } => {
                buf.put_u8(0x52);
                buf.put_u64(*drag_id);
            }
        }
        buf.freeze()
    }

    pub fn encode_framed(&self) -> Bytes {
        let body = self.encode_body();
        let mut out = BytesMut::with_capacity(4 + body.len());
        out.put_u32(body.len() as u32);
        out.put_slice(&body);
        out.freeze()
    }

    pub fn decode_body(mut data: &[u8]) -> Result<Self, ProtoError> {
        if data.is_empty() {
            return Err(ProtoError::Truncated {
                expected: 1,
                actual: 0,
            });
        }
        let tag = data.get_u8();
        match tag {
            0x10 => {
                if data.len() < 8 {
                    return Err(ProtoError::Truncated {
                        expected: 8,
                        actual: data.len(),
                    });
                }
                if data[..4] != PROTOCOL_MAGIC {
                    return Err(ProtoError::InvalidMagic);
                }
                data.advance(4);
                let version = data.get_u16();
                let edge_on_server = Edge::from_u8(data.get_u8())?;
                let os = OsKind::from_u8(data.get_u8())?;
                let peer_name = get_string(&mut data)?;
                Ok(Self::Hello {
                    version,
                    peer_name,
                    edge_on_server,
                    os,
                })
            }
            0x11 => {
                if data.len() < 11 {
                    return Err(ProtoError::Truncated {
                        expected: 11,
                        actual: data.len(),
                    });
                }
                if data[..4] != PROTOCOL_MAGIC {
                    return Err(ProtoError::InvalidMagic);
                }
                data.advance(4);
                let version = data.get_u16();
                let accepted = data.get_u8() != 0;
                let server_scale = data.get_f32();
                let server_name = get_string(&mut data)?;
                let reason = get_string(&mut data)?;
                Ok(Self::Welcome {
                    version,
                    server_name,
                    accepted,
                    reason,
                    server_scale,
                })
            }
            0x20 => {
                if data.len() < 5 {
                    return Err(ProtoError::Truncated {
                        expected: 5,
                        actual: data.len(),
                    });
                }
                let usage = HidUsage(data.get_u32());
                let pressed = data.get_u8() != 0;
                Ok(Self::Key { usage, pressed })
            }
            0x21 => {
                if data.len() < 2 {
                    return Err(ProtoError::Truncated {
                        expected: 2,
                        actual: data.len(),
                    });
                }
                let raw_btn = data.get_u8();
                let button = MouseButton::from_u8(raw_btn).ok_or(ProtoError::InvalidEnum {
                    kind: "MouseButton",
                    value: raw_btn,
                })?;
                let pressed = data.get_u8() != 0;
                Ok(Self::Button { button, pressed })
            }
            0x22 => {
                if data.is_empty() {
                    return Err(ProtoError::Truncated {
                        expected: 1,
                        actual: 0,
                    });
                }
                Ok(Self::ModifierSync {
                    mask: ModifierMask(data.get_u8()),
                })
            }
            0x30 => {
                if data.len() < 6 {
                    return Err(ProtoError::Truncated {
                        expected: 6,
                        actual: data.len(),
                    });
                }
                let edge = Edge::from_u8(data.get_u8())?;
                let normalized_pos = data.get_f32().clamp(0.0, 1.0);
                let lock_state = LockState::from_u8(data.get_u8());
                Ok(Self::Enter {
                    edge,
                    normalized_pos,
                    lock_state,
                })
            }
            0x31 => {
                if data.len() < 13 {
                    return Err(ProtoError::Truncated {
                        expected: 13,
                        actual: data.len(),
                    });
                }
                let edge = Edge::from_u8(data.get_u8())?;
                let normalized_pos = data.get_f32().clamp(0.0, 1.0);
                let cursor_norm_x = data.get_f32().clamp(0.0, 1.0);
                let cursor_norm_y = data.get_f32().clamp(0.0, 1.0);
                Ok(Self::Leave {
                    edge,
                    normalized_pos,
                    cursor_norm_x,
                    cursor_norm_y,
                })
            }
            0x32 => Ok(Self::ReleaseAll),
            0x33 => {
                if data.len() < 9 {
                    return Err(ProtoError::Truncated {
                        expected: 9,
                        actual: data.len(),
                    });
                }
                let norm_x = data.get_f32().clamp(0.0, 1.0);
                let norm_y = data.get_f32().clamp(0.0, 1.0);
                let lock_state = LockState::from_u8(data.get_u8());
                Ok(Self::EnterAt {
                    norm_x,
                    norm_y,
                    lock_state,
                })
            }
            0x34 => {
                if data.is_empty() {
                    return Err(ProtoError::Truncated {
                        expected: 1,
                        actual: 0,
                    });
                }
                Ok(Self::LockSync {
                    lock_state: LockState::from_u8(data.get_u8()),
                })
            }
            0x35 => {
                if data.is_empty() {
                    return Err(ProtoError::Truncated {
                        expected: 1,
                        actual: 0,
                    });
                }
                Ok(Self::SetEdge {
                    edge_on_server: Edge::from_u8(data.get_u8())?,
                })
            }
            0x38 => {
                if data.len() < 37 {
                    return Err(ProtoError::Truncated {
                        expected: 37,
                        actual: data.len(),
                    });
                }
                let format = ClipboardFormat::from_u8(data.get_u8())?;
                let mut sha256 = [0u8; 32];
                sha256.copy_from_slice(&data[..32]);
                data.advance(32);
                let byte_len = data.get_u32();
                Ok(Self::ClipboardOffer {
                    format,
                    sha256,
                    byte_len,
                })
            }
            0x39 => {
                if data.len() < 32 {
                    return Err(ProtoError::Truncated {
                        expected: 32,
                        actual: data.len(),
                    });
                }
                let mut sha256 = [0u8; 32];
                sha256.copy_from_slice(&data[..32]);
                Ok(Self::ClipboardRequest { sha256 })
            }
            0x40 => {
                if data.len() < 8 {
                    return Err(ProtoError::Truncated {
                        expected: 8,
                        actual: data.len(),
                    });
                }
                Ok(Self::Ping {
                    ts_us: data.get_u64(),
                })
            }
            0x41 => {
                if data.len() < 8 {
                    return Err(ProtoError::Truncated {
                        expected: 8,
                        actual: data.len(),
                    });
                }
                Ok(Self::Pong {
                    ts_us: data.get_u64(),
                })
            }
            0x50 => {
                if data.len() < 10 {
                    return Err(ProtoError::Truncated {
                        expected: 10,
                        actual: data.len(),
                    });
                }
                let drag_id = data.get_u64();
                let count = data.get_u16() as usize;
                if count > MAX_DRAG_FILES {
                    return Err(ProtoError::FrameTooLarge {
                        len: count,
                        max: MAX_DRAG_FILES,
                    });
                }
                let mut files = Vec::with_capacity(count);
                for _ in 0..count {
                    let filename = sanitize_filename(&get_string(&mut data)?);
                    if data.len() < 8 {
                        return Err(ProtoError::Truncated {
                            expected: 8,
                            actual: data.len(),
                        });
                    }
                    let byte_len = data.get_u64();
                    files.push(DragFileInfo { filename, byte_len });
                }
                Ok(Self::DragStart { drag_id, files })
            }
            0x51 => {
                if data.len() < 8 {
                    return Err(ProtoError::Truncated {
                        expected: 8,
                        actual: data.len(),
                    });
                }
                Ok(Self::DragDrop {
                    drag_id: data.get_u64(),
                })
            }
            0x52 => {
                if data.len() < 8 {
                    return Err(ProtoError::Truncated {
                        expected: 8,
                        actual: data.len(),
                    });
                }
                Ok(Self::DragCancel {
                    drag_id: data.get_u64(),
                })
            }
            other => Err(ProtoError::UnknownTag(other)),
        }
    }
}

/// Bulk clipboard transfer carried on Lane C.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPayload {
    pub format: ClipboardFormat,
    pub sha256: [u8; 32],
    pub data: Bytes,
}

/// Bulk file transfer carried on Lane C (GUI dropzone / `glide send-file`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePayload {
    pub filename: String,
    pub sha256: [u8; 32],
    pub data: Bytes,
}

/// Bulk file transfer associated with an active cross-edge drag-and-drop operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragFilePayload {
    pub drag_id: u64,
    pub file: FilePayload,
}

/// Lane C unidirectional QUIC stream payloads (independent of mouse & keyboard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkTransfer {
    Clipboard(ClipboardPayload),
    File(FilePayload),
    DragFile(DragFilePayload),
}

pub const BULK_KIND_CLIPBOARD: u8 = 0x01;
pub const BULK_KIND_FILE: u8 = 0x02;
pub const BULK_KIND_DRAG_FILE: u8 = 0x03;

/// Strips directory components and unsafe characters so received filenames can never traverse paths.
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("file.bin").trim();
    let cleaned: String = base
        .chars()
        .filter(|&c| {
            !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "glide-file.bin".to_string()
    } else {
        cleaned
    }
}

impl ClipboardPayload {
    pub const HEADER_LEN: usize = 1 + 32 + 4;

    pub fn encode_header(&self) -> [u8; Self::HEADER_LEN] {
        let mut hdr = [0u8; Self::HEADER_LEN];
        hdr[0] = self.format as u8;
        hdr[1..33].copy_from_slice(&self.sha256);
        hdr[33..37].copy_from_slice(&(self.data.len() as u32).to_be_bytes());
        hdr
    }

    pub fn decode_header(
        hdr: &[u8; Self::HEADER_LEN],
    ) -> Result<(ClipboardFormat, [u8; 32], usize), ProtoError> {
        let format = ClipboardFormat::from_u8(hdr[0])?;
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&hdr[1..33]);
        let len = u32::from_be_bytes([hdr[33], hdr[34], hdr[35], hdr[36]]) as usize;
        if len > MAX_CLIPBOARD_BYTES {
            return Err(ProtoError::FrameTooLarge {
                len,
                max: MAX_CLIPBOARD_BYTES,
            });
        }
        Ok((format, sha256, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_roundtrip() {
        let move_msg = DatagramMessage::MouseMove {
            seq: 42,
            dx: -13.25,
            dy: 7.5,
        };
        assert_eq!(
            DatagramMessage::decode(&move_msg.encode()).unwrap(),
            move_msg
        );

        let scroll_msg = DatagramMessage::MouseScroll {
            seq: 99,
            dx_120: -120,
            dy_120: 60,
        };
        assert_eq!(
            DatagramMessage::decode(&scroll_msg.encode()).unwrap(),
            scroll_msg
        );
    }

    #[test]
    fn control_messages_roundtrip() {
        let locks = LockState {
            caps_lock: true,
            num_lock: false,
        };
        let samples = vec![
            ControlMessage::Hello {
                version: PROTOCOL_VERSION,
                peer_name: "ubuntu-laptop".into(),
                edge_on_server: Edge::Right,
                os: OsKind::LinuxX11,
            },
            ControlMessage::Welcome {
                version: PROTOCOL_VERSION,
                server_name: "win11-desktop".into(),
                accepted: true,
                reason: String::new(),
                server_scale: 1.5,
            },
            ControlMessage::Key {
                usage: HidUsage::ALT_RIGHT,
                pressed: true,
            },
            ControlMessage::Button {
                button: MouseButton::Back,
                pressed: false,
            },
            ControlMessage::ModifierSync {
                mask: ModifierMask(ModifierMask::ALT_RIGHT | ModifierMask::SHIFT_LEFT),
            },
            ControlMessage::Enter {
                edge: Edge::Left,
                normalized_pos: 0.42,
                lock_state: locks,
            },
            ControlMessage::EnterAt {
                norm_x: 0.3,
                norm_y: 0.7,
                lock_state: locks,
            },
            ControlMessage::Leave {
                edge: Edge::Right,
                normalized_pos: 0.85,
                cursor_norm_x: 0.99,
                cursor_norm_y: 0.85,
            },
            ControlMessage::ReleaseAll,
            ControlMessage::LockSync { lock_state: locks },
            ControlMessage::ClipboardOffer {
                format: ClipboardFormat::ImagePng,
                sha256: [0x11; 32],
                byte_len: 1_500_000,
            },
            ControlMessage::ClipboardRequest { sha256: [0x11; 32] },
            ControlMessage::SetEdge {
                edge_on_server: Edge::Top,
            },
            ControlMessage::Ping { ts_us: 123_456_789 },
            ControlMessage::Pong { ts_us: 123_456_789 },
            ControlMessage::DragStart {
                drag_id: 77,
                files: vec![
                    DragFileInfo {
                        filename: "report äöüß.pdf".into(),
                        byte_len: 1_048_576,
                    },
                    DragFileInfo {
                        filename: "photo 1.png".into(),
                        byte_len: 42_000,
                    },
                ],
            },
            ControlMessage::DragDrop { drag_id: 77 },
            ControlMessage::DragCancel { drag_id: 77 },
        ];

        for msg in samples {
            let body = msg.encode_body();
            let decoded = ControlMessage::decode_body(&body).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn drag_start_sanitizes_filenames_and_rejects_excessive_file_count() {
        let msg = ControlMessage::DragStart {
            drag_id: 9,
            files: vec![DragFileInfo {
                filename: "../../secret.txt".into(),
                byte_len: 128,
            }],
        };
        let decoded = ControlMessage::decode_body(&msg.encode_body()).unwrap();
        assert_eq!(
            decoded,
            ControlMessage::DragStart {
                drag_id: 9,
                files: vec![DragFileInfo {
                    filename: "secret.txt".into(),
                    byte_len: 128,
                }],
            }
        );

        // Craft a raw frame with count > MAX_DRAG_FILES and ensure decoder rejects it.
        let mut raw = BytesMut::new();
        raw.put_u8(0x50);
        raw.put_u64(1);
        raw.put_u16((MAX_DRAG_FILES + 1) as u16);
        assert!(matches!(
            ControlMessage::decode_body(&raw),
            Err(ProtoError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn filename_sanitization_prevents_traversal() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"C:\Users\me\photo.png"), "photo.png");
        assert_eq!(sanitize_filename(".."), "glide-file.bin");
    }
}
