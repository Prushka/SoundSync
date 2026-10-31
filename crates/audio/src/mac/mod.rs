//! CoreAudio backend (macOS is receive-only: output and microphone, no loopback capture).

mod clock;
mod props;

pub use clock::now_ns;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers::{AliveListener, audio_unit_from_device_id_uninitialized};
use coreaudio::audio_unit::render_callback::{Args, data};
use coreaudio::audio_unit::{AudioUnit, Element, SampleFormat, Scope};
use objc2_core_audio_types::AudioTimeStampFlags;
use soundsync_core::timeline::FrameClock;

use crate::{
    AudioError, AudioHost, CaptureInfo, CaptureSink, DeviceEvent, DeviceInfo, DeviceSelector, Direction, EndpointVolume, LoopbackOptions, Nanos,
    OutputOptions, RenderCallback, RenderTiming, Stream, StreamFormat, StreamHandle, select_device,
};

const RECEIVE_ONLY: &str = "receive-only on macOS";
const NO_VOLUME: &str = "this output has no volume control";

fn err(e: impl std::fmt::Display) -> AudioError {
    AudioError::Os(e.to_string())
}

/// A running AudioUnit; dropping it stops the unit.
struct MacStream {
    unit: AudioUnit,
    alive: Option<AliveListener>,
    format: StreamFormat,
    latency_ns: Nanos,
}

// SAFETY: AudioUnit/AliveListener wrap CoreAudio handles that may be stopped and released from any
// thread; the stream is only ever used by its owner.
unsafe impl Send for MacStream {}

impl StreamHandle for MacStream {
    fn format(&self) -> StreamFormat {
        self.format
    }
    fn latency_ns(&self) -> Nanos {
        self.latency_ns
    }
    fn is_alive(&self) -> bool {
        self.alive.as_ref().is_none_or(|a| a.is_alive())
    }
}

impl Drop for MacStream {
    fn drop(&mut self) {
        let _ = self.unit.stop();
        if let Some(mut a) = self.alive.take() {
            let _ = a.unregister();
        }
    }
}

fn unit_format(f: StreamFormat) -> coreaudio::audio_unit::StreamFormat {
    coreaudio::audio_unit::StreamFormat {
        sample_rate: f.sample_rate as f64,
        sample_format: SampleFormat::F32,
        flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
        channels: f.channels as u32,
    }
}

fn alive_listener(id: u32) -> Option<AliveListener> {
    let mut a = AliveListener::new(id);
    a.register().ok().map(|_| a)
}

fn open_output_unit(id: u32, info: &DeviceInfo, mut cb: Box<dyn RenderCallback>) -> Result<Stream, AudioError> {
    let format = info.mix_format;
    let mut unit = audio_unit_from_device_id_uninitialized(id, false).map_err(err)?;
    // The client format matches the device's nominal rate so AUHAL adds no converter latency.
    unit.set_stream_format(unit_format(format), Scope::Input, Element::Output).map_err(err)?;
    unit.initialize().map_err(err)?;
    let latency_ns = (props::output_latency_frames(id) as f64 * 1e9 / format.sample_rate as f64) as Nanos;
    unit.set_render_callback(move |args: Args<data::Interleaved<f32>>| {
        let buf = args.data.buffer;
        buf.fill(0.0);
        let ts = args.time_stamp;
        let start = if ts.mFlags.contains(AudioTimeStampFlags::HostTimeValid) { clock::host_ns(ts.mHostTime) } else { now_ns() };
        cb.render(buf, format, RenderTiming { now_ns: now_ns(), dac_time_ns: start + latency_ns });
        Ok(())
    })
    .map_err(err)?;
    unit.start().map_err(err)?;
    Ok(Box::new(MacStream { unit, alive: alive_listener(id), format, latency_ns }))
}

fn open_input_unit(id: u32, info: &DeviceInfo, mut sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
    let format = info.mix_format;
    let mut unit = audio_unit_from_device_id_uninitialized(id, true).map_err(err)?;
    unit.set_stream_format(unit_format(format), Scope::Output, Element::Input).map_err(err)?;
    unit.initialize().map_err(err)?;
    let mut clock = FrameClock::new(format.sample_rate);
    let mut frames_seen: u64 = 0;
    unit.set_input_callback(move |args: Args<data::Interleaved<f32>>| {
        let ts = args.time_stamp;
        let raw = if ts.mFlags.contains(AudioTimeStampFlags::HostTimeValid) { clock::host_ns(ts.mHostTime) } else { now_ns() };
        clock.add(frames_seen, raw);
        let info = CaptureInfo { time_ns: clock.time_of(frames_seen), raw_time_ns: raw, frames: args.num_frames, silent: false, discontinuity: false };
        sink.on_capture(args.data.buffer, format, info);
        frames_seen += args.num_frames as u64;
        Ok(())
    })
    .map_err(err)?;
    unit.start().map_err(err)?;
    Ok(Box::new(MacStream { unit, alive: alive_listener(id), format, latency_ns: 0 }))
}

pub struct MacHost {
    _private: (),
}

impl MacHost {
    pub fn new() -> Result<MacHost, AudioError> {
        Ok(MacHost { _private: () })
    }

    fn resolve(dir: Direction, sel: &DeviceSelector) -> Result<(DeviceInfo, u32), AudioError> {
        let all = props::list(dir);
        let infos: Vec<DeviceInfo> = all.iter().map(|(i, _)| i.clone()).collect();
        let chosen = select_device(&infos, sel)?;
        all.into_iter().find(|(i, _)| i.id == chosen.id).ok_or_else(|| AudioError::NotFound(sel.to_string()))
    }
}

/// Polls the device list (CoreAudio listeners need run-loop plumbing; once a second is plenty).
struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn snapshot() -> (Vec<String>, Option<String>, Option<String>) {
    let render = props::list(Direction::Render);
    let capture = props::list(Direction::Capture);
    let default_out = render.iter().find(|(i, _)| i.is_default).map(|(i, _)| i.id.clone());
    let default_in = capture.iter().find(|(i, _)| i.is_default).map(|(i, _)| i.id.clone());
    let ids = render.into_iter().chain(capture).map(|(i, _)| i.id).collect();
    (ids, default_out, default_in)
}

impl AudioHost for MacHost {
    fn now_ns(&self) -> Nanos {
        now_ns()
    }

    fn devices(&self, dir: Direction) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(props::list(dir).into_iter().map(|(i, _)| i).collect())
    }

    fn open_output(&self, sel: &DeviceSelector, _opts: OutputOptions, cb: Box<dyn RenderCallback>) -> Result<Stream, AudioError> {
        if *sel == DeviceSelector::Null {
            let fmt = StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 0x3 };
            return Ok(crate::null::open_null_output(Arc::new(now_ns), fmt, cb));
        }
        let (info, id) = Self::resolve(Direction::Render, sel)?;
        open_output_unit(id, &info, cb)
    }

    fn open_loopback(&self, _sel: &DeviceSelector, _opts: LoopbackOptions, _sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
        Err(AudioError::Unsupported(RECEIVE_ONLY))
    }

    fn open_input(&self, sel: &DeviceSelector, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
        let (info, id) = Self::resolve(Direction::Capture, sel)?;
        open_input_unit(id, &info, sink)
    }

    fn probe_loopback_latency(&self, _sel: &DeviceSelector) -> Result<Nanos, AudioError> {
        Err(AudioError::Unsupported(RECEIVE_ONLY))
    }

    fn endpoint_volume(&self, sel: &DeviceSelector) -> Result<EndpointVolume, AudioError> {
        let (_, id) = Self::resolve(Direction::Render, sel)?;
        props::volume(id).ok_or(AudioError::Unsupported(NO_VOLUME))
    }

    fn set_endpoint_volume(&self, sel: &DeviceSelector, scalar: f32) -> Result<(), AudioError> {
        let (_, id) = Self::resolve(Direction::Render, sel)?;
        props::set_volume(id, scalar.clamp(0.0, 1.0)).then_some(()).ok_or(AudioError::Unsupported(NO_VOLUME))
    }

    fn set_endpoint_mute(&self, sel: &DeviceSelector, muted: bool) -> Result<(), AudioError> {
        let (_, id) = Self::resolve(Direction::Render, sel)?;
        props::set_mute(id, muted).then_some(()).ok_or(AudioError::Unsupported(NO_VOLUME))
    }

    fn active_sessions(&self, _sel: &DeviceSelector) -> Result<usize, AudioError> {
        Err(AudioError::Unsupported(RECEIVE_ONLY))
    }

    fn watch(&self, on_event: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Result<Box<dyn Send>, AudioError> {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("soundsync-devwatch".into())
            .spawn(move || {
                let (mut ids, mut out, mut inp) = snapshot();
                while !s.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_secs(1));
                    let (new_ids, new_out, new_in) = snapshot();
                    for id in new_ids.iter().filter(|i| !ids.contains(i)) {
                        on_event(DeviceEvent::Added(id.clone()));
                    }
                    for id in ids.iter().filter(|i| !new_ids.contains(i)) {
                        on_event(DeviceEvent::Removed(id.clone()));
                    }
                    if new_out != out {
                        on_event(DeviceEvent::DefaultChanged(Direction::Render));
                    }
                    if new_in != inp {
                        on_event(DeviceEvent::DefaultChanged(Direction::Capture));
                    }
                    (ids, out, inp) = (new_ids, new_out, new_in);
                }
            })
            .map_err(err)?;
        Ok(Box::new(Watcher { stop, thread: Some(thread) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_time_is_monotonic_ns() {
        let a = now_ns();
        std::thread::sleep(Duration::from_millis(20));
        let b = now_ns();
        assert!((15_000_000..200_000_000).contains(&(b - a)), "{}", b - a);
    }

    #[test]
    #[ignore = "requires audio hardware"]
    fn mac_lists_output_devices() {
        let host = MacHost::new().unwrap();
        let devices = host.devices(Direction::Render).unwrap();
        assert!(!devices.is_empty());
        assert!(devices.iter().all(|d| d.mix_format.channels > 0 && d.mix_format.sample_rate > 0));
    }

    struct Rec(Arc<std::sync::Mutex<Vec<RenderTiming>>>);
    impl RenderCallback for Rec {
        fn render(&mut self, _out: &mut [f32], _f: StreamFormat, t: RenderTiming) {
            self.0.lock().unwrap().push(t);
        }
    }

    #[test]
    #[ignore = "requires audio hardware (writes silence to the default output)"]
    fn mac_render_reports_monotonic_dac_times() {
        let host = MacHost::new().unwrap();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = host.open_output(&DeviceSelector::Default, OutputOptions::default(), Box::new(Rec(calls.clone()))).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        drop(s);
        let calls = calls.lock().unwrap();
        assert!(calls.len() > 10);
        assert!(calls.windows(2).all(|w| w[1].dac_time_ns > w[0].dac_time_ns));
        assert!(calls.iter().all(|t| t.dac_time_ns > t.now_ns));
    }
}
