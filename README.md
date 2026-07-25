# Repadio

Cross-platform **audio & video** player (H.264 / H.265 / AV1 + common audio codecs).  
Reference app for the [Repose](https://github.com/mlm-games/repose) GUI stack. Runs on **Linux, macOS, Windows, Android, and Web (WASM).**

**Demo:** https://mlm-games.github.io/repadio/

## Features

- Audio via Symphonia (MP3, FLAC, OGG/Opus, WAV, AAC, …) + CPAL output  
- Video via [videoson](https://github.com/mlm-games/videoson) (H.264, H.265, AV1/rav1d) → NV12 GPU path  
- A/V sync (audio clock master; silence injection for video-only files)  
- Playlist, metadata / album art, native file pickers  
- Desktop, Android (intents), Web (OPFS + COOP/COEP)

## Build

Requires the pinned nightly toolchain (`rust-toolchain.toml`).

```bash
# Desktop
cargo run -p repadio --bin repadio-desktop -- /path/to/media

# Web (Trunk)
trunk serve

# Android (example ABI)
cargo rapk build --target aarch64-linux-android --lib
```

Linux deps: libasound2-dev, libudev-dev, X11/fontconfig packages (see `.github/workflows/ci.yml`).

## License

GPL-3.0-or-later. see [LICENSE](LICENSE).
