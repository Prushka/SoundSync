//! WASAPI loopback capture of a render endpoint, with a silent keep-alive stream.
//!
//! Loopback only delivers packets while the endpoint is running, so a second client renders
//! zeros to it for as long as the capture runs (with IAudioClient3's minimum period when
//! requested, which also lowers the endpoint's engine period and thus the capture latency).
//! Packet timestamps (100 ns QPC units) are smoothed by a [`FrameClock`] over the device
//! position.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;

use soundsync_core::timeline::FrameClock;
use wasapi::{Direction as WDir, StreamMode};

use super::com::{ensure_com, os_err};
use super::enumerate::resolve;
use super::mmcss::ProAudioGuard;
use super::render::{init_render_client, require_f32, to_stream_format};
use crate::null::{InitResult, ThreadStream};
use crate::{AudioError, CaptureInfo, CaptureSink, DeviceSelector, Direction, LoopbackOptions, Stream};

/// Writes zeros into whatever space the keep-alive stream has free.
struct KeepAlive {
    client: wasapi::AudioClient,
    render: wasapi::AudioRenderClient,
    buffer_frames: usize,
    zeros: Vec<u8>,
}

impl KeepAlive {
    fn start(device: &wasapi::Device, low_latency: bool) -> Result<KeepAlive, AudioError> {
        let (client, mix) = init_render_client(device, low_latency)?;
        // Polling is fine here: we top it up whenever the capture loop wakes.
        let _event = client.set_get_eventhandle().map_err(|e| os_err("keep-alive event", e))?;
        let render = client.get_audiorenderclient().map_err(|e| os_err("keep-alive render client", e))?;
        let buffer_frames = client.get_buffer_size().map_err(|e| os_err("keep-alive buffer", e))? as usize;
        let zeros = vec![0u8; buffer_frames * mix.get_blockalign() as usize];
        let ka = KeepAlive { client, render, buffer_frames, zeros };
        ka.top_up();
        ka.client.start_stream().map_err(|e| os_err("start keep-alive", e))?;
        Ok(ka)
    }

    fn top_up(&self) {
        if let Ok(padding) = self.client.get_current_padding() {
            let free = self.buffer_frames.saturating_sub(padding as usize);
            if free > 0 {
                let bytes = free * self.zeros.len() / self.buffer_frames;
                let _ = self.render.write_to_device(free, &self.zeros[..bytes], None);
            }
        }
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

fn run(sel: DeviceSelector, opts: LoopbackOptions, mut sink: Box<dyn CaptureSink>, init: SyncSender<InitResult>, stop: Arc<AtomicBool>) {
    ensure_com();
    let opened = (|| {
        let (_, device) = resolve(Direction::Render, &sel)?;
        let keep = if opts.no_keepalive { None } else { Some(KeepAlive::start(&device, opts.keepalive_low_latency)?) };
        let mut client = device.get_iaudioclient().map_err(|e| os_err("activate loopback client", e))?;
        let mix = client.get_mixformat().map_err(|e| os_err("get mix format", e))?;
        require_f32(&mix)?;
        client
            .initialize_client(&mix, &WDir::Capture, &StreamMode::EventsShared { autoconvert: false, buffer_duration_hns: 0 })
            .map_err(|e| os_err("initialize loopback", e))?;
        let event = client.set_get_eventhandle().map_err(|e| os_err("loopback event", e))?;
        let capture = client.get_audiocaptureclient().map_err(|e| os_err("capture client", e))?;
        let buffer_frames = client.get_buffer_size().map_err(|e| os_err("loopback buffer", e))? as usize;
        client.start_stream().map_err(|e| os_err("start loopback", e))?;
        Ok::<_, AudioError>((keep, client, mix, event, capture, buffer_frames))
    })();
    let (keep, client, mix, event, capture, buffer_frames) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    let format = to_stream_format(&mix);
    let ch = format.channels as usize;
    let block = mix.get_blockalign() as usize;
    // Packets never exceed the buffer; leave headroom anyway.
    let mut bytes = vec![0u8; buffer_frames * block * 2];
    let mut samples = vec![0.0f32; buffer_frames * ch * 2];
    let mut clock = FrameClock::new(format.sample_rate);
    let _mmcss = ProAudioGuard::new();
    let _ = init.send(Ok((format, 0)));

    'outer: while !stop.load(Ordering::Acquire) {
        let _ = event.wait_for_event(20);
        if let Some(k) = &keep {
            k.top_up();
        }
        loop {
            let next = match capture.get_next_packet_size() {
                Ok(Some(n)) => n,
                Ok(None) => 0,
                Err(_) => break 'outer, // device invalidated
            };
            if next == 0 {
                break;
            }
            let (frames, info) = match capture.read_from_device(&mut bytes) {
                Ok(v) => v,
                Err(_) => break 'outer,
            };
            let frames = frames as usize;
            if frames == 0 {
                break;
            }
            let raw = info.timestamp as i64 * 100;
            let discontinuity = info.flags.data_discontinuity;
            if discontinuity {
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
            // Without any valid timestamp yet, fall back to the raw one.
            let time_ns = if clock.is_empty() { raw } else { clock.time_of(info.index) };
            sink.on_capture(out, format, CaptureInfo { time_ns, raw_time_ns: raw, frames, silent: info.flags.silent, discontinuity });
        }
    }
    let _ = client.stop_stream();
    drop(keep);
}

pub(crate) fn open_loopback(sel: &DeviceSelector, opts: LoopbackOptions, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> {
    let sel = sel.clone();
    Ok(Box::new(ThreadStream::spawn_with_init("soundsync-loopback", move |init, stop| run(sel, opts, sink, init, stop))?))
}
