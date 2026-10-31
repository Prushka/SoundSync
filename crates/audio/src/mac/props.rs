//! CoreAudio property access and device listing.

use std::ffi::c_void;
use std::ptr::NonNull;

use coreaudio::audio_unit::Scope;
use coreaudio::audio_unit::macos_helpers::{get_audio_device_ids, get_audio_device_supports_scope, get_default_device_id, get_device_name};
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyBufferFrameSizeRange, kAudioDevicePropertyLatency, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyPreferredChannelLayout, kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreams,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput,
    kAudioStreamPropertyLatency,
};
use soundsync_core::layout::{default_mask, mask_from_coreaudio_labels};

use crate::{DeviceInfo, Direction, Nanos, StreamFormat};

fn scope(dir: Direction) -> u32 {
    match dir {
        Direction::Render => kAudioObjectPropertyScopeOutput,
        Direction::Capture => kAudioObjectPropertyScopeInput,
    }
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: kAudioObjectPropertyElementMain }
}

/// Fixed-size property value.
pub(crate) fn get<T: Copy + Default>(id: AudioObjectID, selector: u32, scope: u32) -> Option<T> {
    let addr = address(selector, scope);
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(id, NonNull::from(&addr), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::from(&mut value).cast::<c_void>())
    };
    (status == 0).then_some(value)
}

/// Variable-size property value as bytes (8-byte aligned buffer).
pub(crate) fn get_bytes(id: AudioObjectID, selector: u32, scope: u32) -> Option<Vec<u64>> {
    let addr = address(selector, scope);
    let mut size = 0u32;
    if unsafe { AudioObjectGetPropertyDataSize(id, NonNull::from(&addr), 0, std::ptr::null(), NonNull::from(&mut size)) } != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    let status = unsafe {
        AudioObjectGetPropertyData(id, NonNull::from(&addr), 0, std::ptr::null(), NonNull::from(&mut size), NonNull::new(buf.as_mut_ptr().cast::<c_void>())?)
    };
    (status == 0).then_some(buf)
}

fn u32_at(buf: &[u64], byte_offset: usize) -> u32 {
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), buf.len() * 8) };
    u32::from_ne_bytes(bytes[byte_offset..byte_offset + 4].try_into().expect("4 bytes"))
}

/// Channels in a scope, from the device's stream configuration (an AudioBufferList).
pub(crate) fn channels(id: AudioObjectID, dir: Direction) -> u16 {
    let Some(buf) = get_bytes(id, kAudioDevicePropertyStreamConfiguration, scope(dir)) else { return 0 };
    // AudioBufferList { mNumberBuffers: u32, [pad], mBuffers: [AudioBuffer { mNumberChannels: u32, mDataByteSize: u32, mData: *mut c_void }] }
    let n = u32_at(&buf, 0) as usize;
    (0..n).map(|i| u32_at(&buf, 8 + i * 16)).sum::<u32>() as u16
}

/// Speaker layout from the preferred channel layout's channel labels.
fn channel_mask(id: AudioObjectID, dir: Direction, channels: u16) -> u32 {
    let Some(buf) = get_bytes(id, kAudioDevicePropertyPreferredChannelLayout, scope(dir)) else { return default_mask(channels) };
    // AudioChannelLayout { tag: u32, bitmap: u32, count: u32, descriptions: [{ label: u32, flags: u32, coords: [f32; 3] }] }
    let n = u32_at(&buf, 8) as usize;
    let labels: Vec<u32> = (0..n).map(|i| u32_at(&buf, 12 + i * 20)).collect();
    let mask = mask_from_coreaudio_labels(&labels);
    if mask.count_ones() == channels as u32 { mask } else { default_mask(channels) }
}

pub(crate) fn sample_rate(id: AudioObjectID) -> f64 {
    get::<f64>(id, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal).unwrap_or(48_000.0)
}

/// Output latency in frames: device latency + latency of its first output stream. The safety
/// offset and buffer size are not included because the IO callback's host time already is.
pub(crate) fn output_latency_frames(id: AudioObjectID) -> u32 {
    let device = get::<u32>(id, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput).unwrap_or(0);
    let stream = get_bytes(id, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput)
        .map(|b| u32_at(&b, 0))
        .and_then(|sid| get::<u32>(sid, kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal))
        .unwrap_or(0);
    device + stream
}

fn period_ns(id: AudioObjectID, rate: f64) -> (Nanos, Nanos) {
    let frames = get::<u32>(id, kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal).unwrap_or(512);
    #[derive(Clone, Copy, Default)]
    struct Range {
        min: f64,
        _max: f64,
    }
    let min = get::<Range>(id, kAudioDevicePropertyBufferFrameSizeRange, kAudioObjectPropertyScopeGlobal).map_or(frames as f64, |r| r.min);
    ((frames as f64 * 1e9 / rate) as Nanos, (min * 1e9 / rate) as Nanos)
}

pub(crate) fn list(dir: Direction) -> Vec<(DeviceInfo, AudioObjectID)> {
    let default = get_default_device_id(dir == Direction::Capture);
    let s = match dir {
        Direction::Render => Scope::Output,
        Direction::Capture => Scope::Input,
    };
    get_audio_device_ids()
        .unwrap_or_default()
        .into_iter()
        .filter(|id| get_audio_device_supports_scope(*id, s).unwrap_or(false))
        .filter_map(|id| {
            let ch = channels(id, dir);
            if ch == 0 {
                return None;
            }
            let rate = sample_rate(id);
            let (default_period_ns, min_period_ns) = period_ns(id, rate);
            let info = DeviceInfo {
                id: format!("ca:{id}"),
                name: get_device_name(id).ok()?,
                direction: dir,
                is_default: Some(id) == default,
                active: true,
                mix_format: StreamFormat { sample_rate: rate.round() as u32, channels: ch, channel_mask: channel_mask(id, dir, ch) },
                default_period_ns,
                min_period_ns,
            };
            Some((info, id))
        })
        .collect()
}
