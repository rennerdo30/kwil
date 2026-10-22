//! Temporary editor zoom. Logical font size and monitor scale remain independent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Zoom {
    steps: i8,
}

impl Zoom {
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

#[cfg(test)]
mod tests {
    use super::*;

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
