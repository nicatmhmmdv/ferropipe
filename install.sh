#!/usr/bin/env bash
# Install Ferropipe for the current user: binary on PATH + desktop launcher + icon.
# Usage: ./install.sh          (builds release if needed, installs to ~/.local)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="${PREFIX:-$HOME/.local}"
bindir="$prefix/bin"
appdir="$prefix/share/applications"
icondir="$prefix/share/icons/hicolor/scalable/apps"

echo "==> Building release binary"
( cd "$here" && cargo build --release )

echo "==> Installing binary to $bindir/ferropipe"
install -Dm755 "$here/target/release/ferropipe" "$bindir/ferropipe"

echo "==> Installing icon"
install -Dm644 "$here/assets/ferropipe.svg" "$icondir/ferropipe.svg"

echo "==> Installing desktop entry"
install -Dm644 "$here/assets/ferropipe.desktop" "$appdir/ferropipe.desktop"

# Refresh caches so it appears in the launcher/search immediately.
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$appdir" || true
command -v gtk-update-icon-cache   >/dev/null 2>&1 && gtk-update-icon-cache -f -t "$prefix/share/icons/hicolor" 2>/dev/null || true

echo
echo "Installed. Launch from your app menu (search 'Ferropipe') or run: ferropipe"
case ":$PATH:" in
  *":$bindir:"*) : ;;
  *) echo "NOTE: $bindir is not on your PATH. Add it with:"
     echo "      echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.profile" ;;
esac
