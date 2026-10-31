//! Sleep/resume detection: a long gap between engine ticks means the machine slept (or the
//! process was frozen), so clocks and streams must start over.

use soundsync_core::Nanos;

const GAP_NS: Nanos = 2_000_000_000;

/// Trips when more than 2 s passed since the previous tick.
#[derive(Debug, Default)]
pub(crate) struct GapDetector {
    last: Option<Nanos>,
}

impl GapDetector {
    /// Some(gap) when the time since the previous tick exceeds 2 s.
    pub fn tick(&mut self, now: Nanos) -> Option<Nanos> {
        let gap = self.last.map(|l| now - l);
        self.last = Some(now);
        gap.filter(|g| *g > GAP_NS)
    }
}

/// Wall-clock time in ns. The host clock of a Mac stops while it sleeps; the wall clock does not.
pub(crate) fn wall_ns() -> Nanos {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as Nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Nanos = 1_000_000_000;

    #[test]
    fn gap_detector_trips_over_2s() {
        let mut g = GapDetector::default();
        assert_eq!(g.tick(5 * S), None, "the first tick has nothing to compare with");
        assert_eq!(g.tick(5 * S + 10_000_000), None);
        assert_eq!(g.tick(7 * S + 10_000_000), None, "exactly 2 s is not a gap");
        assert_eq!(g.tick(37 * S + 10_000_000), Some(30 * S));
        assert_eq!(g.tick(37 * S + 20_000_000), None);
        assert_eq!(g.tick(30 * S), None, "a clock going backwards is not a sleep");
    }
}
