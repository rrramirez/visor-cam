# Linux build/test environment for Visor. Debian trixie ships FFmpeg 7.1 and
# PipeWire 1.4. Used by scripts/linux-check.sh; the repository is mounted at
# /src rather than copied in.
FROM debian:trixie

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git \
        build-essential pkg-config clang libclang-dev \
        libavcodec-dev libavformat-dev libavutil-dev libavdevice-dev \
        libavfilter-dev libswscale-dev libswresample-dev \
        libpipewire-0.3-dev libspa-0.2-dev \
        libasound2-dev libdbus-1-dev \
        libfontconfig1-dev libfreetype-dev libxkbcommon-dev libxkbcommon-x11-dev \
        libwayland-dev libx11-dev libx11-xcb-dev libxcb1-dev libxcursor-dev \
        libxrandr-dev libxi-dev libgl-dev libegl-dev libgles-dev \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
        --default-toolchain stable --component clippy,rustfmt \
    && chmod -R a+w $RUSTUP_HOME $CARGO_HOME

WORKDIR /src
