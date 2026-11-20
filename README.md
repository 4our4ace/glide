# Glide v0.2

**Ultra-low-latency, native-feeling cross-platform software KVM over QUIC — with a clean, modern, identical GUI on Windows 11 and Ubuntu X11.**

Zero missing DLLs. Zero missing `.so` dependencies. Zero IP fiddling.

---

## Highlights

- **Modern Desktop GUI + Windows System Tray:** Identical, clean UI on Windows 11 and Ubuntu X11 — interactive multi-monitor edge map showing your real screens & DPI %, LocalSend-style LAN radar, live RTT latency sparkline, live sensitivity/scroll/dead-corner sliders (hot-reloads while connected), clipboard history with one-click re-send, key remapping editor, and drag-and-drop file beaming.
- **Zero TCP head-of-line blocking (3-lane QUIC / TLS 1.3):**
  - Unreliable QUIC datagrams for raw relative mouse deltas & hi-res scroll (1000Hz coalescing, flushed before clicks/keys)
  - Reliable ordered control stream for physical USB HID scancodes, mouse buttons, lock-key sync, and transitions
  - Independent unidirectional QUIC bulk streams for clipboard & file transfers
- **German / EU ISO & AltGr native:** Transmits raw USB HID Usage IDs (`HidUsage`), filters Windows' synthetic fake `Left Ctrl` (`0x21D`) before `AltGr`, and maps ISO `< > |` (`KEY_102ND`), `#/'`, umlauts (`ä ö ü ß`), dead keys (`^` `´` `` ` ``), F13–F24, and media keys losslessly.
- **Universal Bidirectional Clipboard:** UTF-8 text (`\r\n` ↔ `\n` normalized) + universal screenshot images (`PNG` **and** automatic Windows `CF_DIB`/`CF_DIBV5` ↔ `PNG` conversion so `Win+Shift+S`, Snipping Tool, Paint, Chrome, and Ubuntu Flameshot all paste seamlessly) + lazy on-demand fetching for payloads > 256 KB.
- **Instant File Drop (QUIC):** Drag any file onto the Glide GUI window to beam it at full LAN speed into the peer's `Downloads/Glide/` folder (SHA-256 verified).
- **Anti-accident ergonomics & hotkeys:**
  - Invisible corner cursor parking on Windows while controlling Ubuntu (no ghost cursor stuck at the border)
  - Dead-corner guard (24 px default) so clicking close `X` or Show Desktop never jumps screens
  - Fullscreen game/app auto-lock + manual cursor lock (`Ctrl + Alt + L`)
  - Direct keyboard jumps (`Ctrl + Alt + Arrows`) with **per-screen cursor memory**
  - `ScrollLock` unconditional escape failsafe + automatic `release_all()` on leave/disconnect
  - Optional Linux `/dev/uinput` hardware-native HID backend for native `libinput` acceleration & true high-res smooth scrolling (`REL_WHEEL_HI_RES`).

---

## Quick Start (30 Seconds)

### 1. Ubuntu 22.04 (X11)

```bash
sudo apt install ./glide_0.2.0_amd64.deb
```

Then open **Glide KVM** from your Ubuntu application launcher (or run `glide` in a terminal).

### 2. Windows 11

Double-click **`glide-windows-x86_64.exe`** (standalone static-CRT executable — zero extra DLLs needed).

- Click **Start Server** on Windows 11 (if Windows Firewall prompts, allow **Private networks** — or click **Configure Firewall** in the *Clipboard & System* tab).
- On Ubuntu, your Windows PC appears automatically in the **LAN Auto-Discovery** card — pick which edge Ubuntu sits on (e.g. **Right**) and click **Connect**!

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

Requires Rust 1.80+ (`https://rustup.rs`). Pure-Rust dependencies — zero system X11 C dev headers or OpenSSL needed.

- **Linux:** `./scripts/build-linux.sh` (or `./scripts/build-deb.sh` to build the `.deb`)
- **Windows:** `.\scripts\build-windows.ps1`
- **Tests:** `cargo test --workspace`

See [`ARCHITECTURE.md`](ARCHITECTURE.md) and [`TO-DO.md`](TO-DO.md).
