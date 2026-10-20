# Visor

An offline, open source screen recorder. Record your screen, a window, an app
or an area, with camera and audio; pause and resume; trim; export to MP4, WebM
or GIF. Everything stays on your machine.

Built in Rust with [Slint](https://slint.dev) (native UI, no webview) and
FFmpeg. See [DESIGN.md](DESIGN.md) for the architecture and roadmap.

## Building (macOS)

```sh
brew install ffmpeg pkgconf
cargo test
cargo run -p visor
```

## License

GPL-3.0-or-later.
