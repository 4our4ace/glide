# Glide Roadmap & TO-DO

Long-horizon engineering plan to make **Glide** a genuinely native, zero-lag, zero-tech-debt Deskflow replacement — prioritizing **Windows 11 (Server) ↔ Ubuntu 22.04 X11 (Client)** first, then expanding cleanly to Wayland and macOS.

---

## Completed Foundation (v0.1.0 + v0.2.0)

- [x] 3-lane QUIC / TLS 1.3 transport (`glide-net`: unreliable datagrams for motion/scroll, reliable control stream for keys/buttons/transitions, independent uni streams for clipboard & files)
- [x] Persistent Ed25519 TLS identity + SHA-256 pinning & automatic TOFU
- [x] LocalSend-style UDP multicast + broadcast LAN auto-discovery with real hostnames
- [x] Strictly bijective USB HID Usage scancode table (`glide-hid`) with German ISO (`< > |`, `#/'`, dead keys, umlauts) and Windows fake-`LCtrl` (`0x21D`) `AltGr` filter
- [x] Multi-monitor & mixed-HiDPI normalized edge geometry (`Per-Monitor DPI Awareness V2` on Windows, XRandR on X11)
- [x] Bidirectional event-driven clipboard (UTF-8 text with `\r\n` ↔ `\n` normalization + universal PNG/`CF_DIB`/`CF_DIBV5` screenshots, SHA-256 anti-loop, lazy transfer > 256 KB)
- [x] Static Linux musl binary + `.deb` package (`glide_0.2.0_amd64.deb`) and standalone static-CRT Windows binary (`glide-windows-x86_64.exe`)

---

## Completed Daily-Driver Milestones (Steps 1–7 & 10)

### 1. Native Desktop GUI + System Tray (`glide-gui`)
- [x] Decoupled pure-Rust `glide-gui` crate sharing `glide-core` (headless CLI/daemon intact; zero C GUI deps)
- [x] Interactive visual monitor & edge layout editor showing real detected monitors, resolutions, and DPI %
- [x] Live LocalSend-style discovered LAN devices list with one-click connect & automatic fingerprint pinning
- [x] Native Windows 11 System Tray (`Shell_NotifyIconW`) + frameless desktop app window (`--app` mode) on Windows 11 & Ubuntu + live sensitivity / scroll / dead-corner / dwell sliders without restarting

### 2. Seamless Windows Remote Cursor Hiding & Clean Parking
- [x] Park the Windows cursor cleanly in the invisible bottom-right corner pixel while controlling Ubuntu (no ghost cursor stuck on the monitor border)
- [x] Guaranteed cursor restoration on return, escape hotkey (`ScrollLock`), client disconnect, or process exit (`Drop`)

### 3. Universal Image Clipboard (`CF_DIB` / `CF_DIBV5` ↔ `PNG`) + Lazy Clipboard Transfers
- [x] Pure-Rust Windows `CF_DIB` / `CF_DIBV5` ↔ `PNG` converter (`glide-backend::dib_png`) so Snipping Tool (`Win+Shift+S`), Paint, browsers, and Ubuntu Flameshot paste screenshots seamlessly both ways
- [x] Lazy on-demand clipboard offers (`ClipboardOffer` / `ClipboardRequest`) for payloads > 256 KB so local large copies never waste LAN/Wi-Fi bandwidth
- [x] Live Clipboard History in GUI (last 8 items with one-click re-send)

### 4. Dead-Corner Guards, Edge Dwell & Fullscreen App Lock
- [x] Configurable corner dead-zone (24 px default) so clicking Windows' top-right `X` close button or bottom-right Show Desktop never accidentally jumps screens
- [x] Configurable edge dwell delay (ms) alongside pixel push-through threshold
- [x] Fullscreen game/app auto-lock on Windows + manual "Lock Cursor to Current Screen" (`Ctrl+Alt+L`, GUI button, Windows tray menu)

### 5. Direct Keyboard Screen-Switching Hotkeys & Per-Screen Cursor Memory
- [x] `Ctrl+Alt+Arrow` direct screen jumps (using Left Alt specifically so German `AltGr` never collides!)
- [x] Per-screen normalized cursor memory (`EnterAt` / `leave_remote_at`) so hotkey jumps restore your cursor right where you left it

### 6. Scale-Aware Pointer Feel, Scroll Tuning & Optional Linux `uinput` Native Mode
- [x] Optional `auto_dpi_scale`, live `sensitivity`, `scroll_speed`, and `invert_scroll` (natural scrolling) controls
- [x] Optional pure-Rust Linux `/dev/uinput` virtual kernel HID backend (`--backend uinput` / GUI selector) delivering 100% native `libinput` pointer acceleration & `REL_WHEEL_HI_RES` smooth scrolling, with clean fallback to XTest

### 7. Lock-Key Sync (`CapsLock` / `NumLock`) & Per-Client Key Remapping
- [x] Synchronize `CapsLock` and `NumLock` state on screen entry (`GetKeyState` on Windows, `get_keyboard_control` on X11)
- [x] Declarative per-client physical key remapping (`[[client.key_remap]]` + live GUI editor with presets)

### 10. Zero-Friction OS Integration & File Drop
- [x] Windows: one-click login autostart (`HKCU\...\Run` Registry API), one-click Windows Firewall rule helper (UDP `24800`/`24801`), and one-click Elevated Scheduled Task helper (for UAC/Admin windows)
- [x] Ubuntu `.deb`: `/usr/share/applications/glide.desktop`, crisp SVG icon, XDG autostart support, `systemd --user` units, and optional `/usr/share/doc/glide/99-glide-uinput.rules`
- [x] Instant drag-and-drop File Transfer over QUIC bulk streams (SHA-256 verified, safe filename sanitization, saved to `Downloads/Glide/`)

---

## Remaining Polish Steps (Next Up After Real-Hardware Test)

### 8. Bulletproof Sleep/Wake, Display Hotplug & Network Roaming Recovery
- [ ] Subscribe to `WM_POWERBROADCAST` / `WM_DISPLAYCHANGE` / `WTSRegisterSessionNotification` on Windows and XRandR `ScreenChangeNotify` on X11
- [ ] Instant recovery after laptop lid close/open, monitor plug/unplug, or Wi-Fi ↔ Ethernet roaming

### 9. Interactive Scancode / AltGr Live Inspector & Deep Telemetry
- [x] Live Ping/Pong RTT (ms) + sparkline in GUI
- [ ] Jitter, UDP datagram drop counter, and effective mouse Hz meter
- [ ] Interactive `glide doctor --keys` live raw scancode inspector

---

## Deferred / Later Horizons (Explicitly Skipped For Now)

### Linux Wayland Backend (`crates/glide-backend/src/linux_wayland/`)
- [ ] `xdg-desktop-portal` (`InputCapture` + `RemoteDesktop`) + `libei` / `reis` support for GNOME 46+, KDE Plasma 6.1+, and Hyprland/wlroots portals
- [ ] Wayland clipboard bridge (`wlr-data-control` / `ext-data-control`)
- [ ] Auto-detect X11 vs Wayland at runtime inside `glide-backend`

### macOS Backend (`crates/glide-backend/src/macos.rs`)
- [ ] Capture: `CGEventTap` (`kCGHIDEventTap`) + `CGAssociateMouseAndMouseCursorPosition(false)` for raw unaccelerated deltas + `IOHIDManager` for extra keys/buttons
- [ ] Injection: `CGEventPost` using existing `HidUsage::to_macos_keycode()` mappings + default `Ctrl ↔ Cmd` remap preset
- [ ] Clipboard: `NSPasteboard` change-count monitor for UTF-8 text & PNG images
- [ ] Apple code-signing & notarization packaging (`.app` / `.dmg`) so Accessibility & Input Monitoring permissions persist across updates
