//! The window's settings, kept between runs in %APPDATA%\MilerCast\settings.ini.
//! Never holds the stream key: that lives in Windows Credential Manager.

use std::path::PathBuf;

pub struct Config {
    /// "auto", "window:<title>" or "screen:<index>".
    pub capture: String,
    /// "youtube", "twitch" or "custom".
    pub destination: String,
    pub custom_server: String,
    pub remember_key: bool,
    pub quality: usize,
    pub desktop_on: bool,
    pub desktop_volume: u32,
    pub mic_on: bool,
    pub mic_volume: u32,
    /// Empty for the Windows default microphone.
    pub mic_name: String,
    pub save_copy: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capture: "auto".into(),
            destination: "youtube".into(),
            custom_server: String::new(),
            remember_key: true,
            quality: 0,
            desktop_on: true,
            desktop_volume: 100,
            mic_on: true,
            mic_volume: 100,
            mic_name: String::new(),
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
                "remember_key" => config.remember_key = flag,
                "quality" => config.quality = number.unwrap_or(0) as usize,
                "desktop_on" => config.desktop_on = flag,
                "desktop_volume" => config.desktop_volume = number.unwrap_or(100).min(200),
                "mic_on" => config.mic_on = flag,
                "mic_volume" => config.mic_volume = number.unwrap_or(100).min(200),
                "mic_name" => config.mic_name = value,
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
            "capture={}\ndestination={}\ncustom_server={}\nremember_key={}\nquality={}\n\
             desktop_on={}\ndesktop_volume={}\nmic_on={}\nmic_volume={}\nmic_name={}\nsave_copy={}\n",
            self.capture,
            self.destination,
            self.custom_server,
            flag(self.remember_key),
            self.quality,
            flag(self.desktop_on),
            self.desktop_volume,
            flag(self.mic_on),
            self.mic_volume,
            self.mic_name,
            flag(self.save_copy),
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
}
