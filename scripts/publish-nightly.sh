#!/usr/bin/env bash
# Fast local cross-build + publish helper for the rolling `nightly` GitHub prerelease.
# Builds Linux (binary + .deb) and standalone static-CRT Windows (.exe) with embedded
# `GLIDE_BUILD_CHANNEL=nightly` and 7-char commit SHA, generates SHA256SUMS.txt, and
# updates the rolling `nightly` prerelease without touching the Stable `Latest` tag.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

SHORT_SHA="$(git rev-parse --short=7 HEAD)"
OUT_DIR="$ROOT_DIR/dist/nightly"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

JOBS="${CARGO_BUILD_JOBS:-2}"
export PATH="$HOME/.cargo/bin:$PATH"
if [ ! -d "$(rustc --print sysroot)/lib/rustlib/x86_64-pc-windows-msvc" ] && [ -d "$HOME/.cache/rustup" ]; then
    export RUSTUP_HOME="$HOME/.cache/rustup"
fi
export XWIN_CACHE_DIR="${XWIN_CACHE_DIR:-$HOME/.cache/xwin}"

echo "==> Building Glide Nightly (${SHORT_SHA}) with CARGO_BUILD_JOBS=${JOBS}..."

# 1. Linux release binary + .deb
GLIDE_BUILD_CHANNEL=nightly GLIDE_BUILD_SHA="$SHORT_SHA" \
    CARGO_BUILD_JOBS="$JOBS" cargo build --release --locked --bin glide
cp target/release/glide "$OUT_DIR/glide-linux-x86_64-standalone"
./scripts/build-deb.sh target/release/glide
DEB_PATH="$(ls -t dist/glide_*_amd64.deb | head -n 1)"
cp "$DEB_PATH" "$OUT_DIR/"
DEB_FILE="$(basename "$DEB_PATH")"

# 2. Windows standalone static-CRT .exe via cargo-xwin
GLIDE_BUILD_CHANNEL=nightly GLIDE_BUILD_SHA="$SHORT_SHA" \
    CARGO_BUILD_JOBS="$JOBS" \
    CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="-C target-feature=+crt-static" \
    cargo-xwin build --release --locked --bin glide --target x86_64-pc-windows-msvc
cp target/x86_64-pc-windows-msvc/release/glide.exe "$OUT_DIR/glide-windows-x86_64-standalone.exe"

# 3. Checksums
(cd "$OUT_DIR" && sha256sum glide-windows-x86_64-standalone.exe glide-linux-x86_64-standalone "$DEB_FILE" > SHA256SUMS.txt)
cat "$OUT_DIR/SHA256SUMS.txt"

# 4. Update rolling nightly tag + GitHub Prerelease (never --latest)
git tag -f nightly HEAD
git push -f origin refs/tags/nightly

RECENT_COMMITS="$(git log -n 10 --pretty=format:'- `%h` %s')"
NOTES_FILE="$OUT_DIR/nightly-notes.md"
cat > "$NOTES_FILE" <<EOF
Rolling development build from the \`nightly\` branch.

Commit: \`${SHORT_SHA}\`

Switch to **Settings → Update channel → Nightly** inside Glide to receive automatic update alerts and one-click verified **Update & Restart** (or switch back to **Stable** anytime).

### Recent commits
${RECENT_COMMITS}

### Downloads
- **Windows 11 Standalone (Portable):** \`glide-windows-x86_64-standalone.exe\` *(no installer or extra DLLs needed)*
- **Ubuntu / Debian Linux (X11 & Wayland):** \`${DEB_FILE}\`
- **Linux Standalone (Portable):** \`glide-linux-x86_64-standalone\`
- **Checksums:** \`SHA256SUMS.txt\`
EOF

ASSETS=("$OUT_DIR/glide-windows-x86_64-standalone.exe" "$OUT_DIR/glide-linux-x86_64-standalone" "$OUT_DIR/$DEB_FILE" "$OUT_DIR/SHA256SUMS.txt")
if gh release view nightly >/dev/null 2>&1; then
    gh release upload nightly "${ASSETS[@]}" --clobber
    gh release edit nightly \
        --title "Glide Nightly (${SHORT_SHA})" \
        --notes-file "$NOTES_FILE" \
        --prerelease \
        --latest=false
else
    gh release create nightly "${ASSETS[@]}" \
        --title "Glide Nightly (${SHORT_SHA})" \
        --notes-file "$NOTES_FILE" \
        --prerelease \
        --latest=false
fi

echo "==> Published Glide Nightly (${SHORT_SHA})!"
