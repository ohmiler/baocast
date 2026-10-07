# Baocast

> *bao* (เบา) is Thai for "light".

The lightest way to stream your games on Windows.

Baocast captures, encodes and sends your gameplay without frames ever leaving
the GPU. It has no scenes, no browser sources and no preview window eating
into your frame rate, and it picks sensible settings for you so it can't be
misconfigured into lagging.

**Status:** early development. Milestone 1 records a window or monitor to an
`.flv` file. Audio and live streaming come next.

## How it works

```
game window ─► Windows Graphics Capture ─► D3D11 video processor ─► hardware H.264 ─► FLV ─► (RTMP)
                (GPU texture)               (resize + BGRA→NV12)    (NVENC / AMF / QSV
                                                                      via Media Foundation)
```

The only dependency is Microsoft's [`windows`](https://crates.io/crates/windows) crate.

Early numbers (RTX 3060 Ti, i7-12700, recording a 1440p monitor to 1080p60):
0.18% CPU, about 64 MB RAM, and a 225 KB executable. Game frame-rate
benchmarks against OBS are still to come.

## Usage

```
baocast list                                  # show capturable windows
baocast record                                # primary monitor, until Ctrl+C
baocast record --window "Valorant" --seconds 60
baocast record --monitor 1 --height 720 --fps 30 --bitrate 4000 --out clip.flv
```

## Building

Requires Windows 10 2004 or newer and Rust with the MSVC toolchain
(`rustup default stable-msvc` plus the Visual Studio C++ Build Tools).

```
cargo build --release
```

## Roadmap

1. Record a window or monitor to a file (video). ✅
2. Game audio (WASAPI loopback) and microphone.
3. Live streaming over RTMP to Twitch / YouTube, with v0.1 release.
4. Tray icon, automatic settings, stream health indicator.
5. Optional webcam corner and image overlay.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
