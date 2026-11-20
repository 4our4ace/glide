//! Event-driven Windows 11 clipboard backend using `AddClipboardFormatListener`
//! (`WM_CLIPBOARDUPDATE`, zero polling).
//!
//! Supports UTF-8 text (via `CF_UNICODETEXT` with `\r\n` <-> `\n` normalization)
//! and PNG images (`"PNG"` clipboard format) with SHA-256 anti-loop protection.

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
    TranslateMessage, HWND_MESSAGE, MSG, WM_APP, WM_CLIPBOARDUPDATE, WNDCLASSW,
};

const CF_UNICODETEXT: u32 = 13;
const WM_APP_SET_CLIPBOARD: u32 = WM_APP + 10;

struct ClipState {
    tx: mpsc::UnboundedSender<ClipboardPayload>,
    pending_write: Mutex<Option<ClipboardPayload>>,
    last_remote_hash: Mutex<Option<[u8; 32]>>,
    png_format: u32,
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

fn read_clipboard_payload(hwnd: HWND, png_format: u32) -> Option<(ClipboardFormat, Vec<u8>)> {
    if !open_clipboard_with_retry(hwnd) {
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
                // Normalize CRLF -> LF for clean Linux/macOS pasting
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
    } else if png_format != 0 && unsafe { IsClipboardFormatAvailable(png_format) } != 0 {
        let handle = unsafe { GetClipboardData(png_format) };
        if !handle.is_null() {
            let size = unsafe { GlobalSize(handle) };
            if size > 0 && size <= MAX_CLIPBOARD_BYTES {
                let ptr = unsafe { GlobalLock(handle) } as *const u8;
                if !ptr.is_null() {
                    let data = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
                    unsafe {
                        GlobalUnlock(handle);
                    }
                    Some((ClipboardFormat::ImagePng, data))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    unsafe {
        CloseClipboard();
    }
    result
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
                // Convert LF -> CRLF for native Windows apps (Notepad, etc.)
                let crlf = s.replace("\r\n", "\n").replace('\n', "\r\n");
                let mut wide: Vec<u16> = crlf.encode_utf16().collect();
                wide.push(0);
                let byte_len = wide.len() * 2;
                let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
                if !hmem.is_null() {
                    let dst = unsafe { GlobalLock(hmem) } as *mut u16;
                    if !dst.is_null() {
                        unsafe {
                            ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                            GlobalUnlock(hmem);
                            SetClipboardData(CF_UNICODETEXT, hmem as HANDLE);
                        }
                    }
                }
            }
        }
        ClipboardFormat::ImagePng => {
            if png_format != 0 && !payload.data.is_empty() {
                let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, payload.data.len()) };
                if !hmem.is_null() {
                    let dst = unsafe { GlobalLock(hmem) } as *mut u8;
                    if !dst.is_null() {
                        unsafe {
                            ptr::copy_nonoverlapping(
                                payload.data.as_ptr(),
                                dst,
                                payload.data.len(),
                            );
                            GlobalUnlock(hmem);
                            SetClipboardData(png_format, hmem as HANDLE);
                        }
                    }
                }
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
                if let Some((format, data)) = read_clipboard_payload(hwnd, state.png_format) {
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

    let (tx, rx) = mpsc::unbounded_channel();
    let state = Arc::new(ClipState {
        tx,
        pending_write: Mutex::new(None),
        last_remote_hash: Mutex::new(None),
        png_format,
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
