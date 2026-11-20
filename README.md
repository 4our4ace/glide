# Glide v0.3

**Ultra-low-latency, native cross-platform software KVM over QUIC — with a truly native, GPU-rendered Rust GUI on Windows 11 and Ubuntu X11.**

Zero browser or WebView dependency. Zero missing Windows DLLs. Zero IP fiddling.

---

## Highlights

- **Truly Native GPU-Rendered GUI + System Tray:** Identical, flat, modern UI rendered directly in Rust (Rune / wgpu / winit) on Windows 11 and Ubuntu X11 — zero Chrome or WebView2 dependency. Includes a drag-and-drop screen map, LocalSend-style LAN auto-discovery, live RTT sparkline, instant sensitivity/scroll/dead-corner sliders, clipboard history with one-click re-send, key remapping editor, drag-and-drop file beaming, and native system tray support (Windows taskbar & Ubuntu StatusNotifierItem).
- **Zero TCP head-of-line blocking (3-lane QUIC / TLS 1.3):**
  - Unreliable QUIC datagrams for raw relative mouse deltas & hi-res scroll (1000Hz coalescing, flushed before clicks/keys)
  - Reliable ordered control stream for physical USB HID scancodes, mouse buttons, lock-key sync, and transitions
  - Independent unidirectional QUIC bulk streams for clipboard & file transfers
- **German / EU ISO & AltGr native:** Transmits raw USB HID Usage IDs (`HidUsage`), filters Windows' synthetic fake `Left Ctrl` (`0x21D`) before `AltGr`, and maps ISO `< > |` (`KEY_102ND`), `#/'`, umlauts (`ä ö ü ß`), dead keys (`^` `´` `` ` ``), F13–F24, and media keys losslessly.
- **Universal Bidirectional Clipboard:** UTF-8 text (`\r\n` ↔ `\n` normalized) + universal screenshot images (`PNG` **and** automatic Windows `CF_DIB`/`CF_DIBV5` ↔ `PNG` conversion so `Win+Shift+S`, Snipping Tool, Paint, Chrome, and Ubuntu Flameshot all paste seamlessly) + lazy on-demand fetching for payloads > 256 KB.
- **Instant File Drop (QUIC):** Drag any file onto the Glide GUI window (or click browse) to beam it at full LAN speed into the peer's `Downloads/Glide/` folder (SHA-256 verified).
- **Anti-accident ergonomics & hotkeys:**
  - Clean cursor hiding & corner parking on both Windows and Ubuntu X11 (`XFixesHideCursor` + corner park, automatically re-shown if you move a local mouse/touchpad) so no ghost cursor sits idly on the screen edge
  - Dead-corner guard (24 px default) so clicking close `X` or Show Desktop never jumps screens
  - Fullscreen game/app auto-lock + manual cursor lock (`Ctrl + Alt + L`)
  - Direct keyboard jumps (`Ctrl + Alt + Arrows`) with **per-screen cursor memory**
  - `ScrollLock` unconditional escape failsafe + automatic `release_all()` on leave/disconnect
  - Optional Linux `/dev/uinput` hardware-native HID backend for native `libinput` acceleration & true high-res smooth scrolling (`REL_WHEEL_HI_RES`).

---

## Quick Start (30 Seconds)

### 1. Ubuntu 22.04 (X11)

```bash
sudo apt install ./glide_0.3.0_amd64.deb
```

Then open **Glide KVM** from your Ubuntu application launcher (or run `glide` in a terminal). Standard Ubuntu desktop libraries (`libvulkan1`, `mesa-vulkan-drivers`, `libxkbcommon-x11-0`, `libwayland-client0`) are already preinstalled on desktop Ubuntu and declared as package dependencies.

### 2. Windows 11

Double-click **`glide-windows-x86_64.exe`** (standalone static-CRT executable — zero extra DLLs needed, uses Direct3D 12 with automatic WARP fallback).

- Click **Start Server** on Windows 11 (if Windows Firewall prompts, allow **Private networks** — or click **Allow** under *Allow through firewall* in the *System* tab).
- On Ubuntu, your Windows PC appears automatically under **Nearby servers** — drag **This PC** to the side it sits on and click **Connect**!

### Prefer the CLI / Headless Mode?

Both machines support full CLI & daemon operation too:

```powershell
# Windows 11 Primary
.\glide-windows-x86_64.exe server
```

```bash
# Ubuntu 22.04 Client (auto-discovers server on LAN!)
glide client --edge right

# Optional hardware-native /dev/uinput mode (after installing 99-glide-uinput.rules)
glide client --edge right --backend uinput

# Diagnostics & LAN scan
glide doctor
glide discover
```

---

## Building From Source

Requires Rust (stable).

- **Linux:** `./scripts/build-linux.sh` (or `./scripts/build-deb.sh` to build the `.deb`)
- **Windows:** `.\scripts\build-windows.ps1`
- **Tests:** `cargo test --workspace`

See [`ARCHITECTURE.md`](ARCHITECTURE.md) and [`TO-DO.md`](TO-DO.md).
