#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="0.1.0"
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
    "$PKG_ROOT/usr/share/doc/glide/examples"
chmod 0755 "$PKG_ROOT" "$PKG_ROOT/DEBIAN" "$PKG_ROOT/usr" "$PKG_ROOT/usr/bin" \
    "$PKG_ROOT/usr/lib" "$PKG_ROOT/usr/lib/systemd" "$PKG_ROOT/usr/lib/systemd/user" \
    "$PKG_ROOT/usr/share" "$PKG_ROOT/usr/share/doc" "$PKG_ROOT/usr/share/doc/glide" \
    "$PKG_ROOT/usr/share/doc/glide/examples"

install -m 0755 "$BIN_PATH" "$PKG_ROOT/usr/bin/glide"
install -m 0644 "$ROOT_DIR/README.md" "$PKG_ROOT/usr/share/doc/glide/README.md"
install -m 0644 "$ROOT_DIR/ARCHITECTURE.md" "$PKG_ROOT/usr/share/doc/glide/ARCHITECTURE.md"
install -m 0644 "$ROOT_DIR/examples/glide.toml" "$PKG_ROOT/usr/share/doc/glide/examples/glide.toml"

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
 Share one physical mouse, keyboard, and clipboard across Windows 11 and
 Linux X11 machines over LAN with zero TCP head-of-line blocking, raw
 physical USB HID scancodes (full German/ISO AltGr support), high-resolution
 scrolling, multi-monitor HiDPI awareness, and zero-config LAN auto-discovery.
EOF

DEB_PATH="$OUT_DIR/glide_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$PKG_ROOT" "$DEB_PATH"
echo "Built Debian package: $DEB_PATH"
