//! The MilerCast window: Home (game, destination, sound, camera, go live), a
//! small Live panel while streaming, and a separate Settings window for things
//! set once. Plain Win32 controls: no GPU, no web engine, and nothing is drawn
//! or measured while the window is minimised or behind the game.

mod config;
mod home;
mod key_dialog;
mod live;
mod secret;
mod settings;
mod text;
mod tray;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_GRAYTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, CreateFontIndirectW, CreatePen, CreateSolidBrush, DT_CENTER,
    DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, FW_SEMIBOLD, FillRect, GetSysColor, GetSysColorBrush,
    HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, PS_NULL, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW, RedrawWindow, RoundRect, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, ICC_BAR_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, ODS_DISABLED,
    ODS_SELECTED,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, GetDpiForWindow, SetProcessDpiAwarenessContext,
    SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use milercast::audio::{self, Gain};
use milercast::camera::{self, Overlay};
use milercast::capture;
use milercast::engine::{CameraChoice, Engine, Mic, Phase, Settings, Video};

use config::Config;
use text::Text;

// Control messages and styles, spelled out to keep the window code readable.
const CB_ADDSTRING: u32 = 0x0143;
const CB_GETCURSEL: u32 = 0x0147;
const CB_RESETCONTENT: u32 = 0x014B;
const CB_SETCURSEL: u32 = 0x014E;
const CB_SETDROPPEDWIDTH: u32 = 0x0160;
const CBN_SELCHANGE: u32 = 1;
const CBN_DROPDOWN: u32 = 7;
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const BN_CLICKED: u32 = 0;
const EM_SETPASSWORDCHAR: u32 = 0x00CC;
const EM_SETCUEBANNER: u32 = 0x1501;
const TBM_GETPOS: u32 = 0x0400;
const TBM_SETPOS: u32 = 0x0405;
const TBM_SETRANGE: u32 = 0x0406;
const BS_AUTOCHECKBOX: u32 = 0x3;
const BS_AUTORADIOBUTTON: u32 = 0x9;
const BS_OWNERDRAW: u32 = 0xB;
const BS_PUSHLIKE: u32 = 0x1000;
const ES_PASSWORD: u32 = 0x20;
const ES_AUTOHSCROLL: u32 = 0x80;
const CBS_DROPDOWNLIST: u32 = 0x3;
const SS_OWNERDRAW: u32 = 0xD;
const SS_ENDELLIPSIS: u32 = 0x4000;
const TBS_NOTICKS: u32 = 0x10;
const TABSTOP: u32 = 0x0001_0000;
const GROUP: u32 = 0x0002_0000;
const VSCROLL: u32 = 0x0020_0000;
const CLIENTEDGE: u32 = 0x200;
const BLACK_DOT: usize = 0x25CF;
const WM_TRAY: u32 = WM_APP + 1;

const TIMER: usize = 1;
const WIDTH: i32 = 420;

/// (height, fps, kbps, note: 0 none, 1 recommended, 2 YouTube, 3 slow internet)
const PRESETS: [(u32, u32, u32, u8); 5] =
    [(1080, 60, 6000, 1), (1080, 60, 9000, 2), (1080, 30, 4500, 0), (720, 60, 4500, 0), (720, 30, 3000, 3)];

/// Stream destinations: config name, label, where to find the key.
const DESTINATIONS: [(&str, &str, &str); 3] = [
    ("youtube", "YouTube", "https://www.youtube.com/live_dashboard"),
    ("twitch", "Twitch", "https://dashboard.twitch.tv/settings/stream"),
    ("custom", "", ""),
];

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// What painting needs, kept apart from `APP` so controls can repaint
    /// even while the app is busy handling a message.
    static PAINT: RefCell<Paint> = RefCell::new(Paint::default());
}

#[derive(Default)]
struct Paint {
    meters: [HWND; 2],
    levels: [f32; 2],
    muted: Vec<HWND>,
    colors: Vec<(HWND, COLORREF)>,
    /// The big red button(s).
    accent: Vec<HWND>,
    accent_font: Option<HFONT>,
    corner_buttons: [HWND; 4],
    corner: u8,
    brushes: Option<Brushes>,
}

#[derive(Clone, Copy)]
struct Brushes {
    track: HBRUSH,
    green: HBRUSH,
    amber: HBRUSH,
    red: HBRUSH,
    red_dark: HBRUSH,
    grey: HBRUSH,
    accent_soft: HBRUSH,
    outline: HBRUSH,
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Plain,
    Muted,
    Good,
    Warn,
    Bad,
    Live,
}

impl Tone {
    fn color(self) -> Option<COLORREF> {
        match self {
            Tone::Plain => None,
            Tone::Muted => Some(COLORREF(unsafe { GetSysColor(COLOR_GRAYTEXT) })),
            Tone::Good => Some(rgb(16, 124, 65)),
            Tone::Warn => Some(rgb(176, 108, 0)),
            Tone::Bad => Some(rgb(196, 43, 28)),
            Tone::Live => Some(rgb(200, 30, 45)),
        }
    }
}

struct Fonts {
    normal: HFONT,
    bold: HFONT,
    big: HFONT,
}

enum Choice {
    Auto,
    Window(HWND, String),
    Screen(usize, windows::Win32::Graphics::Gdi::HMONITOR),
}

impl Choice {
    fn key(&self) -> String {
        match self {
            Choice::Auto => "auto".into(),
            Choice::Window(_, title) => format!("window:{title}"),
            Choice::Screen(index, _) => format!("screen:{index}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Starting,
    Running,
    Stopping,
}

struct App {
    hwnd: HWND,
    text: &'static Text,
    dpi: i32,
    fonts: Fonts,
    config: Config,
    home: home::Home,
    live: live::Live,
    settings: Option<settings::SettingsWindow>,
    key: Option<key_dialog::KeyDialog>,
    choices: Vec<Choice>,
    /// Microphone names; "" is the Windows default.
    mics: Vec<String>,
    cameras: Vec<String>,
    desktop_gain: Gain,
    mic_gain: Gain,
    overlay: Arc<Overlay>,
    /// Audio opened only for the level meters, while Home is in front.
    preview: Vec<(usize, audio::Source)>,
    preview_on: bool,
    levels: [f32; 2],
    engine: Option<Engine>,
    mode: Mode,
    /// Streaming (true) or only recording (false).
    live_mode: bool,
    camera_opened: bool,
    warned: bool,
    record_path: Option<PathBuf>,
    minimized: bool,
    interval: u32,
    last_status: Instant,
    last_hint: Instant,
    upload: (u64, Instant, f64),
    drops: (u64, Instant),
}

/// Opens the window and runs until it's closed.
pub fn run() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES,
        });
        let instance = GetModuleHandleW(None).unwrap_or_default();
        for class in [w!("MilerCast"), w!("MilerCastPanel")] {
            RegisterClassExW(&WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
                hbrBackground: GetSysColorBrush(COLOR_WINDOW),
                lpszClassName: class,
                ..Default::default()
            });
        }
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("MilerCast"),
            w!("MilerCast"),
            MAIN_STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            WIDTH,
            400,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let dpi = GetDpiForWindow(hwnd) as i32;
        let app = App::new(hwnd, dpi);
        APP.with(|cell| *cell.borrow_mut() = Some(app));
        APP.with(|cell| cell.borrow_mut().as_mut().unwrap().init());
        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            // Tab moves between controls in whichever of our windows has focus.
            let root = GetAncestor(msg.hwnd, GA_ROOT);
            if !root.0.is_null() && IsDialogMessageW(root, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // Dropping the app stops the engine if it's still running.
        APP.with(|cell| cell.borrow_mut().take());
    }
}

// WS_CLIPCHILDREN: the window's own background never paints over its controls.
const MAIN_STYLE: WINDOW_STYLE =
    WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_CLIPCHILDREN.0);
const PANEL_STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_POPUP.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_CLIPCHILDREN.0);

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let painted = match msg {
        WM_CTLCOLORSTATIC => PAINT.with(|p| p.borrow().color_static(wparam, lparam)),
        WM_DRAWITEM => PAINT.with(|p| p.borrow().draw_item(lparam)),
        _ => None,
    };
    if let Some(result) = painted {
        return result;
    }
    let handled = APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut app) => app.as_mut().and_then(|app| app.handle(hwnd, msg, wparam, lparam)),
        // Re-entered, e.g. while a message box is open: let Windows handle it.
        Err(_) => None,
    });
    handled.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
}

impl Paint {
    fn color_static(&self, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let hdc = HDC(wparam.0 as *mut _);
        let control = HWND(lparam.0 as *mut _);
        unsafe {
            SetBkMode(hdc, TRANSPARENT);
            let color = match self.colors.iter().find(|(hwnd, _)| *hwnd == control) {
                Some((_, color)) => *color,
                None if self.muted.contains(&control) => COLORREF(GetSysColor(COLOR_GRAYTEXT)),
                None => COLORREF(GetSysColor(COLOR_WINDOWTEXT)),
            };
            SetTextColor(hdc, color);
            Some(LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize))
        }
    }

    fn draw_item(&self, lparam: LPARAM) -> Option<LRESULT> {
        let item = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
        let b = self.brushes?;
        if let Some(index) = self.meters.iter().position(|&m| m == item.hwndItem) {
            self.draw_meter(item, index, b);
        } else if let Some(corner) = self.corner_buttons.iter().position(|&c| c == item.hwndItem) {
            self.draw_corner(item, corner as u8, b);
        } else if self.accent.contains(&item.hwndItem) {
            self.draw_accent(item, b);
        } else {
            return None;
        }
        Some(LRESULT(1))
    }

    fn draw_meter(&self, item: &DRAWITEMSTRUCT, index: usize, b: Brushes) {
        let level = self.levels[index];
        let rect = item.rcItem;
        let filled = RECT { right: rect.left + ((rect.right - rect.left) as f32 * level) as i32, ..rect };
        let brush = if level > 0.95 {
            b.red
        } else if level > 0.8 {
            b.amber
        } else {
            b.green
        };
        unsafe {
            FillRect(item.hDC, &rect, b.track);
            FillRect(item.hDC, &filled, brush);
        }
    }

    /// A little screen with the chosen corner filled in.
    fn draw_corner(&self, item: &DRAWITEMSTRUCT, corner: u8, b: Brushes) {
        let r = item.rcItem;
        let chosen = corner == self.corner;
        unsafe {
            FillRect(item.hDC, &r, if chosen { b.accent_soft } else { GetSysColorBrush(COLOR_WINDOW) });
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let screen = RECT { left: r.left + w / 5, top: r.top + h / 4, right: r.right - w / 5, bottom: r.bottom - h / 4 };
            frame(item.hDC, screen, if chosen { b.red_dark } else { b.outline });
            let (sw, sh) = (screen.right - screen.left, screen.bottom - screen.top);
            let (cw, ch) = (sw * 2 / 5, sh * 2 / 5);
            let left = if corner % 2 == 0 { screen.left + 2 } else { screen.right - cw - 2 };
            let top = if corner < 2 { screen.top + 2 } else { screen.bottom - ch - 2 };
            let cam = RECT { left, top, right: left + cw, bottom: top + ch };
            FillRect(item.hDC, &cam, if chosen { b.red } else { b.grey });
        }
    }

    fn draw_accent(&self, item: &DRAWITEMSTRUCT, b: Brushes) {
        let r = item.rcItem;
        let pressed = item.itemState.0 & ODS_SELECTED.0 != 0;
        let disabled = item.itemState.0 & ODS_DISABLED.0 != 0;
        let fill = if disabled {
            b.grey
        } else if pressed {
            b.red_dark
        } else {
            b.red
        };
        unsafe {
            let pen = CreatePen(PS_NULL, 0, COLORREF(0));
            let old_pen = SelectObject(item.hDC, HGDIOBJ(pen.0));
            let old_brush = SelectObject(item.hDC, HGDIOBJ(fill.0));
            let radius = (r.bottom - r.top) / 4;
            let _ = RoundRect(item.hDC, r.left, r.top, r.right, r.bottom, radius, radius);
            SelectObject(item.hDC, old_brush);
            SelectObject(item.hDC, old_pen);
            let _ = DeleteObject(HGDIOBJ(pen.0));
            let mut label = [0u16; 64];
            let len = GetWindowTextW(item.hwndItem, &mut label);
            SetBkMode(item.hDC, TRANSPARENT);
            SetTextColor(item.hDC, rgb(255, 255, 255));
            let old_font = self.accent_font.map(|f| SelectObject(item.hDC, HGDIOBJ(f.0)));
            let mut text_rect = r;
            DrawTextW(item.hDC, &mut label[..len.max(0) as usize], &mut text_rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            if let Some(old) = old_font {
                SelectObject(item.hDC, old);
            }
        }
    }
}

/// A one-pixel outline.
unsafe fn frame(hdc: HDC, r: RECT, brush: HBRUSH) {
    unsafe {
        for edge in [
            RECT { bottom: r.top + 1, ..r },
            RECT { top: r.bottom - 1, ..r },
            RECT { right: r.left + 1, ..r },
            RECT { left: r.right - 1, ..r },
        ] {
            FillRect(hdc, &edge, brush);
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

/// Peak (0..1) to meter position: -60 dB is empty, 0 dB is full.
fn meter_level(peak: f32) -> f32 {
    if peak <= 0.001 { 0.0 } else { ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0) }
}

fn word(value: usize) -> u32 {
    (value & 0xFFFF) as u32
}

impl App {
    fn new(hwnd: HWND, dpi: i32) -> Self {
        let text = text::current();
        let config = Config::load();
        let overlay = Overlay::new(false, config.camera_corner, config.camera_size, config.camera_mirror);
        let mut app = App {
            hwnd,
            text,
            dpi,
            fonts: fonts(dpi, text.thai),
            home: home::Home::default(),
            live: live::Live::default(),
            settings: None,
            key: None,
            choices: Vec::new(),
            mics: Vec::new(),
            cameras: Vec::new(),
            desktop_gain: Gain::new(0.0),
            mic_gain: Gain::new(0.0),
            overlay,
            config,
            preview: Vec::new(),
            preview_on: false,
            levels: [0.0; 2],
            engine: None,
            mode: Mode::Idle,
            live_mode: false,
            camera_opened: false,
            warned: false,
            record_path: None,
            minimized: false,
            interval: 0,
            last_status: Instant::now(),
            last_hint: Instant::now(),
            upload: (0, Instant::now(), 0.0),
            drops: (0, Instant::now()),
        };
        app.home = app.build_home();
        app.live = app.build_live();
        PAINT.with(|p| {
            let mut p = p.borrow_mut();
            p.accent_font = Some(app.fonts.big);
            p.brushes = Some(unsafe {
                Brushes {
                    track: CreateSolidBrush(rgb(228, 228, 228)),
                    green: CreateSolidBrush(rgb(38, 166, 91)),
                    amber: CreateSolidBrush(rgb(230, 162, 60)),
                    red: CreateSolidBrush(rgb(214, 48, 49)),
                    red_dark: CreateSolidBrush(rgb(170, 30, 35)),
                    grey: CreateSolidBrush(rgb(180, 180, 180)),
                    accent_soft: CreateSolidBrush(rgb(252, 228, 228)),
                    outline: CreateSolidBrush(rgb(150, 150, 150)),
                }
            });
        });
        app
    }

    fn init(&mut self) {
        self.mics = std::iter::once(String::new())
            .chain(audio::microphones().unwrap_or_default().into_iter().map(|d| d.name))
            .collect();
        if !self.mics.contains(&self.config.mic_name) {
            self.config.mic_name.clear();
        }
        self.cameras = camera::cameras();
        if !self.cameras.contains(&self.config.camera_name) {
            // The first real camera: virtual ones re-send another app's picture.
            self.config.camera_name = self
                .cameras
                .iter()
                .find(|c| !c.to_lowercase().contains("virtual"))
                .or(self.cameras.first())
                .cloned()
                .unwrap_or_default();
        }
        if self.cameras.is_empty() {
            self.config.camera_on = false;
        }
        self.init_home();
        self.apply_gains();
        self.show_page(false);
        tray::add(self.hwnd, "MilerCast");
        self.check_activity();
    }

    /// Hotkeys exist only while streaming or recording: a system-wide shortcut
    /// should never change anything (like switching the camera on) while idle.
    fn hotkeys(&self, on: bool) {
        for (id, key) in [(1, 'M'), (2, 'G'), (3, 'C')] {
            unsafe {
                if on {
                    let _ = RegisterHotKey(Some(self.hwnd), id, MOD_CONTROL | MOD_ALT | MOD_NOREPEAT, key as u32);
                } else {
                    let _ = UnregisterHotKey(Some(self.hwnd), id);
                }
            }
        }
    }

    fn s(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }

    /// A child control on `parent`, in 96-dpi units.
    fn add(&self, parent: HWND, class: PCWSTR, label: &str, style: u32, ex: u32, rect: (i32, i32, i32, i32), id: u16) -> HWND {
        let (x, y, w, h) = rect;
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(ex),
                class,
                &HSTRING::from(label),
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
                self.s(x),
                self.s(y),
                self.s(w),
                self.s(h),
                Some(parent),
                Some(HMENU(id as usize as *mut _)),
                None,
                None,
            )
            .unwrap_or_default();
            set_font(hwnd, self.fonts.normal);
            hwnd
        }
    }

    fn label(&self, parent: HWND, label: &str, rect: (i32, i32, i32, i32)) -> HWND {
        self.add(parent, w!("STATIC"), label, SS_ENDELLIPSIS, 0, rect, 0)
    }

    fn heading(&self, parent: HWND, label: &str, rect: (i32, i32, i32, i32)) -> HWND {
        let hwnd = self.label(parent, label, rect);
        set_font(hwnd, self.fonts.bold);
        hwnd
    }

    /// Resizes `window` so its client area is `height` (96-dpi units) tall.
    fn fit_window(&self, window: HWND, height: i32, style: WINDOW_STYLE) {
        let mut frame = RECT { left: 0, top: 0, right: self.s(WIDTH), bottom: self.s(height) };
        unsafe {
            let _ = AdjustWindowRectExForDpi(&mut frame, style, false, WINDOW_EX_STYLE(0), self.dpi as u32);
            let _ = SetWindowPos(
                window,
                None,
                0,
                0,
                frame.right - frame.left,
                frame.bottom - frame.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// A small window over the main one (Settings, stream key), hidden until shown.
    fn panel(&self, title: &str, width: i32, height: i32) -> Option<HWND> {
        unsafe {
            let mut frame = RECT { left: 0, top: 0, right: self.s(width), bottom: self.s(height) };
            let _ = AdjustWindowRectExForDpi(&mut frame, PANEL_STYLE, false, WINDOW_EX_STYLE(0), self.dpi as u32);
            let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
            let mut owner = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut owner);
            let x = owner.left + ((owner.right - owner.left) - w) / 2;
            let y = owner.top + self.s(40);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("MilerCastPanel"),
                &HSTRING::from(title),
                PANEL_STYLE,
                x,
                y,
                w,
                h,
                Some(self.hwnd),
                None,
                None,
                None,
            )
            .ok()
        }
    }

    /// Home when idle, the small Live panel while running.
    fn show_page(&self, live: bool) {
        for (controls, visible) in [(&self.home.all, !live), (&self.live.all, live)] {
            for &control in controls {
                unsafe {
                    let _ = ShowWindow(control, if visible { SW_SHOW } else { SW_HIDE });
                }
            }
        }
        self.fit_window(self.hwnd, if live { live::HEIGHT } else { home::HEIGHT }, MAIN_STYLE);
        // Repaint everything now, so no half-drawn page shows after the switch.
        unsafe {
            let _ = RedrawWindow(Some(self.hwnd), None, None, RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW);
        }
    }

    fn handle(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        if self.settings.as_ref().is_some_and(|s| s.hwnd == hwnd) {
            return self.settings_message(msg, wparam);
        }
        if self.key.as_ref().is_some_and(|k| k.hwnd == hwnd) {
            return self.key_message(msg, wparam);
        }
        if hwnd != self.hwnd {
            return None;
        }
        match msg {
            WM_COMMAND => {
                let (id, code) = (word(wparam.0) as u16, word(wparam.0 >> 16));
                if !self.home_command(id, code) {
                    self.live_command(id, code);
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                self.tick();
                Some(LRESULT(0))
            }
            WM_HOTKEY => {
                match wparam.0 {
                    1 => self.set_mic(!self.config.mic_on),
                    2 => self.set_game(!self.config.desktop_on),
                    3 => self.set_camera(!self.config.camera_on),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_ACTIVATE => {
                self.check_activity();
                None
            }
            WM_SIZE => {
                self.minimized = wparam.0 == SIZE_MINIMIZED as usize;
                self.check_activity();
                None
            }
            WM_TRAY => {
                self.tray_event(word(lparam.0 as usize));
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                if self.engine.is_some() && !self.confirm(self.text.quit_while_live) {
                    return Some(LRESULT(0));
                }
                self.shutdown();
                None // DefWindowProc destroys the window
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    /// Meters and quick updates only while one of our windows is in front;
    /// a slow tick otherwise, just enough to notice the engine finishing.
    fn check_activity(&mut self) {
        let front = unsafe { GetForegroundWindow() };
        let ours = front == self.hwnd
            || self.settings.as_ref().is_some_and(|s| s.hwnd == front)
            || self.key.as_ref().is_some_and(|k| k.hwnd == front);
        let active = ours && !self.minimized;
        let want_preview = active && self.mode == Mode::Idle;
        if want_preview != self.preview_on {
            self.preview_on = want_preview;
            if want_preview {
                self.start_preview();
            } else {
                self.preview.clear();
                self.levels = [0.0; 2];
                self.repaint_meters();
            }
        }
        let interval = if self.minimized {
            1000
        } else if active {
            50
        } else {
            500
        };
        if interval != self.interval {
            self.interval = interval;
            unsafe { SetTimer(Some(self.hwnd), TIMER, interval, None) };
        }
    }

    fn start_preview(&mut self) {
        self.preview.clear();
        if let Ok(device) = audio::default_output() {
            if let Ok(source) = audio::Source::open(&device, true, self.desktop_gain.clone()) {
                self.preview.push((0, source));
            }
        }
        let mic = match self.config.mic_name.as_str() {
            "" => audio::default_microphone().ok(),
            name => audio::microphones().ok().and_then(|all| all.into_iter().find(|d| d.name == name)),
        };
        if let Some(source) = mic.and_then(|d| audio::Source::open(&d, false, self.mic_gain.clone()).ok()) {
            self.preview.push((1, source));
        }
    }

    fn tick(&mut self) {
        self.check_activity();
        if self.preview_on {
            let mut peaks = [0.0f32; 2];
            for (slot, source) in &mut self.preview {
                peaks[*slot] = peaks[*slot].max(source.drain_peak().unwrap_or(0.0));
            }
            for (level, peak) in self.levels.iter_mut().zip(peaks) {
                *level = meter_level(peak).max(*level - 0.04);
            }
            self.repaint_meters();
        }
        if self.last_status.elapsed() >= Duration::from_millis(500) {
            self.last_status = Instant::now();
            self.update_engine();
        }
        if self.mode == Mode::Idle && self.preview_on && self.last_hint.elapsed() >= Duration::from_secs(2) {
            self.last_hint = Instant::now();
            self.update_hint();
        }
    }

    fn repaint_meters(&self) {
        let meters = PAINT.with(|p| {
            let mut p = p.borrow_mut();
            p.levels = self.levels;
            p.meters
        });
        for meter in meters {
            unsafe {
                let _ = InvalidateRect(Some(meter), None, false);
            }
        }
    }

    /// Volumes and mute switches take effect at once, even while live.
    fn apply_gains(&self) {
        let gain = |on: bool, volume: u32| if on { volume as f32 / 100.0 } else { 0.0 };
        self.desktop_gain.set(gain(self.config.desktop_on, self.config.desktop_volume));
        self.mic_gain.set(gain(self.config.mic_on, self.config.mic_volume));
    }

    fn set_mic(&mut self, on: bool) {
        self.config.mic_on = on;
        self.apply_gains();
        self.sync_switches();
    }

    fn set_game(&mut self, on: bool) {
        self.config.desktop_on = on;
        self.apply_gains();
        self.sync_switches();
    }

    fn set_camera(&mut self, on: bool) {
        let running = self.mode != Mode::Idle;
        if self.cameras.is_empty() || (running && !self.camera_opened) {
            self.sync_switches();
            return;
        }
        self.config.camera_on = on;
        if self.camera_opened {
            self.overlay.set_visible(on);
        }
        self.sync_switches();
    }

    /// Home's checkboxes and the Live panel's buttons show the same switches.
    fn sync_switches(&self) {
        self.sync_home_switches();
        self.sync_live_switches();
    }

    fn start(&mut self, live: bool) {
        self.read_settings_window();
        let video = match self.resolve_video() {
            Ok(video) => video,
            Err(problem) => return self.notice(problem, Tone::Bad),
        };
        let mut stream = None;
        if live {
            let destination = self.config.destination.clone();
            let server = match destination.as_str() {
                "custom" => self.config.custom_server.trim().to_string(),
                other => other.to_string(),
            };
            if destination == "custom" && !server.starts_with("rtmp://") {
                return self.notice(self.text.need_server, Tone::Bad);
            }
            let Some(key) = secret::load(&destination) else {
                // First time: ask for the key instead of failing.
                return self.open_key_dialog();
            };
            stream = Some((server, key));
        }
        self.record_path = (!live || self.config.save_copy).then(recording_path);
        let (height, fps, video_kbps, _) = PRESETS[self.config.quality.min(PRESETS.len() - 1)];
        // The camera opens only if it's switched on: its light should never come on by surprise.
        let camera = (self.config.camera_on && !self.cameras.is_empty()).then(|| {
            self.overlay.set_visible(true);
            CameraChoice { name: Some(self.config.camera_name.clone()), overlay: self.overlay.clone() }
        });
        self.camera_opened = camera.is_some();
        // Both sources always open, so muting and unmuting works mid-stream.
        let settings = Settings {
            video,
            height,
            fps,
            video_kbps,
            desktop_audio: Some(self.desktop_gain.clone()),
            mic: Some(Mic {
                name: (!self.config.mic_name.is_empty()).then(|| self.config.mic_name.clone()),
                gain: self.mic_gain.clone(),
            }),
            audio_kbps: 160,
            camera,
            live: stream,
            record_to: self.record_path.clone(),
            stop_after: None,
        };
        self.config.save();
        self.preview.clear();
        self.preview_on = false;
        self.live_mode = live;
        self.warned = false;
        self.engine = Some(Engine::start(settings));
        self.mode = Mode::Starting;
        self.hotkeys(true);
        self.upload = (0, Instant::now(), 0.0);
        self.drops = (0, Instant::now());
        self.notice("", Tone::Plain);
        self.enter_live_panel();
        self.update_settings_window();
    }

    fn stop(&mut self) {
        if let Some(engine) = &self.engine {
            engine.stop();
            self.mode = Mode::Stopping;
            self.update_live_panel();
        }
    }

    fn update_engine(&mut self) {
        let Some(engine) = &self.engine else { return };
        let state = engine.state().clone();
        match state.phase() {
            Phase::Starting => {}
            Phase::Running => {
                if self.mode == Mode::Starting {
                    self.mode = Mode::Running;
                }
                self.refresh_live(&state);
            }
            Phase::Finished(_) => {
                let was_running = self.mode != Mode::Starting;
                let result = self.engine.take().map(Engine::join).unwrap_or(Ok(()));
                self.mode = Mode::Idle;
                self.hotkeys(false);
                self.camera_opened = false;
                self.show_page(false);
                self.sync_switches();
                self.update_settings_window();
                tray::update(self.hwnd, "MilerCast");
                let t = self.text;
                match (result, &self.record_path) {
                    (Ok(()), Some(path)) => self.notice(&format!("{}{}", t.saved_to, path.display()), Tone::Good),
                    (Ok(()), None) => self.notice(t.stream_ended, Tone::Muted),
                    (Err(e), _) => {
                        let prefix = if was_running { t.stopped_because } else { t.could_not_start };
                        self.notice(&format!("{prefix}{e}"), Tone::Bad);
                    }
                }
                self.check_activity();
            }
        }
    }

    fn resolve_video(&self) -> Result<Video, &'static str> {
        match self.choices.get(selected(self.home.capture)) {
            Some(Choice::Window(hwnd, title)) => {
                if unsafe { IsWindow(Some(*hwnd)) }.as_bool() {
                    Ok(Video::window(*hwnd))
                } else {
                    capture::find_window(title).map(|(hwnd, _)| Video::window(hwnd)).ok_or(self.text.window_gone)
                }
            }
            Some(Choice::Screen(_, monitor)) => Ok(Video::monitor(*monitor)),
            _ => capture::find_game().map(|(hwnd, _)| Video::window(hwnd)).ok_or(self.text.no_game),
        }
    }

    fn tray_event(&mut self, mouse: u32) {
        match mouse {
            WM_LBUTTONUP => self.bring_to_front(),
            WM_RBUTTONUP => {
                let t = self.text;
                let running = self.engine.is_some();
                let end = if self.live_mode { t.end_stream } else { t.stop_recording };
                let choice = tray::menu(
                    self.hwnd,
                    &[
                        (1, t.tray_show, false),
                        (0, "", false),
                        (2, t.mic, self.config.mic_on),
                        (3, t.game, self.config.desktop_on),
                        (0, "", false),
                        (if running { 4 } else { 0 }, if running { end } else { "" }, false),
                        (5, t.quit, false),
                    ],
                );
                match choice {
                    1 => self.bring_to_front(),
                    2 => self.set_mic(!self.config.mic_on),
                    3 => self.set_game(!self.config.desktop_on),
                    4 => self.stop(),
                    5 => {
                        if self.engine.is_none() || self.confirm(t.quit_while_live) {
                            self.shutdown();
                            unsafe {
                                let _ = DestroyWindow(self.hwnd);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn bring_to_front(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    /// Stops the engine, saves settings and tidies up before the window goes.
    fn shutdown(&mut self) {
        if let Some(engine) = self.engine.take() {
            engine.stop();
            let _ = engine.join();
        }
        self.read_settings_window();
        self.config.save();
        self.close_settings_window();
        tray::remove(self.hwnd);
    }

    fn confirm(&self, question: &str) -> bool {
        let answer =
            unsafe { MessageBoxW(Some(self.hwnd), &HSTRING::from(question), w!("MilerCast"), MB_YESNO | MB_ICONQUESTION) };
        answer == IDYES
    }
}

fn fonts(dpi: i32, thai: bool) -> Fonts {
    let mut metrics = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some(&mut metrics as *mut _ as *mut _),
            0,
            dpi as u32,
        );
    }
    let mut font = metrics.lfMessageFont;
    if thai {
        // Thai's own Windows UI font, a touch larger so the tone marks stay legible.
        let face: Vec<u16> = "Leelawadee UI".encode_utf16().collect();
        font.lfFaceName = [0; 32];
        font.lfFaceName[..face.len()].copy_from_slice(&face);
        font.lfHeight = font.lfHeight * 11 / 10;
    }
    let mut bold = font;
    bold.lfWeight = FW_SEMIBOLD.0 as i32;
    let mut big = bold;
    big.lfHeight = bold.lfHeight * 3 / 2;
    unsafe {
        Fonts { normal: CreateFontIndirectW(&font), bold: CreateFontIndirectW(&bold), big: CreateFontIndirectW(&big) }
    }
}

fn recordings_folder() -> PathBuf {
    let videos = unsafe {
        SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None).ok().map(|path| {
            let text = path.to_string().unwrap_or_default();
            CoTaskMemFree(Some(path.0 as *const _));
            PathBuf::from(text)
        })
    };
    videos.unwrap_or_else(|| PathBuf::from(".")).join("MilerCast")
}

fn recording_path() -> PathBuf {
    let t = unsafe { GetLocalTime() };
    recordings_folder().join(format!(
        "milercast-{:04}{:02}{:02}-{:02}{:02}{:02}.flv",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    ))
}

/// Opens a web page or folder the user asked for.
fn open(target: &str) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(target), None, None, SW_SHOWNORMAL);
    }
}

fn clock(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn set_font(control: HWND, font: HFONT) {
    unsafe {
        SendMessageW(control, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
}

fn set_text(control: HWND, text: &str) {
    unsafe {
        let _ = SetWindowTextW(control, &HSTRING::from(text));
    }
}

/// Sets a label's text and colour.
fn paint_text(control: HWND, text: &str, tone: Tone) {
    PAINT.with(|p| {
        let mut p = p.borrow_mut();
        p.colors.retain(|(hwnd, _)| *hwnd != control);
        if let Some(color) = tone.color() {
            p.colors.push((control, color));
        }
    });
    set_text(control, text);
    unsafe {
        let _ = InvalidateRect(Some(control), None, true);
    }
}

/// Destroys a panel and drops its labels from the paint lists, because
/// Windows hands their handles out again to new controls.
fn destroy_panel(window: HWND) {
    PAINT.with(|p| {
        let mut p = p.borrow_mut();
        p.muted.retain(|&hwnd| !unsafe { IsChild(window, hwnd) }.as_bool());
        p.colors.retain(|&(hwnd, _)| !unsafe { IsChild(window, hwnd) }.as_bool());
    });
    unsafe {
        let _ = DestroyWindow(window);
    }
}

fn get_text(control: HWND) -> String {
    unsafe {
        let mut buffer = vec![0u16; GetWindowTextLengthW(control) as usize + 1];
        let len = GetWindowTextW(control, &mut buffer);
        String::from_utf16_lossy(&buffer[..len.max(0) as usize])
    }
}

fn set_cue(control: HWND, text: &str) {
    let wide = HSTRING::from(text);
    unsafe {
        SendMessageW(control, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(wide.as_ptr() as isize)));
    }
}

fn add_item(combo: HWND, text: &str) {
    let wide = HSTRING::from(text);
    unsafe {
        SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(wide.as_ptr() as isize)));
    }
}

fn select(combo: HWND, index: usize) {
    unsafe {
        SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index)), None);
    }
}

fn selected(combo: HWND) -> usize {
    unsafe { SendMessageW(combo, CB_GETCURSEL, None, None).0.max(0) as usize }
}

fn set_check(button: HWND, on: bool) {
    unsafe {
        SendMessageW(button, BM_SETCHECK, Some(WPARAM(on as usize)), None);
    }
}

fn checked(button: HWND) -> bool {
    unsafe { SendMessageW(button, BM_GETCHECK, None, None).0 == 1 }
}

fn slider(control: HWND) -> u32 {
    unsafe { SendMessageW(control, TBM_GETPOS, None, None).0.clamp(0, 200) as u32 }
}

fn enable(control: HWND, on: bool) {
    unsafe {
        let _ = EnableWindow(control, on);
    }
}

fn invalidate(control: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(control), None, true);
    }
}

/// Cursor position, for placing the tray menu.
fn cursor() -> POINT {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    point
}
