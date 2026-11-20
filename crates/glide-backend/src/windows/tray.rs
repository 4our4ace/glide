//! Native Windows 11 System Tray icon powered directly by `Shell_NotifyIconW`
//! (zero external dependencies or DLLs).

use crate::icon::render_bgra;
pub use crate::TrayAction;
use parking_lot::Mutex;
use std::mem::size_of;
use std::ptr;
use std::sync::OnceLock;
use std::thread;
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, ICONINFO,
    IDI_APPLICATION, MF_SEPARATOR, MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_APP,
    WM_COMMAND, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
};

const WM_TRAY_ICON: u32 = WM_APP + 50;
const MENU_OPEN_UI: usize = 1001;
const MENU_TOGGLE_LOCK: usize = 1002;
const MENU_QUIT: usize = 1003;

/// Builds a crisp 32x32 ARGB HICON from our pure-Rust Glide icon renderer.
unsafe fn create_glide_hicon() -> HICON {
    let size = 32i32;
    let bgra = render_bgra(size as u32);
    let mut bi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = size;
    bi.bmiHeader.biHeight = -size; // top-down DIB
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;

    let mut bits: *mut std::ffi::c_void = ptr::null_mut();
    let hbm_color = unsafe {
        CreateDIBSection(
            ptr::null_mut(),
            &bi,
            DIB_RGB_COLORS,
            &mut bits,
            ptr::null_mut(),
            0,
        )
    };
    if hbm_color.is_null() || bits.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
    }
    let mask_bytes = vec![0u8; ((size as usize * size as usize) / 8).max(4)];
    let hbm_mask = unsafe { CreateBitmap(size, size, 1, 1, mask_bytes.as_ptr().cast()) };

    let info = ICONINFO {
        fIcon: 1,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let hicon = unsafe { CreateIconIndirect(&info) };
    unsafe {
        DeleteObject(hbm_color);
        if !hbm_mask.is_null() {
            DeleteObject(hbm_mask);
        }
    }
    hicon
}

static TRAY_TX: OnceLock<Mutex<Option<mpsc::UnboundedSender<TrayAction>>>> = OnceLock::new();

fn emit_tray(action: TrayAction) {
    if let Some(cell) = TRAY_TX.get() {
        if let Some(tx) = cell.lock().as_ref() {
            let _ = tx.send(action);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY_ICON => {
            let ev = (lparam as u32) & 0xFFFF;
            match ev {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                    emit_tray(TrayAction::OpenUi);
                }
                WM_RBUTTONUP => {
                    let hmenu = unsafe { CreatePopupMenu() };
                    if !hmenu.is_null() {
                        let s_open = wide("Open Glide KVM");
                        let s_lock = wide("Toggle Cursor Lock (Ctrl+Alt+L)");
                        let s_quit = wide("Quit Glide");
                        unsafe {
                            AppendMenuW(hmenu, MF_STRING, MENU_OPEN_UI, s_open.as_ptr());
                            AppendMenuW(hmenu, MF_STRING, MENU_TOGGLE_LOCK, s_lock.as_ptr());
                            AppendMenuW(hmenu, MF_SEPARATOR, 0, ptr::null());
                            AppendMenuW(hmenu, MF_STRING, MENU_QUIT, s_quit.as_ptr());
                            let mut pt = POINT { x: 0, y: 0 };
                            GetCursorPos(&mut pt);
                            SetForegroundWindow(hwnd);
                            TrackPopupMenu(
                                hmenu,
                                TPM_LEFTALIGN | TPM_BOTTOMALIGN,
                                pt.x,
                                pt.y,
                                0,
                                hwnd,
                                ptr::null(),
                            );
                            DestroyMenu(hmenu);
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_COMMAND => {
            let cmd = wparam & 0xFFFF;
            match cmd {
                MENU_OPEN_UI => emit_tray(TrayAction::OpenUi),
                MENU_TOGGLE_LOCK => emit_tray(TrayAction::ToggleLock),
                MENU_QUIT => {
                    emit_tray(TrayAction::Quit);
                    unsafe {
                        PostQuitMessage(0);
                    }
                }
                _ => {}
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Spawns the native Windows 11 taskbar system tray icon on a background thread.
pub fn spawn_windows_tray() -> mpsc::UnboundedReceiver<TrayAction> {
    let (tx, rx) = mpsc::unbounded_channel();
    let cell = TRAY_TX.get_or_init(|| Mutex::new(None));
    *cell.lock() = Some(tx);

    let _ = thread::Builder::new()
        .name("glide-win-tray".into())
        .spawn(move || {
            let class_name = wide("GlideSystemTrayWindow");
            let hinstance = unsafe { GetModuleHandleW(ptr::null()) };
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(tray_wnd_proc),
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
                    ptr::null_mut(),
                    ptr::null_mut(),
                    hinstance,
                    ptr::null(),
                )
            };
            if hwnd.is_null() {
                return;
            }

            let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_TRAY_ICON;
            let custom_icon = unsafe { create_glide_hicon() };
            nid.hIcon = if !custom_icon.is_null() {
                custom_icon
            } else {
                unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) }
            };

            let tip = "Glide Software KVM — Click to Open";
            for (dst, src) in nid.szTip.iter_mut().zip(tip.encode_utf16()) {
                *dst = src;
            }

            unsafe {
                Shell_NotifyIconW(NIM_ADD, &nid);
            }

            let mut msg: MSG = unsafe { std::mem::zeroed() };
            while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &nid);
                if !custom_icon.is_null() {
                    DestroyIcon(custom_icon);
                }
            }
        });

    rx
}
