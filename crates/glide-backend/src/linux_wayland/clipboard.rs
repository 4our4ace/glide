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
use glide_proto::{ClipboardFormat, ClipboardPayload};
use parking_lot::Mutex;
use std::io::Read;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wl_clipboard_rs::copy::{
    ClipboardType as CopyClipboardType, MimeSource, MimeType as CopyMimeType, Options,
    Seat as CopySeat, Source,
};
use wl_clipboard_rs::paste::{ClipboardType as PasteClipboardType, Seat as PasteSeat};
use wl_clipboard_rs::watch::{CancelHandle, ClipboardEvent, ClipboardType as WatchClipboardType, Watcher};

const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

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
        *self.shared.last_written_hash.lock() = Some(payload.sha256);

        let mut opts = Options::new();
        opts.clipboard(CopyClipboardType::Regular)
            .seat(CopySeat::All)
            .foreground(true);

        let prepared = match payload.format {
            ClipboardFormat::TextUtf8 => {
                // Normalize CRLF -> LF for clean Linux pasting
                let normalized = match std::str::from_utf8(&payload.data) {
                    Ok(s) if s.contains("\r\n") => s.replace("\r\n", "\n").into_bytes(),
                    _ => payload.data.to_vec(),
                };
                // Also store normalized hash so our watcher skips the echo
                *self.shared.last_written_hash.lock() = Some(sha256_bytes(&normalized));
                let sources = vec![
                    MimeSource {
                        source: Source::Bytes(normalized.clone().into()),
                        mime_type: CopyMimeType::Specific("text/plain;charset=utf-8".into()),
                    },
                    MimeSource {
                        source: Source::Bytes(normalized.clone().into()),
                        mime_type: CopyMimeType::Specific("UTF8_STRING".into()),
                    },
                    MimeSource {
                        source: Source::Bytes(normalized.into()),
                        mime_type: CopyMimeType::Specific("text/plain".into()),
                    },
                ];
                opts.prepare_copy_multi(sources)
                    .map_err(|e| anyhow!("Wayland clipboard prepare_copy text failed: {e}"))?
            }
            ClipboardFormat::ImagePng => {
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
                                let Some((format, mime)) = pick_best_mime(&mime_types) else {
                                    continue;
                                };
                                let Ok(pipe) = offer.receive(&mime) else {
                                    continue;
                                };
                                let mut buf = Vec::new();
                                if pipe
                                    .take((MAX_CLIPBOARD_BYTES + 1) as u64)
                                    .read_to_end(&mut buf)
                                    .is_err()
                                {
                                    continue;
                                }
                                if buf.is_empty() || buf.len() > MAX_CLIPBOARD_BYTES {
                                    continue;
                                }
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
}
