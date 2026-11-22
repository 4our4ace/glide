# Glide Architecture

Glide is a clean-slate, ultra-low-latency software KVM designed for long-term maintainability and native hardware feel across **Windows 11**, **Linux (X11 & Wayland — portal/`libei`, `XTest`, and `/dev/uinput`)**, and **macOS (planned)**.

## Core Design Principles

1. **Zero TCP head-of-line blocking & Wi-Fi anti-spike tuning (3-lane QUIC transport)**
   - **Lane A (QUIC Unreliable Datagrams):** Raw relative mouse deltas (`f32`) and high-resolution scroll (`i16`, 120 units = 1 notch) with tight `2–4 KiB` send/receive buffers and leading-edge / backpressure-aware coalescing. A dropped Wi-Fi packet never stalls newer pointer movement.
   - **Lane B (QUIC Reliable Bidirectional Control Stream — `+100` priority):** Physical key transitions, mouse buttons, screen enter/leave transitions, modifier sync, drag handoff metadata, and heartbeats — backed by 45 ms QUIC keepalive, 2 ms max ACK delay (`ack_eliciting_threshold(1)`), GSO disabled, Linux `SO_PRIORITY = 6` (`AC_VO` voice-class QoS), and Windows 10/11 EcoQoS opt-out.
   - **Lane C (QUIC Unidirectional Bulk Streams — `-100` priority):** Chunked bidirectional clipboard payloads (UTF-8 text & PNG images) and cross-edge / drag-and-drop file transfers verified with off-hot-path async SHA-256. Copying a large screenshot or beaming a file never delays mouse or keyboard input.

2. **Physical USB HID Scancodes — never keysym translation**
   - Every key on the wire is a canonical `HidUsage` (`(usage_page << 16) | usage_id`), covering USB HID Keyboard/Keypad (`0x07`) and Consumer Control media keys (`0x0C`).
   - Receiving OSes apply their own native keyboard layout — identical to plugging a physical USB keyboard into the client.
   - **German / EU ISO support:**
     - `AltGr` is transmitted as physical `RightAlt` (`HID 0xE6` ↔ Windows `E0 38` ↔ Linux evdev `100` / X11 `108`).
     - On Windows servers, the synthetic fake `VK_LCONTROL` (`scanCode == 0x21D`) injected by Windows' German layout before `VK_RMENU` is explicitly filtered in remote mode so Linux never receives `Ctrl+AltGr`.
     - The ISO 102nd key (`< > |`) maps losslessly (`HID 0x64` ↔ Windows `0x56` ↔ Linux evdev `86` / X11 `94`).
     - Dead keys (`^`, `´`, `` ` ``) and `#/'` (`0x31`/`0x32`) work natively.

3. **Native pointer feel across mixed HiDPI, X11, Wayland & multi-monitor setups**
   - Servers capture raw, unaccelerated hardware deltas (`WM_INPUT` on Windows, `XI_RawMotion` on X11, `xdg-desktop-portal` `InputCapture` + `libei` on Wayland) and coalesce high-polling-rate mice (e.g. 4000 Hz → 1000 Hz default) with zero pixel loss, flushing pending motion before any click or keypress.
   - Clients inject relative deltas (`SendInput`, `XTest`, Wayland `RemoteDesktop` + `libei`, or `/dev/uinput`) so the client OS renders its own native acceleration curve and hardware cursor, with calibrated X11/`uinput` pointer sync and zero-stall non-blocking OS hot paths (`wake_pipe()` for X11 clipboard/DnD, persistent `WaylandGeometryWatcher`).
   - Screen transitions transmit normalized edge positions `t ∈ [0.0, 1.0]` computed against active outer monitors (Per-Monitor DPI Awareness V2 on Windows, XRandR CRTCs on X11, Wayland outputs), preventing dead-zone traps between monitors of different resolutions or scaling factors.

4. **Cross-edge file drag-and-drop & private clipboard**
   - Lazy drag-only edge arming (`≥ 4 px` movement within `160 px` of an active edge) keeps normal desktop clicks completely overhead-free, while crossing an edge during a file drag streams files over QUIC Lane C with a live floating preview and native drop target integration (Windows Shell / X11 XDND / Wayland `Ctrl+V` handoff + cursor tooltip).
   - Bidirectional clipboard sync automatically skips concealed password-manager entries (1Password, Bitwarden, KeePassXC) and normalizes PNG / `CF_DIB` / `CF_DIBV5` screenshots.

5. **Zero-config LocalSend-style LAN discovery + TLS 1.3 security**
   - Servers generate a persistent Ed25519 TLS certificate and announce their real hostname, QUIC port, OS, and SHA-256 fingerprint via non-blocking UDP multicast (`239.255.71.76:24801`) + broadcast fallback (`255.255.255.255:24801`).
   - Clients auto-discover running servers in milliseconds with automatic Trust-On-First-Use (TOFU), saved server profiles (`known_servers.toml`), or explicit SHA-256 fingerprint pinning.

6. **Never lock yourself out & live-customizable hotkeys**
   - Lock-free `AtomicHotkeys` support live rebinding or disabling of **Jump to a side** (`Ctrl+Alt+Arrow`), **Lock current screen** (`Ctrl+Alt+L`), and **Bring cursor home** (`ScrollLock`) across Windows, X11, and Wayland with `AltGr` / `Win+L` / `Ctrl+Alt+Del` / conflict protection.
   - QUIC 4-second idle timeout auto-ungrabs the server if a client laptop sleeps or loses Wi-Fi.
   - Client injectors track every pressed key and button and synthesize release events (`release_all()`) on every `Leave` or disconnect — preventing stuck modifiers.

7. **Passwordless, SHA-256 verified updates (Stable & Nightly Beta)**
   - Stable (`main`, `GLIDE_BUILD_CHANNEL=stable`) is the default channel; switching to Nightly (`nightly`, labeled **Beta**) shows an explicit warning modal.
   - Windows updates use per-user `.msi` installs (`%LOCALAPPDATA%\Programs\Glide`, zero UAC) or standalone `.exe` replacement. Linux updates install verified `glide-linux-x86_64-standalone` to `~/.local/bin/glide`, write an XDG launcher override (`~/.local/share/applications/glide.desktop`), update autostart preserving flags, and relaunch with zero `sudo` prompt.

## Crate Map

| Crate | Responsibility |
|---|---|
| `glide-hid` | Bijective USB HID Usage ↔ Windows Set 1 ScanCode ↔ Linux evdev/X11 ↔ macOS keycode tables + Windows AltGr filter |
| `glide-proto` | Versioned binary wire protocol for datagrams, reliable control frames, drag handoff, and clipboard/file bulk headers |
| `glide-net` | Wi-Fi-tuned QUIC transport (`quinn` + `rustls`), stream priorities, Linux `SO_PRIORITY`, Ed25519 identity + SHA-256/TOFU pinning, non-blocking LAN discovery |
| `glide-backend` | Cross-platform traits (`CaptureControl`, `InputInjector`, `ClipboardBackend`, `DndBackend`), `DesktopGeometry` + lock-free `AtomicHotkeys`, and native `windows` (EcoQoS opt-out, Shell DnD, tray), `linux_x11` (`XFixes`, XDND, `wake_pipe()`, `StatusNotifierItem` tray), `linux_wayland` (portal + `libei` + `wl-clipboard-rs`), `linux_uinput`, `macos` (stub), and `mock` implementations |
| `glide-core` | Platform-agnostic server & client state machines, `CaptureHotkeys` config/hot-reload, motion coalescer, multi-edge router, clipboard & file-drop coordinators, local IPC (`glide ctl`), and atomic serialized TOML config |
| `glide-gui` | Native GPU-rendered desktop GUI (Rune: `wgpu` + `winit` + `taffy` + `parley`), monochrome vector icon set, Stable/Nightly verified updater, system tray wiring, single-instance handoff, and loopback control fallback |
| `glide-cli` | Single `glide` binary (`gui`, `server`, `client`, `ctl`, `send`, `servers`, `discover`, `init`, `setup`, `update`, `doctor`) |
