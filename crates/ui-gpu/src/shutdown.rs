//! Post-event-loop settlement. An unrelated error must never skip accepted work.
pub(super) type Job = Box<dyn FnOnce() -> Result<(), String>>;

pub(super) fn drain<const N: usize>(jobs: [Option<Job>; N]) -> Result<(), String> {
    let mut errors = Vec::new();
    for job in jobs.into_iter().flatten() {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error),
            Err(_) => errors.push("shutdown lane panicked".into()),
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[test]
    fn marker_and_preparation_errors_do_not_skip_settings_or_retirement() {
        let order = Rc::new(RefCell::new(Vec::new()));
        let jobs: [Option<Job>; 4] = std::array::from_fn(|index| {
            let order = order.clone();
            Some(Box::new(move || {
                order.borrow_mut().push(index);
                match index {
                    0 => Err("marker write failed; retained temporary".into()),
                    1 => panic!("preparation panic"),
                    2 => Ok(()), // accepted settings B must still settle
                    _ => Err("retirement error".into()),
                }
            }) as Job)
        });
        let error = drain(jobs).unwrap_err();
        assert_eq!(*order.borrow(), vec![0, 1, 2, 3]);
        for message in ["marker write failed", "retained temporary", "panicked", "retirement error"] {
            assert!(error.contains(message));
        }
    }
}
