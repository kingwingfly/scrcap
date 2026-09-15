/// Drops frames that arrive faster than the configured cap.
///
/// No platform enforces its own framerate knob reliably, so the cap is also applied here,
/// before the frame is copied out of the capture buffer.
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
        // Advance by whole intervals so the average rate is the cap, but resync after a
        // stall so catching up cannot emit a burst.
        self.next = Some(if ts > next + self.interval {
            ts + self.interval
        } else {
            next + self.interval
        });
        true
    }
}
