//! Diagnostic: renders silence for 1 s on each active output and reports how IAudioClock's
//! position advances (granularity) and how stable (dac - now) is. Silent; changes no settings.
#[cfg(windows)]
fn main() {
    use soundsync_audio::{AudioHost, DeviceSelector, Direction, OutputOptions, RenderCallback, RenderTiming, StreamFormat};
    use std::sync::{Arc, Mutex};
    struct Rec(Arc<Mutex<Vec<RenderTiming>>>);
    impl RenderCallback for Rec {
        fn render(&mut self, _out: &mut [f32], _f: StreamFormat, t: RenderTiming) { self.0.lock().unwrap().push(t); }
    }
    let host = soundsync_audio::win::WinHost::new().unwrap();
    for d in host.devices(Direction::Render).unwrap().into_iter().filter(|d| d.active) {
        for run in 0..3 {
            let v = Arc::new(Mutex::new(Vec::new()));
            let s = match host.open_output(&DeviceSelector::Id(d.id.clone()), OutputOptions::default(), Box::new(Rec(v.clone()))) { Ok(s) => s, Err(e) => { println!("{}: {e}", d.name); break; } };
            std::thread::sleep(std::time::Duration::from_millis(1500));
            drop(s);
            let v = v.lock().unwrap();
            let leads: Vec<f64> = v.iter().skip(20).map(|t| (t.dac_time_ns - t.now_ns) as f64 / 1e6).collect();
            let mean = leads.iter().sum::<f64>() / leads.len() as f64;
            let (min, max) = leads.iter().fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
            println!("{:45} run {run}: lead mean {mean:6.2} ms  min {min:6.2}  max {max:6.2}", d.name);
        }
    }
}
#[cfg(not(windows))]
fn main() {}
