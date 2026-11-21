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
}

/// Sanitizes a normalized `[0.0, 1.0]` screen coordinate, replacing `NaN` or `±Infinity` with `0.5`.
pub fn sanitize_unit(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

fn put_string(buf: &mut BytesMut, s: &str) {
    let truncated = truncate_utf8_bytes(s, u16::MAX as usize);
    let bytes = truncated.as_bytes();
    buf.put_u16(bytes.len() as u16);
    buf.put_slice(bytes);
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
                let scale = if server_scale.is_finite() && *server_scale > 0.0 {
                    *server_scale
                } else {
                    1.0
                };
                buf.put_f32(scale);
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
                buf.put_f32(sanitize_unit(*normalized_pos));
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
                buf.put_f32(sanitize_unit(*normalized_pos));
                buf.put_f32(sanitize_unit(*cursor_norm_x));
                buf.put_f32(sanitize_unit(*cursor_norm_y));
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
                buf.put_f32(sanitize_unit(*norm_x));
                buf.put_f32(sanitize_unit(*norm_y));
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
                let raw_scale = data.get_f32();
                let server_scale = if raw_scale.is_finite() && raw_scale > 0.0 {
                    raw_scale
                } else {
                    1.0
                };
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
                let normalized_pos = sanitize_unit(data.get_f32());
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
                let normalized_pos = sanitize_unit(data.get_f32());
                let cursor_norm_x = sanitize_unit(data.get_f32());
                let cursor_norm_y = sanitize_unit(data.get_f32());
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
                let norm_x = sanitize_unit(data.get_f32());
                let norm_y = sanitize_unit(data.get_f32());
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

/// Bulk file transfer carried on Lane C (drag-and-drop / `glide send-file`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePayload {
    pub filename: String,
    pub sha256: [u8; 32],
    pub data: Bytes,
}

/// Lane C unidirectional QUIC stream payloads (independent of mouse & keyboard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkTransfer {
    Clipboard(ClipboardPayload),
    File(FilePayload),
}

pub const BULK_KIND_CLIPBOARD: u8 = 0x01;
pub const BULK_KIND_FILE: u8 = 0x02;

/// Maximum UTF-8 byte length allowed for sanitized received filenames.
pub const MAX_FILENAME_BYTES: usize = 200;

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn is_windows_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    if stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
    {
        return true;
    }
    let bytes = stem.as_bytes();
    if bytes.len() == 4 && matches!(bytes[3], b'1'..=b'9') {
        let prefix = &stem[..3];
        if prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT") {
            return true;
        }
    }
    false
}

/// Strips directory components, unsafe characters, trailing dots/spaces, and Windows
/// reserved device names, and caps UTF-8 byte length so received filenames are safe
/// across Linux, macOS, and Windows filesystems.
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|&c| {
            !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
        .collect();
    let trimmed = cleaned
        .trim_start()
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace());

    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "glide-file.bin".to_string();
    }

    let mut result = if is_windows_reserved_device_name(trimmed) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };

    if result.len() > MAX_FILENAME_BYTES {
        result = match result.rfind('.') {
            Some(dot_idx)
                if dot_idx > 0
                    && result.len() - dot_idx <= 32
                    && result.len() - dot_idx < MAX_FILENAME_BYTES =>
            {
                let ext = &result[dot_idx..];
                let stem_budget = MAX_FILENAME_BYTES - ext.len();
                let stem = truncate_utf8_bytes(&result[..dot_idx], stem_budget)
                    .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
                if stem.is_empty() {
                    format!("file{ext}")
                } else {
                    format!("{stem}{ext}")
                }
            }
            _ => truncate_utf8_bytes(&result, MAX_FILENAME_BYTES)
                .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
                .to_string(),
        };
    }

    let final_trimmed = result
        .trim_start()
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    if final_trimmed.is_empty() || final_trimmed == "." || final_trimmed == ".." {
        "glide-file.bin".to_string()
    } else if is_windows_reserved_device_name(final_trimmed) {
        let prefixed = format!("_{final_trimmed}");
        truncate_utf8_bytes(&prefixed, MAX_FILENAME_BYTES).to_string()
    } else if final_trimmed.len() != result.len() {
        final_trimmed.to_string()
    } else {
        result
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
        ];

        for msg in samples {
            let body = msg.encode_body();
            let decoded = ControlMessage::decode_body(&body).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn filename_sanitization_prevents_traversal() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"C:\Users\me\photo.png"), "photo.png");
        assert_eq!(sanitize_filename(".."), "glide-file.bin");
    }

    #[test]
    fn filename_sanitization_neutralizes_windows_reserved_names() {
        for reserved in [
            "CON", "con", "PRN", "AUX", "NUL", "COM1", "com9", "LPT1", "lpt9",
        ] {
            let safe = sanitize_filename(reserved);
            assert_eq!(safe, format!("_{reserved}"));
        }
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_filename("nul.tar.gz"), "_nul.tar.gz");
        assert_eq!(sanitize_filename("Lpt3.log"), "_Lpt3.log");
        assert_eq!(sanitize_filename("CON   .txt"), "_CON   .txt");
        assert_eq!(sanitize_filename("aux..."), "_aux");
        // Non-reserved neighbors remain intact.
        assert_eq!(sanitize_filename("COM0.txt"), "COM0.txt");
        assert_eq!(sanitize_filename("COM10.txt"), "COM10.txt");
        assert_eq!(sanitize_filename("CONSOLE.txt"), "CONSOLE.txt");
    }

    #[test]
    fn filename_sanitization_strips_trailing_dots_and_spaces() {
        assert_eq!(sanitize_filename("report.pdf   "), "report.pdf");
        assert_eq!(sanitize_filename("report.pdf..."), "report.pdf");
        assert_eq!(sanitize_filename("  notes.txt . . "), "notes.txt");
        assert_eq!(sanitize_filename("...   "), "glide-file.bin");
        assert_eq!(sanitize_filename(". ."), "glide-file.bin");
    }

    #[test]
    fn filename_sanitization_caps_long_utf8_names_safely() {
        let long_ascii = format!("{}.png", "a".repeat(250));
        let safe_ascii = sanitize_filename(&long_ascii);
        assert!(safe_ascii.len() <= MAX_FILENAME_BYTES);
        assert!(safe_ascii.ends_with(".png"));

        let long_emoji = format!("{}.txt", "🚀".repeat(60));
        let safe_emoji = sanitize_filename(&long_emoji);
        assert!(safe_emoji.len() <= MAX_FILENAME_BYTES);
        assert!(safe_emoji.ends_with(".txt"));
        assert!(safe_emoji.starts_with("🚀"));

        let long_no_ext = format!("a{}", "🔥".repeat(60));
        let safe_no_ext = sanitize_filename(&long_no_ext);
        assert!(safe_no_ext.len() <= MAX_FILENAME_BYTES);
        assert!(safe_no_ext.len() == 1 + 49 * 4);
    }

    #[test]
    fn sanitize_unit_handles_nan_and_infinity_on_encode_and_decode() {
        assert_eq!(sanitize_unit(f32::NAN), 0.5);
        assert_eq!(sanitize_unit(f32::INFINITY), 0.5);
        assert_eq!(sanitize_unit(f32::NEG_INFINITY), 0.5);
        assert_eq!(sanitize_unit(-2.0), 0.0);
        assert_eq!(sanitize_unit(5.0), 1.0);
        assert_eq!(sanitize_unit(0.25), 0.25);

        // Raw wire Enter message with NaN normalized_pos must decode to 0.5, never NaN.
        let mut raw_enter = BytesMut::new();
        raw_enter.put_u8(0x30);
        raw_enter.put_u8(Edge::Right as u8);
        raw_enter.put_f32(f32::NAN);
        raw_enter.put_u8(0);
        match ControlMessage::decode_body(&raw_enter).unwrap() {
            ControlMessage::Enter { normalized_pos, .. } => assert_eq!(normalized_pos, 0.5),
            other => panic!("unexpected variant: {other:?}"),
        }

        // Raw wire EnterAt message with +Inf / -Inf must decode to (0.5, 0.5).
        let mut raw_enter_at = BytesMut::new();
        raw_enter_at.put_u8(0x33);
        raw_enter_at.put_f32(f32::INFINITY);
        raw_enter_at.put_f32(f32::NEG_INFINITY);
        raw_enter_at.put_u8(0);
        match ControlMessage::decode_body(&raw_enter_at).unwrap() {
            ControlMessage::EnterAt { norm_x, norm_y, .. } => {
                assert_eq!(norm_x, 0.5);
                assert_eq!(norm_y, 0.5);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn put_string_truncates_on_utf8_char_boundary() {
        // 1 ASCII byte + 16384 4-byte emojis = 65537 bytes (> u16::MAX = 65535);
        // raw byte slicing at 65535 would split the final 4-byte UTF-8 sequence.
        let oversized = format!("a{}", "🚀".repeat(16_384));
        assert!(oversized.len() > u16::MAX as usize);
        let msg = ControlMessage::Hello {
            version: PROTOCOL_VERSION,
            peer_name: oversized,
            edge_on_server: Edge::Left,
            os: OsKind::LinuxWayland,
        };
        let encoded = msg.encode_body();
        match ControlMessage::decode_body(&encoded).unwrap() {
            ControlMessage::Hello { peer_name, .. } => {
                assert!(peer_name.len() <= u16::MAX as usize);
                assert!(peer_name.starts_with("a🚀"));
                assert!(peer_name.ends_with('🚀'));
                assert_eq!(peer_name.len(), 1 + 16_383 * 4);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
