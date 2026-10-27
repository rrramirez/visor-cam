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

if $publish; then
    # GitHub tags the release at what is on GitHub, so that must be this.
    [ -z "$(git status --porcelain)" ] || { echo "error: commit your changes first" >&2; exit 1; }
    git fetch -q origin main
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
        { echo "error: push main first (git push)" >&2; exit 1; }
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
# Sparkle's update window shows the GitHub release notes.
notes="https://github.com/$repo/releases/tag/$tag"
grep -q "<sparkle:releaseNotesLink>$notes<" site/appcast.xml ||
    perl -0pi -e "s|(<sparkle:version>\Q$version\E</sparkle:version>)|\$1\n            <sparkle:releaseNotesLink>$notes</sparkle:releaseNotesLink>|" site/appcast.xml

# A copy with a fixed name, so the site's button can always link to
# releases/latest/download/Visor.dmg.
stable="$root/target/release-assets/Visor.dmg"
mkdir -p "$(dirname "$stable")"
cp "$dmg" "$stable"

if $publish; then
    echo "==> Publishing $tag"
    # The release (and its files) first, then the feed that points at them.
    gh release create "$tag" "$dmg" "$stable" --repo "$repo" --title "Visor $version" --generate-notes
    scripts/deploy-site.sh
    git add site/appcast.xml
    git commit -q -m "Release $version: update feed" \
        -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_013s7xb2wD6CNckNdeb1rn2s"
    git push -q origin main
    echo "Released $tag"
else
    echo "Next: scripts/release-macos.sh --publish   (or by hand:)"
    echo "      gh release create $tag $dmg $stable --repo $repo --title \"Visor $version\" --generate-notes"
    echo "      scripts/deploy-site.sh   (after committing site/appcast.xml)"
fi
