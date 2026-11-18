#!/usr/bin/env bash
set -e
cd "$(dirname "$0")"

echo "== ClipForge installer (Arch Linux) =="

echo "Installing dependencies via pacman (may require sudo)..."
sudo pacman -S --needed --noconfirm \
    rust gtk4 libadwaita \
    gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav gst-plugin-gtk4 gtk4-layer-shell \
    ffmpeg pkgconf

cargo build --release

install -Dm755 target/release/clipforge "$HOME/.local/bin/clipforge"
ICONS="$HOME/.local/share/icons/hicolor"
install -Dm644 data/se.local.clipforge.svg "$ICONS/scalable/apps/se.local.clipforge.svg"
if command -v rsvg-convert >/dev/null; then
    for size in 16 24 32 48 64 128 256 512; do
        mkdir -p "$ICONS/${size}x${size}/apps"
        rsvg-convert -w $size -h $size data/se.local.clipforge.svg -o "$ICONS/${size}x${size}/apps/se.local.clipforge.png"
    done
fi
gtk-update-icon-cache -q -t "$ICONS" 2>/dev/null || true

sed "s|^Exec=.*|Exec=$HOME/.local/bin/clipforge|" clipforge.desktop > "$HOME/.local/share/applications/clipforge.desktop.tmp"
mkdir -p "$HOME/.local/share/applications"
mv "$HOME/.local/share/applications/clipforge.desktop.tmp" "$HOME/.local/share/applications/clipforge.desktop"

echo
echo "== Installed =="
echo "Launch ClipForge from your app menu, or run: clipforge"
echo "(make sure ~/.local/bin is on your PATH)"
echo "Default clips folder is ~/Videos; change it from the menu in the app."
