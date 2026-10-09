//! Game and microphone audio through WASAPI. Windows converts every device to
//! 48 kHz stereo float for us, so mixing is just adding samples.

use std::collections::VecDeque;
use std::time::Duration;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::Result;

pub const SAMPLE_RATE: u64 = 48_000;
const CHANNELS: usize = 2;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// The mix runs this far behind real time, so every device has delivered
/// its samples before we need them.
const MIX_DELAY: Duration = Duration::from_millis(100);
/// Packets landing this close to where we expected them are treated as
/// continuous, which hides timestamp jitter.
const SNAP_FRAMES: i64 = (SAMPLE_RATE / 50) as i64; // 20 ms

pub struct Device {
    pub name: String,
    device: IMMDevice,
}

/// Whatever plays on the default speakers or headphones (the game, plus anything else).
pub fn default_output() -> Result<Device> {
    unsafe { device(enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?) }
}

pub fn default_microphone() -> Result<Device> {
    unsafe { device(enumerator()?.GetDefaultAudioEndpoint(eCapture, eConsole)?) }
}

pub fn microphones() -> Result<Vec<Device>> {
    unsafe {
        let all = enumerator()?.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        (0..all.GetCount()?).map(|i| device(all.Item(i)?)).collect()
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn device(device: IMMDevice) -> Result<Device> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName)?;
        let text = PropVariantToStringAlloc(&value);
        let _ = PropVariantClear(&mut value);
        let text = text?;
        let name = text.to_string().unwrap_or_default();
        CoTaskMemFree(Some(text.0 as *const _));
        Ok(Device { name, device })
    }
}

/// Maps WASAPI packet timestamps (QPC in 100 ns units) onto our timeline,
/// which starts at the same moment as the video's.
#[derive(Clone, Copy)]
pub struct Timeline {
    start_100ns: i64,
}

impl Timeline {
    pub fn starting_now() -> Self {
        Self { start_100ns: qpc_100ns() }
    }

    fn frame_at(&self, time_100ns: u64) -> i64 {
        (time_100ns as i64 - self.start_100ns) * SAMPLE_RATE as i64 / 10_000_000
    }
}

fn qpc_100ns() -> i64 {
    let (mut counter, mut frequency) = (0, 0);
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    (counter as i128 * 10_000_000 / frequency as i128) as i64
}

/// One source's samples laid out on the shared timeline, waiting to be mixed.
struct Track {
    /// Interleaved stereo; `buffer[0]` sits at timeline frame `start`.
    buffer: VecDeque<f32>,
    start: i64,
    gain: f32,
}

impl Track {
    fn new(gain: f32) -> Self {
        Self { buffer: VecDeque::new(), start: 0, gain }
    }

    /// Puts samples on the timeline at frame `at` (None: right after the previous packet).
    fn place(&mut self, at: Option<i64>, mut samples: &[f32]) {
        let end = self.start + (self.buffer.len() / CHANNELS) as i64;
        let mut at = match at {
            Some(at) if (at - end).abs() > SNAP_FRAMES => at,
            _ => end,
        };
        if at < 0 {
            // Captured before the recording started.
            let skip = ((-at) as usize * CHANNELS).min(samples.len());
            samples = &samples[skip..];
            at = 0;
        }
        if at >= end {
            self.buffer.extend(std::iter::repeat_n(0.0, (at - end) as usize * CHANNELS));
        } else {
            // Overlaps what we already have (clock drift): keep the older samples.
            let skip = ((end - at) as usize * CHANNELS).min(samples.len());
            samples = &samples[skip..];
        }
        self.buffer.extend(samples.iter().map(|s| s * self.gain));
    }

    /// Adds the samples for the frames starting at `from` into `out`.
    fn mix_into(&mut self, from: i64, out: &mut [f32]) {
        let to = from + (out.len() / CHANNELS) as i64;
        if self.start < from {
            // Arrived too late to be used.
            let stale = ((from - self.start) as usize * CHANNELS).min(self.buffer.len());
            self.buffer.drain(..stale);
            self.start = from;
        }
        let offset = ((self.start - from) as usize * CHANNELS).min(out.len());
        let count = (out.len() - offset).min(self.buffer.len());
        for (mixed, sample) in out[offset..offset + count].iter_mut().zip(self.buffer.drain(..count)) {
            *mixed += sample;
        }
        self.start += (count / CHANNELS) as i64;
        if self.buffer.is_empty() {
            self.start = self.start.max(to);
        }
    }
}

pub struct Source {
    pub name: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    track: Track,
    failed: bool,
}

impl Source {
    /// With `loopback`, records what an output device plays instead of recording from it.
    pub fn open(device: &Device, loopback: bool, gain: f32) -> Result<Self> {
        unsafe {
            let client: IAudioClient = device.device.Activate(CLSCTX_ALL, None)?;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: CHANNELS as u16,
                nSamplesPerSec: SAMPLE_RATE as u32,
                nAvgBytesPerSec: SAMPLE_RATE as u32 * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            // Windows resamples and remixes whatever the device uses into our format.
            let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            if loopback {
                flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
            }
            // 200 ms of device buffer; we drain it every video frame.
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format, None)?;
            let capture = client.GetService()?;
            client.Start()?;
            Ok(Self { name: device.name.clone(), client, capture, track: Track::new(gain), failed: false })
        }
    }

    /// Moves everything the device captured onto the track, placed by its timestamp.
    fn read(&mut self, timeline: &Timeline) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0, 0, 0);
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let count = frames as usize * CHANNELS;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                let zeros;
                let samples = if silent {
                    zeros = vec![0.0; count];
                    &zeros[..]
                } else {
                    std::slice::from_raw_parts(data as *const f32, count)
                };
                let bad_time = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0;
                self.track.place((!bad_time).then(|| timeline.frame_at(qpc)), samples);
                self.capture.ReleaseBuffer(frames)?;
            }
        }
        Ok(())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

pub struct Mixer {
    sources: Vec<Source>,
    timeline: Timeline,
    mixed: i64,
}

impl Mixer {
    pub fn new(sources: Vec<Source>, timeline: Timeline) -> Self {
        Self { sources, timeline, mixed: 0 }
    }

    /// Mixes every source up to `elapsed` minus the mix delay and returns
    /// interleaved stereo samples, continuing where the previous call stopped.
    pub fn mix(&mut self, elapsed: Duration) -> Vec<f32> {
        let until = elapsed.saturating_sub(MIX_DELAY);
        let target = (until.as_nanos() * SAMPLE_RATE as u128 / 1_000_000_000) as i64;
        let frames = (target - self.mixed).max(0) as usize;
        let mut out = vec![0.0; frames * CHANNELS];
        for source in self.sources.iter_mut().filter(|source| !source.failed) {
            if let Err(e) = source.read(&self.timeline) {
                // E.g. headphones unplugged: keep going without this source.
                eprintln!("\nwarning: lost audio from \"{}\": {e}", source.name);
                source.failed = true;
                continue;
            }
            source.track.mix_into(self.mixed, &mut out);
        }
        self.mixed += frames as i64;
        for sample in &mut out {
            *sample = sample.clamp(-1.0, 1.0);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mix(track: &mut Track, from: i64, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * CHANNELS];
        track.mix_into(from, &mut out);
        out
    }

    #[test]
    fn places_by_timestamp_and_fills_gaps_with_silence() {
        let mut track = Track::new(1.0);
        track.place(Some(4800), &[0.5; 4]); // 2 frames at 100 ms
        assert_eq!(mix(&mut track, 4799, 3), vec![0.0, 0.0, 0.5, 0.5, 0.5, 0.5]);
    }

    #[test]
    fn small_timestamp_jitter_stays_continuous() {
        let mut track = Track::new(1.0);
        track.place(Some(0), &[0.1; 4]);
        track.place(Some(5), &[0.2; 4]); // 3 frames late, inside the snap window
        assert_eq!(mix(&mut track, 0, 4), vec![0.1, 0.1, 0.1, 0.1, 0.2, 0.2, 0.2, 0.2]);
    }

    #[test]
    fn drops_samples_from_before_the_recording() {
        let mut track = Track::new(1.0);
        let mut packet = vec![0.9; 2000 * CHANNELS]; // 2000 frames before the start...
        packet.extend([0.3, 0.3]); // ...and one frame right at it
        track.place(Some(-2000), &packet);
        assert_eq!(mix(&mut track, 0, 1), vec![0.3, 0.3]);
    }

    #[test]
    fn slightly_late_packets_move_forward_and_gain_applies() {
        let mut track = Track::new(0.5);
        assert_eq!(mix(&mut track, 0, 2), vec![0.0; 4]); // nothing arrived yet
        track.place(Some(0), &[1.0; 8]); // 2 frames late: shifted, not lost
        assert_eq!(mix(&mut track, 2, 4), vec![0.5; 8]);
    }

    #[test]
    fn very_late_packets_are_dropped() {
        let mut track = Track::new(1.0);
        mix(&mut track, 0, 2000);
        track.place(Some(0), &[1.0; 8]); // far older than what was already mixed
        assert_eq!(mix(&mut track, 2000, 2), vec![0.0; 4]);
    }
}
