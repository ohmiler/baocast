//! The capture -> encode -> output pipeline, running on its own thread.
//! The GUI and the CLI start it, read its progress from `State`, and stop it.

use std::error::Error;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_LITE, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

use crate::aac::{self, AacEncoder};
use crate::audio::{self, Gain, Mixer, Source as AudioSource, Timeline};
use crate::camera::{Camera, Overlay};
use crate::capture::{Capture, Poll, Target};
use crate::convert::{CameraLayer, Converter};
use crate::encoder::{Encoder, Event};
use crate::flv::{FlvFile, Muxer, Sink};
use crate::gpu::Gpu;
use crate::rtmp::{Metadata, Monitor, Rtmp};

/// What to capture. Raw handles, so settings can cross threads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Video {
    Window(isize),
    Monitor(isize),
}

impl Video {
    pub fn window(hwnd: HWND) -> Self {
        Video::Window(hwnd.0 as isize)
    }
    pub fn monitor(monitor: HMONITOR) -> Self {
        Video::Monitor(monitor.0 as isize)
    }
}

pub struct Mic {
    /// Part of the device name; None for the Windows default microphone.
    pub name: Option<String>,
    pub gain: Gain,
}

pub struct CameraChoice {
    /// Part of the device name; None for the first camera.
    pub name: Option<String>,
    /// Placement, shared with whoever wants to change it while live.
    pub overlay: Arc<Overlay>,
}

pub struct Settings {
    pub video: Video,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    /// What plays on the speakers/headphones; None to leave it out entirely.
    pub desktop_audio: Option<Gain>,
    pub mic: Option<Mic>,
    pub audio_kbps: u32,
    pub camera: Option<CameraChoice>,
    /// (server, stream key) to go live.
    pub live: Option<(String, String)>,
    pub record_to: Option<PathBuf>,
    pub stop_after: Option<Duration>,
}

#[derive(Clone, Debug)]
pub enum Phase {
    Starting,
    Running,
    Finished(Result<(), String>),
}

/// Progress, readable from any thread while the engine runs.
pub struct State {
    stop: AtomicBool,
    phase: Mutex<Phase>,
    started: Mutex<Option<Instant>>,
    pub encoded: AtomicU64,
    pub captured: AtomicU64,
    pub skipped: AtomicU64,
    pub bytes: AtomicU64,
    desktop_peak: AtomicU32,
    mic_peak: AtomicU32,
    network: Mutex<Option<Monitor>>,
    info: Mutex<Vec<String>>,
}

impl State {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            phase: Mutex::new(Phase::Starting),
            started: Mutex::new(None),
            encoded: AtomicU64::new(0),
            captured: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            desktop_peak: AtomicU32::new(0),
            mic_peak: AtomicU32::new(0),
            network: Mutex::new(None),
            info: Mutex::new(Vec::new()),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase.lock().unwrap().clone()
    }

    /// Time since capture started (zero while still starting).
    pub fn elapsed(&self) -> Duration {
        self.started.lock().unwrap().map(|s| s.elapsed()).unwrap_or_default()
    }

    /// Loudest desktop and mic samples since the last call (0.0 to 1.0).
    pub fn take_peaks(&self) -> (f32, f32) {
        let take = |peak: &AtomicU32| f32::from_bits(peak.swap(0, Ordering::Relaxed));
        (take(&self.desktop_peak), take(&self.mic_peak))
    }

    pub fn network(&self) -> Option<Monitor> {
        self.network.lock().unwrap().clone()
    }

    /// Setup details (devices, encoder, sizes) for anyone who wants to show them.
    pub fn info(&self) -> Vec<String> {
        self.info.lock().unwrap().clone()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn note(&self, line: String) {
        self.info.lock().unwrap().push(line);
    }

    fn raise_peak(peak: &AtomicU32, value: f32) {
        let _ = peak.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            (value > f32::from_bits(old)).then_some(value.to_bits())
        });
    }
}

pub struct Engine {
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    /// Starts in the background; watch `state().phase()` for progress.
    pub fn start(settings: Settings) -> Self {
        let state = Arc::new(State::new());
        let thread_state = state.clone();
        let thread = std::thread::Builder::new()
            .name("milercast-engine".into())
            .spawn(move || {
                let result = run(settings, &thread_state).map_err(|e| e.to_string());
                *thread_state.phase.lock().unwrap() = Phase::Finished(result);
            })
            .expect("can't start the engine thread");
        Self { state, thread: Some(thread) }
    }

    pub fn state(&self) -> &Arc<State> {
        &self.state
    }

    /// Asks the engine to finish: flush the encoders, end the stream, close the file.
    pub fn stop(&self) {
        self.state.stop();
    }

    /// Waits until the engine has finished.
    pub fn join(mut self) -> Result<(), String> {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        match self.state.phase() {
            Phase::Finished(result) => result,
            _ => Err("the engine stopped unexpectedly".into()),
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.state.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Desktop,
    Mic,
}

fn run(s: Settings, state: &State) -> Result<(), Box<dyn Error>> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
    }

    let gpu = Gpu::new()?;
    let target = match s.video {
        Video::Window(hwnd) => Target::Window(HWND(hwnd as *mut _)),
        Video::Monitor(monitor) => Target::Monitor(HMONITOR(monitor as *mut _)),
    };
    let mut capture = Capture::new(&gpu, target)?;
    let canvas = canvas_size(capture.height, s.height);

    // Like audio devices, a camera that won't open is a warning, not an error.
    let mut camera = None;
    if let Some(choice) = &s.camera {
        let opened = Camera::open(choice.name.as_deref()).and_then(|cam| {
            let layer = CameraLayer::new(&gpu, (cam.width, cam.height), cam.format, choice.overlay.clone())
                .map_err(|e| format!("can't make the camera texture: {}", e.message()))?;
            Ok((cam, layer))
        });
        match opened {
            Ok((cam, layer)) => {
                state.note(format!("Camera : \"{}\" {}x{} {:?}", cam.name, cam.width, cam.height, cam.format));
                camera = Some((cam, layer));
            }
            Err(e) => state.note(format!("warning: no camera: {e}")),
        }
    }
    let layer = camera.as_ref().map(|(_, layer)| layer);
    let mut converter = Converter::new(&gpu, &capture.texture, (capture.width, capture.height), canvas, s.fps, layer)?;
    if let Some(problem) = &converter.camera_problem {
        state.note(format!("warning: no camera: {problem}"));
        camera = None;
    }
    let encoder = Encoder::new(&gpu, canvas.0, canvas.1, s.fps, s.video_kbps * 1000)?;

    // A missing audio device is a warning, not an error: streaming without a
    // mic beats not streaming at all.
    let mut sources = Vec::new();
    let mut kinds = Vec::new();
    if let Some(gain) = &s.desktop_audio {
        match audio::default_output().and_then(|d| AudioSource::open(&d, true, gain.clone())) {
            Ok(source) => {
                sources.push(source);
                kinds.push(Kind::Desktop);
            }
            Err(e) => state.note(format!("warning: no desktop audio: {e}")),
        }
    }
    if let Some(mic) = &s.mic {
        let device = match &mic.name {
            Some(query) => {
                let query = query.to_lowercase();
                audio::microphones()?
                    .into_iter()
                    .find(|d| d.name.to_lowercase().contains(&query))
                    .ok_or(format!("no microphone name contains '{query}'"))
            }
            None => audio::default_microphone().map_err(|e| e.to_string()),
        };
        match device.and_then(|d| AudioSource::open(&d, false, mic.gain.clone()).map_err(|e| e.to_string())) {
            Ok(source) => {
                sources.push(source);
                kinds.push(Kind::Mic);
            }
            Err(e) => state.note(format!("warning: no microphone: {e}")),
        }
    }
    let audio_names: Vec<String> = sources.iter().map(|s| format!("\"{}\"", s.name)).collect();
    let mut aac = if sources.is_empty() { None } else { Some(AacEncoder::new(s.audio_kbps)?) };

    state.note(format!("GPU    : {}", gpu.name));
    let mut encoder_line = format!("Encoder: {}", encoder.name);
    if !encoder.ignored.is_empty() {
        encoder_line += &format!(" (ignored: {})", encoder.ignored.join(", "));
    }
    state.note(encoder_line);
    state.note(format!(
        "Video  : {}x{} -> {}x{} @ {} fps, {} kbps",
        capture.width, capture.height, canvas.0, canvas.1, s.fps, s.video_kbps
    ));
    state.note(match audio_names.is_empty() {
        true => "Audio  : off".into(),
        false => format!("Audio  : {} ({} kbps AAC)", audio_names.join(" + "), s.audio_kbps),
    });

    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
    if let Some((server, key)) = &s.live {
        let metadata = Metadata {
            width: canvas.0,
            height: canvas.1,
            fps: s.fps,
            video_kbps: s.video_kbps,
            audio_kbps: aac.as_ref().map(|_| s.audio_kbps),
        };
        let rtmp = Rtmp::connect(server, key.clone(), metadata)?;
        *state.network.lock().unwrap() = Some(rtmp.monitor());
        state.note(format!("Live   : {}", Rtmp::describe(server)));
        sinks.push(Box::new(rtmp));
    }
    if let Some(path) = &s.record_to {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        sinks.push(Box::new(FlvFile::new(BufWriter::new(File::create(path)?), aac.is_some())?));
        state.note(format!("Output : {}", path.display()));
    }
    let mut muxer = Muxer::new(sinks);
    if let Some(header) = encoder.sequence_header() {
        muxer.set_parameter_sets(&header);
    }
    if aac.is_some() {
        muxer.audio_config(&aac::CONFIG)?;
    }

    let fps = s.fps as u64;
    let interval = Duration::from_nanos(1_000_000_000 / fps);
    let frame_100ns = 10_000_000 / fps as i64;
    // Audio and video share one clock that starts here.
    let start = Instant::now();
    let mut mixer = Mixer::new(sources, Timeline::starting_now());
    *state.started.lock().unwrap() = Some(start);
    *state.phase.lock().unwrap() = Phase::Running;

    let network = state.network();
    let mut index: u64 = 0;
    let mut current = None;
    let mut first_time = None;
    let mut draining = false;
    let mut camera_seen = 0;
    let mut overlay_seen = None;

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
                        state.skipped.fetch_add(behind, Ordering::Relaxed);
                    }
                }
                // Redraw when the game or the camera has something new, or the
                // camera was moved; otherwise the last picture is sent again.
                let mut redraw = current.is_none();
                if let Some((cam, layer)) = &camera {
                    let uploaded = cam.with_new_frame(camera_seen, |frame| {
                        let _ = layer.upload(&gpu.context, frame);
                    });
                    if let Some(sequence) = uploaded {
                        camera_seen = sequence;
                        redraw = true;
                    }
                    let placement = layer.overlay.snapshot();
                    if overlay_seen != Some(placement) {
                        overlay_seen = Some(placement);
                        redraw = true;
                    }
                }
                match capture.poll()? {
                    Poll::NewFrame => {
                        redraw = true;
                        state.captured.fetch_add(1, Ordering::Relaxed);
                    }
                    Poll::Resized => {
                        // Same canvas, new picture size: refit it; the next frame brings the content.
                        let layer = camera.as_ref().map(|(_, layer)| layer);
                        converter = Converter::new(
                            &gpu,
                            &capture.texture,
                            (capture.width, capture.height),
                            canvas,
                            s.fps,
                            layer,
                        )?;
                    }
                    Poll::Unchanged => {}
                }
                if redraw {
                    current = Some(converter.convert()?);
                }
                if network.as_ref().is_some_and(Monitor::take_keyframe_request) {
                    encoder.force_keyframe();
                }
                encoder.push(current.as_ref().unwrap(), index as i64 * frame_100ns, frame_100ns)?;
                index += 1;
                state.encoded.fetch_add(1, Ordering::Relaxed);

                let elapsed = start.elapsed();
                if let Some(aac) = aac.as_mut() {
                    let pcm = mixer.mix(elapsed);
                    for (kind, peak) in kinds.iter().zip(mixer.take_peaks()) {
                        let slot = if *kind == Kind::Desktop { &state.desktop_peak } else { &state.mic_peak };
                        State::raise_peak(slot, peak);
                    }
                    for frame in aac.encode(&pcm)? {
                        muxer.audio(&frame.data, frame.ms)?;
                        state.bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                    }
                }

                if state.stop.load(Ordering::Relaxed) || s.stop_after.is_some_and(|limit| elapsed >= limit) {
                    encoder.drain()?;
                    draining = true;
                }
            }
            Event::HaveOutput => {
                if let Some(packet) = encoder.pull()? {
                    let base = *first_time.get_or_insert(packet.time);
                    muxer.video(&packet.data, ((packet.time - base) / 10_000) as u32, packet.keyframe)?;
                    state.bytes.fetch_add(packet.data.len() as u64, Ordering::Relaxed);
                }
            }
            Event::DrainComplete => break,
            _ => {}
        }
    }
    if let Some(aac) = aac.as_mut() {
        for frame in aac.finish()? {
            muxer.audio(&frame.data, frame.ms)?;
        }
    }
    muxer.finish()?;
    Ok(())
}

/// A 16:9 canvas `height` tall, but never taller than the source (no upscaling).
fn canvas_size(source_height: u32, height: u32) -> (u32, u32) {
    let height = height.min(source_height).max(144) & !1;
    let width = (height * 16 / 9 + 1) & !1;
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::canvas_size;

    #[test]
    fn canvas_is_16_by_9_and_never_upscales() {
        assert_eq!(canvas_size(1440, 1080), (1920, 1080));
        assert_eq!(canvas_size(1440, 720), (1280, 720));
        assert_eq!(canvas_size(720, 1080), (1280, 720));
        assert_eq!(canvas_size(1920, 1080), (1920, 1080)); // portrait monitor
    }
}
