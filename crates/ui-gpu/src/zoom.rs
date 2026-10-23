//! Temporary editor zoom. Logical font size and monitor scale remain independent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Zoom {
    steps: i8,
}

impl Zoom {
    pub(crate) fn recognizes(id: &str) -> bool {
        matches!(id, "view.zoomIn" | "view.zoomOut" | "view.zoomReset")
    }
    /// Returns whether this command changed zoom. Unknown commands are ignored.
    pub(crate) fn command(&mut self, id: &str) -> bool {
        let next = match id {
            "view.zoomIn" => self.steps.saturating_add(1).min(16),
            "view.zoomOut" => self.steps.saturating_sub(1).max(-8),
            "view.zoomReset" => 0,
            _ => return false,
        };
        let changed = next != self.steps;
        self.steps = next;
        changed
    }

    pub(crate) fn physical_px(self, configured_logical_px: f32, monitor_scale: f64) -> f32 {
        let base =
            if configured_logical_px.is_finite() && configured_logical_px > 0.0 { configured_logical_px } else { 14.0 };
        let scale = if monitor_scale.is_finite() && monitor_scale > 0.0 { monitor_scale } else { 1.0 };
        ((base as f64 * scale * 1.1_f64.powi(self.steps as i32)).clamp(4.0, 256.0)) as f32
    }
}

/// One nonblocking handoff after the native window reveals its actual scale.
/// The font worker parses in parallel, then rasterizes before first presentation.
pub(crate) fn start_font_rasterization(
    sender: std::sync::mpsc::SyncSender<f32>,
    configured_logical_px: f32,
    monitor_scale: f64,
    zoom: Zoom,
) -> Result<(), std::sync::mpsc::TrySendError<f32>> {
    sender.try_send(zoom.physical_px(configured_logical_px, monitor_scale))
}

impl super::App {
    pub(super) fn font_physical_px(&self) -> f32 {
        self.zoom.physical_px(self.settings.font.size, self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor()))
    }

    pub(super) fn dispatch_zoom(&mut self, id: &str) -> bool {
        if !Zoom::recognizes(id) {
            return false;
        }
        if self.zoom.command(id) {
            self.reload_font();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_plus_minus_bindings_preserve_tombstones_and_vim_normal_mode() {
        use kwil_core_commands::{Binding, Preset, keymap::KeymapEngine, vim::VimResult};
        for preset in [Preset::VsCode, Preset::Vim, Preset::NotepadPlusPlus] {
            let mut keys = KeymapEngine::new(preset, &[]).unwrap();
            for (key, command) in [
                ("ctrl+Plus", "view.zoomIn"),
                ("ctrl+shift+=", "view.zoomIn"),
                ("ctrl+-", "view.zoomOut"),
                ("ctrl+0", "view.zoomReset"),
            ] {
                assert!(
                    matches!(keys.feed_with_selection(key.parse().unwrap()), VimResult::Command(call) if call.id == command)
                );
            }
            let mut keys = KeymapEngine::new(
                preset,
                &[Binding { keys: "ctrl+Plus".into(), command: String::new(), context: None, mode: None }],
            )
            .unwrap();
            assert_eq!(keys.feed_with_selection("ctrl+Plus".parse().unwrap()), VimResult::Unbound);
            assert!(keys.suppresses_fallback());
        }
        let mut zoom = Zoom::default();
        assert!(!zoom.command("view.zoomReset"));
        assert!(Zoom::recognizes("view.zoomReset"), "a no-op reset must still be consumed");
    }

    #[test]
    fn zoom_preserves_dpi_and_reset_uses_configured_size() {
        let mut zoom = Zoom::default();
        assert_eq!(zoom.physical_px(16.0, 1.5), 24.0);
        assert!(zoom.command("view.zoomIn"));
        assert!((zoom.physical_px(16.0, 1.5) - 26.4).abs() < 0.001);
        assert!(zoom.command("view.zoomOut"));
        assert_eq!(zoom.physical_px(16.0, 2.0), 32.0);
        assert!(zoom.command("view.zoomIn"));
        assert!(zoom.command("view.zoomReset"));
        assert_eq!(zoom.physical_px(20.0, 1.5), 30.0);
        assert!(!zoom.command("file.save"));
    }

    #[test]
    fn sizes_and_repeated_commands_are_bounded() {
        let mut zoom = Zoom::default();
        for _ in 0..100 {
            zoom.command("view.zoomIn");
        }
        assert!(!zoom.command("view.zoomIn"));
        assert_eq!(zoom.physical_px(f32::MAX, f64::MAX), 256.0);
        assert!(zoom.physical_px(f32::NAN, f64::NAN).is_finite());
        for _ in 0..100 {
            zoom.command("view.zoomOut");
        }
        assert!(!zoom.command("view.zoomOut"));
        assert_eq!(zoom.physical_px(0.001, 0.001), 4.0);
    }
}
