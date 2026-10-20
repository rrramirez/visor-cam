#!/usr/bin/env bash
# Builds dist/Visor.app (ad-hoc signed, FFmpeg bundled) and dist/Visor-<version>.dmg.
#
#   scripts/package-macos.sh
#
# Requires: Rust, Xcode command line tools, Homebrew `ffmpeg` and `pkgconf`.
# The app is ad-hoc signed, not notarized: on other Macs open it with
# right-click → Open the first time.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
dist="$root/dist"
app="$dist/Visor.app"

echo "==> Building visor $version (release)"
cargo build --release -p visor

echo "==> Assembling $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Frameworks"
cp target/release/visor "$app/Contents/MacOS/visor"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
cp LICENSE "$app/Contents/Resources/LICENSE" 2>/dev/null || true

echo "==> Rendering icon"
iconset="$(mktemp -d)/Visor.iconset"
swift packaging/macos/make-icon.swift "$iconset"
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Visor.icns"

echo "==> Bundling libraries"
python3 packaging/macos/bundle_dylibs.py "$app/Contents/MacOS/visor" "$app/Contents/Frameworks"

echo "==> Signing (ad-hoc)"
find "$app/Contents/Frameworks" -name '*.dylib' -exec codesign --force --sign - --timestamp=none {} \;
codesign --force --sign - --timestamp=none --identifier dev.visor.Visor "$app"
codesign --verify --strict "$app"

echo "==> Checking for unbundled libraries"
if otool -L "$app/Contents/MacOS/visor" "$app"/Contents/Frameworks/*.dylib | grep -E '^\s+/(opt|usr/local)/'; then
    echo "error: the bundle still references Homebrew paths" >&2
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
