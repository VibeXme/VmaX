#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' 'Usage: bash install.sh [--deps] [--appimage | --rpm]' \
    'Build VmaX from this source checkout and install it for your user.' \
    '  --deps   Install system build dependencies on Fedora or Debian/Ubuntu.' \
    '  --appimage   Build a portable VmaX AppImage in this directory.' \
    '  --rpm        Build a Fedora VmaX RPM in this directory.' \
    'Rust and a Node.js version supported by the locked frontend are required.'
}

install_deps=false
build_appimage=false
build_rpm=false
for option in "$@"; do
  case "$option" in
    --help|-h) usage; exit 0 ;;
    --deps) install_deps=true ;;
    --appimage) build_appimage=true ;;
    --rpm) build_rpm=true ;;
    *) usage >&2; exit 2 ;;
  esac
done

fail() {
  printf 'VmaX: %s\n' "$*" >&2
  exit 1
}

if "$build_appimage" && "$build_rpm"; then
  fail 'Choose either --appimage or --rpm.'
fi

[[ $(uname -s) == Linux ]] || fail 'Run this installer on Linux.'
(( EUID != 0 )) || fail 'Run this as your regular desktop user. Only --deps uses sudo.'
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
[[ -f src-tauri/Cargo.lock && -f package-lock.json ]] || fail 'Keep install.sh in the VmaX source directory.'

if "$install_deps"; then
  command -v sudo >/dev/null || fail '--deps requires sudo.'
  if command -v dnf >/dev/null; then
    sudo dnf install gcc gcc-c++ make pkgconf-pkg-config file webkit2gtk4.1-devel \
      libxdo-devel openssl-devel libappindicator-gtk3-devel librsvg2-devel alsa-lib-devel dbus-devel systemd-devel
  elif command -v apt-get >/dev/null; then
    sudo apt-get update
    sudo apt-get install build-essential pkg-config file libwebkit2gtk-4.1-dev \
      libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev libasound2-dev libdbus-1-dev libudev-dev
  else
    fail '--deps requires dnf or apt-get. Install equivalent libraries for your distribution manually.'
  fi
fi

for tool in cargo rustc node npm pkg-config; do
  command -v "$tool" >/dev/null || fail "Install $tool and make sure it is on PATH."
done
pkg-config --exists webkit2gtk-4.1 alsa dbus-1 libudev || fail 'Install WebKitGTK 4.1, ALSA, D-Bus, and libudev development libraries. On Fedora or Debian/Ubuntu, use bash install.sh --deps.'

# A private target directory keeps this build separate from existing Windows/Android builds
# and makes the native output path independent of the user's Cargo configuration.
build_dir=$(mktemp -d)
trap 'rm -rf -- "$build_dir"' EXIT
export CARGO_TARGET_DIR="$build_dir/target"
export CARGO_BUILD_TARGET
CARGO_BUILD_TARGET=$(rustc -vV | sed -n 's/^host: //p')
[[ $CARGO_BUILD_TARGET == *-linux-* ]] || fail 'A native Linux Rust toolchain is required.'

npm ci
if "$build_rpm"; then
  npm run tauri -- build --ci --bundles rpm --target "$CARGO_BUILD_TARGET" -- --locked
  shopt -s nullglob
  bundles=("$CARGO_TARGET_DIR/$CARGO_BUILD_TARGET/release/bundle/rpm/"*.rpm)
  (( ${#bundles[@]} == 1 )) || fail 'Expected exactly one RPM from the Linux build.'
  output=${bundles[0]##*/}
  install -m 644 "${bundles[0]}" "$output"
  printf 'Built %s/%s\n' "$PWD" "$output"
  exit 0
fi
if "$build_appimage"; then
  APPIMAGE_EXTRACT_AND_RUN=1 npm run tauri -- build --ci --bundles appimage --target "$CARGO_BUILD_TARGET" -- --locked
  shopt -s nullglob
  bundles=("$CARGO_TARGET_DIR/$CARGO_BUILD_TARGET/release/bundle/appimage/"*.AppImage)
  (( ${#bundles[@]} == 1 )) || fail 'Expected exactly one AppImage from the Linux build.'
  output="VmaX-${CARGO_BUILD_TARGET%%-*}.AppImage"
  install -m 755 "${bundles[0]}" "$output"
  printf 'Built %s/%s\n' "$PWD" "$output"
  exit 0
fi
npm run tauri -- build --ci --no-bundle --target "$CARGO_BUILD_TARGET" -- --locked
binary="$CARGO_TARGET_DIR/$CARGO_BUILD_TARGET/release/vmax"
[[ -x $binary ]] || fail 'The build completed without producing the VmaX executable.'

data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
install -D -m 755 "$binary" "$HOME/.local/bin/vmax"
install -D -m 644 public/vmax-icon.png "$data_dir/icons/hicolor/512x512/apps/com.vmax.app.png"
mkdir -p -- "$data_dir/applications"

# Exec uses desktop-entry quoting rather than shell quoting.
exec_path=$(printf '%s' "$HOME/.local/bin/vmax" | sed 's/[\\`"$]/\\\\\\&/g; s/%/%%/g')
cat > "$data_dir/applications/com.vmax.app.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=VmaX
Comment=Connect to a VmaX streaming host
Exec="$exec_path"
Icon=com.vmax.app
Terminal=false
Categories=Network;
EOF

printf 'Installed VmaX. Launch it from your app menu or run "%s".\n' "$HOME/.local/bin/vmax"
