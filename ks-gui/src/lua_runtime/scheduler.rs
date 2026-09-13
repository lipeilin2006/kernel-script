use std::time::Duration;

pub use super::types::RuntimeControl;

pub struct FixedUpdateScheduler {
    step: Duration,
    max_elapsed: Duration,
    max_steps: usize,
    accumulator: Duration,
}

impl FixedUpdateScheduler {
    pub fn new(step: Duration, max_elapsed: Duration, max_steps: usize) -> Self {
        Self {
            step,
            max_elapsed,
            max_steps,
            accumulator: Duration::ZERO,
        }
    }

    pub fn advance(&mut self, elapsed: Duration) -> usize {
        self.accumulator += elapsed.min(self.max_elapsed);
        let available = (self.accumulator.as_nanos() / self.step.as_nanos()) as usize;
        let steps = available.min(self.max_steps);
        self.accumulator = if available > self.max_steps {
            Duration::ZERO
        } else {
            self.accumulator - self.step * steps as u32
        };
        steps
    }

    pub fn reset(&mut self) {
        self.accumulator = Duration::ZERO;
    }
}

pub fn update_steps(
    scheduler: &mut FixedUpdateScheduler,
    control: &RuntimeControl,
    elapsed: Duration,
) -> usize {
    if control.paused.load(std::sync::atomic::Ordering::Relaxed) {
        scheduler.reset();
        control.consume_manual_step()
    } else {
        scheduler.advance(elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_fractional_time() {
        let mut scheduler =
            FixedUpdateScheduler::new(Duration::from_millis(10), Duration::from_millis(100), 4);
        let control = RuntimeControl::default();
        assert_eq!(
            update_steps(&mut scheduler, &control, Duration::from_millis(6)),
            0
        );
        assert_eq!(
            update_steps(&mut scheduler, &control, Duration::from_millis(6)),
            1
        );
        assert_eq!(
            update_steps(&mut scheduler, &control, Duration::from_millis(8)),
            1
        );
    }

    #[test]
    fn drops_excessive_backlog() {
        let mut scheduler =
            FixedUpdateScheduler::new(Duration::from_millis(10), Duration::from_millis(100), 4);
        let control = RuntimeControl::default();
        assert_eq!(
            update_steps(&mut scheduler, &control, Duration::from_secs(1)),
            4
        );
        assert_eq!(update_steps(&mut scheduler, &control, Duration::ZERO), 0);
    }

    #[test]
    fn paused_runtime_consumes_one_manual_step() {
        let mut scheduler =
            FixedUpdateScheduler::new(Duration::from_millis(10), Duration::from_millis(100), 4);
        let control = RuntimeControl::default();
        control
            .paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
        control
            .pending_steps
            .store(3, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(update_steps(&mut scheduler, &control, Duration::ZERO), 1);
        assert_eq!(update_steps(&mut scheduler, &control, Duration::ZERO), 0);
    }
}
