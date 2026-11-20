#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="0.2.0"
ARCH="amd64"
BIN_PATH="${1:-$ROOT_DIR/target/x86_64-unknown-linux-musl/release/glide}"
OUT_DIR="$ROOT_DIR/dist"
PKG_ROOT="$(mktemp -d)"

trap 'rm -rf "$PKG_ROOT"' EXIT

if [[ ! -f "$BIN_PATH" ]]; then
    echo "Error: binary not found at $BIN_PATH" >&2
    exit 1
fi

mkdir -p \
    "$OUT_DIR" \
    "$PKG_ROOT/DEBIAN" \
    "$PKG_ROOT/usr/bin" \
    "$PKG_ROOT/usr/lib/systemd/user" \
    "$PKG_ROOT/usr/share/applications" \
    "$PKG_ROOT/usr/share/icons/hicolor/scalable/apps" \
    "$PKG_ROOT/usr/share/doc/glide/examples"

chmod 0755 \
    "$PKG_ROOT" \
    "$PKG_ROOT/DEBIAN" \
    "$PKG_ROOT/usr" \
    "$PKG_ROOT/usr/bin" \
    "$PKG_ROOT/usr/lib" \
    "$PKG_ROOT/usr/lib/systemd" \
    "$PKG_ROOT/usr/lib/systemd/user" \
    "$PKG_ROOT/usr/share" \
    "$PKG_ROOT/usr/share/applications" \
    "$PKG_ROOT/usr/share/icons" \
    "$PKG_ROOT/usr/share/icons/hicolor" \
    "$PKG_ROOT/usr/share/icons/hicolor/scalable" \
    "$PKG_ROOT/usr/share/icons/hicolor/scalable/apps" \
    "$PKG_ROOT/usr/share/doc" \
    "$PKG_ROOT/usr/share/doc/glide" \
    "$PKG_ROOT/usr/share/doc/glide/examples"

install -m 0755 "$BIN_PATH" "$PKG_ROOT/usr/bin/glide"
install -m 0644 "$ROOT_DIR/README.md" "$PKG_ROOT/usr/share/doc/glide/README.md"
install -m 0644 "$ROOT_DIR/ARCHITECTURE.md" "$PKG_ROOT/usr/share/doc/glide/ARCHITECTURE.md"
install -m 0644 "$ROOT_DIR/TO-DO.md" "$PKG_ROOT/usr/share/doc/glide/TO-DO.md"
install -m 0644 "$ROOT_DIR/examples/glide.toml" "$PKG_ROOT/usr/share/doc/glide/examples/glide.toml"

cat > "$PKG_ROOT/usr/share/icons/hicolor/scalable/apps/glide.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <defs>
    <linearGradient id="glide-g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#3b82f6"/>
      <stop offset="1" stop-color="#06b6d4"/>
    </linearGradient>
  </defs>
  <rect x="2" y="2" width="60" height="60" rx="14" fill="url(#glide-g)"/>
  <path d="M20 9V46.5L28 39.76V17Z" fill="#fff" fill-opacity=".55"/>
  <path d="M32 21V43.88L36 52.5L42.3 49.6L36 36H47Z" fill="#fff"/>
</svg>
EOF

cat > "$PKG_ROOT/usr/share/icons/hicolor/scalable/apps/glide-symbolic.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="7.5 4.75 52 52">
  <path d="M20 9V46.5L28 39.76V17Z" fill="#fff" fill-opacity=".55"/>
  <path d="M32 21V43.88L36 52.5L42.3 49.6L36 36H47Z" fill="#fff"/>
</svg>
EOF

cat > "$PKG_ROOT/usr/share/applications/glide.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Version=1.0
Name=Glide KVM
GenericName=Software KVM
Comment=Ultra-low-latency mouse, keyboard, clipboard, and file sharing over QUIC
Exec=/usr/bin/glide gui
Icon=glide
StartupWMClass=Glide
Terminal=false
Categories=Utility;Network;
StartupNotify=true
EOF

cat > "$PKG_ROOT/usr/share/doc/glide/99-glide-uinput.rules" <<'EOF'
# Optional: enable Glide's hardware-native /dev/uinput HID backend on Ubuntu/Debian.
# Install with:
#   sudo cp /usr/share/doc/glide/99-glide-uinput.rules /etc/udev/rules.d/
#   sudo usermod -aG input "$USER"
#   sudo udevadm control --reload-rules && sudo udevadm trigger
KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"
EOF

cat > "$PKG_ROOT/usr/lib/systemd/user/glide-client.service" <<'EOF'
[Unit]
Description=Glide Software KVM Client (auto-discovers server on LAN)
After=graphical-session.target network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/glide client
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
EOF

cat > "$PKG_ROOT/usr/lib/systemd/user/glide-server.service" <<'EOF'
[Unit]
Description=Glide Software KVM Server
After=graphical-session.target network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/glide server
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
EOF

chmod 0644 \
    "$PKG_ROOT/usr/share/icons/hicolor/scalable/apps/glide.svg" \
    "$PKG_ROOT/usr/share/icons/hicolor/scalable/apps/glide-symbolic.svg" \
    "$PKG_ROOT/usr/share/applications/glide.desktop" \
    "$PKG_ROOT/usr/share/doc/glide/99-glide-uinput.rules" \
    "$PKG_ROOT/usr/lib/systemd/user/glide-client.service" \
    "$PKG_ROOT/usr/lib/systemd/user/glide-server.service"

INSTALLED_KB="$(du -sk "$PKG_ROOT/usr" | awk '{print $1}')"

cat > "$PKG_ROOT/DEBIAN/control" <<EOF
Package: glide
Version: $VERSION
Section: net
Priority: optional
Architecture: $ARCH
Installed-Size: $INSTALLED_KB
Maintainer: Glide Contributors <glide@local>
Description: Ultra-low-latency cross-platform software KVM over QUIC
 Share one physical mouse, keyboard, clipboard, and files across Windows 11
 and Linux X11 machines over LAN with zero TCP head-of-line blocking, raw
 physical USB HID scancodes (full German/ISO AltGr support), high-resolution
 scrolling, multi-monitor HiDPI awareness, zero-config LAN auto-discovery,
 native system tray integration, and a clean modern desktop GUI.
EOF
chmod 0644 "$PKG_ROOT/DEBIAN/control"

cat > "$PKG_ROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor || true
fi
exit 0
EOF
chmod 0755 "$PKG_ROOT/DEBIAN/postinst"

DEB_PATH="$OUT_DIR/glide_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$PKG_ROOT" "$DEB_PATH"
echo "Built Debian package: $DEB_PATH"
