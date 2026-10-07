//! Hardware H.264 through Media Foundation. Windows provides the GPU vendor's
//! encoder (NVENC, AMD AMF or Intel Quick Sync) and we feed it GPU textures.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{E_FAIL, LUID, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
use windows::core::{Error, Interface, PWSTR, Result};

use crate::gpu::Gpu;

pub struct Packet {
    /// One encoded frame, Annex-B (start-code) H.264.
    pub data: Vec<u8>,
    /// Presentation time in 100 ns units.
    pub time: i64,
    pub keyframe: bool,
}

pub enum Event {
    NeedInput,
    HaveOutput,
    DrainComplete,
    Other,
}

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    input_id: u32,
    output_id: u32,
    /// Set when the encoder expects us to allocate output buffers (of this size).
    output_buffer_size: Option<u32>,
    _manager: IMFDXGIDeviceManager,
    pub name: String,
    /// Settings this encoder refused, worth showing so users know what they're getting.
    pub ignored: Vec<&'static str>,
}

impl Encoder {
    pub fn new(gpu: &Gpu, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            let (activate, name) = find_hardware_encoder(gpu.luid)?;
            let transform: IMFTransform = activate.ActivateObject()?;
            let attributes = transform.GetAttributes()?;
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            attributes.SetUINT32(&MF_LOW_LATENCY, 1)?;

            let mut token = 0;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(&gpu.device, token)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;

            // Most encoders use fixed stream IDs 0/0 and answer E_NOTIMPL here.
            let (mut input_id, mut output_id) = (0, 0);
            if transform
                .GetStreamIDs(std::slice::from_mut(&mut input_id), std::slice::from_mut(&mut output_id))
                .is_err()
            {
                (input_id, output_id) = (0, 0);
            }

            // Streaming-friendly settings, applied before the media types because some
            // encoders only read them then. Not every encoder supports every knob.
            let mut ignored = Vec::new();
            let codec = transform.cast::<ICodecAPI>().ok();
            let settings = [
                ("CBR", CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32)),
                ("bitrate", CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate)),
                // A keyframe every 2 seconds, which Twitch and YouTube require.
                ("keyframe interval", CODECAPI_AVEncMPVGOPSize, variant_u32(fps * 2)),
                ("no B-frames", CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0)),
                ("low latency", CODECAPI_AVLowLatencyMode, variant_true()),
            ];
            for (label, api, value) in &settings {
                if codec.as_ref().is_none_or(|codec| codec.SetValue(api, value).is_err()) {
                    ignored.push(*label);
                }
            }

            // Encoders want the output type before the input type.
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            set_video_format(&output, width, height, fps)?;
            transform.SetOutputType(output_id, &output, 0)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            set_video_format(&input, width, height, fps)?;
            transform.SetInputType(input_id, &input, 0)?;

            let info = transform.GetOutputStreamInfo(output_id)?;
            let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
            let output_buffer_size = (info.dwFlags & provides == 0).then_some(info.cbSize.max(width * height));

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events = transform.cast()?;

            Ok(Self { transform, events, input_id, output_id, output_buffer_size, _manager: manager, name, ignored })
        }
    }

    /// Blocks until the encoder asks for input, has output, or finished draining.
    #[allow(non_upper_case_globals)] // matching on Windows' own constant names
    pub fn next_event(&self) -> Result<Event> {
        let kind = unsafe { self.events.GetEvent(MF_EVENT_FLAG_NONE)?.GetType()? };
        Ok(match MF_EVENT_TYPE(kind as i32) {
            METransformNeedInput => Event::NeedInput,
            METransformHaveOutput => Event::HaveOutput,
            METransformDrainComplete => Event::DrainComplete,
            _ => Event::Other,
        })
    }

    pub fn push(&self, texture: &ID3D11Texture2D, time: i64, duration: i64) -> Result<()> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.transform.ProcessInput(self.input_id, &sample, 0)
        }
    }

    pub fn pull(&self) -> Result<Option<Packet>> {
        unsafe {
            let sample = match self.output_buffer_size {
                Some(size) => {
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&MFCreateMemoryBuffer(size)?)?;
                    Some(sample)
                }
                None => None,
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = ManuallyDrop::take(&mut buffers[0].pSample);
            drop(ManuallyDrop::take(&mut buffers[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder refined its output format: accept what it offers.
                    let offered = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                    self.transform.SetOutputType(self.output_id, &offered, 0)?;
                    return Ok(None);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) => return Err(e),
            }
            let Some(sample) = sample else { return Ok(None) };

            let time = sample.GetSampleTime()?;
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            Ok(Some(Packet { data, time, keyframe }))
        }
    }

    /// Asks the encoder to flush every frame it still holds; it answers with DrainComplete.
    pub fn drain(&self) -> Result<()> {
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
        }
    }

    /// SPS/PPS from the output type, for encoders that don't repeat them in the stream.
    pub fn sequence_header(&self) -> Option<Vec<u8>> {
        unsafe {
            let current = self.transform.GetOutputCurrentType(self.output_id).ok()?;
            let size = current.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
            let mut blob = vec![0; size as usize];
            current.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None).ok()?;
            Some(blob)
        }
    }
}

/// Picks the hardware H.264 encoder that lives on the same GPU as our device.
unsafe fn find_hardware_encoder(luid: LUID) -> Result<(IMFActivate, String)> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )?;
        let encoders: Vec<IMFActivate> = (0..count as usize).filter_map(|i| list.add(i).read()).collect();
        CoTaskMemFree(Some(list as *const _));

        let wanted = ((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64;
        let index = encoders
            .iter()
            .position(|e| e.GetUINT64(&MFT_ENUM_ADAPTER_LUID).ok() == Some(wanted))
            .unwrap_or(0);
        let activate = encoders
            .into_iter()
            .nth(index)
            .ok_or_else(|| Error::new(E_FAIL, "no hardware H.264 encoder found"))?;

        let mut name = PWSTR::null();
        let mut len = 0;
        let label = match activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len) {
            Ok(()) => {
                let label = name.to_string().unwrap_or_default();
                CoTaskMemFree(Some(name.0 as *const _));
                label
            }
            Err(_) => "Hardware H.264 encoder".into(),
        };
        Ok((activate, label))
    }
}

fn set_video_format(media_type: &IMFMediaType, width: u32, height: u32, fps: u32) -> Result<()> {
    let pack = |hi: u32, lo: u32| ((hi as u64) << 32) | lo as u64;
    unsafe {
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // BT.709 limited range, matching what the video processor produces,
        // so players don't have to guess the colours.
        media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
    }
}

fn variant(vt: VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 { vt, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: value }),
        },
    }
}

fn variant_u32(value: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}

fn variant_true() -> VARIANT {
    variant(VT_BOOL, VARIANT_0_0_0 { boolVal: VARIANT_TRUE })
}
