//! Resize + colour conversion (BGRA -> NV12) on the GPU's fixed-function
//! video processor, the same block video players use. No shaders, no CPU.

use std::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::{Interface, Result};

use crate::gpu::Gpu;

/// The encoder may still be reading a texture a few frames after we hand it
/// over, so we rotate through a small ring instead of reusing one texture.
const RING: usize = 8;

pub struct Converter {
    context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl Converter {
    pub fn new(gpu: &Gpu, source: &ID3D11Texture2D, from: (u32, u32), to: (u32, u32), fps: u32) -> Result<Self> {
        unsafe {
            let device: ID3D11VideoDevice = gpu.device.cast()?;
            let context: ID3D11VideoContext = gpu.context.cast()?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: from.0,
                InputHeight: from.1,
                OutputFrameRate: rate,
                OutputWidth: to.0,
                OutputHeight: to.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = device.CreateVideoProcessorEnumerator(&content)?;
            let processor = device.CreateVideoProcessor(&enumerator, 0)?;

            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input = None;
            device.CreateVideoProcessorInputView(source, &enumerator, &input_desc, Some(&mut input))?;

            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut outputs = Vec::with_capacity(RING);
            for _ in 0..RING {
                let texture = create_nv12(&gpu.device, to.0, to.1)?;
                let mut view = None;
                device.CreateVideoProcessorOutputView(&texture, &enumerator, &output_desc, Some(&mut view))?;
                outputs.push((texture, view.unwrap()));
            }

            // Screens are full-range RGB; streaming platforms expect BT.709 limited-range YUV.
            if let Ok(context1) = context.cast::<ID3D11VideoContext1>() {
                context1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                context1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // No driver "enhancements" (sharpening, denoise...): we want a faithful copy.
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

            Ok(Self { context, processor, input: input.unwrap(), outputs, next: 0 })
        }
    }

    /// Converts the current capture texture into the next ring slot and returns it.
    pub fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let (texture, view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        let result = unsafe {
            self.context.VideoProcessorBlt(&self.processor, view, 0, std::slice::from_ref(&stream))
        };
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result?;
        Ok(texture.clone())
    }
}

fn create_nv12(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    Ok(texture.unwrap())
}
