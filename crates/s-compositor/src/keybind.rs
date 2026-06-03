//! Configurable global keyboard shortcuts.
//!
//! Bindings are stored in the config file as `bind = <combo> = <action>`, e.g.
//! `bind = Super+Return = spawn:alacritty`. Modifiers are `Super`/`Ctrl`/`Alt`/
//! `Shift`; the key is a letter, digit or a name (`Return`, `Space`, `Tab`,
//! `Escape`, `Minus`, `Equal`, `Comma`, `Period`, `Plus`).

/// What a shortcut does.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Spawn(String),
    StartMenu,
    Launcher,
    Settings,
    QuickSettings,
    Lock,
    Logout,
    CloseWindow,
    NextWindow,
    PrevWindow,
    Workspace(u32),       // 1-based
    MoveToWorkspace(u32), // 1-based
    WorkspaceNext,
    WorkspacePrev,
    VolumeUp,
    VolumeDown,
    VolumeMute,
}

/// A modifier combination plus a (normalized) key, bound to an action.
#[derive(Debug, Clone, PartialEq)]
pub struct Keybind {
    pub meta: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: String,
    pub action: Action,
}

impl Keybind {
    /// Does this binding match a key event? `key` must already be normalized
    /// with [`normalize_text`].
    pub fn matches(&self, key: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> bool {
        self.key == key
            && self.meta == meta
            && self.ctrl == ctrl
            && self.alt == alt
            && self.shift == shift
    }

    /// Serialize back to a `<combo> = <action>` string (without the `bind =`).
    pub fn to_config_string(&self) -> String {
        let mut combo = String::new();
        for (on, name) in [
            (self.meta, "Super"),
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
        ] {
            if on {
                combo.push_str(name);
                combo.push('+');
            }
        }
        combo.push_str(&key_to_name(&self.key));
        format!("{combo} = {}", action_to_string(&self.action))
    }
}

/// Inverse of [`normalize_key`] for keys whose character would clash with the
/// config syntax (notably `=`), so a binding round-trips through the file.
fn key_to_name(key: &str) -> String {
    match key {
        "return" => "Return",
        "space" => "Space",
        "escape" => "Escape",
        "tab" => "Tab",
        "-" => "Minus",
        "=" => "Equal",
        "+" => "Plus",
        "," => "Comma",
        "." => "Period",
        other => other,
    }
    .to_string()
}

/// Map a key-event `text` to a stable, comparable key name.
pub fn normalize_text(text: &str) -> String {
    match text {
        "\n" | "\r" => "return".to_string(),
        " " => "space".to_string(),
        "\u{1b}" => "escape".to_string(),
        "\t" => "tab".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// Normalize a key *name* from a binding string to match [`normalize_text`].
fn normalize_key(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "return" | "enter" => "return".to_string(),
        "space" => "space".to_string(),
        "escape" | "esc" => "escape".to_string(),
        "tab" => "tab".to_string(),
        "minus" => "-".to_string(),
        "equal" => "=".to_string(),
        "plus" => "+".to_string(),
        "comma" => ",".to_string(),
        "period" => ".".to_string(),
        other => other.to_string(),
    }
}

/// Parse a full `<combo> = <action>` binding string.
pub fn parse(spec: &str) -> Option<Keybind> {
    let (combo, action) = spec.split_once('=')?;
    let action = parse_action(action.trim())?;
    let (mut meta, mut ctrl, mut alt, mut shift) = (false, false, false, false);
    let mut key = None;
    for token in combo.split('+').map(|t| t.trim()).filter(|t| !t.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "super" | "meta" | "mod" | "win" => meta = true,
            "ctrl" | "control" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            other => key = Some(normalize_key(other)),
        }
    }
    Some(Keybind {
        meta,
        ctrl,
        alt,
        shift,
        key: key?,
        action,
    })
}

fn parse_action(s: &str) -> Option<Action> {
    if let Some(cmd) = s.strip_prefix("spawn:") {
        return Some(Action::Spawn(cmd.trim().to_string()));
    }
    if let Some(n) = s.strip_prefix("workspace:") {
        return n.trim().parse().ok().map(Action::Workspace);
    }
    if let Some(n) = s.strip_prefix("move-to-workspace:") {
        return n.trim().parse().ok().map(Action::MoveToWorkspace);
    }
    Some(match s {
        "start-menu" => Action::StartMenu,
        "launcher" | "run" => Action::Launcher,
        "settings" => Action::Settings,
        "quick-settings" => Action::QuickSettings,
        "lock" => Action::Lock,
        "logout" => Action::Logout,
        "close-window" | "close" => Action::CloseWindow,
        "next-window" => Action::NextWindow,
        "prev-window" => Action::PrevWindow,
        "workspace-next" => Action::WorkspaceNext,
        "workspace-prev" => Action::WorkspacePrev,
        "volume-up" => Action::VolumeUp,
        "volume-down" => Action::VolumeDown,
        "volume-mute" | "mute" => Action::VolumeMute,
        _ => return None,
    })
}

fn action_to_string(action: &Action) -> String {
    match action {
        Action::Spawn(cmd) => format!("spawn:{cmd}"),
        Action::Workspace(n) => format!("workspace:{n}"),
        Action::MoveToWorkspace(n) => format!("move-to-workspace:{n}"),
        Action::StartMenu => "start-menu".into(),
        Action::Launcher => "launcher".into(),
        Action::Settings => "settings".into(),
        Action::QuickSettings => "quick-settings".into(),
        Action::Lock => "lock".into(),
        Action::Logout => "logout".into(),
        Action::CloseWindow => "close-window".into(),
        Action::NextWindow => "next-window".into(),
        Action::PrevWindow => "prev-window".into(),
        Action::WorkspaceNext => "workspace-next".into(),
        Action::WorkspacePrev => "workspace-prev".into(),
        Action::VolumeUp => "volume-up".into(),
        Action::VolumeDown => "volume-down".into(),
        Action::VolumeMute => "volume-mute".into(),
    }
}

/// Sensible default bindings, used when the config file lists none.
pub fn defaults() -> Vec<Keybind> {
    let mut binds = vec![
        "Super+d = start-menu",
        "Super+Space = launcher",
        "Super+Return = spawn:alacritty",
        "Super+e = spawn:s-files",
        "Super+l = lock",
        "Super+q = close-window",
        "Alt+Tab = next-window",
        "Alt+Shift+Tab = prev-window",
        "Super+Comma = settings",
        "Super+Equal = volume-up",
        "Super+Minus = volume-down",
        "Super+0 = volume-mute",
    ]
    .into_iter()
    .filter_map(parse)
    .collect::<Vec<_>>();

    // Super+1..4 switch workspace, Super+Shift+1..4 move the window there.
    for n in 1..=4 {
        if let Some(b) = parse(&format!("Super+{n} = workspace:{n}")) {
            binds.push(b);
        }
        if let Some(b) = parse(&format!("Super+Shift+{n} = move-to-workspace:{n}")) {
            binds.push(b);
        }
    }
    binds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_action() {
        let b = parse("Super+Shift+Return = spawn:foot").unwrap();
        assert!(b.meta && b.shift && !b.ctrl && !b.alt);
        assert_eq!(b.key, "return");
        assert_eq!(b.action, Action::Spawn("foot".into()));
    }

    #[test]
    fn matches_normalized_event() {
        let b = parse("Alt+Tab = next-window").unwrap();
        assert!(b.matches(&normalize_text("\t"), false, true, false, false));
        assert!(!b.matches(&normalize_text("\t"), true, true, false, false));
    }

    #[test]
    fn letters_are_case_insensitive() {
        let b = parse("Super+Q = close-window").unwrap();
        assert!(b.matches(&normalize_text("Q"), false, false, false, true));
        assert!(b.matches(&normalize_text("q"), false, false, false, true));
    }

    #[test]
    fn round_trips_through_config_string() {
        for spec in ["Super+d = start-menu", "Super+Equal = volume-up"] {
            let b = parse(spec).unwrap();
            let reparsed = parse(&b.to_config_string()).unwrap();
            assert_eq!(b, reparsed);
        }
    }

    #[test]
    fn defaults_are_valid() {
        assert!(!defaults().is_empty());
    }
}
