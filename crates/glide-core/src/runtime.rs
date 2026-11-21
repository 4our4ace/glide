//! Shared live runtime telemetry, clipboard history, file transfer log,
//! and hot-reload control channel connecting `glide-core` to `glide-gui` and CLI.

use crate::config::KeyRemapRule;
use bytes::Bytes;
use glide_backend::geometry::DesktopGeometry;
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
    pub fingerprint: Option<String>,
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
            fingerprint: None,
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
        clipboard: bool,
    },
    UpdateClientTuning {
        sensitivity: f32,
        scroll_speed: f32,
        invert_scroll: bool,
        auto_dpi_scale: bool,
        edge_threshold_px: i32,
        dead_corner_px: i32,
        sync_lock_keys: bool,
        clipboard: bool,
        key_remap: Vec<KeyRemapRule>,
    },
    ToggleLock,
    SwitchToEdge(Edge),
    ReturnLocal,
    MovePeerEdge {
        peer_name: Option<String>,
        edge: Edge,
    },
    SendFile {
        filename: String,
        data: Bytes,
    },
    ResendClipboard(usize),
    Shutdown,
}

/// Thread-safe handle for updating and subscribing to live runtime telemetry.
#[derive(Clone)]
pub struct RuntimeState {
    inner: Arc<RwLock<StatusSnapshot>>,
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

    pub fn snapshot(&self) -> StatusSnapshot {
        self.inner.read().clone()
    }

    pub fn update<F: FnOnce(&mut StatusSnapshot)>(&self, f: F) {
        {
            let mut guard = self.inner.write();
            f(&mut guard);
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
            ClipboardFormat::TextUtf8 => {
                let s = String::from_utf8_lossy(&payload.data);
                let single = s.replace(['\r', '\n'], " ");
                if single.chars().count() > 72 {
                    let cut: String = single.chars().take(72).collect();
                    format!("{cut}…")
                } else {
                    single
                }
            }
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
            ring.iter().cloned().collect()
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
}
