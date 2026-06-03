//! Persisted settings, stored as a small `key = value` file in the working
//! directory.

use std::path::PathBuf;

/// Settings file name (created in the current working directory).
pub const FILE: &str = "s-compositor.conf";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub dark: bool,
    /// Accent colour as `0xRRGGBB`.
    pub accent: u32,
    pub panel_edge: i32,
    pub panel_size: f32,
    pub background: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dark: true,
            accent: 0x89b4fa,
            panel_edge: 0,
            panel_size: 72.0,
            background: None,
        }
    }
}

impl Config {
    fn path() -> PathBuf {
        PathBuf::from(FILE)
    }

    /// Load from the working directory, falling back to defaults.
    pub fn load() -> Self {
        let mut config = Config::default();
        if let Ok(text) = std::fs::read_to_string(Self::path()) {
            config.apply_str(&text);
        }
        config
    }

    /// Persist to the working directory.
    pub fn save(&self) {
        if let Err(err) = std::fs::write(Self::path(), self.to_text()) {
            log::warn!("failed to save {}: {err}", FILE);
        }
    }

    pub fn to_text(&self) -> String {
        format!(
            "# s-compositor settings\n\
             scheme = {}\n\
             accent = #{:06x}\n\
             panel_edge = {}\n\
             panel_size = {}\n\
             background = {}\n",
            if self.dark { "dark" } else { "light" },
            self.accent,
            self.panel_edge,
            self.panel_size as i32,
            self.background.as_deref().unwrap_or(""),
        )
    }

    pub fn apply_str(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "scheme" => self.dark = value != "light",
                "accent" => {
                    if let Ok(n) = u32::from_str_radix(value.trim_start_matches('#'), 16) {
                        self.accent = n & 0xff_ff_ff;
                    }
                }
                "panel_edge" => {
                    if let Ok(n) = value.parse() {
                        self.panel_edge = n;
                    }
                }
                "panel_size" => {
                    if let Ok(n) = value.parse() {
                        self.panel_size = n;
                    }
                }
                "background" => {
                    self.background = (!value.is_empty()).then(|| value.to_string());
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let config = Config {
            dark: false,
            accent: 0xa6e3a1,
            panel_edge: 2,
            panel_size: 96.0,
            background: Some("/tmp/bg.png".to_string()),
        };
        let mut parsed = Config::default();
        parsed.apply_str(&config.to_text());
        assert_eq!(parsed, config);
    }

    #[test]
    fn defaults_and_partial_parse() {
        let mut c = Config::default();
        assert!(c.dark);
        c.apply_str("# comment\nscheme=light\naccent = #ff0000\nbogus line\n");
        assert!(!c.dark);
        assert_eq!(c.accent, 0xff0000);
        // Unspecified keys keep their defaults.
        assert_eq!(c.panel_edge, 0);
    }
}
