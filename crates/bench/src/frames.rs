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
    fn scrolled() -> Stats {
        let mut stats = Stats::default();
        stats.enter(Step::Scroll);
        for _ in 0..20 {
            stats.present(10, 100, false, 10, Instant::now());
        }
        stats
    }
    #[test]
    fn real_present_contract_accepts_scroll_and_rejects_idle_redraw() {
        assert!(scrolled().report().is_ok());
        let mut broken = scrolled();
        broken.enter(Step::Idle);
        broken.present(10, 0, false, 10, Instant::now());
        assert!(broken.report().is_err());
    }
    #[test]
    fn missing_scroll_and_full_present_cannot_pass() {
        assert!(Stats::default().report().is_err());
        let mut broken = scrolled();
        broken.present(10, 100, true, 10, Instant::now());
        assert!(broken.report().is_err());
    }
}

pub fn spawn_script(send: impl Fn(Step) -> bool + Send + 'static) {
    std::thread::spawn(move || {
        // The GUI starts this only after its index and first frame are ready.
        std::thread::sleep(Duration::from_millis(300));
        if !send(Step::Idle) {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
        for _ in 0..20 {
            if !send(Step::Scroll) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !send(Step::IdleAfter) {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
        send(Step::Exit);
    });
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
    expected_damage: u64,
    frames_us: Vec<u64>,
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

    pub fn present(&mut self, damage: u64, copy: u64, full: bool, expected_damage: u64, started: Instant) {
        match self.phase {
            1 => self.idle += 1,
            3 => self.idle_after += 1,
            2 => {
                self.scroll += 1;
                self.copy += copy;
                self.damage += damage;
                self.full += u64::from(full);
                self.expected_damage += expected_damage;
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
        if cfg!(windows) && (self.copy == 0 || self.damage != self.expected_damage) {
            return Err("Windows scroll must copy and damage only exposed rows plus status".into());
        }
        // TODO(KW-11): native Wayland/X11 copy; currently their copied area is presented as damage.
        // TODO(KW-16): typing scenario, one line + status, keystroke-to-pixels p99.
        Ok(())
    }
}
