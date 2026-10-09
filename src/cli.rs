//! milercast-cli: MilerCast from the command line.

use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleCtrlHandler,
    SetConsoleMode,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::BOOL;

use milercast::audio::{self, Gain};
use milercast::capture;
use milercast::engine::{Engine, Mic, Phase, Settings, State, Video};
use milercast::rtmp::{Monitor, Status};

const USAGE: &str = "\
milercast-cli - MilerCast from the command line

USAGE:
  milercast-cli list                 List windows and microphones you can use
  milercast-cli record [OPTIONS]     Record to an .flv file
  milercast-cli live --server <s>    Go live; s = twitch, youtube or an rtmp:// address.
                                   The stream key is asked for (hidden) or read from
                                   the MILERCAST_STREAM_KEY environment variable.

VIDEO:
  --window <text>          Capture the first window whose title contains <text>
  --monitor <n>            Capture monitor n (0 = primary, the default)
  --fps <n>                Output frame rate (default 60)
  --height <n>             Output height of the 16:9 picture (default 1080)
  --bitrate <kbps>         Video bitrate (default 6000)

AUDIO (game sound and microphone are both on by default):
  --no-desktop-audio       Don't record what plays on your speakers/headphones
  --no-mic                 Don't record a microphone
  --mic <text>             Use the microphone whose name contains <text>
  --desktop-volume <pct>   Desktop audio volume, 0-200 (default 100)
  --mic-volume <pct>       Microphone volume, 0-200 (default 100)
  --audio-bitrate <kbps>   96, 128, 160 (default) or 192

OUTPUT:
  --seconds <n>            Stop after n seconds (default: when you press Ctrl+C)
  --out <file>             Recording file (record: default recordings\\milercast-<date>-<time>.flv;
                           live: also keep a local copy)
";

static STOP: AtomicBool = AtomicBool::new(false);

enum VideoChoice {
    Monitor(usize),
    Window(String),
}

struct Options {
    video: VideoChoice,
    fps: u32,
    height: u32,
    bitrate_kbps: u32,
    desktop_audio: bool,
    mic: bool,
    mic_name: Option<String>,
    desktop_volume: u32,
    mic_volume: u32,
    audio_bitrate_kbps: u32,
    seconds: Option<u64>,
    out: Option<PathBuf>,
    server: Option<String>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("record") => session(parse_options(&args[1..])?),
        Some("live") => {
            let options = parse_options(&args[1..])?;
            if options.server.is_none() {
                return Err("live needs --server (twitch, youtube or an rtmp:// address)".into());
            }
            session(options)
        }
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        video: VideoChoice::Monitor(0),
        fps: 60,
        height: 1080,
        bitrate_kbps: 6000,
        desktop_audio: true,
        mic: true,
        mic_name: None,
        desktop_volume: 100,
        mic_volume: 100,
        audio_bitrate_kbps: 160,
        seconds: None,
        out: None,
        server: None,
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--window" => o.video = VideoChoice::Window(value()?.clone()),
            "--monitor" => o.video = VideoChoice::Monitor(number(value()?)?),
            "--fps" => o.fps = number(value()?)?,
            "--height" => o.height = number(value()?)?,
            "--bitrate" => o.bitrate_kbps = number(value()?)?,
            "--no-desktop-audio" => o.desktop_audio = false,
            "--no-mic" => o.mic = false,
            "--mic" => o.mic_name = Some(value()?.clone()),
            "--desktop-volume" => o.desktop_volume = number(value()?)?,
            "--mic-volume" => o.mic_volume = number(value()?)?,
            "--audio-bitrate" => o.audio_bitrate_kbps = number(value()?)?,
            "--seconds" => o.seconds = Some(number(value()?)?),
            "--out" => o.out = Some(PathBuf::from(value()?)),
            "--server" => o.server = Some(value()?.clone()),
            other => return Err(format!("unknown option {other}\n\n{USAGE}")),
        }
    }
    if o.fps == 0 {
        return Err("--fps must be at least 1".into());
    }
    if o.height < 144 {
        return Err("--height must be at least 144".into());
    }
    if o.desktop_volume > 200 || o.mic_volume > 200 {
        return Err("volumes go from 0 to 200".into());
    }
    if !milercast::aac::BITRATES_KBPS.contains(&o.audio_bitrate_kbps) {
        return Err(format!("--audio-bitrate must be one of {:?}", milercast::aac::BITRATES_KBPS));
    }
    Ok(o)
}

fn number<T: FromStr>(text: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("'{text}' is not a valid number"))
}

fn list() -> Result<(), Box<dyn Error>> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    println!("Windows:");
    for (_, title) in capture::list_windows() {
        println!("  {title}");
    }
    println!("\nMicrophones:");
    let default = audio::default_microphone().map(|d| d.name).unwrap_or_default();
    for mic in audio::microphones()? {
        let marker = if mic.name == default { "  (default)" } else { "" };
        println!("  {}{marker}", mic.name);
    }
    Ok(())
}

/// Records, streams, or both, until Ctrl+C or --seconds.
fn session(o: Options) -> Result<(), Box<dyn Error>> {
    // Asked before anything else so the prompt isn't buried in output.
    let key = match o.server {
        Some(_) => Some(stream_key()?),
        None => None,
    };
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl_c), true)? };

    let video = match &o.video {
        VideoChoice::Window(query) => {
            let (hwnd, title) = capture::find_window(query)
                .ok_or(format!("no window title contains '{query}' (see: milercast-cli list)"))?;
            println!("Source : window \"{title}\"");
            Video::window(hwnd)
        }
        VideoChoice::Monitor(index) => {
            let monitor = *capture::monitors().get(*index).ok_or(format!("there is no monitor {index}"))?;
            println!("Source : monitor {index}");
            Video::monitor(monitor)
        }
    };
    let record_to = match (&o.out, &o.server) {
        (Some(path), _) => Some(path.clone()),
        (None, None) => Some(default_path()),
        (None, Some(_)) => None,
    };
    if let Some(server) = &o.server {
        println!("Live   : connecting to {} ...", milercast::rtmp::Rtmp::describe(server));
    }
    let engine = Engine::start(Settings {
        video,
        height: o.height,
        fps: o.fps,
        video_kbps: o.bitrate_kbps,
        desktop_audio: o.desktop_audio.then(|| Gain::new(o.desktop_volume as f32 / 100.0)),
        mic: o.mic.then(|| Mic { name: o.mic_name.clone(), gain: Gain::new(o.mic_volume as f32 / 100.0) }),
        audio_kbps: o.audio_bitrate_kbps,
        live: o.server.clone().zip(key),
        record_to,
        stop_after: o.seconds.map(Duration::from_secs),
    });

    let state = engine.state().clone();
    let mut printed = 0;
    let mut stats = Stats::default();
    let mut announced = false;
    let mut ticks = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if STOP.swap(false, Ordering::Relaxed) {
            engine.stop();
        }
        let info = state.info();
        for line in &info[printed..] {
            println!("{line}");
        }
        printed = info.len();
        match state.phase() {
            Phase::Starting => {}
            Phase::Running => {
                if !announced {
                    println!("Press Ctrl+C to stop.\n");
                    announced = true;
                }
                ticks += 1;
                if ticks % 10 == 0 {
                    stats.print(&state);
                }
            }
            Phase::Finished(_) => break,
        }
    }
    if announced {
        stats.print(&state);
        println!();
    }
    engine.join()?;
    Ok(())
}

/// From MILERCAST_STREAM_KEY, or typed in without showing it on screen.
fn stream_key() -> Result<String, Box<dyn Error>> {
    if let Ok(key) = std::env::var("MILERCAST_STREAM_KEY") {
        if !key.trim().is_empty() {
            return Ok(key.trim().to_string());
        }
    }
    print!("Stream key (hidden): ");
    std::io::stdout().flush()?;
    let mut key = String::new();
    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE)?;
        let mut mode = CONSOLE_MODE::default();
        let is_console = GetConsoleMode(input, &mut mode).is_ok();
        if is_console {
            let _ = SetConsoleMode(input, CONSOLE_MODE(mode.0 & !ENABLE_ECHO_INPUT.0));
        }
        let read = std::io::stdin().read_line(&mut key);
        if is_console {
            let _ = SetConsoleMode(input, mode);
        }
        read?;
    }
    println!();
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("no stream key given".into());
    }
    Ok(key)
}

fn default_path() -> PathBuf {
    let t = unsafe { GetLocalTime() };
    PathBuf::from(format!(
        "recordings\\milercast-{:04}{:02}{:02}-{:02}{:02}{:02}.flv",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    ))
}

unsafe extern "system" fn on_ctrl_c(_: u32) -> BOOL {
    STOP.store(true, Ordering::Relaxed);
    true.into()
}

#[derive(Default)]
struct Stats {
    /// For the upload rate: bytes the network had sent at the last report.
    last_sent: u64,
}

impl Stats {
    fn print(&mut self, state: &State) {
        let secs = state.elapsed().as_secs();
        let (desktop, mic) = state.take_peaks();
        let peak = desktop.max(mic);
        let audio = if peak < 0.001 { "silent".to_string() } else { format!("{:>3.0} dB", 20.0 * peak.log10()) };
        let live = match state.network() {
            None => String::new(),
            Some(net) => self.live(&net),
        };
        print!(
            "\r{:02}:{:02}  frames {}  new {}  skipped {}  audio {audio}{live}  {:.1} MB   ",
            secs / 60,
            secs % 60,
            state.encoded.load(Ordering::Relaxed),
            state.captured.load(Ordering::Relaxed),
            state.skipped.load(Ordering::Relaxed),
            state.bytes.load(Ordering::Relaxed) as f64 / 1_048_576.0
        );
        let _ = std::io::stdout().flush();
    }

    fn live(&mut self, net: &Monitor) -> String {
        let sent = net.sent_bytes();
        let mbps = (sent - self.last_sent) as f64 * 8.0 / 1_000_000.0;
        self.last_sent = sent;
        let state = match net.status() {
            Status::Live => format!("LIVE {mbps:.1} Mbps"),
            Status::Connecting => "connecting".into(),
            Status::Reconnecting(why) => format!("RECONNECTING ({why})"),
        };
        format!("  {state}  dropped {}", net.dropped_frames())
    }
}
