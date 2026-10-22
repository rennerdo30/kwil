//! M0 scripted-scenario seam. Only measurement builds call spawn_script.
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub enum Step {
    Idle,
    Scroll,
    IdleAfter,
    Exit,
}

#[cfg(test)]
mod tests {
    use super::*;
    const ROW: Region = (0, 90, 10, 5);
    const STATUS: Region = (0, 95, 10, 5);
    fn scroll(stats: &mut Stats, regions: &[Region], copy: u64, full: bool) {
        stats.enter(Step::Scroll);
        stats.expect_scroll(10, 100, 5, true);
        stats.present(regions, copy, full, Instant::now());
    }
    fn scrolled() -> Stats {
        let mut stats = Stats::default();
        for _ in 0..20 {
            scroll(&mut stats, &[ROW, STATUS], 900, false);
        }
        stats
    }
    #[test]
    fn real_present_contract_accepts_scroll_and_rejects_idle_redraw() {
        assert!(scrolled().report().is_ok());
        let mut broken = scrolled();
        broken.enter(Step::Idle);
        broken.present(&[ROW], 0, false, Instant::now());
        assert!(broken.report().is_err());
    }
    #[test]
    fn missing_scroll_and_full_present_cannot_pass() {
        assert!(Stats::default().report().is_err());
        let mut broken = scrolled();
        scroll(&mut broken, &[ROW, STATUS], 900, true);
        assert!(broken.report().is_err());
    }

    #[test]
    fn missing_exposed_row_or_status_cannot_reduce_both_sides_of_the_contract() {
        for (label, remaining) in [("exposed row", STATUS), ("status", ROW)] {
            let mut broken = Stats::default();
            for _ in 0..20 {
                scroll(&mut broken, &[remaining], 900, false);
            }
            assert!(broken.report().is_err(), "missing {label} falsely passed");
        }
    }

    #[test]
    fn one_missing_copy_cannot_hide_in_a_positive_total() {
        let mut broken = Stats::default();
        for i in 0..20 {
            scroll(&mut broken, &[ROW, STATUS], if i == 19 { 0 } else { 900 }, false);
        }
        assert!(broken.report().is_err());
    }

    #[test]
    fn wrong_location_or_compensating_copy_sizes_cannot_pass() {
        let mut wrong_region = Stats::default();
        let mut wrong_copy = Stats::default();
        for i in 0..20 {
            // Same damaged area and total copied area as the valid scenario.
            scroll(&mut wrong_region, &[(0, 0, 10, 5), STATUS], 900, false);
            scroll(&mut wrong_copy, &[ROW, STATUS], if i % 2 == 0 { 800 } else { 1000 }, false);
        }
        assert!(wrong_region.report().is_err());
        assert!(wrong_copy.report().is_err());
    }

    #[test]
    fn unsupported_native_copy_requires_the_moved_region_as_damage() {
        for omit_moved in [false, true] {
            let mut stats = Stats::default();
            for _ in 0..20 {
                stats.enter(Step::Scroll);
                stats.expect_scroll(10, 100, 5, false);
                let regions = if omit_moved { vec![ROW, STATUS] } else { vec![(0, 0, 10, 90), ROW, STATUS] };
                stats.present(&regions, 0, false, Instant::now());
            }
            assert_eq!(stats.report().is_ok(), !omit_moved);
        }
    }

    #[test]
    fn script_waits_for_each_present_before_sending_another_scroll() {
        use std::sync::mpsc::{TryRecvError, channel};
        let (steps_tx, steps_rx) = channel();
        let (ack_tx, ack_rx) = channel();
        let (waiting_tx, waiting_rx) = channel();
        let worker = std::thread::spawn(move || {
            run_script(
                |step| steps_tx.send(step).is_ok(),
                || {
                    waiting_tx.send(()).unwrap();
                    ack_rx.recv_timeout(Duration::from_secs(1)).is_ok()
                },
                |_| {}, // Fake time: never sleep or spin in this focused test.
            )
        });
        let next = || steps_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(next(), Step::Idle));
        for _ in 0..20 {
            assert!(matches!(next(), Step::Scroll));
            waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(matches!(steps_rx.try_recv(), Err(TryRecvError::Empty)));
            ack_tx.send(()).unwrap();
        }
        assert!(matches!(next(), Step::IdleAfter));
        assert!(matches!(next(), Step::Exit));
        worker.join().unwrap();
    }
}

pub fn spawn_script(send: impl Fn(Step) -> bool + Send + 'static, wait: impl Fn() -> bool + Send + 'static) {
    std::thread::spawn(move || {
        run_script(send, wait, std::thread::sleep);
    });
}

fn run_script(send: impl Fn(Step) -> bool, wait: impl Fn() -> bool, sleep: impl Fn(Duration)) {
    // The GUI starts this only after its index and first frame are ready.
    sleep(Duration::from_millis(300));
    if !send(Step::Idle) {
        return;
    }
    sleep(Duration::from_millis(500));
    for _ in 0..20 {
        if !send(Step::Scroll) {
            return;
        }
        // Completion means a successful present, not a presumed 20 ms frame.
        // Backpressure prevents the OS merging two outstanding script actions.
        if !wait() {
            send(Step::Exit);
            return;
        }
        sleep(Duration::from_millis(20));
    }
    if !send(Step::IdleAfter) {
        return;
    }
    sleep(Duration::from_millis(500));
    send(Step::Exit);
}

#[derive(Default)]
pub struct Stats {
    phase: u8,
    idle: u64,
    idle_after: u64,
    scroll: u64,
    copy: u64,
    damage: u64,
    full: u64,
    actions: u64,
    violations: u64,
    expected: Option<ExpectedScroll>,
    frames_us: Vec<u64>,
}

/// x, y, width, height of an actual successful present rectangle.
pub type Region = (usize, usize, usize, usize);

struct ExpectedScroll {
    regions: Vec<Region>,
    copy: u64,
}

impl Stats {
    pub fn enter(&mut self, step: Step) {
        self.phase = match step {
            Step::Idle => 1,
            Step::Scroll => 2,
            Step::IdleAfter => 3,
            Step::Exit => 4,
        };
    }

    /// Called at the scripted action boundary, before asking the view for damage.
    /// Expectations depend only on one downward line step and viewport geometry.
    pub fn expect_scroll(&mut self, width: usize, height: usize, row_height: usize, native_copy: bool) {
        self.actions += 1;
        if self.expected.is_some() || width == 0 || row_height == 0 || height / row_height < 3 {
            self.violations += 1;
            return;
        }
        let rows = height.saturating_sub(row_height) / row_height;
        let kept_height = (rows - 1) * row_height;
        let mut regions = vec![(0, kept_height, width, row_height), (0, height - row_height, width, row_height)];
        if !native_copy {
            regions.push((0, 0, width, kept_height));
        }
        regions.sort_unstable();
        self.expected =
            Some(ExpectedScroll { regions, copy: if native_copy { (width * kept_height) as u64 } else { 0 } });
    }

    pub fn present(&mut self, regions: &[Region], copy: u64, full: bool, started: Instant) {
        match self.phase {
            1 => self.idle += 1,
            3 => self.idle_after += 1,
            2 => {
                self.scroll += 1;
                self.copy += copy;
                self.damage += regions.iter().map(|&(_, _, w, h)| (w * h) as u64).sum::<u64>();
                self.full += u64::from(full);
                let mut actual = regions.to_vec();
                actual.sort_unstable();
                if !self.expected.take().is_some_and(|e| e.regions == actual && e.copy == copy) {
                    self.violations += 1;
                }
                self.frames_us.push(started.elapsed().as_micros() as u64);
            }
            _ => {}
        }
    }

    pub fn report(&mut self) -> Result<(), String> {
        self.frames_us.sort_unstable();
        let p99 = self.frames_us.get(self.frames_us.len().saturating_sub(1)).copied().unwrap_or(0);
        println!(
            "{{\"idle_presents\":{},\"idle_after_presents\":{},\"scroll_presents\":{},\"scroll_copy_px\":{},\"scroll_damage_px\":{},\"scroll_full_presents\":{},\"scroll_p99_ms\":{:.3}}}",
            self.idle,
            self.idle_after,
            self.scroll,
            self.copy,
            self.damage,
            self.full,
            p99 as f64 / 1000.0
        );
        if self.idle != 0 || self.idle_after != 0 || self.scroll != 20 || self.full != 0 {
            return Err(
                "viewer frame contract: idle must present zero, each scroll exactly once without a full present".into(),
            );
        }
        if self.actions != 20 || self.expected.is_some() || self.violations != 0 {
            return Err("each scripted scroll must match independent viewport rectangles and exact copy area".into());
        }
        // TODO(KW-11): native Wayland/X11 copy; currently their copied area is presented as damage.
        // TODO(KW-16): typing scenario, one line + status, keystroke-to-pixels p99.
        Ok(())
    }
}
