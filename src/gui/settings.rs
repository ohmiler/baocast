//! Settings: things set once. Custom server, devices, volumes, quality,
//! recordings. Changes apply at once; there's no Apply button.

use super::*;

const HEIGHT: i32 = 456;

const ID_SERVER: u16 = 200;
const ID_FORGET: u16 = 201;
const ID_MIC: u16 = 202;
const ID_CAMERA: u16 = 203;
const ID_MIRROR: u16 = 204;
const ID_QUALITY: u16 = 205;
const ID_SAVE_COPY: u16 = 206;
const ID_OPEN_FOLDER: u16 = 207;
const ID_CLOSE: u16 = 208;
const IDCANCEL: u16 = 2;

pub(super) struct SettingsWindow {
    pub hwnd: HWND,
    server: HWND,
    forget: HWND,
    mic: HWND,
    mic_volume: HWND,
    mic_pct: HWND,
    game_volume: HWND,
    game_pct: HWND,
    camera: HWND,
    mirror: HWND,
    quality: HWND,
    save_copy: HWND,
}

impl App {
    pub(super) fn open_settings_window(&mut self) {
        if let Some(open) = &self.settings {
            unsafe {
                let _ = SetForegroundWindow(open.hwnd);
            }
            return;
        }
        let t = self.text;
        let Some(w) = self.panel(t.settings_title, WIDTH, HEIGHT) else { return };
        let (button, combo, slider) = (w!("BUTTON"), w!("COMBOBOX"), w!("msctls_trackbar32"));
        let row = |y: i32| (16, y + 3, 104, 20);

        self.heading(w, t.section_stream, (16, 14, 388, 20));
        self.label(w, t.custom_server, row(38));
        let server = self.add(w, w!("EDIT"), "", ES_AUTOHSCROLL | TABSTOP, CLIENTEDGE, (124, 38, 280, 24), ID_SERVER);
        let forget = self.add(w, button, t.forget_keys, TABSTOP, 0, (124, 68, 280, 28), ID_FORGET);

        self.heading(w, t.section_audio, (16, 110, 388, 20));
        self.label(w, t.microphone, row(134));
        let mic = self.add(w, combo, "", CBS_DROPDOWNLIST | VSCROLL | TABSTOP, 0, (124, 134, 280, 240), ID_MIC);
        self.label(w, t.mic_volume, row(166));
        let mic_volume = self.add(w, slider, "", TBS_NOTICKS | TABSTOP, 0, (120, 164, 240, 28), 0);
        let mic_pct = self.label(w, "", (364, 169, 40, 20));
        self.label(w, t.game_volume, row(198));
        let game_volume = self.add(w, slider, "", TBS_NOTICKS | TABSTOP, 0, (120, 196, 240, 28), 0);
        let game_pct = self.label(w, "", (364, 201, 40, 20));

        self.heading(w, t.section_camera, (16, 238, 388, 20));
        self.label(w, t.camera, row(262));
        let camera = self.add(w, combo, "", CBS_DROPDOWNLIST | VSCROLL | TABSTOP, 0, (124, 262, 280, 200), ID_CAMERA);
        let mirror = self.add(w, button, t.mirror, BS_AUTOCHECKBOX | TABSTOP, 0, (124, 292, 280, 22), ID_MIRROR);

        self.heading(w, t.section_quality, (16, 328, 388, 20));
        self.label(w, t.quality, row(352));
        let quality = self.add(w, combo, "", CBS_DROPDOWNLIST | TABSTOP, 0, (124, 352, 280, 200), ID_QUALITY);
        let save_copy = self.add(w, button, t.save_copy, BS_AUTOCHECKBOX | TABSTOP, 0, (124, 382, 280, 22), ID_SAVE_COPY);
        self.add(w, button, t.open_folder, TABSTOP, 0, (16, 416, 200, 28), ID_OPEN_FOLDER);
        self.add(w, button, t.close, TABSTOP, 0, (304, 416, 100, 28), ID_CLOSE);

        set_text(server, &self.config.custom_server);
        set_cue(server, "rtmp://");
        for name in &self.mics {
            add_item(mic, if name.is_empty() { t.default_mic } else { name });
        }
        select(mic, self.mics.iter().position(|m| *m == self.config.mic_name).unwrap_or(0));
        for (slider, value) in [(mic_volume, self.config.mic_volume), (game_volume, self.config.desktop_volume)] {
            unsafe {
                SendMessageW(slider, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM((200 << 16) as isize)));
                SendMessageW(slider, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(value as isize)));
            }
        }
        set_text(mic_pct, &format!("{}%", self.config.mic_volume));
        set_text(game_pct, &format!("{}%", self.config.desktop_volume));
        if self.cameras.is_empty() {
            add_item(camera, t.no_camera);
            select(camera, 0);
        } else {
            for name in &self.cameras {
                add_item(camera, name);
            }
            select(camera, self.cameras.iter().position(|c| *c == self.config.camera_name).unwrap_or(0));
        }
        set_check(mirror, self.config.camera_mirror);
        for &(height, fps, kbps, note) in &PRESETS {
            let note = match note {
                1 => format!(" ({})", t.recommended),
                2 => " (YouTube)".to_string(),
                3 => format!(" ({})", t.slow_internet),
                _ => String::new(),
            };
            add_item(quality, &format!("{height}p {fps} fps · {} Mbps{note}", kbps as f32 / 1000.0));
        }
        select(quality, self.config.quality.min(PRESETS.len() - 1));
        set_check(save_copy, self.config.save_copy);
        PAINT.with(|p| p.borrow_mut().muted.extend([mic_pct, game_pct]));

        self.settings = Some(SettingsWindow {
            hwnd: w,
            server,
            forget,
            mic,
            mic_volume,
            mic_pct,
            game_volume,
            game_pct,
            camera,
            mirror,
            quality,
            save_copy,
        });
        self.update_settings_window();
        unsafe {
            let _ = ShowWindow(w, SW_SHOW);
        }
    }

    pub(super) fn settings_message(&mut self, msg: u32, wparam: WPARAM) -> Option<LRESULT> {
        match msg {
            WM_COMMAND => {
                self.settings_command(word(wparam.0) as u16, word(wparam.0 >> 16));
                Some(LRESULT(0))
            }
            WM_HSCROLL => {
                let s = self.settings.as_ref()?;
                self.config.mic_volume = slider(s.mic_volume);
                self.config.desktop_volume = slider(s.game_volume);
                set_text(s.mic_pct, &format!("{}%", self.config.mic_volume));
                set_text(s.game_pct, &format!("{}%", self.config.desktop_volume));
                self.apply_gains();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.close_settings_window();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn settings_command(&mut self, id: u16, code: u32) {
        let Some(s) = &self.settings else { return };
        let (mic, camera, mirror, quality, save_copy, forget) = (s.mic, s.camera, s.mirror, s.quality, s.save_copy, s.forget);
        match (id, code) {
            (ID_FORGET, BN_CLICKED) => {
                for (name, _, _) in DESTINATIONS {
                    secret::delete(name);
                }
                set_text(forget, self.text.keys_forgotten);
                enable(forget, false);
                self.show_key_status();
            }
            (ID_MIC, CBN_SELCHANGE) => {
                self.config.mic_name = self.mics.get(selected(mic)).cloned().unwrap_or_default();
                if self.preview_on {
                    self.start_preview();
                }
            }
            (ID_CAMERA, CBN_SELCHANGE) => {
                if let Some(name) = self.cameras.get(selected(camera)) {
                    self.config.camera_name = name.clone();
                }
            }
            (ID_MIRROR, BN_CLICKED) => {
                self.config.camera_mirror = checked(mirror);
                self.overlay.set_mirror(self.config.camera_mirror);
            }
            (ID_QUALITY, CBN_SELCHANGE) => self.config.quality = selected(quality).min(PRESETS.len() - 1),
            (ID_SAVE_COPY, BN_CLICKED) => self.config.save_copy = checked(save_copy),
            (ID_OPEN_FOLDER, BN_CLICKED) => {
                let folder = recordings_folder();
                let _ = std::fs::create_dir_all(&folder);
                open(&folder.display().to_string());
            }
            (ID_CLOSE | IDCANCEL, BN_CLICKED) => self.close_settings_window(),
            _ => {}
        }
    }

    /// Devices, quality and the server can't change mid-stream; volumes and mirror can.
    pub(super) fn update_settings_window(&self) {
        let Some(s) = &self.settings else { return };
        let idle = self.mode == Mode::Idle;
        for control in [s.server, s.mic, s.quality, s.save_copy] {
            enable(control, idle);
        }
        enable(s.camera, idle && !self.cameras.is_empty());
        enable(s.mirror, !self.cameras.is_empty());
    }

    pub(super) fn read_settings_window(&mut self) {
        if let Some(s) = &self.settings {
            self.config.custom_server = get_text(s.server).trim().to_string();
        }
    }

    pub(super) fn close_settings_window(&mut self) {
        self.read_settings_window();
        self.config.save();
        if let Some(s) = self.settings.take() {
            destroy_panel(s.hwnd);
        }
        if self.config.destination == "custom" && !self.config.custom_server.is_empty() {
            self.notice("", Tone::Plain);
        }
    }
}
