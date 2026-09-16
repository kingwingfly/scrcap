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
        // Timestamps jitter around their nominal instants, so a source running at the cap
        // lands a little either side of `next`; without slack every early one is dropped.
        // `next` still advances by whole intervals, so the slack cannot raise the rate.
        if ts + self.interval / 4 < next {
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

#[cfg(test)]
mod tests {
    use super::FpsGate;

    const SEC: u64 = 1_000_000_000;
    /// Repeating offsets in ns, a few hundred microseconds either side of the nominal time.
    const JITTER: [i64; 7] = [400_000, -400_000, 100_000, -300_000, 0, -450_000, 350_000];

    /// How many of `frames` frames, one every `SEC * num / den` ns, pass a gate capped at `cap`.
    fn delivered(cap: u32, num: u64, den: u64, frames: u64, jitter: bool) -> usize {
        let mut gate = FpsGate::new(Some(cap));
        (0..frames)
            .map(|i| {
                let ts = SEC + i * SEC * num / den;
                match jitter {
                    true => ts.saturating_add_signed(JITTER[i as usize % JITTER.len()]),
                    false => ts,
                }
            })
            .filter(|&ts| gate.allow(ts))
            .count()
    }

    #[test]
    fn uncapped_delivers_everything() {
        let mut gate = FpsGate::new(None);
        assert!((0..100).all(|i| gate.allow(i)));
        let mut gate = FpsGate::new(Some(0));
        assert!((0..100).all(|i| gate.allow(i)));
    }

    #[test]
    fn faster_sources_are_capped_in_whole_intervals() {
        for jitter in [false, true] {
            // Ten seconds of each.
            assert_eq!(
                delivered(60, 1, 75, 750, jitter),
                600,
                "75 Hz, jitter {jitter}"
            );
            assert_eq!(
                delivered(60, 1, 120, 1200, jitter),
                600,
                "120 Hz, jitter {jitter}"
            );
            assert_eq!(
                delivered(60, 1, 144, 1440, jitter),
                600,
                "144 Hz, jitter {jitter}"
            );
            assert_eq!(
                delivered(30, 1, 60, 600, jitter),
                300,
                "60 Hz under 30, jitter {jitter}"
            );
        }
    }

    #[test]
    fn source_at_the_cap_survives_jitter() {
        assert_eq!(delivered(60, 1, 60, 600, false), 600);
        assert_eq!(delivered(60, 1, 60, 600, true), 600);
        // 59.94 Hz, the usual "60 Hz" display.
        assert_eq!(delivered(60, 1001, 60_000, 600, true), 600);
    }

    #[test]
    fn a_stall_does_not_release_a_burst() {
        let mut gate = FpsGate::new(Some(60));
        assert!(gate.allow(SEC));
        // Nothing for a second, then frames at 240 Hz: the gate re-anchors instead of letting
        // the missed intervals through at once.
        let passed: Vec<u64> = (0..240)
            .map(|i| 2 * SEC + i * SEC / 240)
            .filter(|&ts| gate.allow(ts))
            .collect();
        // 60 slots in the second, plus the one straddling its end that the slack admits.
        assert!((60..=61).contains(&passed.len()), "{}", passed.len());
        assert!(passed.windows(2).all(|w| w[1] - w[0] >= SEC / 60 * 3 / 4));
    }
}
