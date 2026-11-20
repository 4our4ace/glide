//! Compact, versioned binary wire protocol for Glide.
//!
//! Traffic is cleanly separated across three QUIC transport lanes:
//! 1. [`DatagramMessage`] — Unreliable, unordered QUIC datagrams for mouse deltas and
//!    high-resolution scroll (zero head-of-line blocking).
//! 2. [`ControlMessage`] — Reliable, ordered QUIC bidirectional stream for keystrokes,
//!    buttons, screen transitions, modifier reconciliation, and heartbeats.
//! 3. [`ClipboardPayload`] — Separate unidirectional bulk QUIC streams for clipboard
//!    UTF-8 text and PNG images so large copies never delay input.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use glide_hid::{HidUsage, ModifierMask, MouseButton};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_MAGIC: [u8; 4] = *b"GLID";
pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_CONTROL_FRAME_LEN: usize = 64 * 1024;
pub const MAX_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;

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
    /// Raw relative pointer delta (f32 preserves sub-pixel precision).
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
        /// Which edge of the *server* this client sits on (e.g. `Edge::Right`).
        edge_on_server: Edge,
        os: OsKind,
    },
    /// Server -> Client handshake reply.
    Welcome {
        version: u16,
        server_name: String,
        accepted: bool,
        reason: String,
    },
    /// Physical USB HID key press/release.
    Key { usage: HidUsage, pressed: bool },
    /// Mouse button press/release.
    Button { button: MouseButton, pressed: bool },
    /// Periodic / transition modifier state sync to prevent stuck modifiers.
    ModifierSync { mask: ModifierMask },
    /// Server -> Client: take control, warp cursor to `edge` at `normalized_pos ∈ [0.0, 1.0]`.
    Enter { edge: Edge, normalized_pos: f32 },
    /// Client -> Server (user exited back to server) OR Server -> Client (focus revoked).
    Leave { edge: Edge, normalized_pos: f32 },
    /// Force release of every currently held key and mouse button.
    ReleaseAll,
    /// Heartbeat request with sender timestamp in microseconds.
    Ping { ts_us: u64 },
    /// Heartbeat response echoing `ts_us`.
    Pong { ts_us: u64 },
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
    /// Encodes a control message into a payload (without the outer 4-byte frame length).
    pub fn encode_body(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(32);
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
            } => {
                buf.put_u8(0x11);
                buf.put_slice(&PROTOCOL_MAGIC);
                buf.put_u16(*version);
                buf.put_u8(u8::from(*accepted));
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
            } => {
                buf.put_u8(0x30);
                buf.put_u8(*edge as u8);
                buf.put_f32(normalized_pos.clamp(0.0, 1.0));
            }
            Self::Leave {
                edge,
                normalized_pos,
            } => {
                buf.put_u8(0x31);
                buf.put_u8(*edge as u8);
                buf.put_f32(normalized_pos.clamp(0.0, 1.0));
            }
            Self::ReleaseAll => {
                buf.put_u8(0x32);
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

    /// Encodes a length-prefixed frame (`u32` big-endian length + body).
    pub fn encode_framed(&self) -> Bytes {
        let body = self.encode_body();
        let mut out = BytesMut::with_capacity(4 + body.len());
        out.put_u32(body.len() as u32);
        out.put_slice(&body);
        out.freeze()
    }

    /// Decodes a control message body (without the outer 4-byte length prefix).
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
                if data.len() < 7 {
                    return Err(ProtoError::Truncated {
                        expected: 7,
                        actual: data.len(),
                    });
                }
                if data[..4] != PROTOCOL_MAGIC {
                    return Err(ProtoError::InvalidMagic);
                }
                data.advance(4);
                let version = data.get_u16();
                let accepted = data.get_u8() != 0;
                let server_name = get_string(&mut data)?;
                let reason = get_string(&mut data)?;
                Ok(Self::Welcome {
                    version,
                    server_name,
                    accepted,
                    reason,
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
                if data.len() < 5 {
                    return Err(ProtoError::Truncated {
                        expected: 5,
                        actual: data.len(),
                    });
                }
                let edge = Edge::from_u8(data.get_u8())?;
                let normalized_pos = data.get_f32().clamp(0.0, 1.0);
                Ok(Self::Enter {
                    edge,
                    normalized_pos,
                })
            }
            0x31 => {
                if data.len() < 5 {
                    return Err(ProtoError::Truncated {
                        expected: 5,
                        actual: data.len(),
                    });
                }
                let edge = Edge::from_u8(data.get_u8())?;
                let normalized_pos = data.get_f32().clamp(0.0, 1.0);
                Ok(Self::Leave {
                    edge,
                    normalized_pos,
                })
            }
            0x32 => Ok(Self::ReleaseAll),
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

/// Lane C: Bulk clipboard transfer carried over an independent unidirectional QUIC stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPayload {
    pub format: ClipboardFormat,
    pub sha256: [u8; 32],
    pub data: Bytes,
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
            },
            ControlMessage::Leave {
                edge: Edge::Right,
                normalized_pos: 0.85,
            },
            ControlMessage::ReleaseAll,
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
    fn clipboard_header_roundtrip() {
        let payload = ClipboardPayload {
            format: ClipboardFormat::TextUtf8,
            sha256: [0xAB; 32],
            data: Bytes::from_static("Grüße äöüß € @ |".as_bytes()),
        };
        let hdr = payload.encode_header();
        let (fmt, hash, len) = ClipboardPayload::decode_header(&hdr).unwrap();
        assert_eq!(fmt, ClipboardFormat::TextUtf8);
        assert_eq!(hash, [0xAB; 32]);
        assert_eq!(len, payload.data.len());
    }
}
