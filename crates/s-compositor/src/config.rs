//! Persisted settings, stored as a small `key = value` file in the working
//! directory.

use std::path::PathBuf;

/// Settings file name (created in the current working directory).
pub const FILE: &str = "s-compositor.conf";

/// A start-menu application entry.
#[derive(Debug, Clone, PartialEq)]
pub struct AppEntry {
    pub icon: String,
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub dark: bool,
    /// Accent colour as `0xRRGGBB`.
    pub accent: u32,
    pub panel_edge: i32,
    pub panel_size: f32,
    pub background: Option<String>,
    /// Start-menu apps. When empty, the defaults are auto-discovered at startup.
    pub menu: Vec<AppEntry>,
    /// Lock-screen password; empty means the lock unlocks on Enter.
    pub lock_password: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dark: true,
            accent: 0x89b4fa,
            panel_edge: 0,
            panel_size: 72.0,
            background: None,
            menu: Vec::new(),
            lock_password: String::new(),
        }
    }
}

/// Is `bin` an executable on `$PATH`?
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// If `bin` sits next to our own executable (the usual case when running from a
/// build tree or an install prefix), return its absolute path so it can be
/// launched without being on `$PATH`.
fn bundled(bin: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(bin);
    candidate
        .is_file()
        .then(|| candidate.to_string_lossy().into_owned())
}

/// Auto-discover sensible default start-menu apps: a browser, a terminal, a file
/// manager and a few games, picking whatever is installed.
pub fn discover_default_apps() -> Vec<AppEntry> {
    let mut apps = Vec::new();
    let pick = |apps: &mut Vec<AppEntry>, icon: &str, candidates: &[(&str, &str)]| {
        if let Some((bin, name)) = candidates.iter().find(|(bin, _)| on_path(bin)) {
            apps.push(AppEntry {
                icon: icon.to_string(),
                name: name.to_string(),
                command: bin.to_string(),
            });
        }
    };

    pick(
        &mut apps,
        "🌐",
        &[
            ("firefox", "Firefox"),
            ("chromium", "Chromium"),
            ("google-chrome", "Google Chrome"),
            ("brave", "Brave"),
            ("epiphany", "Web"),
        ],
    );
    // Terminal: Alacritty is the shell's bundled default, falling back to
    // whatever else is installed.
    pick(
        &mut apps,
        "🖥",
        &[
            ("alacritty", "Terminal"),
            ("foot", "Terminal"),
            ("kitty", "Terminal"),
            ("wezterm", "Terminal"),
            ("gnome-terminal", "Terminal"),
            ("xterm", "Terminal"),
        ],
    );
    // File manager: prefer our own `s-files` browser (found next to the
    // compositor binary, so it works straight from the build tree), falling back
    // to a system file manager.
    if let Some(command) = bundled("s-files").or_else(|| on_path("s-files").then(|| "s-files".to_string())) {
        apps.push(AppEntry {
            icon: "📁".to_string(),
            name: "Files".to_string(),
            command,
        });
    } else {
        pick(
            &mut apps,
            "📁",
            &[
                ("nautilus", "Files"),
                ("thunar", "Files"),
                ("pcmanfm", "Files"),
                ("dolphin", "Files"),
            ],
        );
    }

    // Up to three games.
    let games = [
        ("supertuxkart", "SuperTuxKart"),
        ("0ad", "0 A.D."),
        ("neverball", "Neverball"),
        ("gnome-mines", "Mines"),
        ("aisleriot", "Solitaire"),
        ("gnome-2048", "2048"),
        ("gnome-sudoku", "Sudoku"),
    ];
    for (bin, name) in games.iter().filter(|(bin, _)| on_path(bin)).take(3) {
        apps.push(AppEntry {
            icon: "🎮".to_string(),
            name: name.to_string(),
            command: bin.to_string(),
        });
    }
    apps
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
        let mut text = format!(
            "# s-compositor settings\n\
             scheme = {}\n\
             accent = #{:06x}\n\
             panel_edge = {}\n\
             panel_size = {}\n\
             background = {}\n\
             lock_password = {}\n",
            if self.dark { "dark" } else { "light" },
            self.accent,
            self.panel_edge,
            self.panel_size as i32,
            self.background.as_deref().unwrap_or(""),
            self.lock_password,
        );
        // Start-menu apps, one per line: `app = icon | name | command`.
        for app in &self.menu {
            text.push_str(&format!(
                "app = {} | {} | {}\n",
                app.icon, app.name, app.command
            ));
        }
        text
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
                "lock_password" => self.lock_password = value.to_string(),
                "app" => {
                    let mut parts = value.splitn(3, '|').map(|p| p.trim().to_string());
                    if let (Some(icon), Some(name), Some(command)) =
                        (parts.next(), parts.next(), parts.next())
                    {
                        if !command.is_empty() {
                            self.menu.push(AppEntry {
                                icon,
                                name,
                                command,
                            });
                        }
                    }
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
            lock_password: "hunter2".to_string(),
            menu: vec![
                AppEntry {
                    icon: "🌐".to_string(),
                    name: "Firefox".to_string(),
                    command: "firefox".to_string(),
                },
                AppEntry {
                    icon: "🖥".to_string(),
                    name: "Terminal".to_string(),
                    command: "foot".to_string(),
                },
            ],
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
