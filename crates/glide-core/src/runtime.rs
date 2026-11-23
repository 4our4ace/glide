//! Shared live runtime telemetry, clipboard history, file transfer log,
//! and hot-reload control channel connecting `glide-core` to `glide-gui` and CLI.

use crate::config::KeyRemapRule;
use bytes::Bytes;
use glide_backend::geometry::DesktopGeometry;
use glide_backend::CaptureHotkeys;
use glide_proto::{ClipboardFormat, ClipboardPayload, Edge, OsKind};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSummary {
    pub name: String,
    pub os: OsKind,
    pub edge: Edge,
    pub addr: String,
    pub rtt_ms: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardHistoryEntry {
    pub id: usize,
    pub format: ClipboardFormat,
    pub preview: String,
    pub bytes: usize,
    pub direction: String,
    #[serde(skip)]
    pub payload: Option<ClipboardPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTransferEntry {
    pub filename: String,
    pub bytes: usize,
    pub direction: String,
    pub saved_path: Option<String>,
}

/// Complete live state snapshot serialized to the GUI via REST / Server-Sent Events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub mode: String,
    pub status: String,
    pub active_target: String,
    pub locked: bool,
    pub local_addr: Option<String>,
    #[serde(default)]
    pub local_ips: Vec<String>,
    pub fingerprint: Option<String>,
    /// SHA-256 fingerprint of the currently connected remote server (client mode only).
    #[serde(default)]
    pub remote_fingerprint: Option<String>,
    pub rtt_ms: Option<f32>,
    pub rtt_history: Vec<f32>,
    pub peers: Vec<PeerSummary>,
    pub geometry: DesktopGeometry,
    pub clipboard_history: Vec<ClipboardHistoryEntry>,
    pub recent_files: Vec<FileTransferEntry>,
    pub last_error: Option<String>,
}

impl Default for StatusSnapshot {
    fn default() -> Self {
        Self {
            mode: "stopped".into(),
            status: "Idle".into(),
            active_target: "Local".into(),
            locked: false,
            local_addr: None,
            local_ips: Vec::new(),
            fingerprint: None,
            remote_fingerprint: None,
            rtt_ms: None,
            rtt_history: Vec::new(),
            peers: Vec::new(),
            geometry: DesktopGeometry::default(),
            clipboard_history: Vec::new(),
            recent_files: Vec::new(),
            last_error: None,
        }
    }
}

/// Live control commands sent from the GUI / Tray into a running server or client task.
#[derive(Debug, Clone)]
pub enum RuntimeCommand {
    UpdateServerTuning {
        edge_threshold_px: i32,
        dead_corner_px: i32,
        edge_dwell_ms: u64,
        auto_lock_fullscreen: bool,
        locked: bool,
        sync_lock_keys: bool,
        pin_media_keys_local: bool,
        unfocus_on_leave: bool,
        clipboard: bool,
        drag_drop_files: bool,
        hotkeys: CaptureHotkeys,
    },
    SetHotkeysSuspended(bool),
    UpdateClientTuning {
        sensitivity: f32,
        scroll_speed: f32,
        invert_scroll: bool,
        auto_dpi_scale: bool,
        edge_threshold_px: i32,
        dead_corner_px: i32,
        sync_lock_keys: bool,
        unfocus_on_leave: bool,
        clipboard: bool,
        drag_drop_files: bool,
        key_remap: Vec<KeyRemapRule>,
    },
    ToggleLock,
    SetLock(bool),
    SwitchToEdge(Edge),
    ReturnLocal,
    SwitchTarget(Option<Edge>),
    MovePeerEdge {
        peer_name: Option<String>,
        edge: Edge,
    },
    SendFile {
        filename: String,
        data: Bytes,
    },
    UpdateMaxFileBytes(usize),
    UpdateReceiveDir(Option<String>),
    ResendClipboard(usize),
    Shutdown,
}

/// Thread-safe handle for updating and subscribing to live runtime telemetry.
#[derive(Clone)]
pub struct RuntimeState {
    inner: Arc<RwLock<StatusSnapshot>>,
    revision: Arc<std::sync::atomic::AtomicU64>,
    clip_ring: Arc<RwLock<VecDeque<ClipboardHistoryEntry>>>,
    next_clip_id: Arc<std::sync::atomic::AtomicUsize>,
    record_history: Arc<std::sync::atomic::AtomicBool>,
    updates_tx: broadcast::Sender<()>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeState {
    pub fn new() -> Self {
        let (updates_tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(RwLock::new(StatusSnapshot::default())),
            revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            clip_ring: Arc::new(RwLock::new(VecDeque::with_capacity(8))),
            next_clip_id: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
            record_history: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            updates_tx,
        }
    }

    pub fn set_record_history(&self, enabled: bool) {
        self.record_history
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        if !enabled {
            self.clear_clipboard_history();
        }
    }

    pub fn clear_clipboard_history(&self) {
        self.clip_ring.write().clear();
        self.update(|s| {
            s.clipboard_history.clear();
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.updates_tx.subscribe()
    }

    /// Monotonically increasing counter incremented on every [`Self::update`].
    pub fn revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Inspects the current [`StatusSnapshot`] under a shared read lock without cloning it.
    pub fn read<R>(&self, f: impl FnOnce(&StatusSnapshot) -> R) -> R {
        f(&self.inner.read())
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        self.inner.read().clone()
    }

    /// Atomically returns the current `(revision, StatusSnapshot)` pair under the read lock.
    pub fn snapshot_with_rev(&self) -> (u64, StatusSnapshot) {
        let guard = self.inner.read();
        let rev = self.revision.load(std::sync::atomic::Ordering::Acquire);
        (rev, guard.clone())
    }

    pub fn update<F: FnOnce(&mut StatusSnapshot)>(&self, f: F) {
        {
            let mut guard = self.inner.write();
            f(&mut guard);
            self.revision
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        let _ = self.updates_tx.send(());
    }

    pub fn record_rtt(&self, rtt_ms: f32) {
        self.update(|s| {
            s.rtt_ms = Some(rtt_ms);
            s.rtt_history.push(rtt_ms);
            if s.rtt_history.len() > 24 {
                s.rtt_history.remove(0);
            }
        });
    }

    /// Atomically updates RTT history (when `Some`) together with peer summaries, active target,
    /// lock state, and status in a single write lock and broadcast notification.
    pub fn record_rtt_and_peers(
        &self,
        rtt_ms: Option<f32>,
        peers: Vec<PeerSummary>,
        active_target: String,
        locked: bool,
        status: String,
    ) {
        self.update(|s| {
            if let Some(ms) = rtt_ms {
                s.rtt_ms = Some(ms);
                s.rtt_history.push(ms);
                if s.rtt_history.len() > 24 {
                    s.rtt_history.remove(0);
                }
            }
            s.peers = peers;
            s.active_target = active_target;
            s.locked = locked;
            s.status = status;
        });
    }

    pub fn record_clipboard(&self, direction: &str, payload: &ClipboardPayload) {
        if !self
            .record_history
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let id = self
            .next_clip_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let preview = match payload.format {
            ClipboardFormat::TextUtf8 => make_text_preview(&payload.data),
            ClipboardFormat::ImagePng => {
                format!("PNG Image ({:.1} KB)", payload.data.len() as f64 / 1024.0)
            }
        };
        let entry = ClipboardHistoryEntry {
            id,
            format: payload.format,
            preview,
            bytes: payload.data.len(),
            direction: direction.to_string(),
            payload: Some(payload.clone()),
        };
        let snapshot_list: Vec<ClipboardHistoryEntry> = {
            let mut ring = self.clip_ring.write();
            ring.push_front(entry);
            while ring.len() > 8 {
                ring.pop_back();
            }
            // Keep heavy clipboard payload buffers solely in `clip_ring` (used by
            // `find_clipboard_payload`) so UI snapshots — including paused tray snapshots —
            // never pin evicted screenshots or large text buffers in memory.
            ring.iter()
                .map(|e| ClipboardHistoryEntry {
                    id: e.id,
                    format: e.format,
                    preview: e.preview.clone(),
                    bytes: e.bytes,
                    direction: e.direction.clone(),
                    payload: None,
                })
                .collect()
        };
        self.update(|s| {
            s.clipboard_history = snapshot_list;
        });
    }

    pub fn find_clipboard_payload(&self, id: usize) -> Option<ClipboardPayload> {
        self.clip_ring
            .read()
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.payload.clone())
    }

    pub fn record_file(
        &self,
        direction: &str,
        filename: String,
        bytes: usize,
        saved: Option<PathBuf>,
    ) {
        self.update(|s| {
            s.recent_files.insert(
                0,
                FileTransferEntry {
                    filename,
                    bytes,
                    direction: direction.to_string(),
                    saved_path: saved.map(|p| p.display().to_string()),
                },
            );
            s.recent_files.truncate(8);
        });
    }
}

/// Builds a single-line 72-character clipboard text preview without decoding or copying
/// multi-megabyte clipboard payloads.
fn make_text_preview(data: &[u8]) -> String {
    const MAX_PREVIEW_CHARS: usize = 72;
    // 74 * 4 bytes guarantees at least 73 complete UTF-8 scalar values even after backing
    // up to 3 bytes to avoid splitting a trailing multi-byte sequence.
    const MAX_PREFIX_BYTES: usize = (MAX_PREVIEW_CHARS + 2) * 4;

    let slice = if data.len() > MAX_PREFIX_BYTES {
        let floor = MAX_PREFIX_BYTES - 3;
        let mut end = MAX_PREFIX_BYTES;
        while end > floor && (data[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
        &data[..end]
    } else {
        data
    };

    let text = String::from_utf8_lossy(slice);
    let mut out = String::with_capacity(text.len().min(MAX_PREVIEW_CHARS * 4 + 3));
    for (chars, ch) in text.chars().enumerate() {
        if chars == MAX_PREVIEW_CHARS {
            out.push('…');
            return out;
        }
        match ch {
            '\r' | '\n' => out.push(' '),
            other => out.push(other),
        }
    }
    if data.len() > slice.len() {
        out.push('…');
    }
    out
}

/// Maximum collision suffix attempts (`file (1).ext` .. `file (1000).ext`) before failing.
const MAX_SAVE_COLLISIONS: u32 = 1000;

/// Safely saves a received file into `dir` without following symlinks or overwriting existing files.
pub fn save_received_file(
    dir: &std::path::Path,
    safe_filename: &str,
    data: &[u8],
) -> anyhow::Result<PathBuf> {
    use std::fs::OpenOptions;
    use std::io::{ErrorKind, Write};

    // Defense-in-depth: re-sanitize even if caller passed an already sanitized filename.
    let safe_name = glide_proto::sanitize_filename(safe_filename);

    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                anyhow::bail!("refusing to save file into symlink directory {}", dir.display());
            }
            if !meta.is_dir() {
                anyhow::bail!("receive path {} is not a directory", dir.display());
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            let meta = std::fs::symlink_metadata(dir)?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                anyhow::bail!(
                    "receive path {} is a symlink or not a directory",
                    dir.display()
                );
            }
        }
        Err(e) => return Err(e.into()),
    }

    let base_path = dir.join(&safe_name);
    let stem = base_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let ext = base_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();

    for idx in 0..=MAX_SAVE_COLLISIONS {
        let candidate = if idx == 0 {
            base_path.clone()
        } else {
            dir.join(format!("{stem} ({idx}){ext}"))
        };

        if let Ok(meta) = std::fs::symlink_metadata(&candidate) {
            if meta.file_type().is_symlink() {
                anyhow::bail!(
                    "refusing to write through symlink target {}",
                    candidate.display()
                );
            }
        }

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(data) {
                    let _ = std::fs::remove_file(&candidate);
                    return Err(e.into());
                }
                return Ok(candidate);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if let Ok(meta) = std::fs::symlink_metadata(&candidate) {
                    if meta.file_type().is_symlink() {
                        anyhow::bail!(
                            "refusing to write through symlink target {}",
                            candidate.display()
                        );
                    }
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }

    anyhow::bail!(
        "failed to allocate a unique filename for '{}' after {} attempts",
        safe_name,
        MAX_SAVE_COLLISIONS
    )
}

/// Reads a regular file from disk while enforcing `max_bytes` both before opening and during the
/// actual bounded read (rejecting directories, FIFOs, devices, and files that grow past the cap).
pub fn read_regular_file_bounded(
    path: &std::path::Path,
    max_bytes: usize,
) -> anyhow::Result<(String, Bytes)> {
    use anyhow::{anyhow, Context};
    use std::io::Read;

    if path.as_os_str().is_empty() {
        anyhow::bail!("file path cannot be empty");
    }
    let meta = std::fs::metadata(path)
        .with_context(|| format!("failed to inspect file '{}'", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("'{}' is not a regular file", path.display());
    }
    if meta.len() > max_bytes as u64 {
        anyhow::bail!(
            "file '{}' ({} bytes) exceeds max file transfer limit ({} bytes)",
            path.display(),
            meta.len(),
            max_bytes
        );
    }

    let file = std::fs::File::open(path)
        .with_context(|| format!("failed to open file '{}'", path.display()))?;
    let opened_meta = file.metadata()?;
    if !opened_meta.is_file() {
        anyhow::bail!("'{}' is not a regular file", path.display());
    }
    if opened_meta.len() > max_bytes as u64 {
        anyhow::bail!(
            "file '{}' ({} bytes) exceeds max file transfer limit ({} bytes)",
            path.display(),
            opened_meta.len(),
            max_bytes
        );
    }

    let initial_cap = opened_meta
        .len()
        .min(max_bytes as u64)
        .min(64 * 1024) as usize;
    let mut data = Vec::with_capacity(initial_cap);
    file.take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut data)
        .with_context(|| format!("failed to read file '{}'", path.display()))?;
    if data.len() > max_bytes {
        anyhow::bail!(
            "file '{}' grew beyond max file transfer limit ({} bytes)",
            path.display(),
            max_bytes
        );
    }

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| anyhow!("cannot determine filename for '{}'", path.display()))?
        .to_owned();

    Ok((filename, Bytes::from(data)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "glide-save-test-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_received_file_creates_unique_names_without_overwriting() {
        let dir = unique_temp_dir("collisions");
        let p0 = save_received_file(&dir, "notes.txt", b"first").unwrap();
        let p1 = save_received_file(&dir, "notes.txt", b"second").unwrap();
        let p2 = save_received_file(&dir, "notes.txt", b"third").unwrap();

        assert_eq!(p0.file_name().unwrap(), "notes.txt");
        assert_eq!(p1.file_name().unwrap(), "notes (1).txt");
        assert_eq!(p2.file_name().unwrap(), "notes (2).txt");
        assert_eq!(std::fs::read(&p0).unwrap(), b"first");
        assert_eq!(std::fs::read(&p1).unwrap(), b"second");
        assert_eq!(std::fs::read(&p2).unwrap(), b"third");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_received_file_errors_when_collision_budget_exhausted() {
        let dir = unique_temp_dir("exhausted");
        std::fs::write(dir.join("dup.txt"), b"0").unwrap();
        for idx in 1..=MAX_SAVE_COLLISIONS {
            std::fs::write(dir.join(format!("dup ({idx}).txt")), b"x").unwrap();
        }
        let err = save_received_file(&dir, "dup.txt", b"new").unwrap_err();
        assert!(err.to_string().contains("unique filename"));
        assert_eq!(std::fs::read(dir.join("dup.txt")).unwrap(), b"0");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_received_file_rejects_symlink_dir_and_symlink_file() {
        use std::os::unix::fs::symlink;

        let root = unique_temp_dir("symlinks");
        let real_dir = root.join("real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let link_dir = root.join("link_dir");
        symlink(&real_dir, &link_dir).unwrap();

        // Target directory itself is a symlink -> must fail.
        assert!(save_received_file(&link_dir, "ok.txt", b"data").is_err());

        // Target file inside real_dir is a symlink (even dangling) -> must fail.
        let victim = root.join("victim.txt");
        std::fs::write(&victim, b"untouched").unwrap();
        symlink(&victim, real_dir.join("payload.txt")).unwrap();
        assert!(save_received_file(&real_dir, "payload.txt", b"evil").is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");

        symlink(root.join("missing.txt"), real_dir.join("dangling.txt")).unwrap();
        assert!(save_received_file(&real_dir, "dangling.txt", b"evil").is_err());
        assert!(!root.join("missing.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn revision_and_read_track_updates() {
        let rt = RuntimeState::new();
        assert_eq!(rt.revision(), 0);
        assert!(!rt.read(|s| s.locked));

        rt.update(|s| {
            s.mode = "server".into();
            s.locked = true;
        });
        assert_eq!(rt.revision(), 1);
        assert!(rt.read(|s| s.locked && s.mode == "server"));

        let (rev, snap) = rt.snapshot_with_rev();
        assert_eq!(rev, 1);
        assert_eq!(snap.mode, "server");
        assert!(snap.locked);
    }

    #[test]
    fn record_clipboard_strips_snapshot_payload_and_bounds_preview() {
        let rt = RuntimeState::new();
        let huge = format!("line1\r\nline2\n{}", "🚀".repeat(500));
        let payload = ClipboardPayload {
            format: ClipboardFormat::TextUtf8,
            sha256: [7u8; 32],
            data: Bytes::from(huge.clone()),
        };

        rt.record_clipboard("sent", &payload);

        let snap = rt.snapshot();
        assert_eq!(snap.clipboard_history.len(), 1);
        let item = &snap.clipboard_history[0];
        assert!(item.payload.is_none());
        assert_eq!(item.bytes, huge.len());
        assert!(item.preview.starts_with("line1  line2 🚀"));
        assert!(item.preview.ends_with('…'));
        assert_eq!(item.preview.chars().count(), 73);

        // Full payload remains available in the ring for Re-send.
        let resend = rt.find_clipboard_payload(item.id).expect("stored in ring");
        assert_eq!(resend.data, payload.data);
    }

    #[test]
    fn make_text_preview_exact_and_short_cases() {
        assert_eq!(make_text_preview(b"hello\nworld"), "hello world");
        let exact_72 = "a".repeat(72);
        assert_eq!(make_text_preview(exact_72.as_bytes()), exact_72);
        let over_72 = "a".repeat(73);
        assert_eq!(
            make_text_preview(over_72.as_bytes()),
            format!("{}…", "a".repeat(72))
        );
    }

    #[test]
    fn read_regular_file_bounded_validates_type_and_size() {
        let dir = unique_temp_dir("bounded-read");

        // Directory is rejected as non-regular file
        assert!(read_regular_file_bounded(&dir, 1024).is_err());

        // File within limit succeeds
        let ok_file = dir.join("sample.txt");
        std::fs::write(&ok_file, b"hello bounded").unwrap();
        let (name, data) = read_regular_file_bounded(&ok_file, 1024).unwrap();
        assert_eq!(name, "sample.txt");
        assert_eq!(&data[..], b"hello bounded");

        // File exceeding limit is rejected
        let big_file = dir.join("too-big.bin");
        std::fs::write(&big_file, vec![0x55; 2048]).unwrap();
        let err = read_regular_file_bounded(&big_file, 1024).unwrap_err();
        assert!(err.to_string().contains("exceeds max file transfer limit"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
