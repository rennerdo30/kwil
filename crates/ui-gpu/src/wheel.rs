/// Two fractional row remainders, tied to the current input coordinate space.
#[derive(Default)]
pub struct Wheel {
    context: Option<[u64; 5]>,
    remainder: [f64; 2],
}
impl Wheel {
    pub fn clear(&mut self) {
        self.context = None;
        self.remainder = [0.0; 2];
    }
    pub fn take(&mut self, context: [u64; 5], x: f64, y: f64) -> (i64, i64) {
        if self.context != Some(context) {
            self.clear();
            self.context = Some(context);
        }
        let mut output = [0; 2];
        for (axis, delta) in [x, y].into_iter().enumerate() {
            if !delta.is_finite() {
                self.remainder[axis] = 0.0;
                continue;
            }
            let sum = self.remainder[axis] + delta;
            output[axis] = sum as i64;
            self.remainder[axis] = sum.fract();
        }
        (output[0], output[1])
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::Wheel;
    #[test]
    fn fractional_events_accumulate_and_reverse_without_idle_work() {
        let mut wheel = Wheel::default();
        let context = [1, 2, 20, 10, 0];
        assert_eq!(wheel.take(context, 0.25, -0.5), (0, 0));
        assert_eq!(wheel.take(context, 0.25, -0.5), (0, -1));
        assert_eq!(wheel.take(context, 0.25, 0.5), (0, 0));
        assert_eq!(wheel.take(context, 0.25, 0.5), (1, 1));
    }
    #[test]
    fn coordinate_changes_and_modal_input_cannot_leak_old_fractions() {
        let mut wheel = Wheel::default();
        let context = [1, 2, 20, 10, 0];
        wheel.take(context, 0.75, 0.75);
        assert_eq!(wheel.take([2, 2, 20, 10, 0], 0.5, 0.5), (0, 0));
        wheel.clear();
        assert_eq!(wheel.take(context, 0.5, 0.5), (0, 0));
        assert_eq!(wheel.take(context, f64::NAN, f64::INFINITY), (0, 0));
        assert_eq!(wheel.take(context, 0.5, 0.5), (0, 0));
    }
}
