//! WASAPI backend.

mod com;
mod enumerate;
mod input;
mod loopback;
mod mmcss;
mod notify;
pub mod policy;
mod render;
mod volume;

pub use com::now_ns;

use crate::{
    AudioError, AudioHost, CaptureSink, DeviceEvent, DeviceInfo, DeviceSelector, Direction, EndpointVolume, LoopbackOptions, Nanos, OutputOptions,
    RenderCallback, Stream,
};

/// WASAPI host. Stateless: COM objects are created on the thread that uses them.
pub struct WinHost {
    _private: (),
}

impl WinHost {
    /// Checks that WASAPI is usable. The check runs on a short-lived thread so the caller's COM
    /// apartment stays uninitialized (the GUI thread must be single-threaded for winit/OLE).
    pub fn new() -> Result<WinHost, AudioError> {
        std::thread::spawn(|| enumerate::enumerator().map(|_| ()))
            .join()
            .map_err(|_| AudioError::Os("audio probe thread panicked".into()))??;
        Ok(WinHost { _private: () })
    }
}

impl AudioHost for WinHost {
    fn now_ns(&self) -> Nanos {
        now_ns()
    }

    fn devices(&self, dir: Direction) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(enumerate::list(dir)?.into_iter().map(|(info, _)| info).collect())
    }

    fn open_output(&self, sel: &DeviceSelector, opts: OutputOptions, cb: Box<dyn RenderCallback>) -> Result<Stream, AudioError> {
        if *sel == DeviceSelector::Null {
            return Ok(crate::null::open_null_output(
                std::sync::Arc::new(now_ns),
                crate::StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 0x3 },
                cb,
            ));
        }
        render::open_render(sel, opts, cb)
    }

    fn open_loopback(&self, sel: &DeviceSelector, opts: LoopbackOptions, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
        loopback::open_loopback(sel, opts, sink)
    }

    fn open_input(&self, sel: &DeviceSelector, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
        input::open_input(sel, sink)
    }

    /// WASAPI loopback timestamps are the endpoint's presentation times: on this project's
    /// test hardware (HDA/HDMI and USB outputs) a packet's end timestamp sits a stable ~21 ms
    /// after its arrival, identical across sessions to ±0.05 ms. So the delay from capture
    /// timestamp to DAC is 0 by construction; anything the device adds after that is the
    /// node's calibrated extra latency. (`measure::probe_loopback` checks this audibly.)
    fn probe_loopback_latency(&self, _sel: &DeviceSelector) -> Result<Nanos, AudioError> {
        Ok(0)
    }

    fn endpoint_volume(&self, sel: &DeviceSelector) -> Result<EndpointVolume, AudioError> {
        volume::endpoint_volume(sel)
    }

    fn set_endpoint_volume(&self, sel: &DeviceSelector, scalar: f32) -> Result<(), AudioError> {
        volume::set_endpoint_volume(sel, scalar)
    }

    fn set_endpoint_mute(&self, sel: &DeviceSelector, muted: bool) -> Result<(), AudioError> {
        volume::set_endpoint_mute(sel, muted)
    }

    fn active_sessions(&self, sel: &DeviceSelector) -> Result<usize, AudioError> {
        volume::active_sessions(sel)
    }

    fn watch(&self, on_event: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Result<Box<dyn Send>, AudioError> {
        notify::watch(on_event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioHost, Direction};

    const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

    struct Rec(std::sync::Arc<std::sync::Mutex<Vec<(crate::RenderTiming, usize)>>>);
    impl crate::RenderCallback for Rec {
        fn render(&mut self, out: &mut [f32], format: crate::StreamFormat, t: crate::RenderTiming) {
            self.0.lock().unwrap().push((t, out.len() / format.channels as usize));
        }
    }

    #[test]
    #[ignore = "requires audio hardware (silent Steam Streaming Speakers endpoint)"]
    fn render_reports_monotonic_dac_times() {
        let host = WinHost::new().unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sel = crate::DeviceSelector::Name("Steam Streaming Speakers".into());
        let stream = host.open_output(&sel, crate::OutputOptions::default(), Box::new(Rec(calls.clone()))).unwrap();
        let rate = stream.format().sample_rate as f64;
        std::thread::sleep(std::time::Duration::from_secs(2));
        drop(stream);
        let calls = calls.lock().unwrap();
        assert!(calls.len() > 50, "only {} callbacks", calls.len());
        for w in calls.windows(2) {
            let ((a, n), (b, _)) = (w[0], w[1]);
            assert!(b.dac_time_ns > a.dac_time_ns, "dac time went backwards");
            let step = (b.dac_time_ns - a.dac_time_ns) as f64;
            let want = n as f64 * 1e9 / rate;
            assert!((step - want).abs() < 1_000_000.0, "step {step} vs {want}");
            assert!(b.dac_time_ns > b.now_ns, "dac time must be in the future");
        }
    }

    #[test]
    #[ignore = "requires audio hardware (silent Steam Streaming Speakers endpoint)"]
    fn loopback_of_own_render_is_bit_exact() {
        let host = WinHost::new().unwrap();
        let sel = crate::DeviceSelector::Name("Steam Streaming Speakers".into());
        let r = crate::measure::probe_loopback(&host, &sel).unwrap();
        eprintln!("probe: {r:?}");
        assert!(r.bit_exact, "{r:?}");
        assert!((r.gain - 1.0).abs() < 1e-6, "{r:?}");
        assert!(r.offset_ns.abs() < 200_000_000, "{r:?}");
        assert_eq!(host.probe_loopback_latency(&sel).unwrap(), 0, "loopback timestamps are presentation times");
    }

    #[test]
    #[ignore = "requires audio hardware"]
    fn endpoint_volume_reads_scalar() {
        let host = WinHost::new().unwrap();
        let v = host.endpoint_volume(&crate::DeviceSelector::Default).unwrap();
        assert!((0.0..=1.0).contains(&v.scalar) && v.db <= 0.0, "volume {v:?}");
    }

    #[test]
    #[ignore = "requires audio hardware"]
    fn active_sessions_counts_other_processes() {
        let host = WinHost::new().unwrap();
        let sel = crate::DeviceSelector::Name("Steam Streaming Speakers".into());
        let before = host.active_sessions(&sel).unwrap();
        // Our own render session must not be counted.
        struct Silence;
        impl crate::RenderCallback for Silence {
            fn render(&mut self, _: &mut [f32], _: crate::StreamFormat, _: crate::RenderTiming) {}
        }
        let s = host.open_output(&sel, crate::OutputOptions::default(), Box::new(Silence)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(host.active_sessions(&sel).unwrap(), before);
        drop(s);
    }

    struct CapRec(std::sync::Arc<std::sync::Mutex<Vec<crate::CaptureInfo>>>);
    impl crate::CaptureSink for CapRec {
        fn on_capture(&mut self, _d: &[f32], _f: crate::StreamFormat, i: crate::CaptureInfo) {
            self.0.lock().unwrap().push(i);
        }
    }

    #[test]
    #[ignore = "requires an active recording device"]
    fn input_delivers_timestamps_if_present() {
        let host = WinHost::new().unwrap();
        if !host.devices(Direction::Capture).unwrap().iter().any(|d| d.active) {
            eprintln!("no active capture device; skipping");
            return;
        }
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = host.open_input(&crate::DeviceSelector::Default, Box::new(CapRec(got.clone()))).unwrap();
        let rate = s.format().sample_rate as f64;
        std::thread::sleep(std::time::Duration::from_millis(1200));
        drop(s);
        let got = got.lock().unwrap();
        assert!(got.len() > 20, "only {} packets", got.len());
        let now = now_ns();
        assert!(got.iter().all(|i| i.time_ns > now - 5_000_000_000 && i.time_ns < now));
        for w in got.windows(2).skip(5) {
            let step = (w[1].time_ns - w[0].time_ns) as f64;
            let want = w[0].frames as f64 * 1e9 / rate;
            assert!((step - want).abs() < 2_000_000.0, "step {step} vs {want}");
        }
    }

    #[test]
    #[ignore = "requires audio hardware"]
    fn watch_registers_and_drops() {
        let host = WinHost::new().unwrap();
        let guard = host.watch(Box::new(|_e| {})).unwrap();
        drop(guard);
    }

    #[test]
    #[ignore = "requires audio hardware"]
    fn enumerates_render_devices_with_formats() {
        let host = WinHost::new().unwrap();
        let devices = host.devices(Direction::Render).unwrap();
        let active: Vec<_> = devices.iter().filter(|d| d.active).collect();
        assert!(!active.is_empty(), "no active render device");
        assert_eq!(devices.iter().filter(|d| d.is_default).count(), 1, "exactly one default");
        for d in active {
            assert!(d.mix_format.channels >= 1, "{d:?}");
            assert!(RATES.contains(&d.mix_format.sample_rate), "{d:?}");
            assert!(d.min_period_ns > 0 && d.min_period_ns <= d.default_period_ns, "{d:?}");
            assert!(!d.name.is_empty() && !d.id.is_empty());
        }
    }
}
