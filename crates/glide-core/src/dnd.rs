//! Platform-agnostic cross-edge file drag-and-drop orchestration helpers shared by
//! [`crate::server`] and [`crate::client`].

use crate::runtime::{read_regular_file_bounded, save_received_file, RuntimeState};
use glide_backend::dnd::DndBackend;
use glide_backend::os_integration::resolve_receive_dir;
use glide_net::{send_drag_file_on_with_limit, sha256_bytes, Connection};
use glide_proto::{sanitize_filename, DragFileInfo, DragFilePayload, FilePayload, MAX_DRAG_FILES};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

static NEXT_DRAG_ID: AtomicU64 = AtomicU64::new(1);
const EARLY_DRAG_TTL: Duration = Duration::from_secs(10);

/// Allocates a monotonically increasing unique identifier for a cross-edge drag operation.
pub fn next_drag_id() -> u64 {
    NEXT_DRAG_ID.fetch_add(1, Ordering::Relaxed)
}

/// Active outgoing cross-edge drag tracked by the sender until drop or cancellation.
#[derive(Debug, Clone, Copy)]
pub struct OutgoingDragState {
    pub drag_id: u64,
    pub peer_id: u64,
}

/// Bounded buffer for `DragFile` bulk streams (QUIC Lane C) that win the race against
/// their corresponding `DragStart` message (QUIC Lane B control stream).
#[derive(Debug, Default)]
pub struct EarlyDragBuffer {
    entries: Vec<(u64, Instant, FilePayload)>,
}

impl EarlyDragBuffer {
    pub fn push(&mut self, drag_id: u64, file: FilePayload) {
        self.prune_expired();
        if self.entries.len() >= MAX_DRAG_FILES {
            debug!(
                drag_id,
                file = %file.filename,
                "Early drag buffer full; dropping oldest buffered entry"
            );
            self.entries.remove(0);
        }
        self.entries.push((drag_id, Instant::now(), file));
    }

    pub fn drain_for(&mut self, drag_id: u64) -> Vec<FilePayload> {
        self.prune_expired();
        let mut matched = Vec::new();
        let mut remaining = Vec::with_capacity(self.entries.len());
        for (id, at, file) in self.entries.drain(..) {
            if id == drag_id {
                matched.push(file);
            } else {
                remaining.push((id, at, file));
            }
        }
        self.entries = remaining;
        matched
    }

    pub fn remove_drag(&mut self, drag_id: u64) {
        self.entries.retain(|(id, _, _)| *id != drag_id);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        self.entries
            .retain(|(_, at, _)| now.saturating_duration_since(*at) < EARLY_DRAG_TTL);
    }
}

/// Validates and filters local paths captured from an OS drag-out before advertising `DragStart`.
///
/// Only readable regular files within `max_file_bytes` (individually and combined) up to
/// `MAX_DRAG_FILES` are included. Directories and unreadable paths are skipped cleanly.
pub fn collect_local_drag_files(
    paths: &[PathBuf],
    max_file_bytes: usize,
) -> (Vec<DragFileInfo>, Vec<(String, PathBuf)>) {
    let effective_limit = max_file_bytes.min(u32::MAX as usize);
    let mut infos = Vec::new();
    let mut to_send = Vec::new();
    let mut total_bytes: u64 = 0;

    for path in paths.iter().take(MAX_DRAG_FILES) {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                debug!(path = %path.display(), error = %e, "Skipping unreadable dragged path");
                continue;
            }
        };
        if !meta.is_file() {
            debug!(path = %path.display(), "Skipping non-regular dragged item (directory/special)");
            continue;
        }
        let len = meta.len();
        if len > effective_limit as u64 {
            warn!(
                path = %path.display(),
                bytes = len,
                max = effective_limit,
                "Skipping dragged file exceeding configured max_file_bytes limit"
            );
            continue;
        }
        if total_bytes.saturating_add(len) > effective_limit as u64 {
            warn!(
                path = %path.display(),
                max = effective_limit,
                "Skipping additional dragged file exceeding combined drag budget"
            );
            continue;
        }
        // Verify readability before advertising expected_count in DragStart.
        if std::fs::File::open(path).is_err() {
            debug!(path = %path.display(), "Skipping dragged file without read permission");
            continue;
        }

        let raw_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("glide-file.bin");
        let safe_name = sanitize_filename(raw_name);
        total_bytes = total_bytes.saturating_add(len);
        infos.push(DragFileInfo {
            filename: safe_name.clone(),
            byte_len: len,
        });
        to_send.push((safe_name, path.clone()));
    }

    (infos, to_send)
}

/// Streams all dragged files in parallel over independent QUIC Lane C unidirectional streams.
pub fn spawn_send_drag_files(
    conn: Connection,
    drag_id: u64,
    files: Vec<(String, PathBuf)>,
    max_file_bytes: usize,
    runtime: Option<RuntimeState>,
) {
    let effective_limit = max_file_bytes.min(u32::MAX as usize);
    for (filename, path) in files {
        let conn_c = conn.clone();
        let rt_c = runtime.clone();
        tokio::spawn(async move {
            let read_res =
                tokio::task::spawn_blocking(move || read_regular_file_bounded(&path, effective_limit))
                    .await;
            let data = match read_res {
                Ok(Ok((_, bytes))) => bytes,
                Ok(Err(e)) => {
                    warn!(%filename, error = %e, "Failed reading dragged file for transfer");
                    return;
                }
                Err(e) => {
                    warn!(%filename, error = %e, "Drag file read task failed");
                    return;
                }
            };

            let byte_len = data.len();
            let payload = DragFilePayload {
                drag_id,
                file: FilePayload {
                    filename: filename.clone(),
                    sha256: sha256_bytes(&data),
                    data,
                },
            };

            if let Err(e) = send_drag_file_on_with_limit(&conn_c, &payload, effective_limit).await {
                warn!(drag_id, %filename, error = %e, "Failed streaming dragged file over QUIC");
            } else {
                debug!(drag_id, %filename, bytes = byte_len, "Streamed dragged file over QUIC");
                if let Some(ref rt) = rt_c {
                    rt.record_file("sent", filename, byte_len, None);
                }
            }
        });
    }
}

/// Receiver-side state machine for an active incoming cross-edge drag-and-drop transfer.
///
/// Files are buffered until the user commits the drop (`DragDrop` or local LMB release),
/// ensuring a cancelled drag (`DragCancel` or dragging back to the origin PC) never leaves
/// stray files in the receive directory or pollutes `RuntimeState::recent_files`.
#[derive(Debug)]
pub struct IncomingDragState {
    pub drag_id: u64,
    pub expected_count: usize,
    pub received: Vec<FilePayload>,
    pub total_bytes: usize,
    pub max_total_bytes: usize,
    pub drop_requested: bool,
    pub started_at: Instant,
}

impl IncomingDragState {
    pub fn start(drag_id: u64, files: Vec<DragFileInfo>, max_file_bytes: usize) -> Option<Self> {
        if files.is_empty() || files.len() > MAX_DRAG_FILES {
            return None;
        }
        Some(Self {
            drag_id,
            expected_count: files.len(),
            received: Vec::with_capacity(files.len()),
            total_bytes: 0,
            max_total_bytes: max_file_bytes.min(u32::MAX as usize),
            drop_requested: false,
            started_at: Instant::now(),
        })
    }

    pub fn push_file(&mut self, drag_id: u64, file: FilePayload) -> bool {
        if self.drag_id != drag_id || self.received.len() >= self.expected_count {
            return false;
        }
        let next_total = self.total_bytes.saturating_add(file.data.len());
        if next_total > self.max_total_bytes {
            warn!(
                drag_id,
                next_total,
                max = self.max_total_bytes,
                "Rejecting incoming drag file exceeding configured combined drag budget"
            );
            return false;
        }
        self.total_bytes = next_total;
        self.received.push(file);
        true
    }

    pub fn request_drop(&mut self, drag_id: u64) -> bool {
        if self.drag_id != drag_id {
            return false;
        }
        self.drop_requested = true;
        true
    }

    pub fn request_drop_active(&mut self) {
        self.drop_requested = true;
    }

    pub fn is_ready_to_commit(&self) -> bool {
        self.drop_requested && self.received.len() >= self.expected_count && !self.received.is_empty()
    }

    pub fn into_files(self) -> Vec<FilePayload> {
        self.received
    }
}

/// Saves committed drag files into the configured receive directory (or test override, defaulting
/// to `Downloads/Glide/`), records each in [`RuntimeState::recent_files`], and hands them off to
/// [`DndBackend::deliver_drop`] off the Tokio async executor so mouse/keyboard latency stays unaffected.
pub fn spawn_commit_incoming_drag(
    drag_id: u64,
    files: Vec<FilePayload>,
    receive_dir_override: Option<PathBuf>,
    configured_receive_dir: Option<String>,
    dnd: Arc<dyn DndBackend>,
    runtime: Option<RuntimeState>,
) {
    tokio::task::spawn_blocking(move || {
        let dir = match receive_dir_override {
            Some(d) => d,
            None => match resolve_receive_dir(configured_receive_dir.as_deref()) {
                Ok(d) => d,
                Err(e) => {
                    warn!(drag_id, error = %e, "Failed resolving file receive directory for drop");
                    return;
                }
            },
        };

        let mut saved_paths = Vec::with_capacity(files.len());
        for file in files {
            let safe_name = sanitize_filename(&file.filename);
            match save_received_file(&dir, &safe_name, &file.data) {
                Ok(saved) => {
                    info!(
                        drag_id,
                        file = %safe_name,
                        path = %saved.display(),
                        bytes = file.data.len(),
                        "Saved cross-edge dragged file"
                    );
                    if let Some(ref rt) = runtime {
                        rt.record_file("received", safe_name, file.data.len(), Some(saved.clone()));
                    }
                    saved_paths.push(saved);
                }
                Err(e) => {
                    warn!(drag_id, file = %safe_name, error = %e, "Failed saving dragged file");
                }
            }
        }

        if !saved_paths.is_empty() {
            match dnd.deliver_drop(saved_paths) {
                Ok(outcome) => {
                    debug!(drag_id, ?outcome, "Completed OS drop handoff for cross-edge drag");
                }
                Err(e) => {
                    warn!(drag_id, error = %e, "OS drop handoff failed (files remain in receive folder)");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn early_drag_buffer_handles_lane_c_arriving_before_drag_start() {
        let mut early = EarlyDragBuffer::default();
        let f1 = FilePayload {
            filename: "photo.png".into(),
            sha256: [1u8; 32],
            data: Bytes::from_static(b"png-bytes"),
        };
        let f2 = FilePayload {
            filename: "other.txt".into(),
            sha256: [2u8; 32],
            data: Bytes::from_static(b"other"),
        };
        early.push(42, f1);
        early.push(99, f2);

        let mut inc = IncomingDragState::start(
            42,
            vec![DragFileInfo {
                filename: "photo.png".into(),
                byte_len: 9,
            }],
            1024,
        )
        .unwrap();
        for buffered in early.drain_for(42) {
            assert!(inc.push_file(42, buffered));
        }
        inc.request_drop(42);
        assert!(inc.is_ready_to_commit());
        assert_eq!(inc.into_files().len(), 1);
        // Unrelated drag_id 99 remains buffered
        assert_eq!(early.drain_for(99).len(), 1);
    }

    #[test]
    fn custom_max_file_bytes_enforced_on_drag_collect_and_incoming_state() {
        let dir = std::env::temp_dir().join(format!("glide-dnd-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small.txt");
        let large = dir.join("large.bin");
        std::fs::write(&small, vec![0x11; 16]).unwrap();
        std::fs::write(&large, vec![0x22; 64]).unwrap();

        let (infos, paths) = collect_local_drag_files(&[small.clone(), large.clone()], 32);
        assert_eq!(infos.len(), 1);
        assert_eq!(paths.len(), 1);
        assert_eq!(infos[0].filename, "small.txt");

        let mut inc = IncomingDragState::start(
            7,
            vec![DragFileInfo {
                filename: "large.bin".into(),
                byte_len: 64,
            }],
            32,
        )
        .unwrap();
        assert!(!inc.push_file(
            7,
            FilePayload {
                filename: "large.bin".into(),
                sha256: [0u8; 32],
                data: Bytes::from(vec![0x22; 64]),
            },
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
