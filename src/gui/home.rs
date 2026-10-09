//! Home: what's used every time. What to capture, where to stream, sound,
//! camera, and the big Go live button. Everything set once lives in Settings.

use super::*;

pub(super) const HEIGHT: i32 = 352;

const ID_CAPTURE: u16 = 100;
const ID_DESTINATION: u16 = 101;
const ID_KEY: u16 = 102;
const ID_GAME: u16 = 103;
const ID_MIC: u16 = 104;
const ID_CAMERA: u16 = 105;
const ID_CORNER: u16 = 110; // 110..=113
const ID_SIZE: u16 = 120; // 120..=122
const ID_GO_LIVE: u16 = 130;
const ID_RECORD: u16 = 131;
const ID_SETTINGS: u16 = 132;

#[derive(Default)]
pub(super) struct Home {
    pub capture: HWND,
    hint: HWND,
    destination: HWND,
    key_status: HWND,
    key_button: HWND,
    game: HWND,
    mic: HWND,
    camera: HWND,
    corners: [HWND; 4],
    sizes: [HWND; 3],
    notice: HWND,
    pub all: Vec<HWND>,
}

impl App {
    pub(super) fn build_home(&self) -> Home {
        let t = self.text;
        let p = self.hwnd;
        let (button, combo) = (w!("BUTTON"), w!("COMBOBOX"));
        let mut h = Home::default();
        let mut all = Vec::new();
        let mut keep = |hwnd: HWND| {
            all.push(hwnd);
            hwnd
        };

        keep(self.heading(p, t.capture, (16, 19, 84, 20)));
        h.capture = keep(self.add(p, combo, "", CBS_DROPDOWNLIST | VSCROLL | TABSTOP, 0, (104, 16, 300, 320), ID_CAPTURE));
        h.hint = keep(self.label(p, "", (104, 44, 300, 18)));

        keep(self.heading(p, t.stream_to, (16, 75, 84, 20)));
        h.destination = keep(self.add(p, combo, "", CBS_DROPDOWNLIST | TABSTOP, 0, (104, 72, 112, 200), ID_DESTINATION));
        h.key_status = keep(self.label(p, "", (224, 76, 100, 20)));
        h.key_button = keep(self.add(p, button, "", TABSTOP, 0, (326, 71, 78, 26), ID_KEY));

        keep(self.heading(p, t.sound, (16, 112, 84, 20)));
        h.game = keep(self.add(p, button, t.game, BS_AUTOCHECKBOX | TABSTOP, 0, (104, 110, 74, 22), ID_GAME));
        let game_meter = keep(self.add(p, w!("STATIC"), "", SS_OWNERDRAW, 0, (182, 118, 222, 6), 0));
        h.mic = keep(self.add(p, button, t.mic, BS_AUTOCHECKBOX | TABSTOP, 0, (104, 136, 74, 22), ID_MIC));
        let mic_meter = keep(self.add(p, w!("STATIC"), "", SS_OWNERDRAW, 0, (182, 144, 222, 6), 0));

        keep(self.heading(p, t.camera, (16, 176, 84, 20)));
        h.camera = keep(self.add(p, button, t.show, BS_AUTOCHECKBOX | TABSTOP, 0, (104, 174, 70, 22), ID_CAMERA));
        for (i, corner) in h.corners.iter_mut().enumerate() {
            let x = 178 + i as i32 * 30;
            *corner = keep(self.add(p, button, "", BS_OWNERDRAW | TABSTOP, 0, (x, 170, 28, 30), ID_CORNER + i as u16));
        }
        for (i, size) in h.sizes.iter_mut().enumerate() {
            let style = BS_AUTORADIOBUTTON | BS_PUSHLIKE | TABSTOP | if i == 0 { GROUP } else { 0 };
            let x = 302 + i as i32 * 34;
            *size = keep(self.add(p, button, t.sizes[i], style, 0, (x, 172, 34, 26), ID_SIZE + i as u16));
        }

        let go_live = keep(self.add(p, button, t.go_live, BS_OWNERDRAW | TABSTOP | GROUP, 0, (16, 216, 388, 46), ID_GO_LIVE));
        keep(self.add(p, button, t.record, TABSTOP, 0, (16, 272, 120, 28), ID_RECORD));
        keep(self.add(p, button, t.settings, TABSTOP, 0, (284, 272, 120, 28), ID_SETTINGS));
        // Problems and results show up here, in place of message boxes.
        h.notice = keep(self.add(p, w!("STATIC"), "", 0, 0, (16, 310, 388, 36), 0));

        h.all = all;
        PAINT.with(|paint| {
            let mut paint = paint.borrow_mut();
            paint.meters = [game_meter, mic_meter];
            paint.muted.push(h.hint);
            paint.accent.push(go_live);
            paint.corner_buttons = h.corners;
        });
        h
    }

    pub(super) fn init_home(&mut self) {
        let t = self.text;
        unsafe { SendMessageW(self.home.capture, CB_SETDROPPEDWIDTH, Some(WPARAM(self.s(520) as usize)), None) };
        self.fill_captures();
        for (name, label, _) in DESTINATIONS {
            add_item(self.home.destination, if name == "custom" { t.custom } else { label });
        }
        let index = DESTINATIONS.iter().position(|(name, _, _)| *name == self.config.destination).unwrap_or(0);
        select(self.home.destination, index);
        self.config.destination = DESTINATIONS[index].0.to_string();
        self.show_key_status();
        for (i, size) in self.home.sizes.iter().enumerate() {
            set_check(*size, i == self.config.camera_size as usize);
        }
        PAINT.with(|p| p.borrow_mut().corner = self.config.camera_corner);
        let has_camera = !self.cameras.is_empty();
        for control in [self.home.camera].iter().chain(&self.home.corners).chain(&self.home.sizes) {
            enable(*control, has_camera);
        }
        self.sync_home_switches();
        self.update_hint();
    }

    /// Returns false if `id` isn't one of Home's controls.
    pub(super) fn home_command(&mut self, id: u16, code: u32) -> bool {
        match (id, code) {
            (ID_CAPTURE, CBN_DROPDOWN) => self.fill_captures(),
            (ID_CAPTURE, CBN_SELCHANGE) => {
                if let Some(choice) = self.choices.get(selected(self.home.capture)) {
                    self.config.capture = choice.key();
                }
                self.update_hint();
            }
            (ID_DESTINATION, CBN_SELCHANGE) => {
                let index = selected(self.home.destination).min(DESTINATIONS.len() - 1);
                self.config.destination = DESTINATIONS[index].0.to_string();
                self.show_key_status();
                if self.config.destination == "custom" && self.config.custom_server.trim().is_empty() {
                    self.notice(self.text.need_server, Tone::Warn);
                } else {
                    self.notice("", Tone::Plain);
                }
            }
            (ID_KEY, BN_CLICKED) => self.open_key_dialog(),
            (ID_GAME, BN_CLICKED) => self.set_game(checked(self.home.game)),
            (ID_MIC, BN_CLICKED) => self.set_mic(checked(self.home.mic)),
            (ID_CAMERA, BN_CLICKED) => self.set_camera(checked(self.home.camera)),
            (ID_CORNER..=113, BN_CLICKED) => {
                let corner = (id - ID_CORNER) as u8;
                self.config.camera_corner = corner;
                self.overlay.set_corner(corner);
                PAINT.with(|p| p.borrow_mut().corner = corner);
                for button in self.home.corners {
                    invalidate(button);
                }
            }
            (ID_SIZE..=122, BN_CLICKED) => {
                self.config.camera_size = (id - ID_SIZE) as u8;
                self.overlay.set_size(self.config.camera_size);
            }
            (ID_GO_LIVE, BN_CLICKED) => self.start(true),
            (ID_RECORD, BN_CLICKED) => self.start(false),
            (ID_SETTINGS, BN_CLICKED) => self.open_settings_window(),
            _ => return false,
        }
        true
    }

    pub(super) fn sync_home_switches(&self) {
        set_check(self.home.game, self.config.desktop_on);
        set_check(self.home.mic, self.config.mic_on);
        set_check(self.home.camera, self.config.camera_on);
    }

    pub(super) fn show_key_status(&self) {
        let t = self.text;
        let saved = secret::load(&self.config.destination).is_some();
        if saved {
            paint_text(self.home.key_status, t.key_saved, Tone::Good);
        } else {
            paint_text(self.home.key_status, t.key_missing, Tone::Warn);
        }
        set_text(self.home.key_button, if saved { t.change_key } else { t.add_key });
    }

    /// A line under the buttons for problems and results.
    pub(super) fn notice(&self, message: &str, tone: Tone) {
        paint_text(self.home.notice, message, tone);
    }

    pub(super) fn fill_captures(&mut self) {
        let current = self.choices.get(selected(self.home.capture)).map(Choice::key);
        let wanted = current.unwrap_or_else(|| self.config.capture.clone());
        self.choices = vec![Choice::Auto];
        self.choices.extend(capture::list_windows().into_iter().map(|(hwnd, title)| Choice::Window(hwnd, title)));
        self.choices.extend(capture::monitors().into_iter().enumerate().map(|(i, m)| Choice::Screen(i, m)));
        unsafe { SendMessageW(self.home.capture, CB_RESETCONTENT, None, None) };
        for choice in &self.choices {
            let label = match choice {
                Choice::Auto => self.text.auto_game.to_string(),
                Choice::Window(_, title) => title.clone(),
                Choice::Screen(index, monitor) => {
                    let (w, h) = capture::monitor_size(*monitor);
                    format!("{} {} ({w}×{h})", self.text.screen, index + 1)
                }
            };
            add_item(self.home.capture, &label);
        }
        select(self.home.capture, self.choices.iter().position(|c| c.key() == wanted).unwrap_or(0));
    }

    pub(super) fn update_hint(&self) {
        let hint = match self.choices.get(selected(self.home.capture)) {
            Some(Choice::Auto) => match capture::find_game() {
                Some((_, title)) => format!("{}{title}", self.text.hint_found),
                None => self.text.hint_auto.to_string(),
            },
            _ => String::new(),
        };
        set_text(self.home.hint, &hint);
    }
}
