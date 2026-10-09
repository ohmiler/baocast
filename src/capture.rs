//! Windows Graphics Capture: the OS hands us every new frame of a window or
//! monitor as a GPU texture. No hooks into the game, so anti-cheat stays happy.

use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint,
};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindowVisible};
use windows::core::{BOOL, Interface, Result, factory};

use crate::gpu::Gpu;

pub enum Target {
    Monitor(HMONITOR),
    Window(HWND),
}

pub struct Capture {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    device: ID3D11Device,
    winrt_device: IDirect3DDevice,
    context: ID3D11DeviceContext,
    /// Always holds the most recent frame.
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
}

pub enum Poll {
    /// Nothing new since last time (the game didn't draw).
    Unchanged,
    /// `texture` holds a new frame.
    NewFrame,
    /// The window changed size: `texture`, `width` and `height` are new.
    Resized,
}

const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

impl Capture {
    pub fn new(gpu: &Gpu, target: Target) -> Result<Self> {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe {
            match target {
                Target::Monitor(monitor) => interop.CreateForMonitor(monitor)?,
                Target::Window(window) => interop.CreateForWindow(window)?,
            }
        };
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&gpu.winrt_device, FORMAT, 2, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        // Optional extras that older Windows builds lack, so failures are fine.
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);

        let (width, height) = (size.Width as u32, size.Height as u32);
        let texture = create_texture(&gpu.device, width, height)?;
        session.StartCapture()?;
        Ok(Self {
            _item: item,
            pool,
            session,
            device: gpu.device.clone(),
            winrt_device: gpu.winrt_device.clone(),
            context: gpu.context.clone(),
            texture,
            width,
            height,
        })
    }

    /// Copies the newest frame into `texture`, if there is one.
    pub fn poll(&mut self) -> Result<Poll> {
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(older) = newest.replace(frame) {
                older.Close()?;
            }
        }
        let Some(frame) = newest else { return Ok(Poll::Unchanged) };
        let size = frame.ContentSize()?;
        if size.Width <= 0 || size.Height <= 0 {
            // Minimised: keep showing the last frame.
            frame.Close()?;
            return Ok(Poll::Unchanged);
        }
        if (size.Width as u32, size.Height as u32) != (self.width, self.height) {
            frame.Close()?;
            self.resize(size)?;
            return Ok(Poll::Resized);
        }
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        unsafe {
            let source: ID3D11Texture2D = access.GetInterface()?;
            self.context.CopyResource(&self.texture, &source);
        }
        frame.Close()?;
        Ok(Poll::NewFrame)
    }

    fn resize(&mut self, size: SizeInt32) -> Result<()> {
        self.pool.Recreate(&self.winrt_device, FORMAT, 2, size)?;
        (self.width, self.height) = (size.Width as u32, size.Height as u32);
        self.texture = create_texture(&self.device, self.width, self.height)?;
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn create_texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    Ok(texture.unwrap())
}

/// Visible top-level windows that have a title.
pub fn list_windows() -> Vec<(HWND, String)> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let windows = unsafe { &mut *(lparam.0 as *mut Vec<(HWND, String)>) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let mut title = [0u16; 256];
            let len = unsafe { GetWindowTextW(hwnd, &mut title) };
            if len > 0 {
                windows.push((hwnd, String::from_utf16_lossy(&title[..len as usize])));
            }
        }
        true.into()
    }
    let mut windows = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut windows as *mut _ as isize));
    }
    windows
}

/// First visible window whose title contains `query` (case-insensitive).
pub fn find_window(query: &str) -> Option<(HWND, String)> {
    let query = query.to_lowercase();
    list_windows().into_iter().find(|(_, title)| title.to_lowercase().contains(&query))
}

/// All monitors, primary first.
pub fn monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, lparam: LPARAM) -> BOOL {
        unsafe { (*(lparam.0 as *mut Vec<HMONITOR>)).push(monitor) };
        true.into()
    }
    let mut monitors = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut monitors as *mut _ as isize));
    }
    let primary = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    monitors.sort_by_key(|&monitor| monitor != primary);
    monitors
}
