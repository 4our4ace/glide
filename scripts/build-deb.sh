#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${GLIDE_VERSION:-$(awk -F'"' '/^version =/{print $2; exit}' "$ROOT_DIR/Cargo.toml")}"
ARCH="amd64"
BIN_PATH="${1:-$ROOT_DIR/target/release/glide}"
OUT_DIR="$ROOT_DIR/dist"
PKG_ROOT="$(mktemp -d)"

trap 'rm -rf "$PKG_ROOT"' EXIT

if [[ ! -f "$BIN_PATH" ]]; then
    echo "Error: binary not found at $BIN_PATH" >&2
    exit 1
fi

if [[ -z "${SOURCE_DATE_EPOCH:-}" ]]; then
    if command -v git >/dev/null 2>&1 && git -C "$ROOT_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        SOURCE_DATE_EPOCH="$(git -C "$ROOT_DIR" log -1 --format=%ct)"
    else
        SOURCE_DATE_EPOCH="$(date +%s)"
    fi
fi
export SOURCE_DATE_EPOCH

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
install -m 0644 "$ROOT_DIR/assets/logo/glide.svg" "$PKG_ROOT/usr/share/icons/hicolor/scalable/apps/glide.svg"
install -m 0644 "$ROOT_DIR/assets/logo/glide-symbolic.svg" "$PKG_ROOT/usr/share/icons/hicolor/scalable/apps/glide-symbolic.svg"
install -m 0644 "$ROOT_DIR/crates/glide-gui/assets/fonts/OFL-Inter.txt" "$PKG_ROOT/usr/share/doc/glide/OFL-Inter.txt"
install -m 0644 "$ROOT_DIR/crates/glide-gui/assets/fonts/OFL-JetBrainsMono.txt" "$PKG_ROOT/usr/share/doc/glide/OFL-JetBrainsMono.txt"

cat > "$PKG_ROOT/usr/share/doc/glide/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: Glide
Source: https://github.com/4our4ace/glide

Files: *
Copyright: Glide Contributors
License: MIT or Apache-2.0

Files: crates/glide-gui/assets/fonts/inter.woff2
Copyright: 2016 The Inter Project Authors (https://github.com/rsms/inter)
License: OFL-1.1
 Full license text installed at /usr/share/doc/glide/OFL-Inter.txt.

Files: crates/glide-gui/assets/fonts/jetbrains-mono.woff2
Copyright: 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono)
License: OFL-1.1
 Full license text installed at /usr/share/doc/glide/OFL-JetBrainsMono.txt.

License: MIT
 Permission is hereby granted, free of charge, to any person obtaining a copy
 of this software and associated documentation files (the "Software"), to deal
 in the Software without restriction, including without limitation the rights
 to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 copies of the Software, and to permit persons to whom the Software is
 furnished to do so, subject to the following conditions:
 .
 The above copyright notice and this permission notice shall be included in all
 copies or substantial portions of the Software.
 .
 THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 SOFTWARE.

License: Apache-2.0
 Licensed under the Apache License, Version 2.0 (the "License"); you may not
 use this file except in compliance with the License. You may obtain a copy of
 the License at http://www.apache.org/licenses/LICENSE-2.0.
 .
 On Debian systems, the complete text of the Apache License Version 2.0 can be
 found in /usr/share/common-licenses/Apache-2.0.
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
After=graphical-session.target
Conflicts=glide-server.service

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
After=graphical-session.target
Conflicts=glide-client.service

[Service]
Type=simple
ExecStart=/usr/bin/glide server
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
EOF

chmod 0644 \
    "$PKG_ROOT/usr/share/doc/glide/copyright" \
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
Depends: libc6 (>= 2.35), libgcc-s1, libvulkan1, mesa-vulkan-drivers | vulkan-icd, libxkbcommon-x11-0, libxkbcommon0, libwayland-client0
Installed-Size: $INSTALLED_KB
Homepage: https://github.com/4our4ace/glide
Maintainer: Glide Contributors <33606678+4our4ace@users.noreply.github.com>
Description: Ultra-low-latency cross-platform software KVM over QUIC
 Share one physical mouse, keyboard, clipboard, and files across Windows 11
 and Linux (X11 & Wayland) machines over LAN with zero TCP head-of-line blocking, raw
 physical USB HID scancodes (full German/ISO AltGr support), high-resolution
 scrolling, multi-monitor HiDPI awareness, zero-config LAN auto-discovery,
 native system tray integration, and a truly native GPU-rendered desktop GUI.
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

cat > "$PKG_ROOT/DEBIAN/postrm" <<'EOF'
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
chmod 0755 "$PKG_ROOT/DEBIAN/postrm"

(
    cd "$PKG_ROOT"
    find usr -type f -print0 | LC_ALL=C sort -z | xargs -0 md5sum > "$PKG_ROOT/DEBIAN/md5sums"
)
chmod 0644 "$PKG_ROOT/DEBIAN/md5sums"

find "$PKG_ROOT" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +

DEB_PATH="$OUT_DIR/glide_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group -Zxz --build "$PKG_ROOT" "$DEB_PATH"
echo "Built Debian package: $DEB_PATH"
