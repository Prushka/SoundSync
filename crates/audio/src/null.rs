//! Timer-driven output that discards audio, plus the thread-backed stream handle shared by the
//! null and fake backends.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::{Nanos, RenderCallback, RenderTiming, StreamFormat, StreamHandle};

const NULL_LATENCY_NS: Nanos = 10_000_000;

/// A stream backed by one thread that runs until `stop` is set.
pub(crate) struct ThreadStream {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    format: StreamFormat,
    latency_ns: Nanos,
}

impl ThreadStream {
    pub(crate) fn spawn(
        name: &str,
        format: StreamFormat,
        latency_ns: Nanos,
        body: impl FnOnce(Arc<AtomicBool>, Arc<AtomicBool>) + Send + 'static,
    ) -> ThreadStream {
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let (s, a) = (stop.clone(), alive.clone());
        let thread = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                body(s, a.clone());
                a.store(false, Ordering::Release);
            })
            .expect("spawn audio thread");
        ThreadStream { stop, alive, thread: Some(thread), format, latency_ns }
    }
}

/// Result a device thread reports once it has opened (or failed to open) its device.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) type InitResult = Result<(StreamFormat, Nanos), crate::AudioError>;

impl ThreadStream {
    /// Spawns a device thread that first opens the device and reports the result through `init`
    /// (format and latency), then runs until `stop`. Returns the open error, if any.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn spawn_with_init(
        name: &str,
        body: impl FnOnce(std::sync::mpsc::SyncSender<InitResult>, Arc<AtomicBool>) + Send + 'static,
    ) -> Result<ThreadStream, crate::AudioError> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<InitResult>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let (s, a) = (stop.clone(), alive.clone());
        let thread = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                body(tx, s);
                a.store(false, Ordering::Release);
            })
            .map_err(|e| crate::AudioError::Os(format!("spawn {e}")))?;
        match rx.recv() {
            Ok(Ok((format, latency_ns))) => Ok(ThreadStream { stop, alive, thread: Some(thread), format, latency_ns }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(crate::AudioError::Os("audio thread exited during start".into()))
            }
        }
    }
}

impl StreamHandle for ThreadStream {
    fn format(&self) -> StreamFormat {
        self.format
    }
    fn latency_ns(&self) -> Nanos {
        self.latency_ns
    }
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl Drop for ThreadStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Tracks how late a polling device thread wakes up, so the null and fake devices keep enough
/// audio buffered to ride out their own scheduling (a loaded PC, or a CI machine whose timers
/// fire 10-20 ms late). Real devices are clocked by their hardware callbacks instead.
pub(crate) struct WakeTracker {
    worst_ns: f64,
}

impl WakeTracker {
    pub(crate) fn new(nominal: Duration) -> Self {
        WakeTracker { worst_ns: nominal.as_nanos() as f64 }
    }

    /// Sleeps `d` through `sleep` and records how long it really took.
    pub(crate) fn sleep(&mut self, d: Duration, sleep: impl Fn(Duration)) {
        let t = std::time::Instant::now();
        sleep(d);
        let took = t.elapsed().as_nanos() as f64;
        self.worst_ns = took.max(self.worst_ns * 0.97);
    }

    /// Audio to keep buffered beyond what is playing, in ns.
    pub(crate) fn lead_ns(&self) -> f64 {
        2.0 * self.worst_ns
    }
}

/// Calls `cb` on a 10 ms cadence as if a device played `format` with 10 ms of latency.
pub fn open_null_output(now: Arc<dyn Fn() -> Nanos + Send + Sync>, format: StreamFormat, mut cb: Box<dyn RenderCallback>) -> crate::Stream {
    Box::new(ThreadStream::spawn("soundsync-null-out", format, NULL_LATENCY_NS, move |stop, _alive| {
        let ch = format.channels.max(1) as usize;
        let rate = format.sample_rate.max(1) as u64;
        let period = (rate / 100) as i64;
        let max_frames = (rate / 10) as usize;
        let mut buf = vec![0.0f32; max_frames * ch];
        let start = now();
        let mut written: i64 = 0;
        let mut wake = WakeTracker::new(Duration::from_millis(2));
        while !stop.load(Ordering::Acquire) {
            let t = now();
            let elapsed = ((t - start) as i128 * rate as i128 / 1_000_000_000) as i64;
            if elapsed > written {
                written = elapsed; // stalled past the buffer: an underrun, like a real device
            }
            let lead = (wake.lead_ns() * rate as f64 / 1e9) as i64;
            let target = elapsed + period.max(lead);
            if target > written {
                let n = ((target - written) as usize).min(max_frames);
                let dac = start + NULL_LATENCY_NS + (written as i128 * 1_000_000_000 / rate as i128) as Nanos;
                let out = &mut buf[..n * ch];
                out.fill(0.0);
                cb.render(out, format, RenderTiming { now_ns: t, dac_time_ns: dac });
                written += n as i64;
            }
            wake.sleep(Duration::from_millis(2), std::thread::sleep);
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Counts frames and timings; blocks 50 ms on call number `.1` (a scheduling stall).
    struct Count(Arc<Mutex<(usize, Vec<RenderTiming>)>>, Option<usize>);
    impl RenderCallback for Count {
        fn render(&mut self, out: &mut [f32], format: StreamFormat, t: RenderTiming) {
            let calls = {
                let mut g = self.0.lock().unwrap();
                g.0 += out.len() / format.channels as usize;
                g.1.push(t);
                g.1.len()
            };
            if Some(calls) == self.1 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    #[test]
    fn wake_tracker_buffers_for_late_wakeups() {
        let mut w = WakeTracker::new(Duration::from_millis(2));
        assert!(w.lead_ns() < 10e6, "a punctual thread needs little headroom: {}", w.lead_ns());
        // A machine whose 2 ms sleeps take 20 ms (GitHub's macOS runners).
        w.sleep(Duration::from_millis(2), |_| std::thread::sleep(Duration::from_millis(20)));
        assert!(w.lead_ns() >= 40e6, "lead {}", w.lead_ns());
        for _ in 0..200 {
            w.sleep(Duration::from_millis(2), |_| {});
        }
        assert!(w.lead_ns() < 10e6, "headroom shrinks again once wake-ups are punctual: {}", w.lead_ns());
    }

    #[test]
    fn null_output_calls_back_at_real_time_rate() {
        // CI machines oversleep; a stalled render (here: the callback blocks 50 ms once) must
        // skip ahead like a real device's underrun, never report a DAC time in the past.
        let seen = Arc::new(Mutex::new((0usize, Vec::new())));
        let start = std::time::Instant::now();
        let now: Arc<dyn Fn() -> Nanos + Send + Sync> = Arc::new(move || start.elapsed().as_nanos() as Nanos + 1_000_000_000);
        let fmt = StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 3 };
        let stream = open_null_output(now, fmt, Box::new(Count(seen.clone(), Some(10))));
        std::thread::sleep(std::time::Duration::from_millis(1000));
        drop(stream);
        // The test thread can oversleep too. Bound frames by how long the stream actually
        // ran, with the same 100 ms headroom, rather than assuming exactly one second.
        let elapsed_frames = (start.elapsed().as_nanos() * fmt.sample_rate as u128 / 1_000_000_000) as usize;
        let g = seen.lock().unwrap();
        assert!(g.0 <= elapsed_frames + 4_800, "never renders far ahead of real time: {} frames in {elapsed_frames} elapsed frames", g.0);
        assert!(g.0 >= 30_000, "frames {}", g.0);
        for t in &g.1 {
            let lead = t.dac_time_ns - t.now_ns;
            assert!((0..=110_000_000).contains(&lead), "lead {lead}");
        }
        assert!(g.1.windows(2).all(|w| w[1].dac_time_ns > w[0].dac_time_ns));
    }
}
