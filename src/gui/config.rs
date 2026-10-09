//! The window's settings, kept between runs in %APPDATA%\MilerCast\settings.ini.
//! Never holds the stream key: that lives in Windows Credential Manager.

use std::path::PathBuf;

pub struct Config {
    /// "auto", "window:<title>" or "screen:<index>".
    pub capture: String,
    /// "youtube", "twitch" or "custom".
    pub destination: String,
    pub custom_server: String,
    pub quality: usize,
    pub desktop_on: bool,
    pub desktop_volume: u32,
    pub mic_on: bool,
    pub mic_volume: u32,
    /// Empty for the Windows default microphone.
    pub mic_name: String,
    /// Off unless the user turns it on: a camera should never switch on by surprise.
    pub camera_on: bool,
    pub camera_name: String,
    /// 0 top left, 1 top right, 2 bottom left, 3 bottom right.
    pub camera_corner: u8,
    /// 0 small, 1 medium, 2 large.
    pub camera_size: u8,
    pub camera_mirror: bool,
    pub save_copy: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capture: "auto".into(),
            destination: "youtube".into(),
            custom_server: String::new(),
            quality: 0,
            desktop_on: true,
            desktop_volume: 100,
            mic_on: true,
            mic_volume: 100,
            mic_name: String::new(),
            camera_on: false,
            camera_name: String::new(),
            camera_corner: 3,
            camera_size: 1,
            camera_mirror: false,
            save_copy: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("MilerCast").join("settings.ini"))
}

impl Config {
    pub fn load() -> Self {
        let mut config = Self::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return config };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim().to_string();
            let flag = value == "1";
            let number = value.parse::<u32>().ok();
            match key.trim() {
                "capture" => config.capture = value,
                "destination" => config.destination = value,
                "custom_server" => config.custom_server = value,
                "quality" => config.quality = number.unwrap_or(0) as usize,
                "desktop_on" => config.desktop_on = flag,
                "desktop_volume" => config.desktop_volume = number.unwrap_or(100).min(200),
                "mic_on" => config.mic_on = flag,
                "mic_volume" => config.mic_volume = number.unwrap_or(100).min(200),
                "mic_name" => config.mic_name = value,
                "camera_on" => config.camera_on = flag,
                "camera_name" => config.camera_name = value,
                "camera_corner" => config.camera_corner = number.unwrap_or(3).min(3) as u8,
                "camera_size" => config.camera_size = number.unwrap_or(1).min(2) as u8,
                "camera_mirror" => config.camera_mirror = flag,
                "save_copy" => config.save_copy = flag,
                _ => {}
            }
        }
        config
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let flag = |on: bool| if on { "1" } else { "0" };
        let text = format!(
            "capture={}\ndestination={}\ncustom_server={}\nquality={}\n\
             desktop_on={}\ndesktop_volume={}\nmic_on={}\nmic_volume={}\nmic_name={}\n\
             camera_on={}\ncamera_name={}\ncamera_corner={}\ncamera_size={}\ncamera_mirror={}\nsave_copy={}\n",
            self.capture,
            self.destination,
            self.custom_server,
            self.quality,
            flag(self.desktop_on),
            self.desktop_volume,
            flag(self.mic_on),
            self.mic_volume,
            self.mic_name,
            flag(self.camera_on),
            self.camera_name,
            self.camera_corner,
            self.camera_size,
            flag(self.camera_mirror),
            flag(self.save_copy),
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
}
