#!/usr/bin/env bash
# Builds dist/Visor.app and dist/Visor-<version>.dmg.
#
#   scripts/package-macos.sh
#
# Links a minimal FFmpeg built by scripts/build-ffmpeg-macos.sh (only what
# Visor uses) instead of Homebrew's full FFmpeg, which keeps the app small,
# and embeds Sparkle (scripts/fetch-sparkle.sh) for updates.
#
# Signing: with a "Developer ID Application" certificate in the keychain (or
# VISOR_SIGN_IDENTITY set to one), everything is signed with it and, if a
# notarytool keychain profile named "visor-notary" exists (or
# VISOR_NOTARY_PROFILE names one), the app and DMG are notarized and stapled.
# Without a certificate the app is ad-hoc signed: on other Macs it must be
# opened with right-click → Open the first time.
#
# Requires: Rust, Xcode command line tools, Homebrew `x264 libvpx opus
# svt-av1 pkgconf`.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
bundle_id="com.rrramirez.visor"
dist="$root/dist"
app="$dist/Visor.app"

identity="${VISOR_SIGN_IDENTITY:-}"
if [ -z "$identity" ]; then
    identity="$(security find-identity -v -p codesigning 2>/dev/null |
        sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)"
fi
notary_profile="${VISOR_NOTARY_PROFILE:-visor-notary}"

echo "==> Building minimal FFmpeg"
scripts/build-ffmpeg-macos.sh
ffmpeg_prefix="$root/target/ffmpeg-min/prefix"

echo "==> Fetching Sparkle"
scripts/fetch-sparkle.sh
sparkle="$root/target/sparkle/Sparkle.framework"

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
# ditto keeps the framework's symlinks. Visor isn't sandboxed, so Sparkle's
# XPC services (only needed by sandboxed apps) are left out.
ditto "$sparkle" "$app/Contents/Frameworks/Sparkle.framework"
rm -rf "$app/Contents/Frameworks/Sparkle.framework/Versions/B/XPCServices" \
    "$app/Contents/Frameworks/Sparkle.framework/XPCServices"

# The hardened runtime stops other code from being injected into Visor (for
# example with DYLD_INSERT_LIBRARIES) and riding its screen, camera and
# microphone permissions.
entitlements="packaging/macos/Visor.entitlements"
if [ -n "$identity" ]; then
    echo "==> Signing with $identity"
    sign=(codesign --force --timestamp --options runtime --sign "$identity")
else
    echo "==> Signing (ad-hoc; no Developer ID certificate found)"
    sign=(codesign --force --timestamp=none --options runtime --sign -)
    # Ad-hoc code has no Team ID, so library validation would refuse the
    # bundled dylibs; a Developer ID build keeps library validation on.
    entitlements="$(mktemp -d)/Visor.entitlements"
    cp packaging/macos/Visor.entitlements "$entitlements"
    /usr/libexec/PlistBuddy -c "Add :com.apple.security.cs.disable-library-validation bool true" "$entitlements"
fi
# Inside out: nested code first, the app last.
find "$app/Contents/Frameworks" -maxdepth 1 -name '*.dylib' -exec "${sign[@]}" {} \;
fw="$app/Contents/Frameworks/Sparkle.framework/Versions/B"
"${sign[@]}" "$fw/Autoupdate"
"${sign[@]}" "$fw/Updater.app"
"${sign[@]}" "$app/Contents/Frameworks/Sparkle.framework"
"${sign[@]}" --entitlements "$entitlements" --identifier "$bundle_id" "$app"
codesign --verify --deep --strict "$app"

echo "==> Checking for unbundled libraries"
if otool -L "$app/Contents/MacOS/visor" "$app"/Contents/Frameworks/*.dylib | grep -E '^\s+/(opt|usr/local|Users)/'; then
    echo "error: the bundle still references libraries outside it" >&2
    exit 1
fi

notarize() {
    xcrun notarytool submit "$1" --keychain-profile "$notary_profile" --wait
}
can_notarize=false
if [ -n "$identity" ] && xcrun notarytool history --keychain-profile "$notary_profile" >/dev/null 2>&1; then
    can_notarize=true
    echo "==> Notarizing the app"
    zip="$(mktemp -d)/Visor.zip"
    ditto -c -k --keepParent "$app" "$zip"
    notarize "$zip"
    xcrun stapler staple "$app"
fi

echo "==> Creating DMG"
staging="$(mktemp -d)"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
dmg="$dist/Visor-$version.dmg"
rm -f "$dmg"
hdiutil create -quiet -volname "Visor" -srcfolder "$staging" -ov -format UDZO "$dmg"
rm -rf "$staging"
if [ -n "$identity" ]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
fi
if $can_notarize; then
    echo "==> Notarizing the DMG"
    notarize "$dmg"
    xcrun stapler staple "$dmg"
    spctl --assess --type open --context context:primary-signature -v "$dmg"
fi

du -sh "$app" "$dmg"
echo "Done: $dmg"
