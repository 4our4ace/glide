# Glide Roadmap & TO-DO

Long-horizon engineering plan to make **Glide** a genuinely native, zero-lag, zero-tech-debt Deskflow replacement — prioritizing **Windows 11 (Server) ↔ Ubuntu 22.04 X11 (Client)** first, then expanding cleanly to Wayland and macOS.

---

## Completed Foundation (v0.1.0)

- [x] 3-lane QUIC / TLS 1.3 transport (`glide-net`: unreliable datagrams for motion/scroll, reliable control stream for keys/buttons/transitions, independent uni streams for clipboard)
- [x] Persistent Ed25519 TLS identity + SHA-256 pinning & automatic TOFU
- [x] LocalSend-style UDP multicast + broadcast LAN auto-discovery with real hostnames
- [x] Strictly bijective USB HID Usage scancode table (`glide-hid`) with German ISO (`< > |`, `#/'`, dead keys, umlauts) and Windows fake-`LCtrl` (`0x21D`) `AltGr` filter
- [x] Multi-monitor & mixed-HiDPI normalized edge geometry (`Per-Monitor DPI Awareness V2` on Windows, XRandR on X11)
- [x] Bidirectional event-driven clipboard (UTF-8 text with `\r\n` ↔ `\n` normalization + PNG images, SHA-256 anti-loop)
- [x] Static Linux musl binary + `.deb` package (`glide_0.1.0_amd64.deb`) and standalone static-CRT Windows binary (`glide-windows-x86_64.exe`)

---

## Next 10 High-Impact Steps (Windows 11 ↔ Ubuntu X11 Daily Driver)

### 1. Native Desktop GUI + System Tray (`glide-gui`)
- [ ] Add a decoupled `glide-gui` crate sharing `glide-core` (keeping the headless CLI/daemon intact)
- [ ] Visual monitor & edge layout editor (click/drag client onto Left/Right/Top/Bottom of server)
- [ ] Live LocalSend-style discovered LAN devices list with one-click connect and interactive TOFU TLS fingerprint approval dialog
- [ ] System tray icon with live state indicator (Idle / Connected / Controlling Remote), Start/Stop toggle, and live sensitivity / edge-threshold sliders without restarting

### 2. Seamless Windows Remote Cursor Hiding & Clean Parking
- [ ] Hide the local Windows cursor (or park it cleanly out of sight) while controlling a remote client so no motionless "ghost cursor" lingers on the Windows screen border
- [ ] Guarantee automatic cursor restoration on return, escape hotkey (`ScrollLock`), client disconnect, or process exit/crash

### 3. Universal Image Clipboard (`CF_DIB` / `CF_DIBV5` ↔ `PNG`) + Lazy Clipboard Transfers
- [ ] Add transparent Windows `CF_DIB` / `CF_DIBV5` ↔ `PNG` conversion so *every* screenshot tool (Snipping Tool `Win+Shift+S`, Paint, browsers, Ubuntu Flameshot/Screenshot) pastes images bidirectionally
- [ ] Implement lazy on-demand clipboard offers for payloads > 256 KB (`ClipboardOffer` metadata on copy, bulk QUIC fetch only when the peer switches screens or pastes) so local large copies never waste LAN/Wi-Fi bandwidth

### 4. Dead-Corner Guards, Edge Dwell & Fullscreen App Lock
- [ ] Configurable corner dead-zones (e.g., 24 px top/bottom margin on vertical edges) so hitting Windows' top-right `X` close button, bottom-right Show Desktop, or scrollbars never accidentally switches screens
- [ ] Optional edge dwell delay (ms) alongside pixel push-through threshold
- [ ] Auto-lock cursor to Windows when an exclusive fullscreen game/app (or VM/RDP window) is focused, plus a manual "Lock cursor to current screen" toggle

### 5. Direct Keyboard Screen-Switching Hotkeys & Per-Screen Cursor Memory
- [ ] Configurable hotkeys to jump directly to a specific machine without touching the mouse (e.g., `Ctrl+Alt+Right` / `Ctrl+Alt+Left`)
- [ ] Remember the last cursor `(x, y)` on each screen when switching via hotkey so jumping back to an editor or terminal lands right where you left off

### 6. Scale-Aware Pointer Feel, Scroll Tuning & Optional Linux `uinput` Native Mode
- [ ] Optional `auto_dpi_scale` multiplier + per-client sensitivity & scroll-speed / natural-scroll inversion settings
- [ ] Optional Linux `uinput` virtual HID injection backend (alongside zero-privilege XTest): creates a real virtual kernel mouse & keyboard on Ubuntu so `libinput` applies 100% native pointer acceleration, true high-resolution pixel scrolling (`REL_WHEEL_HI_RES`), and works even on login/lock screens

### 7. Lock-Key Sync (`CapsLock` / `NumLock`) & Per-Client Key Remapping
- [ ] Synchronize `CapsLock` and `NumLock` state on screen entry so modifier/lock state never silently inverts between machines
- [ ] Declarative per-client physical key remap rules in `glide.toml` & GUI (e.g., swap `Ctrl`/`Meta` or remap extra keys per target OS)

### 8. Bulletproof Sleep/Wake, Display Hotplug & Network Roaming Recovery
- [ ] Handle OS power, session, and display events (`WM_POWERBROADCAST`, `WM_DISPLAYCHANGE`, `WTSRegisterSessionNotification` on Windows; XRandR `ScreenChangeNotify` on X11)
- [ ] Instant recovery after laptop lid close/open, Windows sleep/wake, monitor plug/unplug, or Wi-Fi ↔ Ethernet switch (leveraging QUIC connection migration) with zero zombie hooks or stuck keys

### 9. Real-Time Telemetry HUD & Interactive Scancode / AltGr Inspector
- [ ] Continuous Ping/Pong RTT (ms), jitter, UDP datagram drop counter, and effective mouse Hz meter surfaced in both GUI and CLI (`--stats`)
- [ ] Interactive `glide doctor --keys` (and GUI key inspector) displaying live raw OS scan code → `HidUsage` → target OS code so any exotic or international key can be verified in seconds

### 10. Zero-Friction OS Integration (Autostart, Firewall, Elevated Windows & File Drop)
- [ ] Windows: one-click "Start with Windows", automatic Private-network UDP firewall rule helper (`24800`/`24801`), and optional elevated Windows service (`uiAccess`) so input continues seamlessly over Administrator windows and UAC prompts
- [ ] Ubuntu `.deb`: desktop application launcher (`.desktop`), autostart toggle, tray integration, and optional `udev` rule for `uinput` mode
- [ ] Stretch: Drag-and-drop file transfer across screen edges over the existing QUIC bulk stream lane

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
