# Baocast

> *bao* (เบา) is Thai for "light".

The lightest way to stream your games on Windows.

Baocast captures, encodes and sends your gameplay without frames ever leaving
the GPU. It has no scenes, no browser sources and no preview window eating
into your frame rate, and it picks sensible settings for you so it can't be
misconfigured into lagging.

**Status:** v0.1, early but working. Baocast streams a window or monitor, game
audio and your microphone live to Twitch, YouTube or any RTMP server, and can
record to an `.flv` file at the same time. It has been tested live on YouTube
while playing Dota 2: stream health "Excellent", and the game's frame rate (Steam
FPS counter) was the same while streaming as without streaming.

Download `baocast.exe` from [Releases](https://github.com/ohmiler/baocast/releases).
Windows may warn that the file is from an unknown publisher, because it isn't
code-signed yet.

## How it works

```
game window ─► Windows Graphics Capture ─► D3D11 video processor ─► hardware H.264 ─┐
                (GPU texture)               (fit into 16:9 canvas, (NVENC / AMF / QSV │
                                             BGRA→NV12)             via Media Foundation)
                                                                                      ├─► FLV tags ─┬─► RTMP (live)
speakers (WASAPI loopback) ─┐                                                         │             └─► .flv file
                            ├─► mix (48 kHz stereo) ─► AAC (Media Foundation) ────────┘
microphone (WASAPI) ────────┘
```

- Audio packets are placed on the same clock as the video using their capture
  timestamps, so sound and picture stay in sync (about 1 ms apart in our tests).
- The picture always fills a 16:9 canvas; windows of other shapes get black bars,
  and resizing the window mid-stream just works.
- Networking runs on its own thread, so a slow connection never stalls capture.
  When the upload can't keep up, Baocast drops video until the next keyframe and
  keeps the audio. When the connection breaks, it reconnects and resumes at a
  fresh keyframe.

The only dependency is Microsoft's [`windows`](https://crates.io/crates/windows) crate.

Early numbers (RTX 3060 Ti, i7-12700, streaming a 1440p monitor at 1080p60 with
desktop audio): 0.39% CPU, about 77 MB RAM, and a 358 KB executable. In Dota 2
the frame rate didn't drop while streaming. A side-by-side table against OBS
(average FPS and 1% lows) is still to come.

## Usage

```
baocast list                                   # capturable windows and microphones
baocast live --server twitch --window "Valorant"
baocast live --server youtube --window "Valorant" --out backup.flv   # stream and keep a copy
baocast record --window "Valorant" --seconds 60
baocast record --no-mic --desktop-volume 80 --height 720 --fps 30 --out clip.flv
```

`live` asks for your stream key without showing it on screen, or reads it from
the `BAOCAST_STREAM_KEY` environment variable. The key is never printed.

`--server` takes `twitch` (Twitch's own default ingest, which picks the nearest
server), `youtube`, or any `rtmp://host/app` address. RTMPS isn't supported yet.

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
3. Live streaming over RTMP to Twitch / YouTube. ✅ (v0.1)
4. Tray icon, automatic settings, stream health indicator, RTMPS.
5. Optional webcam corner and image overlay.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
