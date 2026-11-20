# Glide Architecture

Glide is a clean-slate, ultra-low-latency software KVM designed for long-term maintainability and native hardware feel across **Windows 11**, **Linux (X11 today, Wayland/`libei` & `evdev`/`uinput` next)**, and **macOS (planned)**.

## Core Design Principles

1. **Zero TCP head-of-line blocking (3-lane QUIC transport)**
   - **Lane A (QUIC Unreliable Datagrams):** Raw relative mouse deltas (`f32`) and high-resolution scroll (`i16`, 120 units = 1 notch). A dropped Wi-Fi packet never stalls newer pointer movement.
   - **Lane B (QUIC Reliable Bidirectional Stream):** Physical key transitions, mouse buttons, screen enter/leave transitions, modifier sync, and heartbeats.
   - **Lane C (QUIC Unidirectional Bulk Streams):** Bidirectional clipboard payloads (UTF-8 text & PNG images) verified with SHA-256. Copying a large screenshot never delays mouse or keyboard input.

2. **Physical USB HID Scancodes — never keysym translation**
   - Every key on the wire is a canonical `HidUsage` (`(usage_page << 16) | usage_id`), covering USB HID Keyboard/Keypad (`0x07`) and Consumer Control media keys (`0x0C`).
   - Receiving OSes apply their own native keyboard layout — identical to plugging a physical USB keyboard into the client.
   - **German / EU ISO support:**
     - `AltGr` is transmitted as physical `RightAlt` (`HID 0xE6` ↔ Windows `E0 38` ↔ Linux evdev `100` / X11 `108`).
     - On Windows servers, the synthetic fake `VK_LCONTROL` (`scanCode == 0x21D`) injected by Windows' German layout before `VK_RMENU` is explicitly filtered in remote mode so Linux never receives `Ctrl+AltGr`.
     - The ISO 102nd key (`< > |`) maps losslessly (`HID 0x64` ↔ Windows `0x56` ↔ Linux evdev `86` / X11 `94`).
     - Dead keys (`^`, `´`, `` ` ``) and `#/'` (`0x31`/`0x32`) work natively.

3. **Native pointer feel across mixed HiDPI & multi-monitor setups**
   - Servers capture raw, unaccelerated hardware deltas (`WM_INPUT` on Windows, `XI_RawMotion` on X11) and coalesce high-polling-rate mice (e.g. 4000Hz → 1000Hz default) with zero pixel loss, flushing pending motion before any click or keypress.
   - Clients inject relative deltas so the client OS renders its own native acceleration curve and hardware cursor.
   - Screen transitions transmit normalized edge positions `t ∈ [0.0, 1.0]` computed against active outer monitors (Per-Monitor DPI Awareness V2 on Windows, XRandR CRTCs on X11), preventing dead-zone traps between monitors of different resolutions or scaling factors.

4. **Zero-config LocalSend-style LAN discovery + TLS 1.3 security**
   - Servers generate a persistent Ed25519 TLS certificate and announce their real hostname, QUIC port, OS, and SHA-256 fingerprint via UDP multicast (`239.255.71.76:24801`) + broadcast fallback (`255.255.255.255:24801`).
   - Clients auto-discover running servers in milliseconds with automatic Trust-On-First-Use (TOFU) or explicit SHA-256 fingerprint pinning.

5. **Never lock yourself out**
   - Unconditional local escape hotkey (`ScrollLock` by default) snaps focus back to the server immediately.
   - QUIC 4-second idle timeout auto-ungrabs the server if a client laptop sleeps or loses Wi-Fi.
   - Client injectors track every pressed key and button and synthesize release events (`release_all()`) on every `Leave` or disconnect — preventing stuck modifiers.

## Crate Map

| Crate | Responsibility |
|---|---|
| `glide-hid` | Bijective USB HID Usage ↔ Windows Set 1 ScanCode ↔ Linux evdev/X11 ↔ macOS keycode tables + Windows AltGr filter |
| `glide-proto` | Versioned binary wire protocol for datagrams, reliable control frames, and clipboard bulk headers |
| `glide-net` | QUIC transport (`quinn` + `rustls`), Ed25519 identity + SHA-256/TOFU pinning, LAN multicast/broadcast discovery |
| `glide-backend` | Cross-platform traits (`CaptureControl`, `InputInjector`, `ClipboardBackend`, `DesktopGeometry`) + native `windows`, `linux_x11`, `macos` (stub), and `mock` implementations |
| `glide-core` | Platform-agnostic server & client state machines, motion coalescer, multi-edge router, clipboard coordinator, and TOML config |
| `glide-cli` | Single `glide` binary (`server`, `client`, `discover`, `init`, `doctor`) |
