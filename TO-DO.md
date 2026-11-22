# Glide Roadmap & TO-DO

Engineering roadmap for **Glide** — a lean, pure-Rust, zero-cloud software KVM for **Windows 11**, **Linux (X11 & Wayland)**, and **macOS (planned)**.

---

## Completed Foundation (v0.1.0 – v0.2.0)

- [x] 3-lane QUIC / TLS 1.3 transport (`glide-net`: unreliable datagrams for motion/scroll, reliable control stream for keys/buttons/transitions, independent uni streams for clipboard & files)
- [x] Persistent Ed25519 TLS identity + SHA-256 pinning & automatic TOFU
- [x] LocalSend-style UDP multicast + broadcast LAN auto-discovery with real hostnames
- [x] Strictly bijective USB HID Usage scancode table (`glide-hid`) with German ISO (`< > |`, `#/'`, dead keys, umlauts) and Windows fake-`LCtrl` (`0x21D`) `AltGr` filter
- [x] Multi-monitor & mixed-HiDPI normalized edge geometry (`Per-Monitor DPI Awareness V2` on Windows, XRandR on X11)
- [x] Bidirectional event-driven clipboard (UTF-8 text with `\r\n` ↔ `\n` normalization + universal PNG/`CF_DIB`/`CF_DIBV5` screenshots, SHA-256 anti-loop, lazy transfer > 256 KB)
- [x] Standalone Linux binary + `.deb` package and standalone static-CRT Windows binary

---

## Completed Daily-Driver Milestones (v0.3.0 – v0.5.0)

### 1. Native Desktop GUI + System Tray (`glide-gui`)
- [x] Pure-Rust GPU-rendered desktop UI (Rune: `wgpu` + `winit` + `taffy` + `parley`) with light/dark themes and monochrome vector icons
- [x] Interactive visual monitor & edge layout editor with drag-to-arrange screens
- [x] Live LAN discovered devices list + collapsible saved server profiles (`known_servers.toml`) with one-click connect & automatic fingerprint pinning
- [x] Native Windows 11 System Tray (`Shell_NotifyIconW`) & Linux `StatusNotifierItem` tray + zero background redraws or idle LAN scans when hidden

### 2. Seamless Cursor Hiding, Clean Parking & Gaming Guards
- [x] Clean cursor parking on Windows and `XFixes` cursor hiding on Linux (no ghost cursor stuck on the monitor border), with guaranteed restoration on return, escape hotkey, disconnect, or exit
- [x] Configurable corner dead-zone (24 px default), edge dwell delay, and switch distance
- [x] Fullscreen game/app auto-lock (Windows & Linux X11) + manual lock toggle, media-key pinning to primary PC, and clean `Win+L` handling without double-locking

### 3. Universal Image Clipboard & Privacy
- [x] Pure-Rust Windows `CF_DIB` / `CF_DIBV5` ↔ `PNG` converter (`glide-backend::dib_png`) + Wayland/X11 image clipboard support
- [x] Lazy on-demand clipboard offers (`ClipboardOffer` / `ClipboardRequest`) for payloads > 256 KB
- [x] Live Clipboard History with one-click re-send, **Hide** privacy cover, and automatic password-manager secret exclusion (1Password, Bitwarden, KeePassXC)

### 4. Customizable & Disableable Default Shortcuts
- [x] Live-rebindable and toggleable **Jump to a side** (`Ctrl+Alt+Arrow`), **Lock current screen** (`Ctrl+Alt+L`), and **Bring cursor home** (`ScrollLock`) in *Settings → Shortcuts*
- [x] Lock-free `AtomicHotkeys` across Windows, X11, and Wayland with `AltGr`, `Win+L`, `Ctrl+Alt+Del`, and conflict protection + one-click **Reset defaults**
- [x] Per-screen normalized cursor memory (`EnterAt` / `leave_remote_at`)

### 5. Scale-Aware Pointer Feel, Linux Wayland & `/dev/uinput`
- [x] Optional `auto_dpi_scale`, live `sensitivity`, `scroll_speed`, `invert_scroll`, Lock-key sync (`CapsLock`/`NumLock`), and per-client physical key remapping (`[[client.key_remap]]`)
- [x] Native Linux Wayland backend (`xdg-desktop-portal` `InputCapture` + `RemoteDesktop` + pure-Rust `libei` / `reis`, persistent restore tokens, `wl-clipboard-rs` with XWayland fallback, and auto X11/Wayland detection)
- [x] Pure-Rust Linux `/dev/uinput` virtual kernel HID backend (`--backend uinput` / GUI selector) with calibrated pointer sync & `REL_WHEEL_HI_RES` smooth scrolling

### 6. Live Cross-Edge File Drag-and-Drop
- [x] Drag files straight across screen edges with live floating previews, lazy drag-only edge arming (zero click overhead), native Windows Shell / X11 XDND drop delivery, Wayland `Ctrl+V` handoff + cursor tooltip, SHA-256 verification, configurable max file size (up to 4 GB), and one-click Open/Reveal receive folder
- [x] Terminal file transfer (`glide send`) and live session control IPC (`glide ctl status|lock|unlock|switch`)

### 7. Wi-Fi Anti-Spike & Resilience Hardening
- [x] QUIC tuned for Wi-Fi: 45 ms keepalive, 2 ms max ACK delay (`ack_eliciting_threshold(1)`), GSO disabled, `2–4 KiB` datagram buffers, backpressure-aware datagram coalescing, `+100` control / `-100` chunked bulk stream priority, Linux `SO_PRIORITY = 6` (`AC_VO`), async SHA-256, and non-blocking LAN discovery
- [x] Windows 10/11 EcoQoS opt-out (`SetProcessInformation`), non-blocking `wake_pipe()` for X11 clipboard/DnD, persistent `WaylandGeometryWatcher`, and atomic serialized `glide.toml` saves
- [x] Sleep/wake, display hotplug, and Wi-Fi ↔ Ethernet roaming recovery (`WM_POWERBROADCAST` / `WM_DISPLAYCHANGE` / XRandR `ScreenChangeNotify` / Wayland output changes)

### 8. Installers, Channels & Passwordless Auto-Updates
- [x] Per-user Windows WiX v5 `.msi` installer (`%LOCALAPPDATA%\Programs\Glide`, Start Menu shortcut, Apps & Features uninstaller, no admin/UAC prompt) + standalone `.exe`
- [x] Ubuntu/Debian `.deb` package + standalone Linux binary + Windows/Linux autostart, Firewall, and Elevated Task helpers
- [x] Branch & release channel split: `main` = **Stable** (default), `nightly` = **Nightly — Beta** (prerelease with confirmation warning modal)
- [x] SHA-256 verified one-click **Update & Restart** on both Windows and Linux (passwordless Linux user-space install to `~/.local/bin/glide` with XDG `.desktop` override and autostart rewrite — zero `sudo` prompt)

---

## Remaining / Planned Horizons

### Telemetry & Diagnostics Polish
- [x] Live Ping/Pong RTT (ms) + sparkline in GUI
- [ ] Jitter, UDP datagram drop counter, and effective mouse Hz meter
- [ ] Interactive `glide doctor --keys` live raw scancode inspector

### Packaging Extras
- [ ] Optional `winget` manifest for Windows

### macOS Backend (`crates/glide-backend/src/macos.rs`)
- [ ] Capture: `CGEventTap` (`kCGHIDEventTap`) + `CGAssociateMouseAndMouseCursorPosition(false)` for raw unaccelerated deltas + `IOHIDManager` for extra keys/buttons
- [ ] Injection: `CGEventPost` using existing `HidUsage::to_macos_keycode()` mappings + default `Ctrl ↔ Cmd` remap preset
- [ ] Clipboard: `NSPasteboard` change-count monitor for UTF-8 text & PNG images
- [ ] Apple code-signing & notarization packaging (`.app` / `.dmg`) so Accessibility & Input Monitoring permissions persist across updates
