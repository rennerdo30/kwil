//! Integer scrollbar geometry: document sizes never pass through f32.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub start: usize,
    pub length: usize,
    pub thumb_start: usize,
    pub thumb_length: usize,
    pub maximum: u64,
}

impl Bar {
    pub fn new(start: usize, length: usize, visible: u64, total: u64, offset: u64) -> Self {
        let maximum = total.saturating_sub(visible);
        let thumb_length = if total == 0 || maximum == 0 {
            length
        } else {
            ((length as u128 * visible as u128 / total as u128) as usize).max(18).min(length)
        };
        let travel = length - thumb_length;
        let thumb_start = start
            + if maximum == 0 { 0 } else { (travel as u128 * offset.min(maximum) as u128 / maximum as u128) as usize };
        Self { start, length, thumb_start, thumb_length, maximum }
    }

    pub fn contains_thumb(self, pixel: usize) -> bool {
        (self.thumb_start..self.thumb_start + self.thumb_length).contains(&pixel)
    }

    pub fn drag(self, pixel: usize, grab: usize) -> u64 {
        let travel = self.length - self.thumb_length;
        if travel == 0 {
            return 0;
        }
        let position = pixel.saturating_sub(self.start).saturating_sub(grab).min(travel);
        (position as u128 * self.maximum as u128 / travel as u128) as u64
    }

    pub fn page(self, pixel: usize, offset: u64, visible: u64) -> u64 {
        if pixel < self.thumb_start {
            offset.saturating_sub(visible)
        } else {
            offset.saturating_add(visible).min(self.maximum)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_reaches_both_ends_of_huge_documents() {
        let bar = Bar::new(4, 700, 40, u64::MAX, u64::MAX);
        assert_eq!(bar.thumb_start + bar.thumb_length, 704);
        assert_eq!(bar.drag(0, 5), 0);
        assert_eq!(bar.drag(usize::MAX, 5), bar.maximum);
        assert!(bar.contains_thumb(703));
    }

    #[test]
    fn page_click_and_small_viewport_are_clamped() {
        let bar = Bar::new(0, 100, 10, 100, 50);
        assert_eq!(bar.page(0, 50, 10), 40);
        assert_eq!(bar.page(99, 89, 10), 90);
        let tiny = Bar::new(0, 5, 10, 100, 0);
        assert_eq!(tiny.thumb_length, 5);
        assert_eq!(tiny.drag(100, 0), 0);
        let fits = Bar::new(0, 100, 200, 100, 50);
        assert_eq!(fits.thumb_length, 100);
        assert_eq!(fits.maximum, 0);
    }
}
