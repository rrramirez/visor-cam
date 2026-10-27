#!/usr/bin/env bash
# Builds a minimal FFmpeg for Visor on Windows: shared DLLs with only the
# codecs, formats and filters Visor uses, with x264, libvpx and Opus linked in
# statically. Mirrors scripts/build-ffmpeg-macos.sh. Run in an MSYS2 MINGW64
# shell (the Windows workflow does), with these packages installed:
#   mingw-w64-x86_64-{gcc,nasm,pkgconf,x264,libvpx,opus,svt-av1,ffnvcodec-headers,amf-headers,libvpl}
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

echo "==> Configuring"
# --pkg-config-flags=--static and -static link the codec libraries and the
# MinGW runtime into the DLLs, so they only depend on Windows itself.
./configure \
    --prefix="$prefix" \
    --enable-shared --disable-static \
    --enable-gpl --enable-version3 \
    --disable-programs --disable-doc --disable-debug --disable-postproc \
    --disable-network --disable-autodetect --disable-everything \
    --pkg-config-flags=--static \
    --extra-ldflags="-static" \
    --enable-w32threads \
    --enable-libx264 --enable-libvpx --enable-libopus --enable-libsvtav1 \
    --enable-ffnvcodec --enable-nvenc --enable-amf --enable-libvpl \
    --enable-protocol=file \
    --enable-indev=dshow \
    --enable-demuxer=mov,matroska,gif,aac \
    --enable-muxer=mp4,ipod,mov,webm,matroska,gif \
    --enable-decoder=h264,aac,rawvideo,vp9,opus,gif,mjpeg \
    --enable-encoder=libx264,h264_nvenc,h264_amf,h264_qsv,aac,libvpx_vp9,libsvtav1,libopus,gif \
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
# MSYS2 may only ship SVT-AV1 as a DLL (Homebrew does too); ship it alongside.
if objdump -p "$prefix"/bin/avcodec-*.dll | grep -qi "DLL Name: libSvtAv1Enc"; then
    cp "$(dirname "$(command -v gcc)")"/libSvtAv1Enc*.dll "$prefix"/bin/
fi

echo "==> Checking DLL dependencies"
# Anything beyond Windows system DLLs and FFmpeg's own would have to ship
# alongside; fail instead so it gets noticed.
bad=0
for dll in "$prefix"/bin/*.dll; do
    while read -r dep; do
        case "${dep,,}" in
            av*.dll | sw*.dll | libsvtav1enc*.dll) ;;
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
