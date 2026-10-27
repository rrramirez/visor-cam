#!/usr/bin/env bash
# Builds dist/Visor.app (ad-hoc signed) and dist/Visor-<version>.dmg.
#
#   scripts/package-macos.sh
#
# Links a minimal FFmpeg built by scripts/build-ffmpeg-macos.sh (only what
# Visor uses) instead of Homebrew's full FFmpeg, which keeps the app small.
#
# Requires: Rust, Xcode command line tools, Homebrew `x264 libvpx opus
# svt-av1 pkgconf`.
# The app is ad-hoc signed, not notarized: on other Macs open it with
# right-click → Open the first time.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
dist="$root/dist"
app="$dist/Visor.app"

echo "==> Building minimal FFmpeg"
scripts/build-ffmpeg-macos.sh
ffmpeg_prefix="$root/target/ffmpeg-min/prefix"

echo "==> Building visor $version (release)"
# A separate target dir keeps dev builds linked against Homebrew's FFmpeg.
export CARGO_TARGET_DIR="$root/target/package"
PKG_CONFIG_PATH="$ffmpeg_prefix/lib/pkgconfig" PKG_CONFIG_LIBDIR="$ffmpeg_prefix/lib/pkgconfig" \
    cargo build --release -p visor

echo "==> Assembling $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Frameworks"
cp "$CARGO_TARGET_DIR/release/visor" "$app/Contents/MacOS/visor"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp LICENSE "$app/Contents/Resources/LICENSE" 2>/dev/null || true

echo "==> Rendering icon"
iconset="$(mktemp -d)/Visor.iconset"
swift packaging/macos/make-icon.swift "$iconset"
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Visor.icns"

echo "==> Bundling libraries"
python3 packaging/macos/bundle_dylibs.py "$app/Contents/MacOS/visor" "$app/Contents/Frameworks" "$ffmpeg_prefix/lib"

echo "==> Signing (ad-hoc, hardened runtime)"
# The hardened runtime stops other code from being injected into Visor (for
# example with DYLD_INSERT_LIBRARIES) and riding its screen, camera and
# microphone permissions.
find "$app/Contents/Frameworks" -name '*.dylib' -exec codesign --force --sign - --timestamp=none {} \;
codesign --force --sign - --timestamp=none --options runtime \
    --entitlements packaging/macos/Visor.entitlements --identifier dev.visor.Visor "$app"
codesign --verify --strict "$app"

echo "==> Checking for unbundled libraries"
if otool -L "$app/Contents/MacOS/visor" "$app"/Contents/Frameworks/*.dylib | grep -E '^\s+/(opt|usr/local|Users)/'; then
    echo "error: the bundle still references libraries outside it" >&2
    exit 1
fi

echo "==> Creating DMG"
staging="$(mktemp -d)"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
dmg="$dist/Visor-$version.dmg"
rm -f "$dmg"
hdiutil create -quiet -volname "Visor" -srcfolder "$staging" -ov -format UDZO "$dmg"
rm -rf "$staging"

du -sh "$app" "$dmg"
echo "Done: $dmg"
