#!/usr/bin/env bash
# Builds a release and updates the Sparkle feed:
#
#   scripts/release-macos.sh            # package, sign the update, write site/appcast.xml
#   scripts/release-macos.sh --publish  # ...and create the GitHub release, deploy the site
#
# The version comes from Cargo.toml; bump it first. Refuses to release a build
# that isn't signed with a Developer ID and notarized unless --adhoc is given
# (for testing only: macOS asks ad-hoc builds for permissions again after
# every update). The update itself is signed with the Sparkle key in the
# keychain (generate_keys --account com.rrramirez.visor).
set -euo pipefail

publish=false
adhoc=false
for arg in "$@"; do
    case "$arg" in
        --publish) publish=true ;;
        --adhoc) adhoc=true ;;
        *) echo "usage: $0 [--publish] [--adhoc]" >&2; exit 2 ;;
    esac
done

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
repo="rrramirez/visor-cam"
tag="v$version"

scripts/package-macos.sh
dmg="dist/Visor-$version.dmg"

if ! $adhoc; then
    codesign -dv dist/Visor.app 2>&1 | grep -q "Authority=Developer ID Application" || {
        echo "error: not signed with a Developer ID (pass --adhoc to release anyway)" >&2
        exit 1
    }
    xcrun stapler validate "$dmg" >/dev/null || {
        echo "error: the DMG isn't notarized (pass --adhoc to release anyway)" >&2
        exit 1
    }
fi

echo "==> Signing the update and writing site/appcast.xml"
archives="$root/target/releases"
mkdir -p "$archives"
rm -f "$archives"/*.dmg
cp "$dmg" "$archives/"
target/sparkle/bin/generate_appcast \
    --account com.rrramirez.visor \
    --download-url-prefix "https://github.com/$repo/releases/download/$tag/" \
    --link "https://visor-cam.pages.dev/" \
    -o site/appcast.xml "$archives"

if $publish; then
    echo "==> Publishing $tag"
    gh release create "$tag" "$dmg" --repo "$repo" --title "Visor $version" --generate-notes
    scripts/deploy-site.sh
else
    echo "Next: gh release create $tag $dmg --repo $repo --title \"Visor $version\" --generate-notes"
    echo "      scripts/deploy-site.sh   (after committing site/appcast.xml)"
fi
