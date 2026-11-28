#!/usr/bin/env bash
set -euo pipefail

# Universal Linux Multi-Format Packager (.tar.gz, .deb, .rpm, .AppImage)
# Expects target/release/iphone-mirror to already be compiled.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="$(grep -E '^version = ' Cargo.toml | head -n1 | cut -d'"' -f2)"
BIN="target/release/iphone-mirror"
ICON="resources/AppIcon.png"

if [ ! -x "$BIN" ]; then
  echo "Error: $BIN not found. Run 'cargo build --release --locked' first."
  exit 1
fi

mkdir -p dist build

echo "==> Packaging iPhone Mirror v${VERSION} for Linux (x86_64)..."

# 1. Universal .tar.gz archive + install.sh
TAR_STAGE="build/linux-stage/iPhone-Mirror-v${VERSION}-linux-x86_64"
rm -rf "$TAR_STAGE"
mkdir -p "$TAR_STAGE/resources"
cp "$BIN" "$TAR_STAGE/iphone-mirror"
cp "$ICON" "$TAR_STAGE/resources/AppIcon.png"
cp scripts/install-linux.sh "$TAR_STAGE/install.sh"
cp README.md "$TAR_STAGE/README.md"
chmod +x "$TAR_STAGE/iphone-mirror" "$TAR_STAGE/install.sh"
tar -C "build/linux-stage" -czf "dist/iPhone-Mirror-v${VERSION}-linux-x86_64.tar.gz" "iPhone-Mirror-v${VERSION}-linux-x86_64"
echo "Created dist/iPhone-Mirror-v${VERSION}-linux-x86_64.tar.gz"

# 2. Debian / Ubuntu / Mint .deb package
if command -v dpkg-deb >/dev/null 2>&1; then
  DEB_ROOT="build/deb-root"
  rm -rf "$DEB_ROOT"
  mkdir -p "$DEB_ROOT/DEBIAN" \
           "$DEB_ROOT/usr/bin" \
           "$DEB_ROOT/usr/share/applications" \
           "$DEB_ROOT/usr/share/icons/hicolor/512x512/apps"

  install -m 0755 "$BIN" "$DEB_ROOT/usr/bin/iphone-mirror"
  install -m 0644 "$ICON" "$DEB_ROOT/usr/share/icons/hicolor/512x512/apps/iphone-mirror.png"
  cat > "$DEB_ROOT/usr/share/applications/iphone-mirror.desktop" <<EOF
[Desktop Entry]
Name=iPhone Mirror
Comment=Native iPhone Screen Mirroring & AirPlay Receiver (Wayland & X11)
Exec=/usr/bin/iphone-mirror
Icon=iphone-mirror
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
StartupNotify=true
StartupWMClass=iphone-mirror
EOF
  cat > "$DEB_ROOT/DEBIAN/control" <<EOF
Package: iphone-mirror
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: amd64
Maintainer: iphone-mirror contributors <https://github.com/arifianilhamnrr/iphone-mirror>
Recommends: usbmuxd, avahi-daemon, avahi-utils, ffmpeg, wl-clipboard, xclip, zenity, bluez, libnotify-bin
Description: Native iPhone Screen Mirroring & AirPlay Receiver (Wayland & X11)
 Hardware-friendly iPhone screen mirroring over USB, Wi-Fi (CoreDevice),
 and AirPlay (H.264) across Wayland and X11 Linux desktops.
EOF
  dpkg-deb --build --root-owner-group "$DEB_ROOT" "dist/iphone-mirror_${VERSION}_amd64.deb"
  echo "Created dist/iphone-mirror_${VERSION}_amd64.deb"
else
  echo "Skipping .deb (dpkg-deb not installed)"
fi

# 3. Fedora / RHEL / openSUSE .rpm package
if command -v rpmbuild >/dev/null 2>&1; then
  RPM_TOP="$ROOT_DIR/build/rpmbuild"
  rm -rf "$RPM_TOP"
  mkdir -p "$RPM_TOP"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
  cat > "$RPM_TOP/SPECS/iphone-mirror.spec" <<EOF
Name:           iphone-mirror
Version:        ${VERSION}
Release:        1
Summary:        Native iPhone Screen Mirroring & AirPlay Receiver (Wayland & X11)
License:        GPL-3.0
URL:            https://github.com/arifianilhamnrr/iphone-mirror
BuildArch:      x86_64
Recommends:     usbmuxd, avahi, avahi-tools, ffmpeg, wl-clipboard, xclip, zenity, bluez, libnotify

%description
Native iPhone Screen Mirroring over USB, Wi-Fi (CoreDevice), and AirPlay (H.264)
with automatic Wayland and X11 runtime support and PipeWire/PulseAudio/ALSA audio.

%install
mkdir -p %{buildroot}/usr/bin
mkdir -p %{buildroot}/usr/share/applications
mkdir -p %{buildroot}/usr/share/icons/hicolor/512x512/apps
install -m 0755 "${ROOT_DIR}/${BIN}" %{buildroot}/usr/bin/iphone-mirror
install -m 0644 "${ROOT_DIR}/${ICON}" %{buildroot}/usr/share/icons/hicolor/512x512/apps/iphone-mirror.png
cat > %{buildroot}/usr/share/applications/iphone-mirror.desktop <<'DESKTOP'
[Desktop Entry]
Name=iPhone Mirror
Comment=Native iPhone Screen Mirroring & AirPlay Receiver (Wayland & X11)
Exec=/usr/bin/iphone-mirror
Icon=iphone-mirror
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
StartupNotify=true
StartupWMClass=iphone-mirror
DESKTOP

%files
/usr/bin/iphone-mirror
/usr/share/applications/iphone-mirror.desktop
/usr/share/icons/hicolor/512x512/apps/iphone-mirror.png
EOF
  rpmbuild -bb --define "_topdir $RPM_TOP" "$RPM_TOP/SPECS/iphone-mirror.spec"
  cp "$RPM_TOP/RPMS/x86_64/iphone-mirror-${VERSION}-1.x86_64.rpm" "dist/iphone-mirror-${VERSION}-1.x86_64.rpm"
  echo "Created dist/iphone-mirror-${VERSION}-1.x86_64.rpm"
else
  echo "Skipping .rpm (rpmbuild not installed)"
fi

# 4. Universal Portable AppImage
APPDIR="build/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin"
install -m 0755 "$BIN" "$APPDIR/usr/bin/iphone-mirror"
install -m 0644 "$ICON" "$APPDIR/iphone-mirror.png"
ln -sf iphone-mirror.png "$APPDIR/.DirIcon"
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/iphone-mirror" "$@"
EOF
chmod +x "$APPDIR/AppRun"
cat > "$APPDIR/iphone-mirror.desktop" <<'EOF'
[Desktop Entry]
Name=iPhone Mirror
Comment=Native iPhone Screen Mirroring & AirPlay Receiver (Wayland & X11)
Exec=iphone-mirror
Icon=iphone-mirror
Terminal=false
Type=Application
Categories=Utility;AudioVideo;
StartupNotify=true
StartupWMClass=iphone-mirror
EOF

APPIMAGETOOL=""
if command -v appimagetool >/dev/null 2>&1; then
  APPIMAGETOOL="$(command -v appimagetool)"
elif [ -x "build/appimagetool-x86_64.AppImage" ]; then
  APPIMAGETOOL="build/appimagetool-x86_64.AppImage"
elif command -v curl >/dev/null 2>&1; then
  echo "==> Downloading appimagetool..."
  curl -fsSL -L -o "build/appimagetool-x86_64.AppImage" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage" && \
    chmod +x "build/appimagetool-x86_64.AppImage" && \
    APPIMAGETOOL="build/appimagetool-x86_64.AppImage" || true
fi

if [ -n "$APPIMAGETOOL" ]; then
  ARCH=x86_64 "$APPIMAGETOOL" --appimage-extract-and-run "$APPDIR" "dist/iPhone-Mirror-v${VERSION}-x86_64.AppImage"
  chmod +x "dist/iPhone-Mirror-v${VERSION}-x86_64.AppImage"
  echo "Created dist/iPhone-Mirror-v${VERSION}-x86_64.AppImage"
else
  echo "Skipping .AppImage (appimagetool unavailable)"
fi

(
  cd dist
  files=()
  for f in \
    "iPhone-Mirror-v${VERSION}-linux-x86_64.tar.gz" \
    "iphone-mirror_${VERSION}_amd64.deb" \
    "iphone-mirror-${VERSION}-1.x86_64.rpm" \
    "iPhone-Mirror-v${VERSION}-x86_64.AppImage"; do
    [ -f "$f" ] && files+=("$f")
  done
  if [ "${#files[@]}" -gt 0 ]; then
    sha256sum "${files[@]}" > SHA256SUMS-linux-x86_64.txt
  fi
)
echo "Linux packaging complete."
