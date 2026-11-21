//! Platform-agnostic cross-edge file drag-and-drop orchestration helpers shared by
//! [`crate::server`] and [`crate::client`].

use crate::runtime::{save_received_file, RuntimeState};
use bytes::Bytes;
use glide_backend::dnd::DndBackend;
use glide_backend::os_integration::file_receive_dir;
use glide_net::{send_drag_file_on, sha256_bytes, Connection};
use glide_proto::{
    sanitize_filename, DragFileInfo, DragFilePayload, FilePayload, MAX_DRAG_FILES, MAX_FILE_BYTES,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

static NEXT_DRAG_ID: AtomicU64 = AtomicU64::new(1);

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

/// Validates and filters local paths captured from an OS drag-out before advertising `DragStart`.
///
/// Only readable regular files within `MAX_FILE_BYTES` (individually and combined) up to
/// `MAX_DRAG_FILES` are included. Directories and unreadable paths are skipped cleanly.
pub fn collect_local_drag_files(paths: &[PathBuf]) -> (Vec<DragFileInfo>, Vec<(String, PathBuf)>) {
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
        if len as usize > MAX_FILE_BYTES {
            warn!(
                path = %path.display(),
                bytes = len,
                max = MAX_FILE_BYTES,
                "Skipping dragged file exceeding 256 MiB limit"
            );
            continue;
        }
        if total_bytes.saturating_add(len) as usize > MAX_FILE_BYTES {
            warn!(
                path = %path.display(),
                "Skipping additional dragged file exceeding combined 256 MiB drag budget"
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
    runtime: Option<RuntimeState>,
) {
    for (filename, path) in files {
        let conn_c = conn.clone();
        let rt_c = runtime.clone();
        tokio::spawn(async move {
            let read_res = tokio::task::spawn_blocking(move || std::fs::read(&path)).await;
            let data = match read_res {
                Ok(Ok(bytes)) if bytes.len() <= MAX_FILE_BYTES => bytes,
                Ok(Ok(bytes)) => {
                    warn!(
                        %filename,
                        bytes = bytes.len(),
                        "Dragged file grew past MAX_FILE_BYTES during read; skipping"
                    );
                    return;
                }
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
                    data: Bytes::from(data),
                },
            };

            if let Err(e) = send_drag_file_on(&conn_c, &payload).await {
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
/// stray files in `Downloads/Glide/` or pollutes `RuntimeState::recent_files`.
#[derive(Debug)]
pub struct IncomingDragState {
    pub drag_id: u64,
    pub expected_count: usize,
    pub received: Vec<FilePayload>,
    pub total_bytes: usize,
    pub drop_requested: bool,
    pub started_at: Instant,
}

impl IncomingDragState {
    pub fn start(drag_id: u64, files: Vec<DragFileInfo>) -> Option<Self> {
        if files.is_empty() || files.len() > MAX_DRAG_FILES {
            return None;
        }
        Some(Self {
            drag_id,
            expected_count: files.len(),
            received: Vec::with_capacity(files.len()),
            total_bytes: 0,
            drop_requested: false,
            started_at: Instant::now(),
        })
    }

    pub fn push_file(&mut self, drag_id: u64, file: FilePayload) -> bool {
        if self.drag_id != drag_id || self.received.len() >= self.expected_count {
            return false;
        }
        let next_total = self.total_bytes.saturating_add(file.data.len());
        if next_total > MAX_FILE_BYTES {
            warn!(
                drag_id,
                next_total,
                max = MAX_FILE_BYTES,
                "Rejecting incoming drag file exceeding 256 MiB combined budget"
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

/// Saves committed drag files into `Downloads/Glide/` (or an overridden directory for tests),
/// records each in [`RuntimeState::recent_files`], and hands them off to [`DndBackend::deliver_drop`]
/// off the Tokio async executor so mouse/keyboard latency stays unaffected.
pub fn spawn_commit_incoming_drag(
    drag_id: u64,
    files: Vec<FilePayload>,
    receive_dir: Option<PathBuf>,
    dnd: Arc<dyn DndBackend>,
    runtime: Option<RuntimeState>,
) {
    tokio::task::spawn_blocking(move || {
        let dir = match receive_dir {
            Some(d) => d,
            None => match file_receive_dir() {
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
                    warn!(drag_id, error = %e, "OS drop handoff failed (files remain in Downloads/Glide)");
                }
            }
        }
    });
}
