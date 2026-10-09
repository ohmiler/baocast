# Baocast

> *bao* (เบา) is Thai for "light".

The lightest way to stream your games on Windows.

Baocast captures, encodes and sends your gameplay without frames ever leaving
the GPU. It has no scenes, no browser sources and no preview window eating
into your frame rate, and it picks sensible settings for you so it can't be
misconfigured into lagging.

**Status:** early development. Baocast records a window or monitor, game audio
and your microphone to an `.flv` file. Live streaming comes next.

## How it works

```
game window ─► Windows Graphics Capture ─► D3D11 video processor ─► hardware H.264 ─┐
                (GPU texture)               (resize + BGRA→NV12)    (NVENC / AMF / QSV │
                                                                     via Media Foundation)
                                                                                      ├─► FLV ─► (RTMP)
speakers (WASAPI loopback) ─┐                                                         │
                            ├─► mix (48 kHz stereo) ─► AAC (Media Foundation) ────────┘
microphone (WASAPI) ────────┘
```

Audio packets are placed on the same clock as the video using their capture
timestamps, so sound and picture stay in sync (about 1 ms apart in our tests).

The only dependency is Microsoft's [`windows`](https://crates.io/crates/windows) crate.

Early numbers (RTX 3060 Ti, i7-12700, recording a 1440p monitor to 1080p60 with
desktop audio and microphone): 0.13% CPU, about 70 MB RAM, and a 252 KB
executable. Game frame-rate benchmarks against OBS are still to come.

## Usage

```
baocast list                                   # capturable windows and microphones
baocast record                                 # primary monitor, game audio + mic, until Ctrl+C
baocast record --window "Valorant" --seconds 60
baocast record --window "Valorant" --mic "MV7" --mic-volume 150
baocast record --no-mic --desktop-volume 80 --height 720 --fps 30 --out clip.flv
```

Run `baocast` without arguments to see every option.

## Building

Requires Windows 10 2004 or newer and Rust with the MSVC toolchain
(`rustup default stable-msvc` plus the Visual Studio C++ Build Tools).

```
cargo build --release
```

## Roadmap

1. Record a window or monitor to a file (video). ✅
2. Game audio (WASAPI loopback) and microphone. ✅
3. Live streaming over RTMP to Twitch / YouTube, with v0.1 release.
4. Tray icon, automatic settings, stream health indicator.
5. Optional webcam corner and image overlay.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
