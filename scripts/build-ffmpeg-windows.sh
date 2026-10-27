#!/usr/bin/env bash
# Builds a minimal FFmpeg for Visor on Windows: shared DLLs with only the
# codecs, formats and filters Visor uses, with x264, libvpx, Opus and libaom
# linked in statically. (AV1 uses libaom here: MSYS2's SVT-AV1 is newer than
# FFmpeg 7.1 supports.) Mirrors scripts/build-ffmpeg-macos.sh. Run in an
# MSYS2 MINGW64 shell (the Windows workflow does), with these packages:
#   mingw-w64-x86_64-{gcc,nasm,pkgconf,x264,libvpx,opus,aom,ffnvcodec-headers,amf-headers}
#   make diffutils
#
#   scripts/build-ffmpeg-windows.sh <prefix>
#
# Installs DLLs to <prefix>/bin, MSVC import libraries (*.lib) to <prefix>/lib
# and headers to <prefix>/include, which is what FFMPEG_DIR expects.
set -euo pipefail

version="7.1.2"
sha256="089bc60fb59d6aecc5d994ff530fd0dcb3ee39aa55867849a2bbc4e555f9c304"
prefix="$(realpath -m "$1")"
work="$(mktemp -d)"

echo "==> Downloading FFmpeg $version"
curl -sSfL -o "$work/ffmpeg.tar.xz" "https://ffmpeg.org/releases/ffmpeg-$version.tar.xz"
echo "$sha256  $work/ffmpeg.tar.xz" | sha256sum -c - >/dev/null || {
    echo "error: FFmpeg download checksum mismatch" >&2
    exit 1
}
tar -xf "$work/ffmpeg.tar.xz" -C "$work"
cd "$work/ffmpeg-$version"

# Expose only the codec libraries' static archives (and winpthreads, which
# x264 needs), so they link into FFmpeg's DLLs while FFmpeg's own libraries
# still link to each other as DLLs. (A global -static would also make the
# linker pick up MSYS2's own FFmpeg libraries instead of the ones built here.)
static="$work/static"
mkdir -p "$static/lib/pkgconfig"
for lib in x264 vpx opus aom winpthread; do
    cp "/mingw64/lib/lib$lib.a" "$static/lib/"
done
pc() { # name version libs cflags
    printf 'Name: %s\nDescription: %s\nVersion: %s\nLibs: -L%s/lib %s\nCflags: %s\n' \
        "$1" "$1" "$2" "$static" "$3" "$4" > "$static/lib/pkgconfig/$1.pc"
}
# -l:<file> names the archive itself: plain -lvpx lets the linker choose the
# DLL import library where both exist.
pc x264 "$(pkg-config --modversion x264)" "-l:libx264.a -l:libwinpthread.a" "-I/mingw64/include"
pc vpx "$(pkg-config --modversion vpx)" "-l:libvpx.a -l:libwinpthread.a" "-I/mingw64/include"
pc opus "$(pkg-config --modversion opus)" "-l:libopus.a" "-I/mingw64/include/opus"
pc aom "$(pkg-config --modversion aom)" "-l:libaom.a -l:libwinpthread.a" "-I/mingw64/include"
# NVENC needs only headers; its .pc has no libraries.
cp /mingw64/lib/pkgconfig/ffnvcodec.pc "$static/lib/pkgconfig/"

echo "==> Configuring"
PKG_CONFIG_LIBDIR="$static/lib/pkgconfig" ./configure \
    --prefix="$prefix" \
    --enable-shared --disable-static \
    --enable-gpl --enable-version3 \
    --disable-programs --disable-doc --disable-debug --disable-postproc \
    --disable-network --disable-autodetect --disable-everything \
    --extra-ldflags="-static-libgcc" \
    --enable-w32threads \
    --enable-libx264 --enable-libvpx --enable-libopus --enable-libaom \
    --enable-ffnvcodec --enable-nvenc --enable-amf --enable-mediafoundation \
    --enable-protocol=file \
    --enable-indev=dshow \
    --enable-demuxer=mov,matroska,gif,aac \
    --enable-muxer=mp4,ipod,mov,webm,matroska,gif \
    --enable-decoder=h264,aac,rawvideo,vp9,opus,gif,mjpeg \
    --enable-encoder=libx264,h264_nvenc,h264_amf,h264_mf,aac,libvpx_vp9,libaom_av1,libopus,gif \
    --enable-parser=h264,aac,vp9,opus,av1,mjpeg \
    --enable-bsfs \
    --enable-filter=buffer,buffersink,abuffer,abuffersink,format,aformat,scale,aresample,atempo,null,anull,palettegen,paletteuse,split \
    --enable-swscale --enable-swresample

echo "==> Building"
make -j"$(nproc)"
rm -rf "$prefix"
make install
# The MSVC import libraries are installed next to the DLLs; FFMPEG_DIR users
# (ffmpeg-sys) look in lib/.
mv "$prefix"/bin/*.lib "$prefix"/lib/

echo "==> Checking DLL dependencies"
# Anything beyond Windows system DLLs and FFmpeg's own would have to ship
# alongside; fail instead so it gets noticed.
bad=0
for dll in "$prefix"/bin/*.dll; do
    while read -r dep; do
        case "${dep,,}" in
            av*.dll | sw*.dll) ;;
            kernel32.dll | user32.dll | ole32.dll | oleaut32.dll | advapi32.dll | bcrypt.dll | \
                shell32.dll | shlwapi.dll | gdi32.dll | strmiids.dll | uuid.dll | psapi.dll | \
                msvcrt.dll | ucrtbase.dll | api-ms-win-*.dll | ws2_32.dll | secur32.dll | \
                mfplat.dll | mf.dll | mfuuid.dll | d3d11.dll | dxgi.dll | dxva2.dll | \
                vfw32.dll | avicap32.dll | winmm.dll | crypt32.dll) ;;
            *) echo "error: $(basename "$dll") depends on $dep" >&2; bad=1 ;;
        esac
    done < <(objdump -p "$dll" | sed -n 's/^\s*DLL Name: //p')
done
[ "$bad" = 0 ]
ls -la "$prefix"/bin/*.dll "$prefix"/lib/*.lib
