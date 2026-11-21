//! Event-driven Windows 11 clipboard backend using `AddClipboardFormatListener`
//! (`WM_CLIPBOARDUPDATE`, zero polling).
//!
//! Supports:
//! - UTF-8 text via `CF_UNICODETEXT` (with `\r\n` <-> `\n` normalization)
//! - Universal images: reads `"PNG"`, `CF_DIBV5`, and `CF_DIB` (converting DIB -> PNG),
//!   and writes BOTH `"PNG"` AND `CF_DIB` (converting PNG -> DIB) so every Windows app
//!   (Snipping Tool, Paint, Word, Chrome, Discord, Slack) works seamlessly with Ubuntu.

use crate::clipboard_privacy::{
    is_windows_clipboard_sensitive, WindowsPrivacyCheck, WIN_CAN_INCLUDE_HISTORY_FORMAT,
    WIN_CLIPBOARD_VIEWER_IGNORE_FORMAT, WIN_EXCLUDE_MONITOR_FORMAT,
};
use crate::dib_png::{dib_to_png, png_to_dib};
use crate::{ClipboardBackend, ClipboardHandle};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use glide_net::sha256_bytes;
use glide_proto::{ClipboardFormat, ClipboardPayload, MAX_CLIPBOARD_BYTES};
use parking_lot::Mutex;
use std::ptr;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW, RegisterClassW,
    TranslateMessage, HWND_MESSAGE, MSG, WM_APP, WM_CLIPBOARDUPDATE, WM_QUIT, WNDCLASSW,
};

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_DIBV5: u32 = 17;
const WM_APP_SET_CLIPBOARD: u32 = WM_APP + 10;

#[derive(Clone, Copy)]
struct PrivacyFormats {
    exclude_monitor: u32,
    can_include_history: u32,
    viewer_ignore: u32,
}

impl PrivacyFormats {
    fn register() -> Self {
        let reg = |name: &str| -> u32 {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
        };
        Self {
            exclude_monitor: reg(WIN_EXCLUDE_MONITOR_FORMAT),
            can_include_history: reg(WIN_CAN_INCLUDE_HISTORY_FORMAT),
            viewer_ignore: reg(WIN_CLIPBOARD_VIEWER_IGNORE_FORMAT),
        }
    }

    /// Evaluates Windows privacy clipboard formats while the clipboard is open.
    fn is_current_clipboard_sensitive(self) -> bool {
        let is_avail = |fmt: u32| fmt != 0 && unsafe { IsClipboardFormatAvailable(fmt) } != 0;
        let has_exclude_monitor = is_avail(self.exclude_monitor);
        let has_viewer_ignore = is_avail(self.viewer_ignore);
        let can_include_raw = if is_avail(self.can_include_history) {
            Some(read_raw_format(self.can_include_history))
        } else {
            None
        };
        let check = WindowsPrivacyCheck {
            has_exclude_monitor,
            has_viewer_ignore,
            can_include_in_history: can_include_raw.as_ref().map(|opt| opt.as_deref()),
        };
        is_windows_clipboard_sensitive(check)
    }
}

struct ClipState {
    tx: mpsc::UnboundedSender<ClipboardPayload>,
    pending_write: Mutex<Option<ClipboardPayload>>,
    last_remote_hash: Mutex<Option<[u8; 32]>>,
    png_format: u32,
    privacy_formats: PrivacyFormats,
}

static GLOBAL_CLIP_STATE: OnceLock<Mutex<Option<Arc<ClipState>>>> = OnceLock::new();

fn with_clip_state<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&ClipState) -> R,
{
    let cell = GLOBAL_CLIP_STATE.get()?;
    let guard = cell.lock();
    guard.as_deref().map(f)
}

struct WindowsClipboardBackend {
    hwnd: AtomicIsize,
    state: Arc<ClipState>,
}

impl Drop for WindowsClipboardBackend {
    fn drop(&mut self) {
        let hwnd = self.hwnd.swap(0, Ordering::SeqCst) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_QUIT, 0, 0);
            }
        }
        if let Some(cell) = GLOBAL_CLIP_STATE.get() {
            *cell.lock() = None;
        }
    }
}

impl ClipboardBackend for WindowsClipboardBackend {
    fn set(&self, payload: ClipboardPayload) -> Result<()> {
        *self.state.last_remote_hash.lock() = Some(payload.sha256);
        *self.state.pending_write.lock() = Some(payload);
        let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            unsafe {
                PostMessageW(hwnd, WM_APP_SET_CLIPBOARD, 0, 0);
            }
        }
        Ok(())
    }
}

fn open_clipboard_with_retry(hwnd: HWND) -> bool {
    for _ in 0..6 {
        if unsafe { OpenClipboard(hwnd) } != 0 {
            return true;
        }
        thread::sleep(Duration::from_millis(8));
    }
    false
}

fn read_raw_format(format: u32) -> Option<Vec<u8>> {
    if format == 0 || unsafe { IsClipboardFormatAvailable(format) } == 0 {
        return None;
    }
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return None;
    }
    let size = unsafe { GlobalSize(handle) };
    if size == 0 || size > MAX_CLIPBOARD_BYTES * 2 {
        return None;
    }
    let ptr = unsafe { GlobalLock(handle) } as *const u8;
    if ptr.is_null() {
        return None;
    }
    let data = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
    unsafe {
        GlobalUnlock(handle);
    }
    Some(data)
}

fn read_clipboard_payload(
    hwnd: HWND,
    png_format: u32,
    privacy_formats: PrivacyFormats,
) -> Option<(ClipboardFormat, Vec<u8>)> {
    if !open_clipboard_with_retry(hwnd) {
        return None;
    }

    if privacy_formats.is_current_clipboard_sensitive() {
        debug!("Skipping sensitive Windows clipboard entry (password manager privacy format)");
        unsafe {
            CloseClipboard();
        }
        return None;
    }

    let result = if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } != 0 {
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if !handle.is_null() {
            let ptr = unsafe { GlobalLock(handle) } as *const u16;
            if !ptr.is_null() {
                let bytes = unsafe { GlobalSize(handle) };
                let max_u16 = bytes / 2;
                let slice = unsafe { std::slice::from_raw_parts(ptr, max_u16) };
                let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                let text = String::from_utf16_lossy(&slice[..len]);
                unsafe {
                    GlobalUnlock(handle);
                }
                let normalized = text.replace("\r\n", "\n");
                if !normalized.is_empty() && normalized.len() <= MAX_CLIPBOARD_BYTES {
                    Some((ClipboardFormat::TextUtf8, normalized.into_bytes()))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else if let Some(png_data) = read_raw_format(png_format) {
        if png_data.len() <= MAX_CLIPBOARD_BYTES {
            Some((ClipboardFormat::ImagePng, png_data))
        } else {
            None
        }
    } else if let Some(dib_data) = read_raw_format(CF_DIBV5).or_else(|| read_raw_format(CF_DIB)) {
        match dib_to_png(&dib_data) {
            Ok(png_bytes) if png_bytes.len() <= MAX_CLIPBOARD_BYTES => {
                debug!(
                    dib_len = dib_data.len(),
                    png_len = png_bytes.len(),
                    "Converted Windows CF_DIB screenshot to PNG"
                );
                Some((ClipboardFormat::ImagePng, png_bytes))
            }
            Ok(_) => None,
            Err(e) => {
                debug!(error = %e, "Could not convert CF_DIB to PNG");
                None
            }
        }
    } else {
        None
    };

    unsafe {
        CloseClipboard();
    }
    result
}

fn set_raw_clipboard_bytes(format: u32, bytes: &[u8]) {
    if format == 0 || bytes.is_empty() {
        return;
    }
    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) };
    if !hmem.is_null() {
        let dst = unsafe { GlobalLock(hmem) } as *mut u8;
        if !dst.is_null() {
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
                GlobalUnlock(hmem);
                SetClipboardData(format, hmem as HANDLE);
            }
        }
    }
}

fn write_clipboard_payload(hwnd: HWND, png_format: u32, payload: &ClipboardPayload) {
    if !open_clipboard_with_retry(hwnd) {
        warn!("Failed to open Windows clipboard for writing");
        return;
    }

    unsafe {
        EmptyClipboard();
    }

    match payload.format {
        ClipboardFormat::TextUtf8 => {
            if let Ok(s) = std::str::from_utf8(&payload.data) {
                let crlf = s.replace("\r\n", "\n").replace('\n', "\r\n");
                let mut wide: Vec<u16> = crlf.encode_utf16().collect();
                wide.push(0);
                let byte_slice = unsafe {
                    std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2)
                };
                set_raw_clipboard_bytes(CF_UNICODETEXT, byte_slice);
            }
        }
        ClipboardFormat::ImagePng => {
            // Write BOTH modern "PNG" AND legacy CF_DIB so every Windows app can paste!
            set_raw_clipboard_bytes(png_format, &payload.data);
            match png_to_dib(&payload.data) {
                Ok(dib) => set_raw_clipboard_bytes(CF_DIB, &dib),
                Err(e) => debug!(error = %e, "Could not synthesize CF_DIB companion for PNG"),
            }
        }
    }

    unsafe {
        CloseClipboard();
    }
}

unsafe extern "system" fn clip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            with_clip_state(|state| {
                if let Some((format, data)) =
                    read_clipboard_payload(hwnd, state.png_format, state.privacy_formats)
                {
                    let hash = sha256_bytes(&data);
                    if *state.last_remote_hash.lock() == Some(hash) {
                        trace!("Ignoring Windows clipboard echo matching last remote hash");
                        return;
                    }
                    *state.last_remote_hash.lock() = Some(hash);
                    debug!(
                        ?format,
                        len = data.len(),
                        "Captured local Windows clipboard update"
                    );
                    let _ = state.tx.send(ClipboardPayload {
                        format,
                        sha256: hash,
                        data: Bytes::from(data),
                    });
                }
            });
            0
        }
        WM_APP_SET_CLIPBOARD => {
            with_clip_state(|state| {
                if let Some(payload) = state.pending_write.lock().take() {
                    write_clipboard_payload(hwnd, state.png_format, &payload);
                }
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

pub fn start_windows_clipboard() -> Result<ClipboardHandle> {
    let png_name: Vec<u16> = "PNG\0".encode_utf16().collect();
    let png_format = unsafe { RegisterClipboardFormatW(png_name.as_ptr()) };
    let privacy_formats = PrivacyFormats::register();

    let (tx, rx) = mpsc::unbounded_channel();
    let state = Arc::new(ClipState {
        tx,
        pending_write: Mutex::new(None),
        last_remote_hash: Mutex::new(None),
        png_format,
        privacy_formats,
    });

    let cell = GLOBAL_CLIP_STATE.get_or_init(|| Mutex::new(None));
    *cell.lock() = Some(Arc::clone(&state));

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<isize>>();

    thread::Builder::new()
        .name("glide-win-clipboard".into())
        .spawn(move || {
            let class_name: Vec<u16> = "GlideClipboardListenerWindow\0".encode_utf16().collect();
            let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(clip_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: ptr::null_mut(),
                hCursor: ptr::null_mut(),
                hbrBackground: ptr::null_mut(),
                lpszMenuName: ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            unsafe {
                RegisterClassW(&wc);
            }
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    class_name.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    ptr::null_mut(),
                    hinstance,
                    ptr::null(),
                )
            };
            if hwnd.is_null() {
                let _ = ready_tx.send(Err(anyhow!("CreateWindowExW failed for clipboard window")));
                return;
            }
            if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
                let _ = ready_tx.send(Err(anyhow!("AddClipboardFormatListener failed")));
                return;
            }
            let _ = ready_tx.send(Ok(hwnd as isize));

            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            unsafe {
                RemoveClipboardFormatListener(hwnd);
            }
        })
        .context("failed to spawn glide-win-clipboard thread")?;

    let hwnd_val = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .context("timed out waiting for Windows clipboard thread")??;

    Ok(ClipboardHandle {
        backend: Box::new(WindowsClipboardBackend {
            hwnd: AtomicIsize::new(hwnd_val),
            state,
        }),
        local_changes: rx,
    })
}
