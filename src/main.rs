//! Baocast: the lightest way to stream your games on Windows.
//!
//! Records a window or monitor plus game audio and microphone to an FLV file.
//! Video never leaves the GPU:
//! capture (WGC) -> resize + NV12 (video processor) -> H.264 (hardware MFT) -> FLV.
//! Audio: WASAPI (desktop loopback + mic) -> mix -> AAC -> the same FLV.

mod aac;
mod audio;
mod capture;
mod convert;
mod encoder;
mod flv;
mod gpu;

use std::error::Error;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::BOOL;

use aac::AacEncoder;
use audio::{Mixer, Source as AudioSource, Timeline};
use capture::{Capture, Target};
use convert::Converter;
use encoder::{Encoder, Event};
use flv::FlvWriter;
use gpu::Gpu;

const USAGE: &str = "\
Baocast - lightweight game recorder/streamer

USAGE:
  baocast list              List windows and microphones you can use
  baocast record [OPTIONS]  Record to an .flv file

VIDEO:
  --window <text>          Capture the first window whose title contains <text>
  --monitor <n>            Capture monitor n (0 = primary, the default)
  --fps <n>                Output frame rate (default 60)
  --height <n>             Output height; aspect ratio is kept (default 1080)
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
  --out <file>             Output file (default recordings\\baocast-<date>-<time>.flv)
";

static STOP: AtomicBool = AtomicBool::new(false);

enum VideoSource {
    Monitor(usize),
    Window(String),
}

struct Options {
    video: VideoSource,
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
        Some("record") => record(parse_options(&args[1..])?),
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        video: VideoSource::Monitor(0),
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
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--window" => o.video = VideoSource::Window(value()?.clone()),
            "--monitor" => o.video = VideoSource::Monitor(number(value()?)?),
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
            other => return Err(format!("unknown option {other}\n\n{USAGE}")),
        }
    }
    if o.fps == 0 {
        return Err("--fps must be at least 1".into());
    }
    if o.desktop_volume > 200 || o.mic_volume > 200 {
        return Err("volumes go from 0 to 200".into());
    }
    if !aac::BITRATES_KBPS.contains(&o.audio_bitrate_kbps) {
        return Err(format!("--audio-bitrate must be one of {:?}", aac::BITRATES_KBPS));
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

/// Opens the audio sources the options ask for. A missing device is a warning,
/// not an error: recording without a mic beats not recording at all.
fn open_audio(o: &Options) -> Result<Vec<AudioSource>, Box<dyn Error>> {
    let mut sources = Vec::new();
    if o.desktop_audio {
        match audio::default_output().and_then(|d| AudioSource::open(&d, true, o.desktop_volume as f32 / 100.0)) {
            Ok(source) => sources.push(source),
            Err(e) => eprintln!("warning: no desktop audio: {e}"),
        }
    }
    if o.mic {
        let device = match &o.mic_name {
            Some(query) => {
                let query_lower = query.to_lowercase();
                audio::microphones()?
                    .into_iter()
                    .find(|d| d.name.to_lowercase().contains(&query_lower))
                    .ok_or(format!("no microphone name contains '{query}' (see: baocast list)"))?
            }
            None => match audio::default_microphone() {
                Ok(device) => device,
                Err(e) => {
                    eprintln!("warning: no microphone: {e}");
                    return Ok(sources);
                }
            },
        };
        match AudioSource::open(&device, false, o.mic_volume as f32 / 100.0) {
            Ok(source) => sources.push(source),
            Err(e) => eprintln!("warning: can't open microphone \"{}\": {e}", device.name),
        }
    }
    Ok(sources)
}

fn record(o: Options) -> Result<(), Box<dyn Error>> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
        SetConsoleCtrlHandler(Some(on_ctrl_c), true)?;
    }

    let gpu = Gpu::new()?;
    let target = match &o.video {
        VideoSource::Window(query) => {
            let (hwnd, title) = capture::find_window(query)
                .ok_or(format!("no window title contains '{query}' (see: baocast list)"))?;
            println!("Source : window \"{title}\"");
            Target::Window(hwnd)
        }
        VideoSource::Monitor(index) => {
            let monitor = *capture::monitors().get(*index).ok_or(format!("there is no monitor {index}"))?;
            println!("Source : monitor {index}");
            Target::Monitor(monitor)
        }
    };
    let capture = Capture::new(&gpu, target)?;
    let size = output_size(capture.width, capture.height, o.height);
    let mut converter = Converter::new(&gpu, &capture.texture, (capture.width, capture.height), size, o.fps)?;
    let encoder = Encoder::new(&gpu, size.0, size.1, o.fps, o.bitrate_kbps * 1000)?;

    let sources = open_audio(&o)?;
    let audio_names: Vec<String> = sources.iter().map(|s| format!("\"{}\"", s.name)).collect();
    let mut aac = if sources.is_empty() { None } else { Some(AacEncoder::new(o.audio_bitrate_kbps)?) };

    let path = o.out.clone().unwrap_or_else(default_path);
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut flv = FlvWriter::new(BufWriter::new(File::create(&path)?), aac.is_some())?;
    if let Some(header) = encoder.sequence_header() {
        flv.set_parameter_sets(&header);
    }
    if aac.is_some() {
        flv.write_audio_config(&aac::CONFIG)?;
    }

    println!("GPU    : {}", gpu.name);
    println!("Encoder: {}", encoder.name);
    if !encoder.ignored.is_empty() {
        println!("         (encoder ignored: {})", encoder.ignored.join(", "));
    }
    println!(
        "Video  : {}x{} -> {}x{} @ {} fps, {} kbps",
        capture.width, capture.height, size.0, size.1, o.fps, o.bitrate_kbps
    );
    if audio_names.is_empty() {
        println!("Audio  : off");
    } else {
        println!("Audio  : {} ({} kbps AAC)", audio_names.join(" + "), o.audio_bitrate_kbps);
    }
    println!("Output : {}", path.display());
    println!("Press Ctrl+C to stop.\n");

    let fps = o.fps as u64;
    let interval = Duration::from_nanos(1_000_000_000 / fps);
    let frame_100ns = 10_000_000 / fps as i64;
    let stop_after = o.seconds.map(Duration::from_secs);
    // Audio and video share one clock that starts here.
    let start = Instant::now();
    let mut mixer = Mixer::new(sources, Timeline::starting_now());
    let mut stats = Stats { audio: aac.is_some(), ..Default::default() };
    let mut index: u64 = 0;
    let mut current = None;
    let mut first_time = None;
    let mut draining = false;
    let mut last_report = start;

    loop {
        match encoder.next_event()? {
            Event::NeedInput if !draining => {
                // Feed the encoder at a steady frame rate, however often the game draws.
                let deadline = start + interval * index as u32;
                let now = Instant::now();
                match deadline.checked_duration_since(now) {
                    Some(wait) => std::thread::sleep(wait),
                    None => {
                        let behind = ((now - deadline).as_nanos() / interval.as_nanos()) as u64;
                        index += behind;
                        stats.skipped += behind;
                    }
                }
                if capture.poll()? || current.is_none() {
                    current = Some(converter.convert()?);
                    stats.captured += 1;
                }
                encoder.push(current.as_ref().unwrap(), index as i64 * frame_100ns, frame_100ns)?;
                index += 1;
                stats.encoded += 1;

                let elapsed = start.elapsed();
                if let Some(aac) = aac.as_mut() {
                    let pcm = mixer.mix(elapsed);
                    stats.peak = pcm.iter().fold(stats.peak, |peak, s| peak.max(s.abs()));
                    for frame in aac.encode(&pcm)? {
                        flv.write_audio(&frame.data, frame.ms)?;
                        stats.bytes += frame.data.len() as u64;
                    }
                }

                if STOP.load(Ordering::Relaxed) || stop_after.is_some_and(|limit| elapsed >= limit) {
                    encoder.drain()?;
                    draining = true;
                }
                if last_report.elapsed() >= Duration::from_secs(1) {
                    stats.print(elapsed);
                    stats.peak = 0.0;
                    last_report = Instant::now();
                }
            }
            Event::HaveOutput => {
                if let Some(packet) = encoder.pull()? {
                    let base = *first_time.get_or_insert(packet.time);
                    flv.write_video(&packet.data, ((packet.time - base) / 10_000) as u32, packet.keyframe)?;
                    stats.bytes += packet.data.len() as u64;
                }
            }
            Event::DrainComplete => break,
            _ => {}
        }
    }
    if let Some(aac) = aac.as_mut() {
        for frame in aac.finish()? {
            flv.write_audio(&frame.data, frame.ms)?;
        }
    }
    flv.finish()?;
    stats.print(start.elapsed());
    println!("\n\nSaved {}", path.display());
    Ok(())
}

/// Scales to `height` (never up), keeps the aspect ratio, rounds to even sizes for NV12.
fn output_size(width: u32, height: u32, target_height: u32) -> (u32, u32) {
    let out_height = target_height.min(height);
    let out_width = (width as u64 * out_height as u64 / height as u64) as u32;
    (out_width & !1, out_height & !1)
}

fn default_path() -> PathBuf {
    let t = unsafe { GetLocalTime() };
    PathBuf::from(format!(
        "recordings\\baocast-{:04}{:02}{:02}-{:02}{:02}{:02}.flv",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    ))
}

unsafe extern "system" fn on_ctrl_c(_: u32) -> BOOL {
    STOP.store(true, Ordering::Relaxed);
    true.into()
}

#[derive(Default)]
struct Stats {
    encoded: u64,
    captured: u64,
    skipped: u64,
    bytes: u64,
    audio: bool,
    /// Loudest sample since the last report, 0.0 to 1.0.
    peak: f32,
}

impl Stats {
    fn print(&self, elapsed: Duration) {
        let secs = elapsed.as_secs();
        let audio = match (self.audio, self.peak) {
            (false, _) => String::new(),
            (true, peak) if peak < 0.001 => "  audio silent".into(),
            (true, peak) => format!("  audio {:>3.0} dB", 20.0 * peak.log10()),
        };
        print!(
            "\r{:02}:{:02}  frames {}  new from game {}  skipped {}{audio}  size {:.1} MB   ",
            secs / 60,
            secs % 60,
            self.encoded,
            self.captured,
            self.skipped,
            self.bytes as f64 / 1_048_576.0
        );
        let _ = std::io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::output_size;

    #[test]
    fn output_size_keeps_aspect_and_never_upscales() {
        assert_eq!(output_size(2560, 1440, 1080), (1920, 1080));
        assert_eq!(output_size(1280, 720, 1080), (1280, 720));
        assert_eq!(output_size(1283, 751, 1080), (1282, 750));
    }
}
