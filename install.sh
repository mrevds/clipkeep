#!/bin/sh
# clipkeep installer for Linux and macOS. Downloads the latest ready-made release (no Rust needed),
# puts it into ~/.local/bin and runs `clipkeep setup` (autostart, hotkey, icon).
#
#   curl -fsSL https://raw.githubusercontent.com/OWNER/clipkeep/main/install.sh | sh
#
# Options (environment): CLIPKEEP_BIN_DIR=/other/dir   CLIPKEEP_NO_SETUP=1   CLIPKEEP_REPO=owner/name
set -eu

REPO="${CLIPKEEP_REPO:-OWNER/clipkeep}"
BIN_DIR="${CLIPKEEP_BIN_DIR:-$HOME/.local/bin}"
BASE_URL="${CLIPKEEP_BASE_URL:-https://github.com/$REPO/releases/latest/download}"

say() { printf '%s\n' "$*"; }
die() { printf 'clipkeep install: %s\n' "$*" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
    Linux-x86_64 | Linux-amd64) target=linux-x86_64 ;;
    Darwin-arm64 | Darwin-aarch64) target=macos-arm64 ;;
    Darwin-x86_64) target=macos-x86_64 ;;
    *) die "no ready-made build for $os $arch yet. Build from source: cargo install --git https://github.com/$REPO" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
url="$BASE_URL/clipkeep-$target.tar.gz"
say "Downloading $url"
if command -v curl >/dev/null 2>&1; then
    curl -fL --progress-bar "$url" -o "$tmp/clipkeep.tar.gz" || die "download failed"
elif command -v wget >/dev/null 2>&1; then
    wget -q --show-progress -O "$tmp/clipkeep.tar.gz" "$url" || die "download failed"
else
    die "need curl or wget"
fi
tar -xzf "$tmp/clipkeep.tar.gz" -C "$tmp" || die "the download is not a valid archive"
src="$tmp/clipkeep-$target"
[ -x "$src/clipkeep" ] || die "the archive has no clipkeep binary"

mkdir -p "$BIN_DIR"
# A running daemon keeps the old version; stop it, `setup` starts the new one.
if [ -x "$BIN_DIR/clipkeep" ]; then
    "$BIN_DIR/clipkeep" stop >/dev/null 2>&1 || true
fi
for b in clipkeep clipkeepw; do
    cp "$src/$b" "$BIN_DIR/.$b.new"
    chmod 755 "$BIN_DIR/.$b.new"
    mv -f "$BIN_DIR/.$b.new" "$BIN_DIR/$b" # atomic: a running copy is never half-written
done
if [ "$os" = Darwin ]; then
    # Files fetched by a browser are quarantined by Gatekeeper; ours came via curl, but be sure.
    xattr -d com.apple.quarantine "$BIN_DIR/clipkeep" "$BIN_DIR/clipkeepw" 2>/dev/null || true
fi
say "Installed: $BIN_DIR/clipkeep ($("$BIN_DIR/clipkeep" --version 2>/dev/null || echo '?'))"

if [ -z "${CLIPKEEP_NO_SETUP:-}" ]; then
    "$BIN_DIR/clipkeep" setup
fi

case ":$PATH:" in
    *":$BIN_DIR:"*)
        found=$(command -v clipkeep 2>/dev/null || true)
        if [ -n "$found" ] && [ "$found" != "$BIN_DIR/clipkeep" ]; then
            say ""
            say "Note: another clipkeep comes first in PATH: $found (remove it, or the terminal runs the old one)."
        fi
        ;;
    *)
        say ""
        say "To run clipkeep from a terminal, add this line to ~/.profile (or ~/.zshrc on macOS):"
        say "    export PATH=\"$BIN_DIR:\$PATH\""
        ;;
esac
