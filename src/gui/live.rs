//! The Live panel: the window shrinks to this while streaming or recording,
//! so it stays out of the game's way. Status, mute switches, End stream.

use super::*;
use milercast::engine::State;
use milercast::rtmp::Status;

pub(super) const HEIGHT: i32 = 200;

const ID_MIC: u16 = 140;
const ID_GAME: u16 = 141;
const ID_CAMERA: u16 = 142;
const ID_END: u16 = 143;

#[derive(Default)]
pub(super) struct Live {
    status: HWND,
    stats: HWND,
    mic: HWND,
    game: HWND,
    camera: HWND,
    hint: HWND,
    end: HWND,
    pub all: Vec<HWND>,
}

impl App {
    pub(super) fn build_live(&self) -> Live {
        let p = self.hwnd;
        let button = w!("BUTTON");
        let switch = BS_AUTOCHECKBOX | BS_PUSHLIKE | TABSTOP;
        let mut l = Live {
            status: self.label(p, "", (16, 12, 388, 34)),
            stats: self.label(p, "", (16, 48, 388, 18)),
            mic: self.add(p, button, "", switch, 0, (16, 76, 124, 36), ID_MIC),
            game: self.add(p, button, "", switch, 0, (148, 76, 124, 36), ID_GAME),
            camera: self.add(p, button, "", switch, 0, (280, 76, 124, 36), ID_CAMERA),
            hint: self.label(p, "", (16, 120, 388, 18)),
            end: self.add(p, button, "", TABSTOP, 0, (16, 148, 388, 38), ID_END),
            all: Vec::new(),
        };
        set_font(l.status, self.fonts.big);
        set_font(l.end, self.fonts.bold);
        l.all = vec![l.status, l.stats, l.mic, l.game, l.camera, l.hint, l.end];
        PAINT.with(|paint| paint.borrow_mut().muted.extend([l.stats, l.hint]));
        l
    }

    pub(super) fn enter_live_panel(&self) {
        self.show_page(true);
        paint_text(self.live.hint, self.text.hotkeys, Tone::Muted);
        self.update_live_panel();
    }

    /// The parts that depend on whether we're starting, running or finishing.
    pub(super) fn update_live_panel(&self) {
        let t = self.text;
        match self.mode {
            Mode::Starting => {
                paint_text(self.live.status, if self.live_mode { t.connecting } else { t.starting }, Tone::Plain);
                set_text(self.live.stats, "");
                set_text(self.live.end, t.cancel);
            }
            Mode::Stopping => {
                paint_text(self.live.status, t.finishing, Tone::Plain);
                enable(self.live.end, false);
            }
            _ => set_text(self.live.end, if self.live_mode { t.end_stream } else { t.stop_recording }),
        }
        if self.mode != Mode::Stopping {
            enable(self.live.end, true);
        }
        self.sync_live_switches();
    }

    pub(super) fn sync_live_switches(&self) {
        let t = self.text;
        let switches = [
            (self.live.mic, self.config.mic_on, t.mic_on, t.mic_off),
            (self.live.game, self.config.desktop_on, t.game_on, t.game_off),
            (self.live.camera, self.config.camera_on && self.camera_opened, t.camera_on, t.camera_off),
        ];
        for (button, on, on_label, off_label) in switches {
            set_check(button, on);
            set_text(button, if on { on_label } else { off_label });
        }
        enable(self.live.camera, self.camera_opened);
    }

    pub(super) fn live_command(&mut self, id: u16, code: u32) {
        match (id, code) {
            (ID_MIC, BN_CLICKED) => self.set_mic(checked(self.live.mic)),
            (ID_GAME, BN_CLICKED) => self.set_game(checked(self.live.game)),
            (ID_CAMERA, BN_CLICKED) => self.set_camera(checked(self.live.camera)),
            (ID_END, BN_CLICKED) => self.stop(),
            _ => {}
        }
    }

    /// Twice a second while running: time, upload, connection.
    pub(super) fn refresh_live(&mut self, state: &State) {
        if self.mode == Mode::Stopping {
            return;
        }
        let t = self.text;
        if !self.warned {
            self.warned = true;
            self.update_live_panel();
            // E.g. the camera couldn't be opened: say so where the hotkeys are listed.
            let warnings: Vec<String> = state.info().into_iter().filter(|l| l.starts_with("warning:")).collect();
            if !warnings.is_empty() {
                paint_text(self.live.hint, &warnings.join("  "), Tone::Warn);
            }
        }
        let time = clock(state.elapsed().as_secs());
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
                let status = match net.status() {
                    Status::Reconnecting(why) => {
                        paint_text(self.live.status, &format!("●  {}", t.reconnecting), Tone::Warn);
                        why
                    }
                    _ => {
                        paint_text(self.live.status, &format!("●  {}   {time}", t.live), Tone::Live);
                        String::new()
                    }
                };
                let connection = if unstable { t.connection_unstable } else { t.connection_good };
                let stats = match status.is_empty() {
                    true => format!("{} {:.1} Mbps  ·  {dropped} {}  ·  {connection}", t.upload, self.upload.2, t.dropped),
                    false => status,
                };
                set_text(self.live.stats, &stats);
                tray::update(self.hwnd, &format!("MilerCast · {} {time}", t.live));
            }
            None => {
                paint_text(self.live.status, &format!("●  {}   {time}", t.recording), Tone::Live);
                if let Some(path) = &self.record_path {
                    set_text(self.live.stats, &path.display().to_string());
                }
                tray::update(self.hwnd, &format!("MilerCast · {} {time}", t.recording));
            }
        }
    }
}
