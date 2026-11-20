//! Native Glide app window for Windows, backed by the system WebView2 runtime.
//!
//! Launching Edge in `--app` mode with its own profile meant a full browser cold start
//! (often 1–2 s) every time the window opened. A WebView2 control hosted in our own
//! window appears in a fraction of that time. Closing the window only hides it, so
//! reopening from the tray (or by launching Glide again) is instant.
//!
//! The WebView2 loader is linked statically (`webview2-com-sys`), and the runtime ships
//! with Windows 10/11. If it is missing anyway, the caller falls back to Edge.

use std::sync::{Mutex, OnceLock};

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::EventLoopBuilderExtWindows;
use tao::window::{Icon, Theme, WindowBuilder};
use tracing::{info, warn};
use wry::{NewWindowResponse, WebContext, WebViewBuilder, WebViewBuilderExtWindows};

use glide_core::config::default_config_dir;

#[derive(Debug, Clone, Copy)]
enum UiEvent {
    Show,
}

/// Proxy into the persistent UI thread's event loop, once started.
static PROXY: OnceLock<Mutex<Option<EventLoopProxy<UiEvent>>>> = OnceLock::new();

/// Shows (creating on first use) the native Glide window.
///
/// Returns `false` if the native window is unavailable (e.g. no WebView2 runtime), in
/// which case the caller should fall back to a browser window. Never blocks on page load.
pub fn show(url: &str) -> bool {
    let cell = PROXY.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(proxy) = guard.as_ref() {
        if proxy.send_event(UiEvent::Show).is_ok() {
            return true;
        }
        // The UI thread is gone (webview creation failed): use the fallback from now on.
        return false;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let url = url.to_owned();
    let spawned = std::thread::Builder::new()
        .name("glide-ui".into())
        .spawn(move || run_ui_thread(url, tx));
    if spawned.is_err() {
        return false;
    }
    match rx.recv() {
        Ok(proxy) => {
            *guard = Some(proxy);
            true
        }
        Err(_) => false,
    }
}

fn run_ui_thread(url: String, proxy_tx: std::sync::mpsc::Sender<EventLoopProxy<UiEvent>>) {
    let event_loop = EventLoopBuilder::<UiEvent>::with_user_event()
        .with_any_thread(true)
        .build();
    // Hand the proxy back right away; `show` requests queue up until the loop runs.
    let _ = proxy_tx.send(event_loop.create_proxy());
    drop(proxy_tx);

    let icon = Icon::from_rgba(glide_backend::icon::render_rgba(64), 64, 64).ok();
    let window = match WindowBuilder::new()
        .with_title("Glide")
        .with_inner_size(LogicalSize::new(
            f64::from(crate::GUI_WIDTH),
            f64::from(crate::GUI_HEIGHT),
        ))
        .with_resizable(false)
        .with_maximizable(false)
        .with_window_icon(icon)
        .with_visible(false)
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "Native window unavailable; falling back to Edge");
            let _ = crate::launch_browser_window(&url);
            return;
        }
    };

    // Paint the page background colour before the first frame to avoid a white flash.
    let background = match window.theme() {
        Theme::Dark => (28, 28, 31, 255),
        _ => (255, 255, 255, 255),
    };

    let data_dir = default_config_dir().join("webview2");
    let _ = std::fs::create_dir_all(&data_dir);
    let mut context = WebContext::new(Some(data_dir));
    let origin = url.trim_end_matches('/').to_owned();

    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(&url)
        .with_background_color(background)
        .with_devtools(false)
        .with_hotkeys_zoom(false)
        .with_focused(true)
        .with_browser_accelerator_keys(false)
        .with_default_context_menus(false)
        .with_navigation_handler(move |target| target.starts_with(&origin))
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .build(&window);

    let webview = match webview {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "WebView2 unavailable; falling back to Edge");
            drop(window);
            let _ = crate::launch_browser_window(&url);
            return;
        }
    };

    window.set_visible(true);
    window.set_focus();
    info!("Native Glide window ready");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Keep the webview (and its web context) alive for the life of the process.
        let _ = &webview;
        let _ = &context;
        match event {
            Event::UserEvent(UiEvent::Show) => {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus();
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // Hide instead of destroying: Glide keeps running in the tray and
                // reopening the window is instant.
                window.set_visible(false);
            }
            _ => {}
        }
    });
}
