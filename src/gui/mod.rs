//! The MilerCast window. Plain Win32 controls: no GPU, no web engine, and only
//! what changes gets redrawn. The engine runs on its own thread, so the window
//! can't slow the stream down (or the other way round).

mod config;
mod secret;
mod text;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    COLOR_GRAYTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, CreateFontIndirectW, CreateSolidBrush, FW_SEMIBOLD, FillRect,
    GetSysColor, GetSysColorBrush, HBRUSH, HDC, HFONT, InvalidateRect, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, ICC_BAR_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, GetDpiForWindow, SetProcessDpiAwarenessContext,
    SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, w};

use milercast::audio::{self, Gain};
use milercast::camera::{self, Overlay};
use milercast::capture;
use milercast::engine::{CameraChoice, Engine, Mic, Phase, Settings, Video};
use milercast::rtmp::{self, Status};

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

// Control IDs.
const ID_CAPTURE: u16 = 100;
const ID_YOUTUBE: u16 = 101;
const ID_TWITCH: u16 = 102;
const ID_CUSTOM: u16 = 103;
const ID_SHOW_KEY: u16 = 104;
const ID_REMEMBER: u16 = 105;
const ID_DESKTOP_ON: u16 = 106;
const ID_MIC_ON: u16 = 107;
const ID_MIC: u16 = 108;
const ID_QUALITY: u16 = 109;
const ID_SAVE_COPY: u16 = 110;
const ID_GO_LIVE: u16 = 111;
const ID_RECORD: u16 = 112;
const ID_SERVER: u16 = 113;
const ID_KEY: u16 = 114;
const ID_CAMERA_ON: u16 = 115;
const ID_CAMERA: u16 = 116;
const ID_CORNER: u16 = 117;
const ID_SIZE: u16 = 118;
const ID_MIRROR: u16 = 119;
const ID_OTHER: u16 = 199;

const TIMER: usize = 1;
const CLIENT: (i32, i32) = (420, 642);

/// (height, fps, kbps, note: 0 none, 1 recommended, 2 YouTube, 3 slow internet)
const PRESETS: [(u32, u32, u32, u8); 5] =
    [(1080, 60, 6000, 1), (1080, 60, 9000, 2), (1080, 30, 4500, 0), (720, 60, 4500, 0), (720, 30, 3000, 3)];

const BLACK_DOT: usize = 0x25CF;

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
    muted_text: Vec<HWND>,
    status: HWND,
    status_color: Option<COLORREF>,
    brushes: Option<[HBRUSH; 4]>, // track, green, amber, red
}

#[derive(Default)]
struct Controls {
    capture: HWND,
    capture_hint: HWND,
    youtube: HWND,
    twitch: HWND,
    custom: HWND,
    server: HWND,
    key: HWND,
    show_key: HWND,
    remember: HWND,
    desktop_on: HWND,
    desktop_volume: HWND,
    desktop_pct: HWND,
    mic_on: HWND,
    mic_volume: HWND,
    mic_pct: HWND,
    mic: HWND,
    camera_on: HWND,
    camera: HWND,
    corner: HWND,
    size: HWND,
    mirror: HWND,
    quality: HWND,
    save_copy: HWND,
    go_live: HWND,
    record: HWND,
    status: HWND,
    stats: HWND,
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
    font: HFONT,
    bold: HFONT,
    c: Controls,
    config: Config,
    choices: Vec<Choice>,
    /// Microphone names in the list; "" is the Windows default.
    mics: Vec<String>,
    desktop_gain: Gain,
    mic_gain: Gain,
    /// Camera placement, shared with the engine so it can change while live.
    overlay: Arc<Overlay>,
    cameras: Vec<String>,
    /// Whether the running engine was started with the camera.
    camera_opened: bool,
    /// Audio opened only for the level meters while not live.
    preview: Vec<(usize, audio::Source)>,
    levels: [f32; 2],
    engine: Option<Engine>,
    mode: Mode,
    live: bool,
    showing_key: bool,
    record_path: Option<PathBuf>,
    ticks: u32,
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
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            lpszClassName: w!("MilerCast"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("MilerCast"),
            w!("MilerCast"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CLIENT.0,
            CLIENT.1,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let dpi = GetDpiForWindow(hwnd) as i32;
        let mut frame = RECT { left: 0, top: 0, right: CLIENT.0 * dpi / 96, bottom: CLIENT.1 * dpi / 96 };
        let _ = AdjustWindowRectExForDpi(&mut frame, style, false, WINDOW_EX_STYLE(0), dpi as u32);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            frame.right - frame.left,
            frame.bottom - frame.top,
            SWP_NOMOVE | SWP_NOZORDER,
        );

        let app = App::new(hwnd, dpi);
        APP.with(|cell| *cell.borrow_mut() = Some(app));
        APP.with(|cell| cell.borrow_mut().as_mut().unwrap().init());
        let _ = ShowWindow(hwnd, SW_SHOW);
        SetTimer(Some(hwnd), TIMER, 50, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if IsDialogMessageW(hwnd, &msg).as_bool() {
                continue; // Tab moves between controls
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // Dropping the app stops the engine if it's still running.
        APP.with(|cell| cell.borrow_mut().take());
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let painted = match msg {
        WM_CTLCOLORSTATIC => PAINT.with(|p| p.borrow().color_static(wparam, lparam)),
        WM_DRAWITEM => PAINT.with(|p| p.borrow().draw_meter(lparam)),
        _ => None,
    };
    if let Some(result) = painted {
        return result;
    }
    let handled = APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut app) => app.as_mut().and_then(|app| app.handle(msg, wparam)),
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
            let color = if control == self.status {
                self.status_color.unwrap_or(COLORREF(GetSysColor(COLOR_WINDOWTEXT)))
            } else if self.muted_text.contains(&control) {
                COLORREF(GetSysColor(COLOR_GRAYTEXT))
            } else {
                COLORREF(GetSysColor(COLOR_WINDOWTEXT))
            };
            SetTextColor(hdc, color);
            Some(LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize))
        }
    }

    fn draw_meter(&self, lparam: LPARAM) -> Option<LRESULT> {
        let item = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
        let index = self.meters.iter().position(|&m| m == item.hwndItem)?;
        let [track, green, amber, red] = self.brushes?;
        let level = self.levels[index];
        let rect = item.rcItem;
        let filled = RECT { right: rect.left + ((rect.right - rect.left) as f32 * level) as i32, ..rect };
        let brush = if level > 0.95 {
            red
        } else if level > 0.8 {
            amber
        } else {
            green
        };
        unsafe {
            FillRect(item.hDC, &rect, track);
            FillRect(item.hDC, &filled, brush);
        }
        Some(LRESULT(1))
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
        let (font, bold) = fonts(dpi, text.thai);
        let config = Config::load();
        let overlay = Overlay::new(false, config.camera_corner, config.camera_size, config.camera_mirror);
        let mut app = App {
            hwnd,
            text,
            dpi,
            font,
            bold,
            c: Controls::default(),
            desktop_gain: Gain::new(0.0),
            mic_gain: Gain::new(0.0),
            overlay,
            cameras: Vec::new(),
            camera_opened: false,
            config,
            choices: Vec::new(),
            mics: Vec::new(),
            preview: Vec::new(),
            levels: [0.0; 2],
            engine: None,
            mode: Mode::Idle,
            live: false,
            showing_key: false,
            record_path: None,
            ticks: 0,
            upload: (0, Instant::now(), 0.0),
            drops: (0, Instant::now()),
        };
        app.build();
        app
    }

    fn s(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }

    fn add(&self, class: PCWSTR, label: &str, style: u32, ex: u32, (x, y, w, h): (i32, i32, i32, i32), id: u16) -> HWND {
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
                Some(self.hwnd),
                Some(HMENU(id as usize as *mut _)),
                None,
                None,
            )
            .unwrap_or_default();
            SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
            hwnd
        }
    }

    fn heading(&self, label: &str, y: i32) {
        let hwnd = self.add(w!("STATIC"), label, 0, 0, (16, y, 388, 20), ID_OTHER);
        unsafe { SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(self.bold.0 as usize)), Some(LPARAM(1))) };
    }

    fn label(&self, label: &str, rect: (i32, i32, i32, i32)) -> HWND {
        self.add(w!("STATIC"), label, SS_ENDELLIPSIS, 0, rect, ID_OTHER)
    }

    fn build(&mut self) {
        let t = self.text;
        let button = w!("BUTTON");
        let combo = w!("COMBOBOX");
        let edit = w!("EDIT");
        let slider = w!("msctls_trackbar32");

        self.heading(t.capture, 14);
        self.c.capture = self.add(combo, "", CBS_DROPDOWNLIST | VSCROLL | TABSTOP, 0, (16, 36, 388, 320), ID_CAPTURE);
        self.c.capture_hint = self.label("", (16, 63, 388, 18));

        self.heading(t.stream_to, 92);
        let radio = BS_AUTORADIOBUTTON | BS_PUSHLIKE | TABSTOP;
        self.c.youtube = self.add(button, "YouTube", radio | GROUP, 0, (16, 114, 124, 28), ID_YOUTUBE);
        self.c.twitch = self.add(button, "Twitch", radio, 0, (148, 114, 124, 28), ID_TWITCH);
        self.c.custom = self.add(button, t.custom, radio, 0, (280, 114, 124, 28), ID_CUSTOM);
        self.label(t.server, (16, 155, 84, 20));
        self.c.server = self.add(edit, "", ES_AUTOHSCROLL | TABSTOP | GROUP, CLIENTEDGE, (104, 151, 300, 24), ID_SERVER);
        self.label(t.stream_key, (16, 185, 84, 20));
        self.c.key = self.add(edit, "", ES_AUTOHSCROLL | ES_PASSWORD | TABSTOP, CLIENTEDGE, (104, 181, 232, 24), ID_KEY);
        self.c.show_key = self.add(button, t.show, TABSTOP, 0, (342, 180, 62, 26), ID_SHOW_KEY);
        self.c.remember = self.add(button, t.remember_key, BS_AUTOCHECKBOX | TABSTOP, 0, (104, 210, 300, 20), ID_REMEMBER);

        self.heading(t.audio, 242);
        self.c.desktop_on = self.add(button, t.desktop, BS_AUTOCHECKBOX | TABSTOP, 0, (16, 264, 236, 22), ID_DESKTOP_ON);
        self.c.desktop_volume = self.add(slider, "", TBS_NOTICKS | TABSTOP, 0, (256, 263, 108, 26), ID_OTHER);
        self.c.desktop_pct = self.label("", (368, 266, 40, 20));
        let desktop_meter = self.add(w!("STATIC"), "", SS_OWNERDRAW, 0, (16, 291, 388, 6), ID_OTHER);
        self.c.mic_on = self.add(button, t.microphone, BS_AUTOCHECKBOX | TABSTOP, 0, (16, 306, 236, 22), ID_MIC_ON);
        self.c.mic_volume = self.add(slider, "", TBS_NOTICKS | TABSTOP, 0, (256, 305, 108, 26), ID_OTHER);
        self.c.mic_pct = self.label("", (368, 308, 40, 20));
        self.c.mic = self.add(combo, "", CBS_DROPDOWNLIST | VSCROLL | TABSTOP, 0, (16, 332, 388, 240), ID_MIC);
        let mic_meter = self.add(w!("STATIC"), "", SS_OWNERDRAW, 0, (16, 362, 388, 6), ID_OTHER);

        self.heading(t.camera, 380);
        self.c.camera_on = self.add(button, t.show_camera, BS_AUTOCHECKBOX | TABSTOP, 0, (16, 403, 130, 22), ID_CAMERA_ON);
        self.c.camera = self.add(combo, "", CBS_DROPDOWNLIST | VSCROLL | TABSTOP, 0, (150, 402, 254, 200), ID_CAMERA);
        self.c.corner = self.add(combo, "", CBS_DROPDOWNLIST | TABSTOP, 0, (16, 434, 150, 200), ID_CORNER);
        self.c.size = self.add(combo, "", CBS_DROPDOWNLIST | TABSTOP, 0, (174, 434, 110, 200), ID_SIZE);
        self.c.mirror = self.add(button, t.mirror, BS_AUTOCHECKBOX | TABSTOP, 0, (294, 436, 110, 22), ID_MIRROR);

        self.heading(t.quality, 466);
        self.c.quality = self.add(combo, "", CBS_DROPDOWNLIST | TABSTOP, 0, (16, 488, 388, 200), ID_QUALITY);
        self.c.save_copy = self.add(button, t.save_copy, BS_AUTOCHECKBOX | TABSTOP, 0, (16, 520, 388, 20), ID_SAVE_COPY);

        self.c.go_live = self.add(button, t.go_live, TABSTOP, 0, (16, 552, 250, 38), ID_GO_LIVE);
        self.c.record = self.add(button, t.record, TABSTOP, 0, (274, 552, 130, 38), ID_RECORD);
        self.c.status = self.label(t.ready, (16, 600, 388, 20));
        unsafe { SendMessageW(self.c.status, WM_SETFONT, Some(WPARAM(self.bold.0 as usize)), Some(LPARAM(1))) };
        self.c.stats = self.label("", (16, 620, 388, 18));

        PAINT.with(|p| {
            let mut p = p.borrow_mut();
            p.meters = [desktop_meter, mic_meter];
            p.muted_text = vec![self.c.capture_hint, self.c.stats, self.c.desktop_pct, self.c.mic_pct];
            p.status = self.c.status;
            p.brushes = Some(unsafe {
                [
                    CreateSolidBrush(rgb(228, 228, 228)),
                    CreateSolidBrush(rgb(38, 166, 91)),
                    CreateSolidBrush(rgb(230, 162, 60)),
                    CreateSolidBrush(rgb(220, 53, 69)),
                ]
            });
        });
    }

    /// Fills the controls from the saved settings.
    fn init(&mut self) {
        let t = self.text;
        unsafe {
            SendMessageW(self.c.capture, CB_SETDROPPEDWIDTH, Some(WPARAM(self.s(520) as usize)), None);
            for (slider, volume) in [(self.c.desktop_volume, self.config.desktop_volume), (self.c.mic_volume, self.config.mic_volume)] {
                SendMessageW(slider, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM((200 << 16) as isize)));
                SendMessageW(slider, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(volume as isize)));
            }
            set_cue(self.c.key, t.key_placeholder);
            set_cue(self.c.server, "rtmp://");
        }
        self.fill_captures();
        self.fill_mics();
        for (index, &(height, fps, kbps, note)) in PRESETS.iter().enumerate() {
            let note = match note {
                1 => format!(" ({})", t.recommended),
                2 => " (YouTube)".to_string(),
                3 => format!(" ({})", t.slow_internet),
                _ => String::new(),
            };
            add_item(self.c.quality, &format!("{height}p {fps} fps · {} Mbps{note}", kbps as f32 / 1000.0));
            if index == self.config.quality {
                select(self.c.quality, index);
            }
        }
        set_check(self.c.remember, self.config.remember_key);
        set_check(self.c.desktop_on, self.config.desktop_on);
        set_check(self.c.mic_on, self.config.mic_on);
        set_check(self.c.save_copy, self.config.save_copy);
        for corner in t.corners {
            add_item(self.c.corner, corner);
        }
        for size in t.sizes {
            add_item(self.c.size, size);
        }
        select(self.c.corner, self.config.camera_corner as usize);
        select(self.c.size, self.config.camera_size as usize);
        set_check(self.c.mirror, self.config.camera_mirror);
        set_check(self.c.camera_on, self.config.camera_on);
        self.fill_cameras();
        self.show_destination();
        self.apply_audio();
        self.start_preview();
        self.update_hint();
        self.apply_mode();
    }

    fn handle(&mut self, msg: u32, wparam: WPARAM) -> Option<LRESULT> {
        match msg {
            WM_COMMAND => {
                let (id, code) = (word(wparam.0) as u16, word(wparam.0 >> 16));
                self.command(id, code);
                Some(LRESULT(0))
            }
            WM_HSCROLL => {
                self.apply_audio();
                Some(LRESULT(0))
            }
            WM_TIMER => {
                self.tick();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                if self.engine.is_some() {
                    if !self.confirm(self.text.quit_while_live) {
                        return Some(LRESULT(0));
                    }
                    if let Some(engine) = self.engine.take() {
                        engine.stop();
                        let _ = engine.join();
                    }
                }
                self.read_fields();
                self.config.save();
                None // DefWindowProc destroys the window
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn command(&mut self, id: u16, code: u32) {
        match (id, code) {
            (ID_CAPTURE, CBN_DROPDOWN) => self.fill_captures(),
            (ID_CAPTURE, CBN_SELCHANGE) => {
                if let Some(choice) = self.choices.get(selected(self.c.capture)) {
                    self.config.capture = choice.key();
                }
                self.update_hint();
            }
            (ID_YOUTUBE, BN_CLICKED) => self.choose_destination("youtube"),
            (ID_TWITCH, BN_CLICKED) => self.choose_destination("twitch"),
            (ID_CUSTOM, BN_CLICKED) => self.choose_destination("custom"),
            (ID_SHOW_KEY, BN_CLICKED) => {
                self.showing_key = !self.showing_key;
                let mask = if self.showing_key { 0 } else { BLACK_DOT };
                unsafe {
                    SendMessageW(self.c.key, EM_SETPASSWORDCHAR, Some(WPARAM(mask)), None);
                    let _ = InvalidateRect(Some(self.c.key), None, true);
                }
                set_text(self.c.show_key, if self.showing_key { self.text.hide } else { self.text.show });
            }
            (ID_DESKTOP_ON | ID_MIC_ON, BN_CLICKED) => self.apply_audio(),
            (ID_MIC, CBN_SELCHANGE) => {
                self.config.mic_name = self.mics.get(selected(self.c.mic)).cloned().unwrap_or_default();
                self.start_preview();
            }
            (ID_CAMERA_ON, BN_CLICKED) => {
                self.config.camera_on = checked(self.c.camera_on);
                if self.camera_opened {
                    self.overlay.set_visible(self.config.camera_on);
                }
            }
            (ID_CAMERA, CBN_SELCHANGE) => {
                if let Some(name) = self.cameras.get(selected(self.c.camera)) {
                    self.config.camera_name = name.clone();
                }
            }
            (ID_CORNER, CBN_SELCHANGE) => {
                self.config.camera_corner = selected(self.c.corner).min(3) as u8;
                self.overlay.set_corner(self.config.camera_corner);
            }
            (ID_SIZE, CBN_SELCHANGE) => {
                self.config.camera_size = selected(self.c.size).min(2) as u8;
                self.overlay.set_size(self.config.camera_size);
            }
            (ID_MIRROR, BN_CLICKED) => {
                self.config.camera_mirror = checked(self.c.mirror);
                self.overlay.set_mirror(self.config.camera_mirror);
            }
            (ID_GO_LIVE, BN_CLICKED) => match self.mode {
                Mode::Idle => self.start(true),
                _ => self.stop(),
            },
            (ID_RECORD, BN_CLICKED) => match self.mode {
                Mode::Idle => self.start(false),
                _ => self.stop(),
            },
            _ => {}
        }
    }

    fn fill_captures(&mut self) {
        let current = self.choices.get(selected(self.c.capture)).map(Choice::key);
        let wanted = current.unwrap_or_else(|| self.config.capture.clone());
        self.choices = vec![Choice::Auto];
        self.choices.extend(capture::list_windows().into_iter().map(|(hwnd, title)| Choice::Window(hwnd, title)));
        self.choices.extend(capture::monitors().into_iter().enumerate().map(|(i, m)| Choice::Screen(i, m)));
        unsafe { SendMessageW(self.c.capture, CB_RESETCONTENT, None, None) };
        for choice in &self.choices {
            let label = match choice {
                Choice::Auto => self.text.auto_game.to_string(),
                Choice::Window(_, title) => title.clone(),
                Choice::Screen(index, monitor) => {
                    let (w, h) = capture::monitor_size(*monitor);
                    format!("{} {} ({w}×{h})", self.text.screen, index + 1)
                }
            };
            add_item(self.c.capture, &label);
        }
        select(self.c.capture, self.choices.iter().position(|c| c.key() == wanted).unwrap_or(0));
    }

    fn fill_cameras(&mut self) {
        self.cameras = camera::cameras();
        unsafe { SendMessageW(self.c.camera, CB_RESETCONTENT, None, None) };
        if self.cameras.is_empty() {
            add_item(self.c.camera, self.text.no_camera);
            select(self.c.camera, 0);
            set_check(self.c.camera_on, false);
            self.config.camera_on = false;
            return;
        }
        for name in &self.cameras {
            add_item(self.c.camera, name);
        }
        // The saved camera, else the first real one (virtual cameras re-send another app's picture).
        let index = self
            .cameras
            .iter()
            .position(|c| *c == self.config.camera_name)
            .or_else(|| self.cameras.iter().position(|c| !c.to_lowercase().contains("virtual")))
            .unwrap_or(0);
        select(self.c.camera, index);
        self.config.camera_name = self.cameras[index].clone();
    }

    fn fill_mics(&mut self) {
        self.mics = vec![String::new()];
        self.mics.extend(audio::microphones().unwrap_or_default().into_iter().map(|d| d.name));
        unsafe { SendMessageW(self.c.mic, CB_RESETCONTENT, None, None) };
        for name in &self.mics {
            add_item(self.c.mic, if name.is_empty() { self.text.default_mic } else { name });
        }
        let index = self.mics.iter().position(|m| *m == self.config.mic_name).unwrap_or(0);
        select(self.c.mic, index);
        self.config.mic_name = self.mics[index].clone();
    }

    /// Switches destination, keeping what was typed for the custom server.
    fn choose_destination(&mut self, destination: &str) {
        if self.config.destination == "custom" {
            self.config.custom_server = get_text(self.c.server);
        }
        self.config.destination = destination.to_string();
        self.show_destination();
    }

    fn show_destination(&mut self) {
        let destination = self.config.destination.clone();
        let destination = destination.as_str();
        set_check(self.c.youtube, destination == "youtube");
        set_check(self.c.twitch, destination == "twitch");
        set_check(self.c.custom, destination == "custom");
        let server = match destination {
            "youtube" => rtmp::YOUTUBE.to_string(),
            "twitch" => rtmp::TWITCH.to_string(),
            _ => self.config.custom_server.clone(),
        };
        set_text(self.c.server, &server);
        set_text(self.c.key, &secret::load(destination).unwrap_or_default());
        self.apply_mode();
    }

    /// Volumes and mute switches take effect at once, even while live.
    fn apply_audio(&mut self) {
        self.config.desktop_on = checked(self.c.desktop_on);
        self.config.mic_on = checked(self.c.mic_on);
        self.config.desktop_volume = slider(self.c.desktop_volume);
        self.config.mic_volume = slider(self.c.mic_volume);
        let gain = |on: bool, volume: u32| if on { volume as f32 / 100.0 } else { 0.0 };
        self.desktop_gain.set(gain(self.config.desktop_on, self.config.desktop_volume));
        self.mic_gain.set(gain(self.config.mic_on, self.config.mic_volume));
        set_text(self.c.desktop_pct, &format!("{}%", self.config.desktop_volume));
        set_text(self.c.mic_pct, &format!("{}%", self.config.mic_volume));
    }

    fn read_fields(&mut self) {
        self.config.quality = selected(self.c.quality).min(PRESETS.len() - 1);
        self.config.remember_key = checked(self.c.remember);
        self.config.save_copy = checked(self.c.save_copy);
        if self.config.destination == "custom" {
            self.config.custom_server = get_text(self.c.server);
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

    fn update_hint(&self) {
        let hint = match self.choices.get(selected(self.c.capture)) {
            Some(Choice::Auto) => match capture::find_game() {
                Some((_, title)) => format!("{}{title}", self.text.hint_found),
                None => self.text.hint_auto.to_string(),
            },
            _ => String::new(),
        };
        set_text(self.c.capture_hint, &hint);
    }

    fn resolve_video(&self) -> Result<Video, &'static str> {
        match self.choices.get(selected(self.c.capture)) {
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

    fn start(&mut self, live: bool) {
        self.read_fields();
        let video = match self.resolve_video() {
            Ok(video) => video,
            Err(message) => return self.warn(message),
        };
        let mut stream = None;
        if live {
            let destination = self.config.destination.clone();
            let server = match destination.as_str() {
                "youtube" | "twitch" => destination.clone(),
                _ => get_text(self.c.server).trim().to_string(),
            };
            if !server.starts_with("rtmp://") && destination == "custom" {
                return self.warn(self.text.need_server);
            }
            let key = get_text(self.c.key).trim().to_string();
            if key.is_empty() {
                return self.warn(self.text.need_key);
            }
            if self.config.remember_key {
                secret::save(&destination, &key);
            } else {
                secret::delete(&destination);
            }
            stream = Some((server, key));
        }
        self.record_path = (!live || self.config.save_copy).then(recording_path);
        let (height, fps, video_kbps, _) = PRESETS[self.config.quality];
        // The camera opens only if it's switched on: its light should never come on by surprise.
        let camera = (self.config.camera_on && !self.cameras.is_empty()).then(|| {
            self.overlay.set_visible(true);
            CameraChoice { name: Some(self.config.camera_name.clone()), overlay: self.overlay.clone() }
        });
        self.camera_opened = camera.is_some();
        // Both sources always open, so switching one back on mid-stream works.
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
        self.live = live;
        self.engine = Some(Engine::start(settings));
        self.mode = Mode::Starting;
        self.upload = (0, Instant::now(), 0.0);
        self.drops = (0, Instant::now());
        self.apply_mode();
        self.set_status(if live { self.text.connecting } else { self.text.starting }, None);
        set_text(self.c.stats, "");
    }

    fn stop(&mut self) {
        if let Some(engine) = &self.engine {
            engine.stop();
            self.mode = Mode::Stopping;
            self.apply_mode();
            self.set_status(self.text.finishing, None);
        }
    }

    /// Enables what can be used in the current mode, and labels the big buttons.
    fn apply_mode(&self) {
        let idle = self.mode == Mode::Idle;
        let t = self.text;
        for control in [
            self.c.capture,
            self.c.youtube,
            self.c.twitch,
            self.c.custom,
            self.c.key,
            self.c.show_key,
            self.c.remember,
            self.c.mic,
            self.c.quality,
            self.c.save_copy,
        ] {
            enable(control, idle);
        }
        enable(self.c.server, idle && self.config.destination == "custom");
        let has_camera = !self.cameras.is_empty();
        enable(self.c.camera, idle && has_camera);
        enable(self.c.camera_on, has_camera && (idle || self.camera_opened));
        let busy = matches!(self.mode, Mode::Starting | Mode::Running);
        set_text(self.c.go_live, if busy && self.live { t.end_stream } else { t.go_live });
        set_text(self.c.record, if busy && !self.live { t.stop_recording } else { t.record });
        enable(self.c.go_live, idle || (busy && self.live));
        enable(self.c.record, idle || (busy && !self.live));
    }

    fn tick(&mut self) {
        self.ticks += 1;
        let peaks = match &self.engine {
            Some(engine) => {
                let (desktop, mic) = engine.state().take_peaks();
                [desktop, mic]
            }
            None => {
                let mut peaks = [0.0f32; 2];
                for (slot, source) in &mut self.preview {
                    peaks[*slot] = peaks[*slot].max(source.drain_peak().unwrap_or(0.0));
                }
                peaks
            }
        };
        for (level, peak) in self.levels.iter_mut().zip(peaks) {
            *level = meter_level(peak).max(*level - 0.04);
        }
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
        if self.ticks % 10 == 0 {
            self.update_status();
        }
        if self.mode == Mode::Idle && self.ticks % 40 == 0 {
            self.update_hint();
        }
    }

    fn update_status(&mut self) {
        let Some(engine) = &self.engine else { return };
        let state = engine.state().clone();
        let t = self.text;
        match state.phase() {
            Phase::Starting => {}
            Phase::Running => {
                if self.mode == Mode::Starting {
                    self.mode = Mode::Running;
                    self.apply_mode();
                    // E.g. the camera or a microphone couldn't be opened: say so once.
                    let warnings: Vec<String> = state.info().into_iter().filter(|l| l.starts_with("warning:")).collect();
                    if !warnings.is_empty() {
                        self.warn(&warnings.join("\n"));
                    }
                }
                if self.mode == Mode::Stopping {
                    return;
                }
                let time = clock(state.elapsed().as_secs());
                let red = Some(rgb(200, 30, 45));
                match state.network() {
                    Some(net) => {
                        let now = Instant::now();
                        let since = now.duration_since(self.upload.1).as_secs_f64();
                        if since >= 1.0 {
                            let sent = net.sent_bytes();
                            self.upload = (sent, now, (sent - self.upload.0) as f64 * 8.0 / 1_000_000.0 / since);
                        }
                        let dropped = net.dropped_frames();
                        if dropped != self.drops.0 {
                            self.drops = (dropped, now);
                        }
                        let unstable = dropped > 0 && self.drops.1.elapsed().as_secs() < 10;
                        match net.status() {
                            Status::Reconnecting(why) => {
                                self.set_status(&format!("●  {} ({why})", t.reconnecting), Some(rgb(190, 120, 0)))
                            }
                            _ => self.set_status(&format!("●  {}   {time}", t.live), red),
                        }
                        let connection = if unstable { t.connection_unstable } else { t.connection_good };
                        set_text(
                            self.c.stats,
                            &format!("{} {:.1} Mbps  ·  {} {dropped}  ·  {connection}", t.upload, self.upload.2, t.dropped),
                        );
                    }
                    None => {
                        self.set_status(&format!("●  {}   {time}", t.recording), red);
                        if let Some(path) = &self.record_path {
                            set_text(self.c.stats, &path.display().to_string());
                        }
                    }
                }
            }
            Phase::Finished(_) => {
                let was_running = self.mode != Mode::Starting;
                let result = self.engine.take().map(Engine::join).unwrap_or(Ok(()));
                self.mode = Mode::Idle;
                self.apply_mode();
                self.start_preview();
                set_text(self.c.stats, "");
                match result {
                    Ok(()) => match &self.record_path {
                        Some(path) => {
                            self.set_status(t.ready, None);
                            set_text(self.c.stats, &format!("{}{}", t.saved_to, path.display()));
                        }
                        None => self.set_status(t.ready, None),
                    },
                    Err(e) => {
                        self.set_status(t.ready, None);
                        let prefix = if was_running { t.stopped_because } else { t.could_not_start };
                        self.warn(&format!("{prefix}{e}"));
                    }
                }
            }
        }
    }

    fn set_status(&self, label: &str, color: Option<COLORREF>) {
        PAINT.with(|p| p.borrow_mut().status_color = color);
        set_text(self.c.status, label);
    }

    fn warn(&self, message: &str) {
        unsafe {
            MessageBoxW(Some(self.hwnd), &HSTRING::from(message), w!("MilerCast"), MB_OK | MB_ICONWARNING);
        }
    }

    fn confirm(&self, question: &str) -> bool {
        let answer =
            unsafe { MessageBoxW(Some(self.hwnd), &HSTRING::from(question), w!("MilerCast"), MB_YESNO | MB_ICONQUESTION) };
        answer == IDYES
    }
}

fn fonts(dpi: i32, thai: bool) -> (HFONT, HFONT) {
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
    unsafe { (CreateFontIndirectW(&font), CreateFontIndirectW(&bold)) }
}

fn recording_path() -> PathBuf {
    let videos = unsafe {
        SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None).ok().map(|path| {
            let text = path.to_string().unwrap_or_default();
            CoTaskMemFree(Some(path.0 as *const _));
            PathBuf::from(text)
        })
    };
    let t = unsafe { GetLocalTime() };
    videos.unwrap_or_else(|| PathBuf::from(".")).join("MilerCast").join(format!(
        "milercast-{:04}{:02}{:02}-{:02}{:02}{:02}.flv",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    ))
}

fn clock(secs: u64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn set_text(control: HWND, text: &str) {
    unsafe {
        let _ = SetWindowTextW(control, &HSTRING::from(text));
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
