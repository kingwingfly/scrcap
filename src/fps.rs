/// Minimum backward timestamp jump (1 second) treated as a genuine clock reset
/// rather than a stale or out-of-order frame.
const BACKWARD_RESET_NS: u128 = 1_000_000_000;

#[derive(Debug)]
struct GateState {
    last_ts: u128,
    next: u128,
}

/// Drops frames that arrive faster than the cap, which no platform enforces reliably.
#[derive(Debug)]
pub(crate) struct FpsGate {
    interval: u64,
    state: Option<GateState>,
}

impl FpsGate {
    pub(crate) fn new(fps: Option<u32>) -> Self {
        Self {
            interval: fps
                .filter(|fps| *fps > 0)
                .map_or(0, |fps| 1_000_000_000 / fps as u64),
            state: None,
        }
    }

    /// Whether a frame captured at `ts` (nanoseconds) should be delivered.
    ///
    /// Callers should also pass monotonically non-decreasing per-stream timestamps
    /// (e.g. clamped against a per-stream monotonic floor). When capped, stale or
    /// out-of-order frames whose timestamp falls behind the last accepted frame by
    /// less than `max(1 s, 2 * interval)` are dropped rather than re-anchored,
    /// while a genuine large backward clock reset still re-anchors so delivery
    /// recovers cleanly.
    pub(crate) fn allow(&mut self, ts: u64) -> bool {
        if self.interval == 0 {
            return true;
        }
        // Widen to `u128` so timestamps near `u64::MAX` neither overflow nor lose
        // spacing precision by saturating.
        let interval = self.interval as u128;
        let ts = ts as u128;
        let Some(state) = &mut self.state else {
            self.state = Some(GateState {
                last_ts: ts,
                next: ts + interval,
            });
            return true;
        };
        if ts < state.last_ts {
            // Re-anchor only if the timestamp clock jumped far backwards (for instance,
            // a stream switching from a `CLOCK_MONOTONIC` fallback to stream-relative PTS).
            // Smaller backward jumps (such as a queued frame arriving after a fallback
            // sample, or slight out-of-order jitter) are dropped so already-emitted
            // timestamps never go backwards.
            let reset_threshold = BACKWARD_RESET_NS.max(interval * 2);
            if state.last_ts - ts >= reset_threshold {
                state.last_ts = ts;
                state.next = ts + interval;
                return true;
            }
            return false;
        }
        // Timestamps jitter around their nominal instants, so a source running at the cap
        // lands a little either side of `next`; without slack every early one is dropped.
        // `next` still advances by whole intervals, so the slack cannot raise the rate.
        if ts + interval / 4 < state.next {
            return false;
        }
        // Whole intervals, not from `ts`: a 75 Hz source under a 60 cap would emit 37.5.
        state.last_ts = ts;
        state.next = if ts > state.next + interval {
            ts + interval
        } else {
            state.next + interval
        };
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

    #[test]
    fn fallback_then_queued_frame_is_dropped_not_reanchored() {
        let mut gate = FpsGate::new(Some(60));
        let interval = SEC / 60;
        // Fallback clock sample at 1.000 s.
        assert!(gate.allow(SEC));
        // Queued frame with earlier capture PTS (0.980 s, 20 ms behind) must be dropped,
        // not treated as a clock reset.
        assert!(!gate.allow(SEC - 20_000_000));
        // Even larger stale pipeline latency below the 1 s reset threshold is dropped.
        assert!(!gate.allow(SEC - 500_000_000));
        // Gating cadence anchored at 1.000 s was untouched by the dropped frames.
        assert!(!gate.allow(SEC + interval / 2));
        assert!(gate.allow(SEC + interval));

        // After a slightly late accepted frame (where `next - last_ts < interval / 4`),
        // a small backward jump is still dropped even though `ts + interval / 4 >= next`.
        let mut gate = FpsGate::new(Some(60));
        assert!(gate.allow(SEC));
        assert!(gate.allow(SEC + 30_000_000));
        assert!(!gate.allow(SEC + 29_500_000));
    }

    #[test]
    fn backward_timestamp_jump_reanchors() {
        let mut gate = FpsGate::new(Some(60));
        let interval = SEC / 60;
        assert!(gate.allow(10 * SEC));
        // Genuine large clock reset (10 s -> 1 s) re-anchors and recovers.
        assert!(gate.allow(SEC));
        assert!(!gate.allow(SEC + interval / 2));
        assert!(gate.allow(SEC + interval));

        // Jump far backwards (e.g. CLOCK_MONOTONIC fallback -> stream-relative PTS).
        assert!(gate.allow(10 * SEC));
        assert!(gate.allow(1));
        // Normal gating resumes from the re-anchored timestamp.
        assert!(!gate.allow(1 + interval / 2));
        assert!(gate.allow(1 + interval));

        // Subsequent high-rate frames after a backward jump are still capped at 60 Hz.
        let passed = (0..1200)
            .map(|i| SEC + i * SEC / 120)
            .filter(|&ts| gate.allow(ts))
            .count();
        assert_eq!(passed, 600);

        // Low frame-rate caps respect the `2 * interval` floor on the reset threshold.
        let mut low_fps = FpsGate::new(Some(1));
        assert!(low_fps.allow(10 * SEC));
        assert!(!low_fps.allow(8 * SEC + 500_000_000));
        assert!(low_fps.allow(SEC));
    }

    #[test]
    fn near_u64_max_does_not_overflow_or_stall() {
        let mut gate = FpsGate::new(Some(60));
        assert!(gate.allow(u64::MAX - 500));
        // 500 ns later is far below one 60 Hz interval and must be dropped without overflow.
        assert!(!gate.allow(u64::MAX));
        // Small backward jump near `u64::MAX` is dropped without overflow or re-anchor.
        assert!(!gate.allow(u64::MAX - 20_000_000));
        // Genuine large backward jump away from near `u64::MAX` re-anchors cleanly.
        assert!(gate.allow(u64::MAX - 10 * SEC));
        assert!(gate.allow(u64::MAX));
        assert!(gate.allow(1));

        // 1 second of 120 Hz monotonic frames running right up to `u64::MAX` caps at 60 fps
        // without overflow or non-monotonic admissions.
        let mut gate = FpsGate::new(Some(60));
        let start = u64::MAX - SEC;
        let passed: Vec<u64> = (0..120)
            .map(|i| start + i * SEC / 120)
            .filter(|&ts| gate.allow(ts))
            .collect();
        assert_eq!(passed.len(), 60);
        assert!(passed.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn high_rate_monotonic_does_not_trip_backward_reanchor() {
        // High-rate sources can emit a frame admitted via the `interval / 4` slack followed
        // immediately by another frame still before `next - interval`; it must not re-anchor.
        for (rate, frames) in [(360u64, 3600u64), (1000, 10_000)] {
            for jitter in [false, true] {
                let mut gate = FpsGate::new(Some(60));
                let passed: Vec<u64> = (0..frames)
                    .map(|i| {
                        let ts = SEC + i * SEC / rate;
                        match jitter {
                            true => ts.saturating_add_signed(JITTER[i as usize % JITTER.len()]),
                            false => ts,
                        }
                    })
                    .filter(|&ts| gate.allow(ts))
                    .collect();
                // 600 slots in 10 seconds, plus up to 1 straddling the end admitted by slack.
                assert!(
                    (600..=601).contains(&passed.len()),
                    "{rate} Hz, jitter {jitter}: {}",
                    passed.len()
                );
                let min_gap = SEC / 60 * 3 / 4 - if jitter { 1_000_000 } else { 0 };
                assert!(
                    passed
                        .windows(2)
                        .all(|w| w[1] >= w[0] && w[1] - w[0] >= min_gap),
                    "{rate} Hz, jitter {jitter}: spacing violated"
                );
            }
        }
    }
}
