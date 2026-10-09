//! Resize + colour conversion (BGRA -> NV12) on the GPU's fixed-function
//! video processor, the same block video players use. No shaders, no CPU.
//! The webcam, if any, is a second layer drawn in the same pass.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use windows::Win32::Foundation::{E_INVALIDARG, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_YUY2, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::{Error, Interface, Result};

use crate::camera::{Frame, Overlay, PixelFormat};
use crate::gpu::Gpu;

/// Every input is a whole, single-level 2D texture.
const INPUT_VIEW: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
    FourCC: 0,
    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
};

/// The encoder may still be reading a texture a few frames after we hand it
/// over, so we rotate through a small ring instead of reusing one texture.
const RING: usize = 8;

/// The webcam picture on the GPU, plus where to put it.
pub struct CameraLayer {
    texture: ID3D11Texture2D,
    /// CPU-writable twin: frames are written here, then copied on the GPU.
    staging: ID3D11Texture2D,
    size: (u32, u32),
    format: PixelFormat,
    pub overlay: Arc<Overlay>,
}

impl CameraLayer {
    pub fn new(gpu: &Gpu, size: (u32, u32), format: PixelFormat, overlay: Arc<Overlay>) -> Result<Self> {
        let dxgi = match format {
            PixelFormat::Nv12 => DXGI_FORMAT_NV12,
            PixelFormat::Yuy2 => DXGI_FORMAT_YUY2,
            PixelFormat::Rgb32 => DXGI_FORMAT_B8G8R8A8_UNORM,
        };
        // Drivers differ in which kinds of texture the video processor accepts
        // as input, so try the usual ones and keep the first that works.
        let video: ID3D11VideoDevice = gpu.device.cast()?;
        let rate = DXGI_RATIONAL { Numerator: 30, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: size.0,
            InputHeight: size.1,
            OutputFrameRate: rate,
            OutputWidth: size.0,
            OutputHeight: size.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video.CreateVideoProcessorEnumerator(&content)? };
        let (render, shader, decoder) =
            (D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_BIND_DECODER.0 as u32);
        let texture = [0, render, decoder, shader, render | shader]
            .into_iter()
            .filter_map(|bind| create_texture(&gpu.device, size, dxgi, D3D11_USAGE_DEFAULT, bind, 0).ok())
            .find(|texture| {
                let mut view = None;
                unsafe { video.CreateVideoProcessorInputView(texture, &enumerator, &INPUT_VIEW, Some(&mut view)) }.is_ok()
            })
            .ok_or_else(|| Error::new(E_INVALIDARG, "the video processor accepts no texture in the camera's format"))?;
        let staging = create_texture(&gpu.device, size, dxgi, D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_WRITE.0 as u32)?;
        Ok(Self { texture, staging, size, format, overlay })
    }

    /// Puts a new camera frame on the GPU.
    pub fn upload(&self, context: &ID3D11DeviceContext, frame: &Frame) -> Result<()> {
        let rows = self.format.rows(self.size.1) as usize;
        let row = frame.pitch as usize;
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))?;
            // NV12's colour plane follows the brightness plane at the same pitch.
            for (r, line) in frame.data.chunks_exact(row).take(rows).enumerate() {
                let target = (mapped.pData as *mut u8).add(r * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(line.as_ptr(), target, row.min(mapped.RowPitch as usize));
            }
            context.Unmap(&self.staging, 0);
            context.CopyResource(&self.texture, &self.staging);
        }
        Ok(())
    }
}

struct Layer {
    input: ID3D11VideoProcessorInputView,
    size: (u32, u32),
    overlay: Arc<Overlay>,
    /// Corner, size and mirror last given to the video processor.
    applied: Option<(u8, u8, bool)>,
}

pub struct Converter {
    context: ID3D11VideoContext,
    context1: Option<ID3D11VideoContext1>,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    canvas: (u32, u32),
    layer: Option<Layer>,
    /// Why the camera couldn't be added, if it couldn't: the stream goes on without it.
    pub camera_problem: Option<String>,
}

impl Converter {
    pub fn new(
        gpu: &Gpu,
        source: &ID3D11Texture2D,
        from: (u32, u32),
        to: (u32, u32),
        fps: u32,
        camera: Option<&CameraLayer>,
    ) -> Result<Self> {
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

            let mut input = None;
            device.CreateVideoProcessorInputView(source, &enumerator, &INPUT_VIEW, Some(&mut input))?;

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

            // Fit the picture inside the canvas, keeping its shape; black bars fill the rest.
            let whole = |(w, h): (u32, u32)| RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            let placed = fit(from, to);
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&whole(from)));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&placed));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&whole(to)));
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
            };
            context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);

            let context1 = context.cast::<ID3D11VideoContext1>().ok();
            let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
            let two_layers = enumerator.GetVideoProcessorCaps(&mut caps).is_ok() && caps.MaxInputStreams >= 2;
            let mut camera_problem = None;
            let layer = match camera {
                Some(_) if !two_layers => {
                    camera_problem = Some("this GPU's video processor can't draw a second layer".to_string());
                    None
                }
                Some(camera) => 'layer: {
                    let mut view = None;
                    let made = device.CreateVideoProcessorInputView(&camera.texture, &enumerator, &INPUT_VIEW, Some(&mut view));
                    if let Err(e) = made {
                        camera_problem = Some(format!("can't use the camera picture on the GPU: {}", e.message()));
                        break 'layer None;
                    }
                    context.VideoProcessorSetStreamFrameFormat(&processor, 1, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
                    context.VideoProcessorSetStreamAutoProcessingMode(&processor, 1, false);
                    context.VideoProcessorSetStreamSourceRect(&processor, 1, true, Some(&whole(camera.size)));
                    if let Some(context1) = &context1 {
                        let space = match camera.format {
                            PixelFormat::Rgb32 => DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                            _ => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                        };
                        context1.VideoProcessorSetStreamColorSpace1(&processor, 1, space);
                    }
                    Some(Layer { input: view.unwrap(), size: camera.size, overlay: camera.overlay.clone(), applied: None })
                }
                None => None,
            };

            Ok(Self {
                context,
                context1,
                processor,
                input: input.unwrap(),
                outputs,
                next: 0,
                canvas: to,
                layer,
                camera_problem,
            })
        }
    }

    /// Draws the capture (and the camera, if shown) into the next ring slot and returns it.
    pub fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let (texture, view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        let mut streams = vec![D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        }];
        if let Some(layer) = &mut self.layer {
            let (visible, corner, size, mirror) = layer.overlay.snapshot();
            if visible {
                if layer.applied != Some((corner, size, mirror)) {
                    let rect = camera_rect(self.canvas, layer.size, corner, size);
                    unsafe {
                        self.context.VideoProcessorSetStreamDestRect(&self.processor, 1, true, Some(&rect));
                        if let Some(context1) = &self.context1 {
                            context1.VideoProcessorSetStreamMirror(&self.processor, 1, mirror, true, false);
                        }
                    }
                    layer.applied = Some((corner, size, mirror));
                }
                streams.push(D3D11_VIDEO_PROCESSOR_STREAM {
                    Enable: true.into(),
                    pInputSurface: ManuallyDrop::new(Some(layer.input.clone())),
                    ..Default::default()
                });
            }
        }
        let layers = streams.len();
        let result = unsafe { self.context.VideoProcessorBlt(&self.processor, view, 0, &streams) };
        for stream in &mut streams {
            unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        }
        if let Err(e) = result {
            return Err(Error::new(e.code(), format!("drawing {layers} layer(s): {}", e.message())));
        }
        Ok(texture.clone())
    }
}

/// Where the camera goes: a corner (0 top left, 1 top right, 2 bottom left,
/// 3 bottom right), 20/27/35% of the canvas width, keeping the camera's shape.
fn camera_rect(canvas: (u32, u32), camera: (u32, u32), corner: u8, size: u8) -> RECT {
    let fraction = [0.20, 0.27, 0.35][size.min(2) as usize];
    let width = ((canvas.0 as f64 * fraction) as u32) & !1;
    let height = ((width as u64 * camera.1 as u64 / camera.0.max(1) as u64) as u32 & !1).min(canvas.1);
    let margin = ((canvas.1 as f64 * 0.025) as u32) & !1;
    let left = if corner % 2 == 0 { margin } else { canvas.0 - width - margin };
    let top = if corner < 2 { margin } else { canvas.1.saturating_sub(height + margin) };
    RECT { left: left as i32, top: top as i32, right: (left + width) as i32, bottom: (top + height) as i32 }
}

/// The largest rectangle with the source's shape that fits the canvas, centred,
/// with even coordinates (NV12 stores colour per 2x2 pixels).
fn fit(source: (u32, u32), canvas: (u32, u32)) -> RECT {
    let scale = (canvas.0 as f64 / source.0 as f64).min(canvas.1 as f64 / source.1 as f64);
    let even = |v: f64, max: u32| ((v.round() as u32) & !1).clamp(2, max);
    let width = even(source.0 as f64 * scale, canvas.0);
    let height = even(source.1 as f64 * scale, canvas.1);
    let left = ((canvas.0 - width) / 2) & !1;
    let top = ((canvas.1 - height) / 2) & !1;
    RECT { left: left as i32, top: top as i32, right: (left + width) as i32, bottom: (top + height) as i32 }
}

fn create_nv12(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let bind = D3D11_BIND_RENDER_TARGET.0 as u32;
    create_texture(device, (width, height), DXGI_FORMAT_NV12, D3D11_USAGE_DEFAULT, bind, 0)
}

fn create_texture(
    device: &ID3D11Device,
    (width, height): (u32, u32),
    format: DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind: u32,
    cpu_access: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu_access,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    Ok(texture.unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(r: RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right, r.bottom)
    }

    #[test]
    fn fills_canvas_when_shapes_match() {
        assert_eq!(rect(fit((2560, 1440), (1920, 1080))), (0, 0, 1920, 1080));
    }

    #[test]
    fn letterboxes_a_slightly_short_window() {
        // A maximised window minus the taskbar: thin black bars top and bottom.
        assert_eq!(rect(fit((2560, 1392), (1920, 1080))), (0, 18, 1920, 1062));
    }

    #[test]
    fn pillarboxes_a_portrait_monitor() {
        assert_eq!(rect(fit((1080, 1920), (1920, 1080))), (656, 0, 1264, 1080));
    }

    #[test]
    fn camera_sits_in_the_chosen_corner() {
        // Medium 16:9 camera on a 1080p canvas: 518x290, 26 px from the edges.
        assert_eq!(rect(camera_rect((1920, 1080), (1280, 720), 0, 1)), (26, 26, 544, 316));
        assert_eq!(rect(camera_rect((1920, 1080), (1280, 720), 3, 1)), (1376, 764, 1894, 1054));
    }
}
