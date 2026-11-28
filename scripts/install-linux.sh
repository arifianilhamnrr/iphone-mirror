#!/usr/bin/env bash
set -euo pipefail

# Universal Linux Installer for iPhone Mirror (Wayland & X11, All Major Distros)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." 2>/dev/null && pwd || echo "${SCRIPT_DIR}")"

BIN_SRC=""
for candidate in \
  "${SCRIPT_DIR}/iphone-mirror" \
  "${ROOT_DIR}/target/release/iphone-mirror" \
  "${ROOT_DIR}/iphone-mirror"; do
  if [ -x "$candidate" ]; then
    BIN_SRC="$candidate"
    break
  fi
done

if [ -z "$BIN_SRC" ]; then
  echo "Error: Could not find compiled 'iphone-mirror' binary."
  echo "Build it first with: cargo build --release --locked"
  exit 1
fi

ICON_SRC=""
for candidate in \
  "${SCRIPT_DIR}/resources/AppIcon.png" \
  "${ROOT_DIR}/resources/AppIcon.png"; do
  if [ -f "$candidate" ]; then
    ICON_SRC="$candidate"
    break
  fi
done

echo "==> Installing runtime dependencies (usbmuxd, avahi, ffmpeg, clipboard, zenity)..."
install_deps() {
  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update
    sudo apt-get install -y usbmuxd libimobiledevice-utils avahi-daemon avahi-utils ffmpeg wl-clipboard xclip zenity curl
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y usbmuxd libimobiledevice-utils avahi avahi-tools ffmpeg-free wl-clipboard xclip zenity curl || \
      sudo dnf install -y usbmuxd avahi avahi-tools wl-clipboard xclip zenity curl
  elif command -v pacman >/dev/null 2>&1; then
    sudo pacman -Sy --needed --noconfirm usbmuxd libimobiledevice avahi ffmpeg wl-clipboard xclip zenity curl
  elif command -v zypper >/dev/null 2>&1; then
    sudo zypper install -y usbmuxd avahi avahi-utils ffmpeg wl-clipboard xclip zenity curl
  elif command -v apk >/dev/null 2>&1; then
    sudo apk add usbmuxd avahi avahi-tools ffmpeg wl-clipboard xclip zenity curl
  else
    echo "Notice: Unknown package manager. Please ensure these runtime tools are installed:"
    echo "  usbmuxd, avahi-daemon + avahi-utils, ffmpeg, wl-clipboard (Wayland) / xclip (X11), zenity"
  fi
}

if [ "${IPHONE_MIRROR_SKIP_DEPS:-0}" != "1" ]; then
  install_deps
fi

if command -v systemctl >/dev/null 2>&1; then
  echo "==> Ensuring usbmuxd and avahi-daemon services are active..."
  sudo systemctl enable --now usbmuxd >/dev/null 2>&1 || true
  sudo systemctl enable --now avahi-daemon >/dev/null 2>&1 || true
fi

PREFIX="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$PREFIX/applications"
ICON_DIR="$PREFIX/icons/hicolor/512x512/apps"

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"
install -m 0755 "$BIN_SRC" "$BIN_DIR/iphone-mirror"

if [ -n "$ICON_SRC" ]; then
  install -m 0644 "$ICON_SRC" "$ICON_DIR/iphone-mirror.png"
fi

rm -f "$APP_DIR/iphone-mirror-airplay.desktop"

cat > "$APP_DIR/iphone-mirror.desktop" <<EOF
[Desktop Entry]
Name=iPhone Mirror
Comment=Native iPhone Screen Mirroring & AirPlay Receiver (Wayland & X11)
Exec=$BIN_DIR/iphone-mirror
Icon=iphone-mirror
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
StartupNotify=true
StartupWMClass=iphone-mirror
EOF

chmod +x "$APP_DIR/iphone-mirror.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true

echo ""
echo "Installation complete!"
echo "  Binary:   $BIN_DIR/iphone-mirror"
echo "  Desktop:  $APP_DIR/iphone-mirror.desktop"
echo ""
echo "Run 'iphone-mirror' from your application menu or terminal."
