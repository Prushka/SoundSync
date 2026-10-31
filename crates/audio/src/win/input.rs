//! Microphone / line-in capture (used by calibration), in RAW mode where supported so no AGC,
//! noise suppression or echo cancellation alters the measurement.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;

use soundsync_core::timeline::FrameClock;
use wasapi::{AudioClientProperties, Direction as WDir, StreamMode, StreamOption};

use super::com::{ensure_com, os_err};
use super::enumerate::resolve;
use super::mmcss::ProAudioGuard;
use super::render::{require_f32, to_stream_format};
use crate::null::{InitResult, ThreadStream};
use crate::{AudioError, CaptureInfo, CaptureSink, DeviceSelector, Direction, Stream};

fn run(sel: DeviceSelector, mut sink: Box<dyn CaptureSink>, init: SyncSender<InitResult>, stop: Arc<AtomicBool>) {
    ensure_com();
    let opened = (|| {
        let (_, device) = resolve(Direction::Capture, &sel)?;
        let mut client = device.get_iaudioclient().map_err(|e| os_err("activate capture client", e))?;
        // RAW must be requested before Initialize; not every driver supports it.
        let _ = client.set_properties(AudioClientProperties::new().set_option(StreamOption::Raw));
        let mix = client.get_mixformat().map_err(|e| os_err("get mix format", e))?;
        require_f32(&mix)?;
        client
            .initialize_client(&mix, &WDir::Capture, &StreamMode::EventsShared { autoconvert: false, buffer_duration_hns: 0 })
            .map_err(|e| os_err("initialize capture", e))?;
        let event = client.set_get_eventhandle().map_err(|e| os_err("capture event", e))?;
        let capture = client.get_audiocaptureclient().map_err(|e| os_err("capture client", e))?;
        let buffer_frames = client.get_buffer_size().map_err(|e| os_err("capture buffer", e))? as usize;
        client.start_stream().map_err(|e| os_err("start capture", e))?;
        Ok::<_, AudioError>((client, mix, event, capture, buffer_frames))
    })();
    let (client, mix, event, capture, buffer_frames) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    let format = to_stream_format(&mix);
    let ch = format.channels as usize;
    let block = mix.get_blockalign() as usize;
    let mut bytes = vec![0u8; buffer_frames * block * 2];
    let mut samples = vec![0.0f32; buffer_frames * ch * 2];
    let mut clock = FrameClock::new(format.sample_rate);
    let _mmcss = ProAudioGuard::new();
    let _ = init.send(Ok((format, 0)));

    'outer: while !stop.load(Ordering::Acquire) {
        let _ = event.wait_for_event(50);
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break 'outer,
            }
            let Ok((frames, info)) = capture.read_from_device(&mut bytes) else { break 'outer };
            let frames = frames as usize;
            if frames == 0 {
                break;
            }
            let raw = info.timestamp as i64 * 100;
            if info.flags.data_discontinuity {
                clock.reset();
            }
            if !info.flags.timestamp_error {
                clock.add(info.index, raw);
            }
            let out = &mut samples[..frames * ch];
            if info.flags.silent {
                out.fill(0.0);
            } else {
                for (d, s) in out.iter_mut().zip(bytes[..frames * block].as_chunks::<4>().0) {
                    *d = f32::from_le_bytes(*s);
                }
            }
            let time_ns = if clock.is_empty() { raw } else { clock.time_of(info.index) };
            sink.on_capture(out, format, CaptureInfo {
                time_ns,
                raw_time_ns: raw,
                frames,
                silent: info.flags.silent,
                discontinuity: info.flags.data_discontinuity,
            });
        }
    }
    let _ = client.stop_stream();
}

pub(crate) fn open_input(sel: &DeviceSelector, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
    let sel = sel.clone();
    Ok(Box::new(ThreadStream::spawn_with_init("soundsync-input", move |init, stop| run(sel, sink, init, stop))?))
}
