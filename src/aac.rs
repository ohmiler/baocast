//! AAC-LC through the encoder built into Windows (Media Foundation). Audio is
//! cheap, so this one runs on the CPU at a fraction of a percent.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{Error, Result};

use crate::audio::SAMPLE_RATE;

/// AudioSpecificConfig for AAC-LC, 48 kHz, stereo (ISO 14496-3).
pub const CONFIG: [u8; 2] = [0x11, 0x90];

/// The bitrates the Windows AAC encoder accepts.
pub const BITRATES_KBPS: [u32; 4] = [96, 128, 160, 192];

pub struct AacFrame {
    /// Raw AAC (no ADTS header).
    pub data: Vec<u8>,
    pub ms: u32,
}

pub struct AacEncoder {
    transform: IMFTransform,
    output_size: u32,
    input_frames: u64,
}

impl AacEncoder {
    pub fn new(kbps: u32) -> Result<Self> {
        unsafe {
            let input_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
            let output_info = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
            let mut list = std::ptr::null_mut();
            let mut count = 0;
            MFTEnumEx(
                MFT_CATEGORY_AUDIO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input_info),
                Some(&output_info),
                &mut list,
                &mut count,
            )?;
            let encoders: Vec<IMFActivate> = (0..count as usize).filter_map(|i| list.add(i).read()).collect();
            CoTaskMemFree(Some(list as *const _));
            let activate = encoders.into_iter().next().ok_or_else(|| Error::new(E_FAIL, "no AAC encoder found"))?;
            let transform: IMFTransform = activate.ActivateObject()?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            set_audio_format(&input)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE as u32 * 4)?;
            transform.SetInputType(0, &input, 0)?;

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            set_audio_format(&output)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, kbps * 1000 / 8)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw frames, no ADTS headers
            transform.SetOutputType(0, &output, 0)?;

            let output_size = transform.GetOutputStreamInfo(0)?.cbSize.max(8192);
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { transform, output_size, input_frames: 0 })
        }
    }

    /// Encodes interleaved stereo float samples and returns the AAC frames that are ready.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<AacFrame>> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        unsafe {
            let bytes = (pcm.len() * 2) as u32;
            let buffer = MFCreateMemoryBuffer(bytes)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            let pcm16 = std::slice::from_raw_parts_mut(ptr as *mut i16, pcm.len());
            for (out, sample) in pcm16.iter_mut().zip(pcm) {
                *out = (sample * 32767.0) as i16;
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes)?;

            let frames = (pcm.len() / 2) as u64;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(to_100ns(self.input_frames))?;
            sample.SetSampleDuration(to_100ns(frames))?;
            self.input_frames += frames;
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        self.collect()
    }

    /// Flushes the final partial frame at the end of a recording.
    pub fn finish(&mut self) -> Result<Vec<AacFrame>> {
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)? };
        self.collect()
    }

    fn collect(&mut self) -> Result<Vec<AacFrame>> {
        let mut frames = Vec::new();
        loop {
            unsafe {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(sample.clone())),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
                drop(ManuallyDrop::take(&mut buffers[0].pSample));
                drop(ManuallyDrop::take(&mut buffers[0].pEvents));
                match result {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(frames),
                    Err(e) => return Err(e),
                }
                let time = sample.GetSampleTime().unwrap_or(0).max(0);
                let buffer = sample.ConvertToContiguousBuffer()?;
                let (mut ptr, mut len) = (std::ptr::null_mut(), 0);
                buffer.Lock(&mut ptr, None, Some(&mut len))?;
                let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                buffer.Unlock()?;
                if !data.is_empty() {
                    frames.push(AacFrame { data, ms: (time / 10_000) as u32 });
                }
            }
        }
    }
}

fn set_audio_format(media_type: &IMFMediaType) -> Result<()> {
    unsafe {
        media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE as u32)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)
    }
}

fn to_100ns(frames: u64) -> i64 {
    (frames * 10_000_000 / SAMPLE_RATE) as i64
}
