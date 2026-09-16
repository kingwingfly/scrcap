/// Drops frames that arrive faster than the cap, which no platform enforces reliably.
#[derive(Debug)]
pub(crate) struct FpsGate {
    interval: u64,
    next: Option<u64>,
}

impl FpsGate {
    pub(crate) fn new(fps: Option<u32>) -> Self {
        Self {
            interval: fps
                .filter(|fps| *fps > 0)
                .map_or(0, |fps| 1_000_000_000 / fps as u64),
            next: None,
        }
    }

    /// Whether a frame captured at `ts` (nanoseconds) should be delivered.
    pub(crate) fn allow(&mut self, ts: u64) -> bool {
        if self.interval == 0 {
            return true;
        }
        let Some(next) = self.next else {
            self.next = Some(ts + self.interval);
            return true;
        };
        if ts < next {
            return false;
        }
        // Whole intervals, not from `ts`: a 75 Hz source under a 60 cap would emit 37.5.
        self.next = Some(if ts > next + self.interval {
            ts + self.interval
        } else {
            next + self.interval
        });
        true
    }
}
