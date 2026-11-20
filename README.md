# Glide

**Ultra-low-latency, native-feeling cross-platform software KVM over QUIC.**

Share one physical mouse, keyboard, and clipboard across **Windows 11** and **Linux (X11)** with:

- **Zero TCP head-of-line blocking:** Mouse deltas and high-resolution scroll travel over unreliable QUIC datagrams; keystrokes and screen transitions travel over a reliable ordered QUIC stream; clipboard transfers travel over independent bulk streams.
- **Native physical keyboard feel (German / ISO / AltGr ready):** Transmits physical USB HID scancodes (`HidUsage`), not translated symbols. On Windows servers, the synthetic fake `Left Ctrl` (`scanCode 0x21D`) emitted by German/EU `AltGr` layouts is filtered automatically — so `AltGr + Q` (`@`), `{ [ ] }`, `\`, `~`, `|`, `< > |` (`KEY_102ND`), umlauts (`ä ö ü ß`), and dead keys (`^` `´` `` ` ``) work natively.
- **Native mouse feel across mixed HiDPI & multi-monitor setups:** Raw unaccelerated hardware mouse deltas (`WM_INPUT` on Windows, `XI_RawMotion` on X11), Per-Monitor DPI Awareness V2 on Windows, XRandR multi-monitor geometry on Linux, and configurable per-client sensitivity.
- **Zero-config LocalSend-style LAN auto-discovery:** Open `glide server` on one machine and `glide client` on the other — no IP fiddling needed (real hostnames, automatic TLS 1.3 + TOFU fingerprint pinning).
- **Bidirectional clipboard:** Instant UTF-8 text and PNG image sync (with `\r\n` ↔ `\n` normalization and SHA-256 anti-loop protection).
- **Built-in failsafes:** Press **`ScrollLock`** anytime on the server to immediately snap focus back; disconnecting automatically releases all held keys/buttons so modifiers never get stuck.

---

## Quick Start (Windows 11 Primary → Ubuntu 22.04 X11 Client)

### 1. Install on Ubuntu 22.04 (X11)

```bash
sudo apt install ./glide_0.1.0_amd64.deb
```

Verify your X11 session and monitors:

```bash
glide doctor
```

### 2. Start the Server on Windows 11

Open PowerShell / Terminal where `glide.exe` is located and run:

```powershell
.\glide.exe server
```

> **Windows Firewall tip:** When Windows prompts for firewall access, allow **Private networks** (UDP `24800` for QUIC and UDP `24801` for LAN auto-discovery).
> You can also open them explicitly in an Administrator PowerShell:
> ```powershell
> New-NetFirewallRule -DisplayName "Glide KVM (QUIC + Discovery)" -Direction Inbound -Protocol UDP -LocalPort 24800,24801 -Action Allow -Profile Private
> ```

### 3. Start the Client on Ubuntu 22.04

Tell Glide which side of your Windows monitor the Ubuntu laptop sits on (`right`, `left`, `top`, or `bottom`):

```bash
glide client --edge right
```

Glide automatically discovers your Windows 11 server on the LAN, prints its hostname & SHA-256 fingerprint, saves it via Trust-On-First-Use (TOFU), and connects over encrypted QUIC.

Slide your mouse off the **right edge** of your Windows screen — you're now controlling Ubuntu! Push back off the **left edge** of Ubuntu to glide back to Windows.

---

## Switching Roles (Ubuntu Primary → Windows 11 Client)

Both binaries include full server and client support:

- On Ubuntu: `glide server`
- On Windows 11: `.\glide.exe client --edge left`

---

## Useful Commands & Tuning

| Command | What it does |
|---|---|
| `glide discover` | Scan your LAN and list all running Glide servers (hostname, OS, IP, fingerprint) |
| `glide doctor` | Check monitors, DPI scaling, X11 extensions, and German ISO / AltGr scancode mappings |
| `glide init` | Create a starter `glide.toml` and persistent Ed25519 TLS identity |
| `glide client --edge right --sensitivity 1.2` | Adjust pointer speed on the client without losing sub-pixel accuracy |
| `glide client --edge right --server 192.168.1.42:24800` | Connect explicitly by IP (e.g. across VLANs or VPNs) |
| `glide server --escape-key Pause` | Change the local panic/escape key (default: `ScrollLock`) |
| `glide server --edge-threshold 8` | Require pushing 8px past the screen border before switching (reduces accidental switches) |
| `glide server --coalesce-us 0` | Send every raw mouse packet immediately without 1000Hz coalescing |

### Configuration File (`glide.toml`)

See [`examples/glide.toml`](examples/glide.toml) for all options. Place it at:
- **Linux:** `~/.config/glide/glide.toml`
- **Windows:** `%APPDATA%\glide\config\glide.toml`

### Autostart on Ubuntu (systemd user service)

The `.deb` package installs `glide-client.service` and `glide-server.service`. After creating `~/.config/glide/glide.toml`:

```bash
systemctl --user enable --now glide-client.service
```

---

## Building From Source

Requires Rust 1.80+ (`https://rustup.rs`). Pure-Rust dependencies — no system X11 C dev headers or OpenSSL required.

- **Linux:** `./scripts/build-linux.sh` (or `./scripts/build-deb.sh` to package `.deb`)
- **Windows:** `.\scripts\build-windows.ps1`
- **Tests:** `cargo test --workspace`

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for protocol, HID mapping, and multi-OS backend architecture details.
