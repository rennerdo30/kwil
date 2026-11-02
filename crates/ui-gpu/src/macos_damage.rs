//! Physical framebuffer pixels <-> flipped AppKit view points. Kept free of
//! platform APIs so the exact invalidation/crop contract is tested on every OS.
use super::Rect;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Points {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Copy)]
pub(super) struct Mapping {
    size: (usize, usize),
    bounds: Points,
}

impl Mapping {
    pub fn new(size: (usize, usize), bounds: Points) -> Option<Self> {
        if size.0 == 0
            || size.1 == 0
            || ![bounds.x, bounds.y, bounds.w, bounds.h].iter().all(|n| n.is_finite())
            || bounds.w <= 0.0
            || bounds.h <= 0.0
        {
            return None;
        }
        Some(Self { size, bounds })
    }

    pub fn clip(self, area: Rect) -> Option<Rect> {
        let w = area.w.min(self.size.0.saturating_sub(area.x));
        let h = area.h.min(self.size.1.saturating_sub(area.y));
        (w != 0 && h != 0).then_some(Rect { w, h, ..area })
    }

    pub fn points(self, area: Rect) -> Option<Points> {
        let area = self.clip(area)?;
        Some(Points {
            x: self.bounds.x + area.x as f64 * self.bounds.w / self.size.0 as f64,
            y: self.bounds.y + area.y as f64 * self.bounds.h / self.size.1 as f64,
            w: area.w as f64 * self.bounds.w / self.size.0 as f64,
            h: area.h as f64 * self.bounds.h / self.size.1 as f64,
        })
    }

    pub fn pixels(self, area: Points) -> Option<Rect> {
        if ![area.x, area.y, area.w, area.h].iter().all(|n| n.is_finite()) || area.w <= 0.0 || area.h <= 0.0 {
            return None;
        }
        let x = ((area.x - self.bounds.x) * self.size.0 as f64 / self.bounds.w).floor().max(0.0) as usize;
        let y = ((area.y - self.bounds.y) * self.size.1 as f64 / self.bounds.h).floor().max(0.0) as usize;
        let right = ((area.x + area.w - self.bounds.x) * self.size.0 as f64 / self.bounds.w).ceil().max(0.0) as usize;
        let bottom = ((area.y + area.h - self.bounds.y) * self.size.1 as f64 / self.bounds.h).ceil().max(0.0) as usize;
        self.clip(Rect { x, y, w: right.saturating_sub(x), h: bottom.saturating_sub(y) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retina() -> Mapping {
        Mapping::new((1600, 1200), Points { x: 0.0, y: 0.0, w: 800.0, h: 600.0 }).unwrap()
    }

    #[test]
    fn macos_damage_keeps_line_and_status_separate() {
        let mapping = retina();
        let line = Rect { x: 60, y: 80, w: 1400, h: 40 };
        let status = Rect { x: 1100, y: 1160, w: 300, h: 40 };
        assert_eq!(mapping.points(line), Some(Points { x: 30.0, y: 40.0, w: 700.0, h: 20.0 }));
        assert_eq!(mapping.points(status), Some(Points { x: 550.0, y: 580.0, w: 150.0, h: 20.0 }));
        assert_eq!(mapping.points(line).and_then(|area| mapping.pixels(area)), Some(line));
    }

    #[test]
    fn macos_damage_clips_empty_offscreen_and_overflowing_rectangles() {
        let mapping = retina();
        assert_eq!(
            mapping.points(Rect { x: 1590, y: 1190, w: usize::MAX, h: usize::MAX }),
            Some(Points { x: 795.0, y: 595.0, w: 5.0, h: 5.0 })
        );
        assert_eq!(mapping.points(Rect { x: 1600, y: 0, w: 1, h: 1 }), None);
        assert_eq!(mapping.points(Rect { x: usize::MAX, y: 0, w: 1, h: 1 }), None);
        assert_eq!(mapping.points(Rect::default()), None);
        assert_eq!(
            mapping.pixels(Points { x: -5.0, y: -5.0, w: 10.0, h: 10.0 }),
            Some(Rect { x: 0, y: 0, w: 10, h: 10 })
        );
    }

    #[test]
    fn macos_damage_uses_actual_fractional_bounds_and_top_left_origin() {
        let mapping = Mapping::new((1001, 751), Points { x: 3.0, y: 7.0, w: 500.5, h: 375.5 }).unwrap();
        assert_eq!(mapping.points(Rect { x: 2, y: 4, w: 10, h: 6 }), Some(Points { x: 4.0, y: 9.0, w: 5.0, h: 3.0 }));
        assert_eq!(mapping.pixels(Points { x: 4.1, y: 9.1, w: 4.8, h: 2.8 }), Some(Rect { x: 2, y: 4, w: 10, h: 6 }));
        assert_eq!(
            mapping.points(Rect { x: 0, y: 0, w: 1001, h: 751 }),
            Some(Points { x: 3.0, y: 7.0, w: 500.5, h: 375.5 })
        );
    }

    #[test]
    fn macos_damage_rejects_invalid_geometry() {
        for bounds in [
            Points { x: 0.0, y: 0.0, w: 0.0, h: 1.0 },
            Points { x: 0.0, y: 0.0, w: 1.0, h: f64::NAN },
            Points { x: f64::INFINITY, y: 0.0, w: 1.0, h: 1.0 },
        ] {
            assert!(Mapping::new((1600, 1200), bounds).is_none());
        }
        assert!(Mapping::new((0, 1200), retina().bounds).is_none());
        assert_eq!(retina().pixels(Points { x: 0.0, y: 0.0, w: f64::NAN, h: 1.0 }), None);
    }
}
