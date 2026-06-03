//! Shared data models describing windows and workspaces.

/// Stable identifier for a top-level window, assigned by the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

/// Identifier for a virtual desktop / workspace (0-based index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(pub u32);

/// Information about a top-level window, used to drive the taskbar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub app_id: String,
    pub title: String,
    pub workspace: WorkspaceId,
    pub focused: bool,
}

impl WindowInfo {
    pub fn new(id: WindowId, workspace: WorkspaceId) -> Self {
        Self {
            id,
            app_id: String::new(),
            title: String::new(),
            workspace,
            focused: false,
        }
    }

    /// A human-friendly label for the taskbar: the title, falling back to the
    /// app id, falling back to a placeholder.
    pub fn label(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if !self.app_id.is_empty() {
            &self.app_id
        } else {
            "(untitled)"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_prefers_title_then_app_id() {
        let mut w = WindowInfo::new(WindowId(1), WorkspaceId(0));
        assert_eq!(w.label(), "(untitled)");
        w.app_id = "foot".into();
        assert_eq!(w.label(), "foot");
        w.title = "vim".into();
        assert_eq!(w.label(), "vim");
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;

    #[test]
    fn new_window_is_unfocused_with_empty_strings() {
        let w = WindowInfo::new(WindowId(5), WorkspaceId(2));
        assert_eq!(w.id, WindowId(5));
        assert_eq!(w.workspace, WorkspaceId(2));
        assert!(!w.focused);
        assert_eq!(w.app_id, "");
        assert_eq!(w.title, "");
    }

    #[test]
    fn ids_are_ordered() {
        assert!(WindowId(1) < WindowId(2));
        assert!(WorkspaceId(0) < WorkspaceId(1));
    }
}
