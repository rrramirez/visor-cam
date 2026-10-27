#!/usr/bin/env bash
# Cross-checks the Windows build from macOS (clippy, no linking):
#
#   scripts/windows-check.sh            # clippy -D warnings for x86_64-pc-windows-msvc
#   scripts/windows-check.sh <cargo args>
#
# Fetches into target/windows-kit (once): BtbN's FFmpeg Windows build (shared,
# checked against the checksum GitHub publishes) and MSYS2's MinGW-w64 C and
# winpthreads headers, which bindgen needs to parse FFmpeg's headers for
# Windows. Neither is run; they are only read by the compiler.
#
# Requires: rustup target add x86_64-pc-windows-msvc, gh, zstd (Homebrew).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
kit="$root/target/windows-kit"
ffmpeg_asset="ffmpeg-n8.1-latest-win64-gpl-shared-8.1.zip"

if [ ! -f "$kit/ffmpeg/include/libavutil/avutil.h" ]; then
    echo "==> Fetching FFmpeg for Windows ($ffmpeg_asset)"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    digest="$(gh api repos/BtbN/FFmpeg-Builds/releases/tags/latest \
        --jq ".assets[] | select(.name==\"$ffmpeg_asset\") | .digest" | sed 's/^sha256://')"
    [ -n "$digest" ] || { echo "error: $ffmpeg_asset not found in BtbN's latest release" >&2; exit 1; }
    curl -sSfL -o "$tmp/ffmpeg.zip" \
        "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$ffmpeg_asset"
    echo "$digest  $tmp/ffmpeg.zip" | shasum -a 256 -c - >/dev/null || {
        echo "error: FFmpeg download checksum mismatch" >&2
        exit 1
    }
    rm -rf "$kit/ffmpeg"
    mkdir -p "$kit"
    unzip -q "$tmp/ffmpeg.zip" -d "$tmp/x"
    mv "$tmp"/x/ffmpeg-* "$kit/ffmpeg"
fi

if [ ! -f "$kit/mingw/mingw64/include/pthread_time.h" ]; then
    echo "==> Fetching MinGW-w64 headers"
    tmp2="$(mktemp -d)"
    index="$(curl -sSfL https://repo.msys2.org/mingw/mingw64/)"
    rm -rf "$kit/mingw"
    mkdir -p "$kit/mingw"
    # The C headers, and winpthreads' (MinGW's time.h includes pthread_time.h).
    for name in headers-git winpthreads-git; do
        pkg="$(grep -o "mingw-w64-x86_64-$name-[0-9][^\"<]*\.pkg\.tar\.zst" <<<"$index" | sort -uV | tail -1)"
        curl -sSfL -o "$tmp2/$name.tar.zst" "https://repo.msys2.org/mingw/mingw64/$pkg"
        zstd -dc "$tmp2/$name.tar.zst" | tar -x -C "$kit/mingw"
    done
    rm -rf "$tmp2"
fi

headers="$kit/mingw/mingw64/include"
export FFMPEG_DIR="$kit/ffmpeg"
export BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-w64-mingw32 -D__SCE__ -isystem $headers"
export CARGO_TARGET_DIR="$root/target/windows-check"
cd "$root"
if [ $# -eq 0 ]; then
    set -- clippy -q --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
fi
cargo "$@"
echo "Windows check passed"
