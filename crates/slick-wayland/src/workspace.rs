//! Virtual desktops (workspaces).
//!
//! Workspaces are a pure compositor-side concept: clients are unaware of them.
//! A workspace owns a set of window ids; switching workspaces is purely a matter
//! of which windows are included in the render/input set. This module is plain
//! logic with no Smithay dependency so it can be unit-tested headlessly.

use slick_shell::{WindowId, WorkspaceId};

/// The set of virtual desktops and which one is active.
#[derive(Debug)]
pub struct Workspaces {
    /// `windows[i]` holds the windows assigned to workspace `i`.
    windows: Vec<Vec<WindowId>>,
    active: usize,
}

impl Workspaces {
    /// Create `count` (>= 1) empty workspaces with the first one active.
    pub fn new(count: usize) -> Self {
        let count = count.max(1);
        Self {
            windows: vec![Vec::new(); count],
            active: 0,
        }
    }

    pub fn count(&self) -> usize {
        self.windows.len()
    }

    pub fn active(&self) -> WorkspaceId {
        WorkspaceId(self.active as u32)
    }

    /// Switch to `id`, clamped to the valid range. Returns the new active id.
    pub fn switch_to(&mut self, id: WorkspaceId) -> WorkspaceId {
        let idx = (id.0 as usize).min(self.windows.len() - 1);
        self.active = idx;
        self.active()
    }

    /// Add a new window to the currently active workspace.
    pub fn add_window(&mut self, window: WindowId) {
        self.windows[self.active].push(window);
    }

    /// Remove a window from whichever workspace holds it.
    pub fn remove_window(&mut self, window: WindowId) {
        for ws in &mut self.windows {
            ws.retain(|&w| w != window);
        }
    }

    /// Move a window to another workspace (clamped). No-op if not found.
    pub fn move_window(&mut self, window: WindowId, to: WorkspaceId) {
        let to = (to.0 as usize).min(self.windows.len() - 1);
        self.remove_window(window);
        self.windows[to].push(window);
    }

    /// The windows visible on the active workspace, in stacking order.
    pub fn active_windows(&self) -> &[WindowId] {
        &self.windows[self.active]
    }

    /// Whether a window is on the active workspace (i.e. currently visible).
    pub fn is_visible(&self, window: WindowId) -> bool {
        self.windows[self.active].contains(&window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_switch() {
        let mut ws = Workspaces::new(4);
        assert_eq!(ws.count(), 4);
        ws.add_window(WindowId(1));
        ws.add_window(WindowId(2));
        assert_eq!(ws.active_windows(), &[WindowId(1), WindowId(2)]);

        ws.switch_to(WorkspaceId(1));
        assert!(ws.active_windows().is_empty());
        assert!(!ws.is_visible(WindowId(1)));

        ws.add_window(WindowId(3));
        ws.switch_to(WorkspaceId(0));
        assert_eq!(ws.active_windows(), &[WindowId(1), WindowId(2)]);
    }

    #[test]
    fn move_between_workspaces() {
        let mut ws = Workspaces::new(2);
        ws.add_window(WindowId(1));
        ws.move_window(WindowId(1), WorkspaceId(1));
        assert!(!ws.is_visible(WindowId(1)));
        ws.switch_to(WorkspaceId(1));
        assert!(ws.is_visible(WindowId(1)));
    }

    #[test]
    fn switch_clamps_out_of_range() {
        let mut ws = Workspaces::new(2);
        assert_eq!(ws.switch_to(WorkspaceId(99)), WorkspaceId(1));
    }
}
