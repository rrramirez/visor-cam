#!/usr/bin/env bash
# Downloads Sparkle (macOS app updates) into target/sparkle, pinned to a
# version and checksum.
#
#   scripts/fetch-sparkle.sh
#
# target/sparkle/Sparkle.framework is embedded in Visor.app; target/sparkle/bin
# has generate_keys, sign_update and generate_appcast.
set -euo pipefail

version="2.10.0"
sha256="c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c"

root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/target/sparkle"
if [ -f "$dest/.version" ] && [ "$(cat "$dest/.version")" = "$version" ]; then
    exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "==> Downloading Sparkle $version"
curl -sSfL -o "$tmp/sparkle.tar.xz" \
    "https://github.com/sparkle-project/Sparkle/releases/download/$version/Sparkle-$version.tar.xz"
echo "$sha256  $tmp/sparkle.tar.xz" | shasum -a 256 -c - >/dev/null || {
    echo "error: Sparkle download checksum mismatch" >&2
    exit 1
}
rm -rf "$dest"
mkdir -p "$dest"
tar -xf "$tmp/sparkle.tar.xz" -C "$dest"
echo "$version" > "$dest/.version"
echo "Sparkle $version in $dest"
