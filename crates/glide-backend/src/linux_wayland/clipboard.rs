//! Event-driven Wayland clipboard backend (`ext-data-control` / `wlr-data-control`).
//!
//! Uses `wl-clipboard-rs` (`Watcher` for zero-polling change monitoring and `prepare_copy` +
//! in-process background serving without `fork()`). On compositors that do not expose a
//! data-control protocol (such as GNOME < 49), transparently falls back to the XWayland X11
//! clipboard bridge when `DISPLAY` is available.

use crate::{ClipboardBackend, ClipboardHandle};
use anyhow::{anyhow, Result};
use bytes::Bytes;
use glide_net::sha256_bytes;
use glide_proto::{ClipboardFormat, ClipboardPayload, MAX_CLIPBOARD_BYTES};
use parking_lot::Mutex;
use std::io::Read;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wl_clipboard_rs::copy::{
    ClipboardType as CopyClipboardType, MimeType as CopyMimeType, Options, Seat as CopySeat,
    Source,
};
use wl_clipboard_rs::paste::{ClipboardType as PasteClipboardType, Seat as PasteSeat};
use wl_clipboard_rs::watch::{CancelHandle, ClipboardEvent, ClipboardType as WatchClipboardType, Watcher};

struct SharedState {
    last_written_hash: Mutex<Option<[u8; 32]>>,
}

struct WaylandClipboardBackend {
    shared: Arc<SharedState>,
    cancel: CancelHandle,
}

impl Drop for WaylandClipboardBackend {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl ClipboardBackend for WaylandClipboardBackend {
    fn set(&self, payload: ClipboardPayload) -> Result<()> {
        let mut opts = Options::new();
        opts.clipboard(CopyClipboardType::Regular)
            .seat(CopySeat::All)
            .foreground(true);

        let prepared = match payload.format {
            ClipboardFormat::TextUtf8 => {
                // Normalize CRLF -> LF for clean Linux pasting; reuse the existing
                // SHA-256 when no CRLF replacement was needed.
                let (normalized, hash) = match std::str::from_utf8(&payload.data) {
                    Ok(s) if s.contains("\r\n") => {
                        let bytes = s.replace("\r\n", "\n").into_bytes();
                        let h = sha256_bytes(&bytes);
                        (bytes, h)
                    }
                    _ => (payload.data.to_vec(), payload.sha256),
                };
                *self.shared.last_written_hash.lock() = Some(hash);
                // CopyMimeType::Text registers all standard UTF-8/text MIME types sharing a single Arc<[u8]>.
                opts.prepare_copy(Source::Bytes(normalized.into()), CopyMimeType::Text)
                    .map_err(|e| anyhow!("Wayland clipboard prepare_copy text failed: {e}"))?
            }
            ClipboardFormat::ImagePng => {
                *self.shared.last_written_hash.lock() = Some(payload.sha256);
                let data = payload.data.to_vec();
                opts.prepare_copy(
                    Source::Bytes(data.into()),
                    CopyMimeType::Specific("image/png".into()),
                )
                .map_err(|e| anyhow!("Wayland clipboard prepare_copy image failed: {e}"))?
            }
        };

        // Serve selection requests in-process on a background thread (never fork!).
        // The compositor automatically cancels this source when another app or our next
        // `set()` takes ownership, causing `serve()` to return cleanly.
        std::thread::Builder::new()
            .name("glide-wl-clip-serve".into())
            .spawn(move || {
                if let Err(e) = prepared.serve() {
                    debug!(error = %e, "Wayland clipboard source serve ended");
                }
            })?;

        Ok(())
    }
}

fn pick_best_mime(mime_types: &[String]) -> Option<(ClipboardFormat, String)> {
    if crate::clipboard_privacy::has_sensitive_mime_type(mime_types) {
        return None;
    }
    // Prefer PNG images first (screenshots), then UTF-8 text.
    if let Some(m) = mime_types.iter().find(|m| m.eq_ignore_ascii_case("image/png")) {
        return Some((ClipboardFormat::ImagePng, m.clone()));
    }
    for candidate in [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
    ] {
        if let Some(m) = mime_types.iter().find(|m| m.eq_ignore_ascii_case(candidate)) {
            return Some((ClipboardFormat::TextUtf8, m.clone()));
        }
    }
    None
}

fn read_bounded_clipboard<R: Read>(reader: R, max_bytes: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let limit = (max_bytes as u64).saturating_add(1);
    reader.take(limit).read_to_end(&mut buf).ok()?;
    if buf.is_empty() || buf.len() > max_bytes {
        return None;
    }
    Some(buf)
}

/// Starts the Wayland bidirectional clipboard monitor/writer.
///
/// Falls back cleanly to the XWayland X11 clipboard bridge if the compositor does not
/// implement `ext-data-control` / `wlr-data-control` (e.g. GNOME < 49) and `DISPLAY` is set.
pub fn start_wayland_clipboard() -> Result<ClipboardHandle> {
    match Watcher::new(WatchClipboardType::Regular, PasteSeat::Unspecified) {
        Ok(mut watcher) => {
            let cancel = watcher.cancel_handle();
            let shared = Arc::new(SharedState {
                last_written_hash: Mutex::new(None),
            });
            let (tx, rx) = mpsc::unbounded_channel();
            let shared_bg = Arc::clone(&shared);

            std::thread::Builder::new()
                .name("glide-wl-clip-watch".into())
                .spawn(move || {
                    let mut first_event = true;
                    loop {
                        match watcher.next_event() {
                            Ok(None) => {
                                debug!("Wayland clipboard watcher cancelled");
                                break;
                            }
                            Ok(Some(ClipboardEvent::Cleared { .. })) => {
                                first_event = false;
                            }
                            Ok(Some(ClipboardEvent::Changed {
                                clipboard,
                                mime_types,
                                mut offer,
                            })) => {
                                if clipboard != PasteClipboardType::Regular {
                                    continue;
                                }
                                // Skip initial pre-existing selection on startup so connecting
                                // doesn't immediately clobber the peer's clipboard.
                                if first_event {
                                    first_event = false;
                                    continue;
                                }
                                if crate::clipboard_privacy::has_sensitive_mime_type(&mime_types) {
                                    debug!(
                                        "Skipping sensitive Wayland clipboard selection (x-kde-passwordManagerHint)"
                                    );
                                    continue;
                                }
                                let Some((format, mime)) = pick_best_mime(&mime_types) else {
                                    continue;
                                };
                                let Ok(pipe) = offer.receive(&mime) else {
                                    continue;
                                };
                                let Some(buf) = read_bounded_clipboard(pipe, MAX_CLIPBOARD_BYTES) else {
                                    continue;
                                };
                                let hash = sha256_bytes(&buf);
                                if shared_bg.last_written_hash.lock().as_ref() == Some(&hash) {
                                    continue;
                                }
                                let payload = ClipboardPayload {
                                    format,
                                    sha256: hash,
                                    data: Bytes::from(buf),
                                };
                                if tx.send(payload).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "Wayland clipboard watcher stopped");
                                break;
                            }
                        }
                    }
                })?;

            info!("Wayland native clipboard active (ext-data-control / wlr-data-control)");
            Ok(ClipboardHandle {
                backend: Box::new(WaylandClipboardBackend { shared, cancel }),
                local_changes: rx,
            })
        }
        Err(e) => {
            if std::env::var_os("DISPLAY").is_some() {
                info!(
                    error = %e,
                    "Native Wayland data-control protocol unavailable; using XWayland clipboard bridge"
                );
                crate::linux_x11::clipboard::start_x11_clipboard()
            } else {
                Err(anyhow!(
                    "Wayland clipboard unavailable ({e}) and no XWayland DISPLAY present"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_selection_prefers_png_then_utf8() {
        let mimes = vec![
            "text/plain".to_owned(),
            "image/png".to_owned(),
            "text/plain;charset=utf-8".to_owned(),
        ];
        assert_eq!(
            pick_best_mime(&mimes),
            Some((ClipboardFormat::ImagePng, "image/png".to_owned()))
        );

        let text_only = vec!["text/html".to_owned(), "text/plain;charset=utf-8".to_owned()];
        assert_eq!(
            pick_best_mime(&text_only),
            Some((
                ClipboardFormat::TextUtf8,
                "text/plain;charset=utf-8".to_owned()
            ))
        );
    }

    #[test]
    fn bounded_clipboard_read_enforces_max_bytes() {
        assert_eq!(read_bounded_clipboard(&b""[..], 16), None);
        assert_eq!(
            read_bounded_clipboard(&b"hello"[..], 5),
            Some(b"hello".to_vec())
        );
        // Oversized stream (including an infinite reader) stops at max_bytes + 1 and returns None.
        assert_eq!(read_bounded_clipboard(std::io::repeat(b'x'), 64), None);
    }

    #[test]
    fn mime_selection_skips_password_manager_hint() {
        let secret = vec![
            "text/plain;charset=utf-8".to_owned(),
            "x-kde-passwordManagerHint".to_owned(),
        ];
        assert_eq!(pick_best_mime(&secret), None);
    }
}
