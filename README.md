# MilerCast

The lightest way to stream your games on Windows.

(Formerly Baocast. Release v0.1.0 still carries the old name.)

MilerCast captures, encodes and sends your gameplay without frames ever leaving
the GPU. It has no scenes, no browser sources and no preview window eating
into your frame rate, and it picks sensible settings for you so it can't be
misconfigured into lagging.

**Status:** early but working. MilerCast streams a window or monitor, game audio
and your microphone live to Twitch, YouTube or any RTMP server, and can record to
an `.flv` file at the same time. It has been tested live on YouTube while
playing Dota 2: stream health "Excellent", and the game's frame rate (Steam FPS
counter) was the same while streaming as without streaming.

Download from [Releases](https://github.com/ohmiler/milercast/releases). Windows
may warn that the file is from an unknown publisher, because it isn't
code-signed yet.

## The window

Double-click `milercast.exe`. There are three screens, and most days you only
need the first one.

**Home** has everything you touch before going live:

- **Capture:** the fullscreen game is picked automatically, or choose any window or screen.
- **Stream to:** YouTube, Twitch or a custom RTMP server. **Add key** opens a small
  window to paste your stream key, with a **Where's my key?** button that opens
  the right page on YouTube or Twitch. The key is kept in Windows Credential
  Manager, encrypted with your Windows login, and never shown again.
- **Sound:** game and mic switches with live level meters.
- **Camera:** your webcam in a corner. Pick the corner and size (small, medium,
  large) right there. It's off until you switch it on, so the camera light never
  comes on by surprise.
- A big **Go live** button, and **Record** for a local file.

Problems show up as a line of text under the buttons ("No fullscreen game
found…", "the server refused the stream key"), not as pop-ups.

**Live panel** replaces Home while you're live or recording: the time, upload
rate, dropped frames and connection status, big mic / game / camera switches, and
**End stream**. While it runs, these hotkeys work from inside your game:

| Hotkey | Does |
| --- | --- |
| Ctrl+Alt+M | Mute or unmute the mic |
| Ctrl+Alt+G | Mute or unmute game sound |
| Ctrl+Alt+C | Show or hide the camera |

The hotkeys are only taken while you're live or recording, so they never clash
with other programs (or switch your camera on) the rest of the time. A tray icon
shows the live time in its tooltip, and right-clicking it mutes or ends the
stream without opening the window.

**Settings** holds what you set once: custom server, microphone, mic and game
volume, camera device and mirror, quality preset (1080p60 at 6 Mbps is the
default), saving a copy of every stream, and the recordings folder
(`Videos\MilerCast`). Changes apply immediately; volumes and mirror can change
while live.

The window speaks English or Thai, following your Windows display language
(set `MILERCAST_LANG=th` or `en` to choose). It uses plain Windows controls and
nothing on the GPU: about 0.08% CPU and 22 MB of RAM while open in front. The
level meters only run while the window is in front and not minimized.

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
- The webcam is read in an uncompressed format near 720p (NV12, YUY2 or RGB), so
  nothing needs decoding, and it's drawn as a second layer in the same
  video-processor pass. With an Elgato Facecam Pro at 720p it adds about 0.3% CPU.
  Cameras that only exist as DirectShow devices (OBS Virtual Camera, NVIDIA
  Broadcast) don't show up yet.
- Networking runs on its own thread, so a slow connection never stalls capture.
  When the upload can't keep up, MilerCast drops video until the next keyframe and
  keeps the audio. When the connection breaks, it reconnects and resumes at a
  fresh keyframe.

The only dependency is Microsoft's [`windows`](https://crates.io/crates/windows) crate.

The engine is a library shared by the window (`milercast.exe`) and the command
line (`milercast-cli.exe`).

Early numbers (RTX 3060 Ti, i7-12700, streaming a 1440p monitor at 1080p60 with
desktop audio): 0.39% CPU, about 77 MB RAM, and executables under 500 KB. In
Dota 2 the frame rate didn't drop while streaming. A side-by-side table against
OBS (average FPS and 1% lows) is still to come.

## Command line

```
milercast-cli list                                   # capturable windows and microphones
milercast-cli live --server twitch --window "Valorant"
milercast-cli live --server youtube --window "Valorant" --out backup.flv   # stream and keep a copy
milercast-cli record --window "Valorant" --seconds 60
milercast-cli record --no-mic --desktop-volume 80 --height 720 --fps 30 --out clip.flv
milercast-cli live --server youtube --window "Valorant" --camera "Facecam" --camera-corner bl --mirror
```

`live` asks for your stream key without showing it on screen, or reads it from
the `MILERCAST_STREAM_KEY` environment variable. The key is never printed.

`--server` takes `twitch` (Twitch's own default ingest, which picks the nearest
server), `youtube`, or any `rtmp://host/app` address. RTMPS isn't supported yet.

Run `milercast-cli` without arguments to see every option.

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
4. A window: game auto-detection, saved key, audio meters, presets, live status. ✅
5. Webcam corner. ✅ (Capture cards as the main source are next.)
6. A simpler window: Home / Live / Settings, stream key helper, tray icon, hotkeys. ✅
7. A small preview (only while the window is open), upload speed test, mic test.
8. Game-only audio, RTMPS, automatic bitrate.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
