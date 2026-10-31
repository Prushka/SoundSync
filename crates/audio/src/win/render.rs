//! Event-driven shared-mode render with DAC timestamps.
//!
//! The DAC time of the next frame written is derived from `IAudioClock::GetPosition`, which
//! reports the playback position together with the QPC time it was sampled at. That position
//! already tracks the hardware, so `GetStreamLatency` is not added (it is reported for display
//! only); any remaining constant error is absorbed by calibration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

use wasapi::{Direction as WDir, SampleType, StreamMode, TimingMode, WaveFormat};

use super::com::{ensure_com, now_ns, os_err};
use super::enumerate::resolve;
use super::mmcss::ProAudioGuard;
use crate::null::{InitResult, ThreadStream};
use crate::{AudioError, DeviceSelector, Direction, Nanos, OutputOptions, RenderCallback, RenderTiming, Stream, StreamFormat};

pub(crate) fn to_stream_format(w: &WaveFormat) -> StreamFormat {
    StreamFormat { sample_rate: w.get_samplespersec(), channels: w.get_nchannels(), channel_mask: w.get_dwchannelmask() }
}

pub(crate) fn require_f32(w: &WaveFormat) -> Result<(), AudioError> {
    match (w.get_subformat(), w.get_bitspersample()) {
        (Ok(SampleType::Float), 32) => Ok(()),
        _ => Err(AudioError::Unsupported("device mix format is not 32-bit float")),
    }
}

/// Opens a shared-mode client on `device` for rendering in its mix format.
pub(crate) fn init_render_client(device: &wasapi::Device, low_latency: bool) -> Result<(wasapi::AudioClient, WaveFormat), AudioError> {
    let mut client = device.get_iaudioclient().map_err(|e| os_err("activate audio client", e))?;
    let mix = client.get_mixformat().map_err(|e| os_err("get mix format", e))?;
    require_f32(&mix)?;
    if low_latency
        && let Ok(periods) = client.get_shared_mode_engine_period(&mix)
        && client.initialize_shared_audio_stream(&mix, TimingMode::Events, periods.min).is_ok()
    {
        return Ok((client, mix));
    }
    if low_latency {
        // IAudioClient3 refused (e.g. engine periodicity locked); a fresh client is required.
        client = device.get_iaudioclient().map_err(|e| os_err("activate audio client", e))?;
    }
    client
        .initialize_client(&mix, &WDir::Render, &StreamMode::EventsShared { autoconvert: false, buffer_duration_hns: 0 })
        .map_err(|e| os_err("initialize render client", e))?;
    Ok((client, mix))
}

pub(crate) fn stream_latency_ns(client: &wasapi::AudioClient) -> Nanos {
    unsafe { client.as_iaudioclient().GetStreamLatency() }.map_or(0, |hns| hns * 100)
}

fn as_bytes(samples: &[f32]) -> &[u8] {
    // SAFETY: f32 has no padding and any byte pattern is a valid u8.
    unsafe { std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), std::mem::size_of_val(samples)) }
}

fn run(sel: DeviceSelector, opts: OutputOptions, mut cb: Box<dyn RenderCallback>, init: SyncSender<InitResult>, stop: Arc<AtomicBool>) {
    ensure_com();
    let opened = (|| {
        let (_, device) = resolve(Direction::Render, &sel)?;
        let (client, mix) = init_render_client(&device, opts.low_latency)?;
        let event = client.set_get_eventhandle().map_err(|e| os_err("event handle", e))?;
        let render = client.get_audiorenderclient().map_err(|e| os_err("render client", e))?;
        let clock = client.get_audioclock().map_err(|e| os_err("audio clock", e))?;
        let freq = clock.get_frequency().map_err(|e| os_err("clock frequency", e))?.max(1);
        let buffer_frames = client.get_buffer_size().map_err(|e| os_err("buffer size", e))? as usize;
        Ok::<_, AudioError>((client, mix, event, render, clock, freq, buffer_frames))
    })();
    let (client, mix, event, render, clock, freq, buffer_frames) = match opened {
        Ok(v) => v,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    let format = to_stream_format(&mix);
    let ch = format.channels as usize;
    let rate = format.sample_rate as u64;
    let mut buf = vec![0.0f32; buffer_frames * ch];
    let _mmcss = ProAudioGuard::new();
    // Start with a buffer of silence so the first period never underruns.
    if render.write_to_device(buffer_frames, as_bytes(&buf), None).is_err() || client.start_stream().is_err() {
        let _ = init.send(Err(AudioError::Os("start render stream".into())));
        return;
    }
    let _ = init.send(Ok((format, stream_latency_ns(&client))));
    let mut written = buffer_frames as u64;
    while !stop.load(Ordering::Acquire) {
        if event.wait_for_event(100).is_err() {
            continue;
        }
        let Ok(padding) = client.get_current_padding() else { break };
        let avail = buffer_frames.saturating_sub(padding as usize);
        if avail == 0 {
            continue;
        }
        let Ok((pos, qpc_100ns)) = clock.get_position() else { break };
        let pos_frames = (pos as u128 * rate as u128 / freq as u128) as i64;
        let dac = (qpc_100ns as i64) * 100 + (written as i64 - pos_frames) * 1_000_000_000 / rate as i64;
        let out = &mut buf[..avail * ch];
        out.fill(0.0);
        cb.render(out, format, RenderTiming { now_ns: now_ns(), dac_time_ns: dac });
        if render.write_to_device(avail, as_bytes(out), None).is_err() {
            break; // device invalidated or removed
        }
        written += avail as u64;
    }
    let _ = client.stop_stream();
}

pub(crate) fn open_render(sel: &DeviceSelector, opts: OutputOptions, cb: Box<dyn RenderCallback>) -> Result<Stream, AudioError> {
    let sel = sel.clone();
    Ok(Box::new(ThreadStream::spawn_with_init("soundsync-render", move |init, stop| run(sel, opts, cb, init, stop))?))
}
