//! Baocast: the lightest way to stream your games on Windows.
//!
//! Milestone 1 records a window or monitor to an FLV file with a pipeline
//! that never leaves the GPU:
//! capture (WGC) -> resize + NV12 (video processor) -> H.264 (hardware MFT) -> FLV.

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

use capture::{Capture, Target};
use convert::Converter;
use encoder::{Encoder, Event};
use flv::FlvWriter;
use gpu::Gpu;

const USAGE: &str = "\
Baocast - lightweight game recorder/streamer

USAGE:
  baocast list              List windows you can capture
  baocast record [OPTIONS]  Record to an .flv file

OPTIONS:
  --window <text>   Capture the first window whose title contains <text>
  --monitor <n>     Capture monitor n (0 = primary, the default)
  --seconds <n>     Stop after n seconds (default: when you press Ctrl+C)
  --fps <n>         Output frame rate (default 60)
  --height <n>      Output height; aspect ratio is kept (default 1080)
  --bitrate <kbps>  Video bitrate (default 6000)
  --out <file>      Output file (default recordings\\baocast-<date>-<time>.flv)
";

static STOP: AtomicBool = AtomicBool::new(false);

enum Source {
    Monitor(usize),
    Window(String),
}

struct Options {
    source: Source,
    seconds: Option<u64>,
    fps: u32,
    height: u32,
    bitrate_kbps: u32,
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
        Some("list") => {
            for (_, title) in capture::list_windows() {
                println!("{title}");
            }
            Ok(())
        }
        Some("record") => record(parse_options(&args[1..])?),
        _ => {
            print!("{USAGE}");
            Ok(())
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options =
        Options { source: Source::Monitor(0), seconds: None, fps: 60, height: 1080, bitrate_kbps: 6000, out: None };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--window" => options.source = Source::Window(value.clone()),
            "--monitor" => options.source = Source::Monitor(number(value)?),
            "--seconds" => options.seconds = Some(number(value)?),
            "--fps" => options.fps = number(value)?,
            "--height" => options.height = number(value)?,
            "--bitrate" => options.bitrate_kbps = number(value)?,
            "--out" => options.out = Some(PathBuf::from(value)),
            other => return Err(format!("unknown option {other}\n\n{USAGE}")),
        }
    }
    if options.fps == 0 {
        return Err("--fps must be at least 1".into());
    }
    Ok(options)
}

fn number<T: FromStr>(text: &str) -> Result<T, String> {
    text.parse().map_err(|_| format!("'{text}' is not a valid number"))
}

fn record(options: Options) -> Result<(), Box<dyn Error>> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
        SetConsoleCtrlHandler(Some(on_ctrl_c), true)?;
    }

    let gpu = Gpu::new()?;
    let target = match &options.source {
        Source::Window(query) => {
            let (hwnd, title) = capture::find_window(query)
                .ok_or(format!("no window title contains '{query}' (see: baocast list)"))?;
            println!("Source : window \"{title}\"");
            Target::Window(hwnd)
        }
        Source::Monitor(index) => {
            let monitor = *capture::monitors().get(*index).ok_or(format!("there is no monitor {index}"))?;
            println!("Source : monitor {index}");
            Target::Monitor(monitor)
        }
    };
    let capture = Capture::new(&gpu, target)?;
    let size = output_size(capture.width, capture.height, options.height);
    let mut converter = Converter::new(&gpu, &capture.texture, (capture.width, capture.height), size, options.fps)?;
    let encoder = Encoder::new(&gpu, size.0, size.1, options.fps, options.bitrate_kbps * 1000)?;

    let path = options.out.unwrap_or_else(default_path);
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut flv = FlvWriter::new(BufWriter::new(File::create(&path)?))?;
    if let Some(header) = encoder.sequence_header() {
        flv.set_parameter_sets(&header);
    }

    println!("GPU    : {}", gpu.name);
    println!("Encoder: {}", encoder.name);
    if !encoder.ignored.is_empty() {
        println!("         (encoder ignored: {})", encoder.ignored.join(", "));
    }
    println!(
        "Video  : {}x{} -> {}x{} @ {} fps, {} kbps",
        capture.width, capture.height, size.0, size.1, options.fps, options.bitrate_kbps
    );
    println!("Output : {}", path.display());
    println!("Press Ctrl+C to stop.\n");

    let fps = options.fps as u64;
    let interval = Duration::from_nanos(1_000_000_000 / fps);
    let frame_100ns = 10_000_000 / fps as i64;
    let stop_after = options.seconds.map(Duration::from_secs);
    let start = Instant::now();
    let mut stats = Stats::default();
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
                if STOP.load(Ordering::Relaxed) || stop_after.is_some_and(|limit| elapsed >= limit) {
                    encoder.drain()?;
                    draining = true;
                }
                if last_report.elapsed() >= Duration::from_secs(1) {
                    stats.print(elapsed);
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
}

impl Stats {
    fn print(&self, elapsed: Duration) {
        let secs = elapsed.as_secs();
        print!(
            "\r{:02}:{:02}  frames {}  new from game {}  skipped {}  size {:.1} MB   ",
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
