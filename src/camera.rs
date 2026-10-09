//! Webcams (and capture cards) through Media Foundation. We ask the camera for
//! an uncompressed format near 720p, so nothing needs decoding; a small thread
//! keeps the newest frame ready for the engine to upload to the GPU.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::{Error, GUID, Interface, PWSTR, Result};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PixelFormat {
    Nv12,
    Yuy2,
    Rgb32,
}

impl PixelFormat {
    /// Rows the frame occupies in memory (NV12 adds a half-height colour plane).
    pub fn rows(self, height: u32) -> u32 {
        match self {
            PixelFormat::Nv12 => height + height / 2,
            _ => height,
        }
    }
}

/// The newest camera picture, top-down, `pitch` bytes per row.
pub struct Frame {
    pub data: Vec<u8>,
    pub pitch: u32,
    pub sequence: u64,
}

/// Where the camera sits on the stream. Shared with the window, so every
/// setting can change while live.
pub struct Overlay {
    visible: AtomicBool,
    corner: AtomicU8,
    size: AtomicU8,
    mirror: AtomicBool,
}

impl Overlay {
    /// `corner`: 0 top left, 1 top right, 2 bottom left, 3 bottom right.
    /// `size`: 0 small, 1 medium, 2 large.
    pub fn new(visible: bool, corner: u8, size: u8, mirror: bool) -> Arc<Self> {
        Arc::new(Self {
            visible: AtomicBool::new(visible),
            corner: AtomicU8::new(corner.min(3)),
            size: AtomicU8::new(size.min(2)),
            mirror: AtomicBool::new(mirror),
        })
    }
    pub fn set_visible(&self, on: bool) {
        self.visible.store(on, Ordering::Relaxed);
    }
    pub fn set_corner(&self, corner: u8) {
        self.corner.store(corner.min(3), Ordering::Relaxed);
    }
    pub fn set_size(&self, size: u8) {
        self.size.store(size.min(2), Ordering::Relaxed);
    }
    pub fn set_mirror(&self, on: bool) {
        self.mirror.store(on, Ordering::Relaxed);
    }
    /// (visible, corner, size, mirror)
    pub fn snapshot(&self) -> (bool, u8, u8, bool) {
        (
            self.visible.load(Ordering::Relaxed),
            self.corner.load(Ordering::Relaxed),
            self.size.load(Ordering::Relaxed),
            self.mirror.load(Ordering::Relaxed),
        )
    }
}

/// Names of the cameras and capture cards Windows knows about.
pub fn cameras() -> Vec<String> {
    devices().map(|list| list.into_iter().map(|(name, _)| name).collect()).unwrap_or_default()
}

fn devices() -> Result<Vec<(String, IMFActivate)>> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.unwrap();
        attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        MFEnumDeviceSources(&attributes, &mut list, &mut count)?;
        let activates: Vec<IMFActivate> = (0..count as usize).filter_map(|i| list.add(i).read()).collect();
        CoTaskMemFree(Some(list as *const _));
        let mut devices = Vec::new();
        for activate in activates {
            let mut name = PWSTR::null();
            let mut len = 0;
            if activate.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut name, &mut len).is_ok() {
                devices.push((name.to_string().unwrap_or_default(), activate));
                CoTaskMemFree(Some(name.0 as *const _));
            }
        }
        Ok(devices)
    }
}

pub struct Camera {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    latest: Arc<Mutex<Option<Frame>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Camera {
    /// Opens the camera whose name contains `query` (or the first one) and starts reading.
    pub fn open(query: Option<&str>) -> std::result::Result<Self, String> {
        let query = query.map(str::to_lowercase);
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (thread_latest, thread_stop) = (latest.clone(), stop.clone());
        // Media Foundation objects are created and used on this one thread.
        let thread = std::thread::Builder::new()
            .name("milercast-camera".into())
            .spawn(move || {
                let reader = match open_reader(query.as_deref()) {
                    Ok((reader, info)) => {
                        let _ = ready_tx.send(Ok(info));
                        reader
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                read_frames(&reader, &thread_latest, &thread_stop);
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv() {
            Ok(Ok((name, width, height, format))) => {
                Ok(Self { name, width, height, format, latest, stop, thread: Some(thread) })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err("the camera thread stopped unexpectedly".into()),
        }
    }

    /// Runs `upload` with the newest frame if it's newer than `seen`; returns its sequence number.
    pub fn with_new_frame(&self, seen: u64, upload: impl FnOnce(&Frame)) -> Option<u64> {
        let latest = self.latest.lock().unwrap();
        let frame = latest.as_ref().filter(|f| f.sequence > seen)?;
        upload(frame);
        Some(frame.sequence)
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

type Info = (String, u32, u32, PixelFormat);

fn open_reader(query: Option<&str>) -> std::result::Result<(IMFSourceReader, Info), String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| e.to_string())?;
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        let devices = devices().map_err(|e| e.to_string())?;
        let (name, activate) = devices
            .into_iter()
            .find(|(name, _)| query.is_none_or(|q| name.to_lowercase().contains(q)))
            .ok_or_else(|| match query {
                Some(q) => format!("no camera name contains '{q}'"),
                None => "no camera found".to_string(),
            })?;
        let reader = setup(&activate).map_err(|e| format!("can't open camera \"{name}\": {}", e.message()))?;
        let current = reader.GetCurrentMediaType(STREAM).map_err(|e| e.to_string())?;
        let (width, height) = unpack(current.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0));
        let format = current.GetGUID(&MF_MT_SUBTYPE).ok().and_then(pixel_format).unwrap_or(PixelFormat::Nv12);
        Ok((reader, (name, width, height, format)))
    }
}

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

fn pixel_format(subtype: GUID) -> Option<PixelFormat> {
    match subtype {
        s if s == MFVideoFormat_NV12 => Some(PixelFormat::Nv12),
        s if s == MFVideoFormat_YUY2 => Some(PixelFormat::Yuy2),
        s if s == MFVideoFormat_RGB32 => Some(PixelFormat::Rgb32),
        _ => None,
    }
}

fn unpack(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

/// Lower is better: uncompressed formats first, then closest to 1280x720, up to 60 fps.
fn score(width: u32, height: u32, fps: f64, uncompressed: bool) -> i64 {
    let size = (width as i64 - 1280).abs() + (height as i64 - 720).abs();
    let rate = if fps > 60.5 { 1000 } else { ((60.0 - fps) * 10.0) as i64 };
    let compressed = if uncompressed { 0 } else { 1_000_000 };
    compressed + size * 10 + rate
}

unsafe fn setup(activate: &IMFActivate) -> Result<IMFSourceReader> {
    unsafe {
        let source: IMFMediaSource = activate.ActivateObject()?;
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.unwrap();
        // Lets Windows decode cameras that only send compressed video.
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, &attributes)?;

        let mut best: Option<(i64, IMFMediaType, bool)> = None;
        for index in 0.. {
            let Ok(media_type) = reader.GetNativeMediaType(STREAM, index) else { break };
            let (width, height) = unpack(media_type.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0));
            let (num, den) = unpack(media_type.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0));
            let fps = if den == 0 { 0.0 } else { num as f64 / den as f64 };
            let uncompressed = media_type.GetGUID(&MF_MT_SUBTYPE).ok().and_then(pixel_format).is_some();
            if width == 0 || height == 0 {
                continue;
            }
            let s = score(width, height, fps, uncompressed);
            if best.as_ref().is_none_or(|(b, _, _)| s < *b) {
                best = Some((s, media_type, uncompressed));
            }
        }
        let (_, chosen, uncompressed) = best.ok_or_else(|| Error::new(E_FAIL, "the camera offers no video formats"))?;
        if uncompressed {
            reader.SetCurrentMediaType(STREAM, None, &chosen)?;
        } else {
            // Ask for NV12 at the chosen size; the reader adds a decoder.
            let wanted = MFCreateMediaType()?;
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            wanted.SetUINT64(&MF_MT_FRAME_SIZE, chosen.GetUINT64(&MF_MT_FRAME_SIZE)?)?;
            if let Ok(rate) = chosen.GetUINT64(&MF_MT_FRAME_RATE) {
                wanted.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            }
            reader.SetCurrentMediaType(STREAM, None, &wanted)?;
        }
        Ok(reader)
    }
}

fn read_frames(reader: &IMFSourceReader, latest: &Mutex<Option<Frame>>, stop: &AtomicBool) {
    let Ok(current) = (unsafe { reader.GetCurrentMediaType(STREAM) }) else { return };
    let (width, height) = unpack(unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0));
    let format = unsafe { current.GetGUID(&MF_MT_SUBTYPE) }.ok().and_then(pixel_format).unwrap_or(PixelFormat::Nv12);
    let bytes_per_pixel = if format == PixelFormat::Rgb32 { 4 } else if format == PixelFormat::Yuy2 { 2 } else { 1 };
    // Bottom-up RGB has a negative default stride.
    let default_stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map(|s| s as i32)
        .unwrap_or((width * bytes_per_pixel) as i32);
    let rows = format.rows(height) as usize;
    let mut sequence = 0;
    while !stop.load(Ordering::Relaxed) {
        let mut flags = 0u32;
        let mut sample = None;
        let read = unsafe { reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)) };
        let failed = (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32;
        if read.is_err() || flags & failed != 0 {
            return; // unplugged or stopped: the stream keeps the last picture
        }
        let Some(sample) = sample else { continue };
        let Ok(buffer) = (unsafe { sample.GetBufferByIndex(0) }) else { continue };
        let row = (width * bytes_per_pixel) as usize;
        let mut data = vec![0u8; row * rows];
        let copied = unsafe { copy_rows(&buffer, &mut data, row, rows, default_stride) };
        if !copied {
            continue;
        }
        sequence += 1;
        *latest.lock().unwrap() = Some(Frame { data, pitch: row as u32, sequence });
    }
}

/// Copies `rows` rows of `row` bytes into `out`, top-down, whatever the buffer's layout.
unsafe fn copy_rows(buffer: &IMFMediaBuffer, out: &mut [u8], row: usize, rows: usize, default_stride: i32) -> bool {
    unsafe {
        let (mut ptr, mut pitch) = (std::ptr::null_mut(), 0i32);
        let two_d = buffer.cast::<IMF2DBuffer>().ok();
        let locked = match &two_d {
            Some(b) => b.Lock2D(&mut ptr, &mut pitch).is_ok(),
            None => {
                let mut len = 0;
                let ok = buffer.Lock(&mut ptr, None, Some(&mut len)).is_ok();
                pitch = default_stride;
                if pitch < 0 {
                    // Bottom-up: the first row in memory is the last row of the picture.
                    ptr = ptr.offset((rows as isize - 1) * (-pitch) as isize);
                }
                ok && (len as usize) >= row * rows
            }
        };
        if !locked || ptr.is_null() {
            return false;
        }
        for (r, chunk) in out.chunks_exact_mut(row).enumerate() {
            let source = ptr.offset(r as isize * pitch as isize);
            std::ptr::copy_nonoverlapping(source, chunk.as_mut_ptr(), row);
        }
        match &two_d {
            Some(b) => {
                let _ = b.Unlock2D();
            }
            None => {
                let _ = buffer.Unlock();
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn prefers_uncompressed_720p() {
        let nv12_720p60 = score(1280, 720, 60.0, true);
        let nv12_1080p60 = score(1920, 1080, 60.0, true);
        let mjpeg_720p60 = score(1280, 720, 60.0, false);
        let nv12_720p30 = score(1280, 720, 30.0, true);
        assert!(nv12_720p60 < nv12_720p30);
        assert!(nv12_720p30 < nv12_1080p60);
        assert!(nv12_1080p60 < mjpeg_720p60);
    }
}
