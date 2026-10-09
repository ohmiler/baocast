//! A small window for pasting a stream key, with a link to where the
//! platform shows it. The key goes straight into Windows Credential Manager.

use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;

const WIDTH_KEY: i32 = 380;
const HEIGHT: i32 = 168;

const ID_EDIT: u16 = 300;
const ID_SHOW: u16 = 301;
const ID_WHERE: u16 = 302;
const ID_SAVE: u16 = 303;
const ID_CANCEL: u16 = 304;
// What Enter and Escape send in a window with Tab navigation.
const IDOK: u16 = 1;
const IDCANCEL: u16 = 2;

pub(super) struct KeyDialog {
    pub hwnd: HWND,
    edit: HWND,
    show: HWND,
    showing: bool,
    destination: String,
    help_url: &'static str,
}

impl App {
    pub(super) fn open_key_dialog(&mut self) {
        if self.key.is_some() {
            return;
        }
        let t = self.text;
        let destination = self.config.destination.clone();
        let (_, platform, url) = DESTINATIONS.iter().find(|(name, _, _)| *name == destination).copied().unwrap_or(DESTINATIONS[0]);
        let (platform, help) = match destination.as_str() {
            "youtube" => (platform, t.key_help_youtube),
            "twitch" => (platform, t.key_help_twitch),
            _ => (t.custom, t.key_help_custom),
        };
        let Some(w) = self.panel(&t.key_title(platform), WIDTH_KEY, HEIGHT) else { return };
        let button = w!("BUTTON");
        self.add(w, w!("STATIC"), help, 0, 0, (16, 12, 348, 36), 0);
        let edit = self.add(w, w!("EDIT"), "", ES_AUTOHSCROLL | ES_PASSWORD | TABSTOP, CLIENTEDGE, (16, 54, 270, 26), ID_EDIT);
        let show = self.add(w, button, t.show, TABSTOP, 0, (292, 53, 72, 28), ID_SHOW);
        let note = self.label(w, t.key_note, (16, 88, 348, 18));
        if !url.is_empty() {
            self.add(w, button, t.where_key, TABSTOP, 0, (16, 120, 160, 30), ID_WHERE);
        }
        self.add(w, button, t.cancel, TABSTOP, 0, (196, 120, 80, 30), ID_CANCEL);
        self.add(w, button, t.save, TABSTOP, 0, (284, 120, 80, 30), ID_SAVE);
        set_cue(edit, t.key_placeholder);
        PAINT.with(|p| p.borrow_mut().muted.push(note));

        self.key = Some(KeyDialog { hwnd: w, edit, show, showing: false, destination, help_url: url });
        // Like any dialog: the main window waits until this one closes.
        enable(self.hwnd, false);
        unsafe {
            let _ = ShowWindow(w, SW_SHOW);
            let _ = SetFocus(Some(edit));
        }
    }

    pub(super) fn key_message(&mut self, msg: u32, wparam: WPARAM) -> Option<LRESULT> {
        match msg {
            WM_COMMAND => {
                let (id, code) = (word(wparam.0) as u16, word(wparam.0 >> 16));
                if code == BN_CLICKED {
                    self.key_command(id);
                }
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.close_key_dialog();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn key_command(&mut self, id: u16) {
        let Some(k) = &mut self.key else { return };
        match id {
            ID_SHOW => {
                k.showing = !k.showing;
                let mask = if k.showing { 0 } else { BLACK_DOT };
                unsafe {
                    SendMessageW(k.edit, EM_SETPASSWORDCHAR, Some(WPARAM(mask)), None);
                }
                invalidate(k.edit);
                set_text(k.show, if k.showing { self.text.hide } else { self.text.show });
            }
            ID_WHERE => open(k.help_url),
            ID_SAVE | IDOK => {
                let key = get_text(k.edit).trim().to_string();
                if key.is_empty() {
                    unsafe {
                        let _ = SetFocus(Some(k.edit));
                    }
                    return;
                }
                secret::save(&k.destination, &key);
                self.close_key_dialog();
                self.show_key_status();
                self.notice(self.text.key_saved, Tone::Good);
            }
            ID_CANCEL | IDCANCEL => self.close_key_dialog(),
            _ => {}
        }
    }

    fn close_key_dialog(&mut self) {
        let Some(k) = self.key.take() else { return };
        // Re-enable the main window first, or Windows hands focus to another app.
        enable(self.hwnd, true);
        destroy_panel(k.hwnd);
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
    }
}
