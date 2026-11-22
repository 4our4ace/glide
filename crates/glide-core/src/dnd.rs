//! Platform-agnostic cross-edge file drag-and-drop orchestration helpers shared by
//! [`crate::server`] and [`crate::client`].

use crate::runtime::{read_regular_file_bounded, save_received_file, RuntimeState};
use glide_backend::dnd::DndBackend;
use glide_backend::os_integration::resolve_receive_dir;
use glide_net::{send_drag_file_on_with_limit, sha256_bytes_async, Connection};
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
            let sha256 = sha256_bytes_async(data.clone()).await;
            let payload = DragFilePayload {
                drag_id,
                file: FilePayload {
                    filename: filename.clone(),
                    sha256,
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

#[derive(Debug)]
struct LiveStageInner {
    /// `None` while the blocking save task is still writing files to disk;
    /// `Some(entries)` once saving has completed (empty if saving failed).
    saved: Option<Vec<(String, usize, PathBuf)>>,
    cancelled: bool,
    committed: bool,
}

/// Thread-safe handle to an incoming drag whose files were written early to disk so
/// Windows (`SHDoDragDrop`) or Linux X11 (live XDND v5) can start a real OS drag while
/// the user is still moving the cursor.
///
/// Guarantees:
/// - Each file is written to disk at most once (never duplicated on drop).
/// - Cancelling before drop (even while saving is still in progress) deletes every
///   uncommitted staged file so zero orphan files remain on disk.
#[derive(Debug, Clone)]
pub struct LiveDragStaging {
    inner: Arc<(std::sync::Mutex<LiveStageInner>, std::sync::Condvar)>,
}

impl LiveDragStaging {
    fn new() -> Self {
        Self {
            inner: Arc::new((
                std::sync::Mutex::new(LiveStageInner {
                    saved: None,
                    cancelled: false,
                    committed: false,
                }),
                std::sync::Condvar::new(),
            )),
        }
    }
}

/// Receiver-side state machine for an active incoming cross-edge drag-and-drop transfer.
///
/// On backends that support live OS drags (`DndBackend::supports_live_os_drag()`), files
/// are staged to disk as soon as all payloads arrive so the OS can immediately start a real
/// native drag-and-drop preview & folder-highlight loop while LMB is still held. If the drag
/// is cancelled before dropping, uncommitted staged files are deleted cleanly.
/// On non-live backends (e.g. Wayland), files stay buffered in memory until drop.
#[derive(Debug)]
pub struct IncomingDragState {
    pub drag_id: u64,
    pub filenames: Vec<String>,
    pub expected_count: usize,
    pub received: Vec<FilePayload>,
    pub total_bytes: usize,
    pub max_total_bytes: usize,
    pub drop_requested: bool,
    pub started_at: Instant,
    live_staging: Option<LiveDragStaging>,
}

impl IncomingDragState {
    pub fn start(drag_id: u64, files: Vec<DragFileInfo>, max_file_bytes: usize) -> Option<Self> {
        if files.is_empty() || files.len() > MAX_DRAG_FILES {
            return None;
        }
        let filenames: Vec<String> = files
            .iter()
            .map(|f| sanitize_filename(&f.filename))
            .collect();
        Some(Self {
            drag_id,
            filenames,
            expected_count: files.len(),
            received: Vec::with_capacity(files.len()),
            total_bytes: 0,
            max_total_bytes: max_file_bytes.min(u32::MAX as usize),
            drop_requested: false,
            started_at: Instant::now(),
            live_staging: None,
        })
    }

    pub fn push_file(&mut self, drag_id: u64, file: FilePayload) -> bool {
        if self.drag_id != drag_id
            || self.live_staging.is_some()
            || self.received.len() >= self.expected_count
        {
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

    pub fn all_files_received(&self) -> bool {
        self.live_staging.is_some()
            || (self.received.len() >= self.expected_count && !self.received.is_empty())
    }

    pub fn is_ready_to_commit(&self) -> bool {
        self.drop_requested && self.all_files_received()
    }

    pub fn into_files(self) -> Vec<FilePayload> {
        self.received
    }

    /// If all payloads have arrived while the drag is still in flight and the OS backend
    /// supports live OS drags, stages the files to disk once and invokes
    /// [`DndBackend::on_incoming_drag_files_ready`] off the Tokio executor.
    pub fn maybe_start_live_staging(
        &mut self,
        receive_dir_override: Option<PathBuf>,
        configured_receive_dir: Option<String>,
        dnd: Arc<dyn DndBackend>,
    ) {
        if self.drop_requested
            || self.live_staging.is_some()
            || self.received.len() < self.expected_count
            || self.received.is_empty()
            || !dnd.supports_live_os_drag()
        {
            return;
        }
        let files = std::mem::take(&mut self.received);
        let staging = spawn_stage_live_incoming_drag(
            self.drag_id,
            files,
            receive_dir_override,
            configured_receive_dir,
            dnd,
        );
        self.live_staging = Some(staging);
    }

    /// Commits the drag (either finalizing a live-staged OS drag or saving buffered payloads
    /// and running post-release drop delivery).
    pub fn commit(
        self,
        receive_dir_override: Option<PathBuf>,
        configured_receive_dir: Option<String>,
        dnd: Arc<dyn DndBackend>,
        runtime: Option<RuntimeState>,
    ) {
        if let Some(staging) = self.live_staging {
            spawn_commit_live_incoming_drag(self.drag_id, staging, dnd, runtime);
        } else {
            spawn_commit_incoming_drag(
                self.drag_id,
                self.received,
                receive_dir_override,
                configured_receive_dir,
                dnd,
                runtime,
            );
        }
    }

    /// Cancels an uncommitted drag, hiding any floating preview / live OS drag session
    /// and deleting any uncommitted live-staged files from disk.
    pub fn cancel(self, dnd: Arc<dyn DndBackend>) {
        dnd.on_incoming_drag_cancel();
        if let Some(staging) = self.live_staging {
            spawn_cancel_live_incoming_drag(self.drag_id, staging);
        }
    }
}

fn save_drag_files_to_dir(
    drag_id: u64,
    files: Vec<FilePayload>,
    receive_dir_override: Option<PathBuf>,
    configured_receive_dir: Option<String>,
) -> Vec<(String, usize, PathBuf)> {
    let dir = match receive_dir_override {
        Some(d) => d,
        None => match resolve_receive_dir(configured_receive_dir.as_deref()) {
            Ok(d) => d,
            Err(e) => {
                warn!(drag_id, error = %e, "Failed resolving file receive directory for drop");
                return Vec::new();
            }
        },
    };

    let mut saved_entries = Vec::with_capacity(files.len());
    for file in files {
        let safe_name = sanitize_filename(&file.filename);
        let byte_len = file.data.len();
        match save_received_file(&dir, &safe_name, &file.data) {
            Ok(saved) => {
                saved_entries.push((safe_name, byte_len, saved));
            }
            Err(e) => {
                warn!(drag_id, file = %safe_name, error = %e, "Failed saving dragged file");
            }
        }
    }
    saved_entries
}

fn remove_uncommitted_staged_files(drag_id: u64, entries: &[(String, usize, PathBuf)]) {
    for (_, _, path) in entries {
        match std::fs::remove_file(path) {
            Ok(()) => {
                debug!(
                    drag_id,
                    path = %path.display(),
                    "Removed uncommitted staged drag file after cancel"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                debug!(
                    drag_id,
                    path = %path.display(),
                    error = %e,
                    "Could not remove uncommitted staged drag file"
                );
            }
        }
    }
}

fn spawn_stage_live_incoming_drag(
    drag_id: u64,
    files: Vec<FilePayload>,
    receive_dir_override: Option<PathBuf>,
    configured_receive_dir: Option<String>,
    dnd: Arc<dyn DndBackend>,
) -> LiveDragStaging {
    let staging = LiveDragStaging::new();
    let stage_clone = staging.clone();
    tokio::task::spawn_blocking(move || {
        let entries = save_drag_files_to_dir(
            drag_id,
            files,
            receive_dir_override,
            configured_receive_dir,
        );
        let (lock, cvar) = &*stage_clone.inner;
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if guard.cancelled {
            drop(guard);
            remove_uncommitted_staged_files(drag_id, &entries);
            return;
        }
        let paths_for_live = if !guard.committed && !entries.is_empty() {
            Some(entries.iter().map(|(_, _, p)| p.clone()).collect::<Vec<_>>())
        } else {
            None
        };
        guard.saved = Some(entries);
        cvar.notify_all();
        drop(guard);

        if let Some(paths) = paths_for_live {
            debug!(
                drag_id,
                count = paths.len(),
                "Staged incoming drag files early; starting live OS drag session"
            );
            dnd.on_incoming_drag_files_ready(paths);
        }
    });
    staging
}

fn spawn_commit_live_incoming_drag(
    drag_id: u64,
    staging: LiveDragStaging,
    dnd: Arc<dyn DndBackend>,
    runtime: Option<RuntimeState>,
) {
    tokio::task::spawn_blocking(move || {
        let (lock, cvar) = &*staging.inner;
        let entries = {
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            guard.committed = true;
            while guard.saved.is_none() && !guard.cancelled {
                guard = cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
            guard.saved.take().unwrap_or_default()
        };

        let mut saved_paths = Vec::with_capacity(entries.len());
        for (safe_name, byte_len, saved) in entries {
            info!(
                drag_id,
                file = %safe_name,
                path = %saved.display(),
                bytes = byte_len,
                "Saved cross-edge dragged file"
            );
            if let Some(ref rt) = runtime {
                rt.record_file("received", safe_name, byte_len, Some(saved.clone()));
            }
            saved_paths.push(saved);
        }

        if !saved_paths.is_empty() {
            match dnd.deliver_drop(saved_paths) {
                Ok(outcome) => {
                    debug!(drag_id, ?outcome, "Completed OS drop handoff for live cross-edge drag");
                }
                Err(e) => {
                    warn!(drag_id, error = %e, "OS drop handoff failed (files remain in receive folder)");
                }
            }
        }
    });
}

fn spawn_cancel_live_incoming_drag(drag_id: u64, staging: LiveDragStaging) {
    tokio::task::spawn_blocking(move || {
        let (lock, cvar) = &*staging.inner;
        let to_remove = {
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            guard.cancelled = true;
            cvar.notify_all();
            if !guard.committed {
                guard.saved.take()
            } else {
                None
            }
        };
        if let Some(entries) = to_remove {
            remove_uncommitted_staged_files(drag_id, &entries);
        }
    });
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
        let entries = save_drag_files_to_dir(
            drag_id,
            files,
            receive_dir_override,
            configured_receive_dir,
        );

        let mut saved_paths = Vec::with_capacity(entries.len());
        for (safe_name, byte_len, saved) in entries {
            info!(
                drag_id,
                file = %safe_name,
                path = %saved.display(),
                bytes = byte_len,
                "Saved cross-edge dragged file"
            );
            if let Some(ref rt) = runtime {
                rt.record_file("received", safe_name, byte_len, Some(saved.clone()));
            }
            saved_paths.push(saved);
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
    use glide_backend::dnd::DropDeliveryOutcome;
    use glide_proto::Edge;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrd};

    #[derive(Default)]
    struct MockLiveDnd {
        ready_paths: std::sync::Mutex<Vec<PathBuf>>,
        delivered_paths: std::sync::Mutex<Vec<PathBuf>>,
        cancelled: AtomicBool,
    }

    impl DndBackend for MockLiveDnd {
        fn set_enabled(&self, _enabled: bool) {}
        fn set_active_edges(&self, _edges: &[Edge], _dead_corner_px: i32) {}
        fn set_drag_armed(&self, _armed: bool) {}
        fn take_staged_files(&self, _edge: Edge) -> Option<Vec<PathBuf>> {
            None
        }
        fn cancel_local_drag(&self) {}
        fn supports_live_os_drag(&self) -> bool {
            true
        }
        fn on_incoming_drag_files_ready(&self, paths: Vec<PathBuf>) {
            *self.ready_paths.lock().unwrap() = paths;
        }
        fn on_incoming_drag_cancel(&self) {
            self.cancelled.store(true, AtomicOrd::SeqCst);
        }
        fn deliver_drop(&self, paths: Vec<PathBuf>) -> anyhow::Result<DropDeliveryOutcome> {
            *self.delivered_paths.lock().unwrap() = paths;
            Ok(DropDeliveryOutcome::DroppedIntoTarget)
        }
    }

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

    #[tokio::test]
    async fn live_drag_staging_commits_once_and_cleans_up_on_cancel() {
        let dir = std::env::temp_dir().join(format!(
            "glide-live-drag-test-{}-{}",
            std::process::id(),
            next_drag_id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1. Live stage -> commit writes once and delivers the exact staged path
        let mock = Arc::new(MockLiveDnd::default());
        let mut inc = IncomingDragState::start(
            100,
            vec![DragFileInfo {
                filename: "report.txt".into(),
                byte_len: 5,
            }],
            1024,
        )
        .unwrap();
        assert!(inc.push_file(
            100,
            FilePayload {
                filename: "report.txt".into(),
                sha256: [0u8; 32],
                data: Bytes::from_static(b"hello"),
            },
        ));
        inc.maybe_start_live_staging(Some(dir.clone()), None, mock.clone());

        // Wait briefly for blocking save to trigger on_incoming_drag_files_ready
        for _ in 0..40 {
            if !mock.ready_paths.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let staged = mock.ready_paths.lock().unwrap().clone();
        assert_eq!(staged.len(), 1);
        assert!(staged[0].exists());

        inc.request_drop(100);
        assert!(inc.is_ready_to_commit());
        inc.commit(Some(dir.clone()), None, mock.clone(), None);
        for _ in 0..40 {
            if !mock.delivered_paths.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let delivered = mock.delivered_paths.lock().unwrap().clone();
        assert_eq!(delivered, staged, "commit must reuse staged file without duplicating");
        let entries_after_commit: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries_after_commit.len(), 1, "must not duplicate staged file on disk");

        // 2. Live stage -> cancel deletes uncommitted file cleanly (zero orphans!)
        let mock2 = Arc::new(MockLiveDnd::default());
        let mut inc2 = IncomingDragState::start(
            101,
            vec![DragFileInfo {
                filename: "cancelled.txt".into(),
                byte_len: 6,
            }],
            1024,
        )
        .unwrap();
        assert!(inc2.push_file(
            101,
            FilePayload {
                filename: "cancelled.txt".into(),
                sha256: [0u8; 32],
                data: Bytes::from_static(b"cancel"),
            },
        ));
        inc2.maybe_start_live_staging(Some(dir.clone()), None, mock2.clone());
        for _ in 0..40 {
            if !mock2.ready_paths.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let staged2 = mock2.ready_paths.lock().unwrap().clone();
        assert_eq!(staged2.len(), 1);
        assert!(staged2[0].exists());

        inc2.cancel(mock2.clone());
        assert!(mock2.cancelled.load(AtomicOrd::SeqCst));
        for _ in 0..40 {
            if !staged2[0].exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !staged2[0].exists(),
            "cancelled live-staged file must be deleted cleanly"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
