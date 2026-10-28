#!/usr/bin/env bash
# Builds a minimal FFmpeg for packaging Visor on macOS: shared libraries with
# only the codecs, formats and filters Visor uses, and the codec libraries
# (x264, libvpx, opus) linked in statically. SVT-AV1 stays a separate dylib
# (Homebrew ships no static build; it depends only on libSystem). The result
# replaces Homebrew's full FFmpeg (and its ~90 dependencies) in Visor.app.
#
#   scripts/build-ffmpeg-macos.sh      # installs into target/ffmpeg-min/prefix
#
# Requires Homebrew `x264 libvpx opus svt-av1 pkgconf` (for their static .a files)
# and the Xcode command line tools.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="7.1.2"
work="$root/target/ffmpeg-min"
src="$work/ffmpeg-$version"
prefix="$work/prefix"
brew_prefix="$(brew --prefix)"

# Rebuilt whenever this script (the configuration) changes.
stamp="$prefix/.built-by"
script_hash="$(shasum -a 256 "$0" | cut -d' ' -f1)"
if [ -f "$prefix/lib/pkgconfig/libavcodec.pc" ] && [ "$(cat "$stamp" 2>/dev/null)" = "$script_hash" ] &&
    [ "${1:-}" != "--force" ]; then
    echo "Minimal FFmpeg already built in $prefix (pass --force to rebuild)"
    exit 0
fi

mkdir -p "$work"
if [ ! -d "$src" ]; then
    echo "==> Downloading FFmpeg $version"
    curl -sSfL "https://ffmpeg.org/releases/ffmpeg-$version.tar.xz" -o "$work/ffmpeg.tar.xz"
    tar -xf "$work/ffmpeg.tar.xz" -C "$work"
fi

# Expose only the static archives so the linker can't pick the dylibs.
static="$work/static-libs"
rm -rf "$static"
mkdir -p "$static/lib/pkgconfig"
formula() { if [ "$1" = vpx ]; then echo libvpx; else echo "$1"; fi; }
for lib in x264 vpx opus; do
    ln -sf "$(brew --prefix "$(formula "$lib")")/lib/lib$lib.a" "$static/lib/lib$lib.a"
done
# pkg-config files that describe static linking against those archives.
cat > "$static/lib/pkgconfig/x264.pc" <<EOF
Name: x264
Description: H.264 encoder
Version: 0.164
Libs: -L$static/lib -lx264 -lpthread -lm
Cflags: -I$(brew --prefix x264)/include
EOF
cat > "$static/lib/pkgconfig/vpx.pc" <<EOF
Name: vpx
Description: VP8/VP9 codec
Version: 1.15.2
Libs: -L$static/lib -lvpx -lm -lpthread
Cflags: -I$(brew --prefix libvpx)/include
EOF
cat > "$static/lib/pkgconfig/opus.pc" <<EOF
Name: opus
Description: Opus codec
Version: 1.5.2
Libs: -L$static/lib -lopus -lm
Cflags: -I$(brew --prefix opus)/include/opus
EOF
cp "$(brew --prefix svt-av1)/lib/pkgconfig/SvtAv1Enc.pc" "$static/lib/pkgconfig/"

echo "==> Configuring"
cd "$src"
make distclean >/dev/null 2>&1 || true
PKG_CONFIG_PATH="$static/lib/pkgconfig" PKG_CONFIG_LIBDIR="$static/lib/pkgconfig" ./configure \
    --prefix="$prefix" \
    --enable-shared --disable-static \
    --enable-gpl --enable-version3 \
    --disable-programs --disable-doc --disable-debug --disable-postproc \
    --disable-network --disable-autodetect --disable-everything \
    --enable-pthreads --enable-videotoolbox --enable-audiotoolbox --enable-avfoundation \
    --enable-libx264 --enable-libvpx --enable-libopus --enable-libsvtav1 \
    --enable-protocol=file \
    --enable-indev=avfoundation \
    --enable-demuxer=mov,matroska,gif,aac \
    --enable-muxer=mp4,ipod,mov,webm,matroska,gif \
    --enable-decoder=h264,aac,rawvideo,vp9,opus,gif \
    --enable-encoder=libx264,h264_videotoolbox,aac,libvpx_vp9,libsvtav1,libopus,gif,mjpeg \
    --enable-parser=h264,aac,vp9,opus,av1 \
    --enable-hwaccel=h264_videotoolbox \
    --enable-bsfs \
    --enable-filter=buffer,buffersink,abuffer,abuffersink,format,aformat,scale,aresample,atempo,highpass,afftdn,loudnorm,null,anull,palettegen,paletteuse,split \
    --enable-swscale --enable-swresample \
    --extra-ldflags="-Wl,-headerpad_max_install_names" \
    --install-name-dir='@rpath'

echo "==> Building"
make -j"$(sysctl -n hw.ncpu)"
rm -rf "$prefix"
make install
echo "$script_hash" >"$stamp"

echo "==> Done"
du -sh "$prefix/lib"/*.dylib | sort -h | tail -8
