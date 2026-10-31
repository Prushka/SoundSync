//! Diagnostic: loopback-captures each active output for 1.5 s (keep-alive renders silence) and
//! prints capture timestamp − arrival time. Silent; changes no settings.
#[cfg(windows)]
fn main() {
    use soundsync_audio::{AudioHost, CaptureInfo, CaptureSink, DeviceSelector, Direction, LoopbackOptions, Nanos, StreamFormat};
    use std::sync::{Arc, Mutex};
    struct Rec(Arc<Mutex<Vec<(Nanos, Nanos)>>>);
    impl CaptureSink for Rec {
        fn on_capture(&mut self, _d: &[f32], f: StreamFormat, i: CaptureInfo) {
            let end = i.time_ns + i.frames as i64 * 1_000_000_000 / f.sample_rate as i64;
            self.0.lock().unwrap().push((end, soundsync_audio::win::now_ns()));
        }
    }
    let host = soundsync_audio::win::WinHost::new().unwrap();
    for d in host.devices(Direction::Render).unwrap().into_iter().filter(|d| d.active) {
        for run in 0..3 {
            let v = Arc::new(Mutex::new(Vec::new()));
            let s = match host.open_loopback(&DeviceSelector::Id(d.id.clone()), LoopbackOptions::default(), Box::new(Rec(v.clone()))) { Ok(s) => s, Err(e) => { println!("{}: {e}", d.name); break; } };
            std::thread::sleep(std::time::Duration::from_millis(1500));
            drop(s);
            let v = v.lock().unwrap();
            let d_ms: Vec<f64> = v.iter().skip(20).map(|(end, arrived)| (end - arrived) as f64 / 1e6).collect();
            let mean = d_ms.iter().sum::<f64>() / d_ms.len().max(1) as f64;
            let (min, max) = d_ms.iter().fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
            println!("{:45} run {run}: (end-of-packet ts - arrival) mean {mean:6.2} ms  min {min:6.2}  max {max:6.2}  packets {}", d.name, v.len());
        }
    }
}
#[cfg(not(windows))]
fn main() {}
