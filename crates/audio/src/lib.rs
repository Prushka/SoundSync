//! Audio device I/O behind the [`AudioHost`] trait.
//!
//! * Windows: WASAPI (`win`), including loopback capture.
//! * macOS: CoreAudio (`mac`), output and microphone only (macOS nodes never capture).
//! * [`fake::FakeHost`]: virtual devices with configurable clock drift and latency, for tests.
//!
//! Timestamps are nanoseconds on the host's monotonic clock ([`AudioHost::now_ns`]).

pub mod fake;
#[cfg(target_os = "macos")]
pub mod mac;
pub mod measure;
pub mod null;
pub mod select;
#[cfg(windows)]
pub mod win;

use std::sync::Arc;

pub use select::{DeviceSelector, select_device};
pub use soundsync_core::player::RenderTiming;
pub use soundsync_core::{Nanos, StreamFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Render,
    Capture,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub direction: Direction,
    pub is_default: bool,
    pub active: bool,
    pub mix_format: StreamFormat,
    pub default_period_ns: Nanos,
    pub min_period_ns: Nanos,
}

/// Fills the output buffer (interleaved f32 in `format`) for one device period.
pub trait RenderCallback: Send + 'static {
    fn render(&mut self, out: &mut [f32], format: StreamFormat, timing: RenderTiming);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureInfo {
    /// Smoothed host time of the first frame in this buffer.
    pub time_ns: Nanos,
    /// Raw device timestamp of the first frame.
    pub raw_time_ns: Nanos,
    pub frames: usize,
    /// The device reported silence (data is zeros).
    pub silent: bool,
    /// Frames were lost before this buffer; timing restarts here.
    pub discontinuity: bool,
}

/// Receives captured audio (interleaved f32 in `format`).
pub trait CaptureSink: Send + 'static {
    fn on_capture(&mut self, data: &[f32], format: StreamFormat, info: CaptureInfo);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputOptions {
    /// Request the device's minimum shared-mode period (Windows IAudioClient3).
    pub low_latency: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopbackOptions {
    pub keepalive_low_latency: bool,
    /// Skip the silent keep-alive stream (the caller keeps the endpoint running itself).
    pub no_keepalive: bool,
}

/// A running stream; dropping it stops the stream and joins its thread.
pub trait StreamHandle: Send {
    fn format(&self) -> StreamFormat;
    /// Reported output latency (informational).
    fn latency_ns(&self) -> Nanos;
    /// False once the device went away or the stream failed.
    fn is_alive(&self) -> bool;
}

pub type Stream = Box<dyn StreamHandle>;

/// Windows default render endpoint ids for each role.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DefaultDevices {
    pub console: String,
    pub multimedia: String,
    pub communications: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultRole {
    Console,
    Multimedia,
    Communications,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceEvent {
    DefaultChanged(Direction),
    Added(String),
    Removed(String),
    StateChanged(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    #[error("audio device not found: {0}")]
    NotFound(String),
    #[error("device name matches several devices: {}", .0.join(", "))]
    Ambiguous(Vec<String>),
    #[error("not supported: {0}")]
    Unsupported(&'static str),
    #[error("audio system error: {0}")]
    Os(String),
}

/// A device's own (OS) volume: the slider position, its level in dB, and mute.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EndpointVolume {
    /// Slider position, 0..=1 (the OS maps it to a loudness curve).
    pub scalar: f32,
    pub db: f32,
    pub muted: bool,
}

impl EndpointVolume {
    /// The factor this volume applies to the sound (from its dB level; 0 when muted or at 0%).
    pub fn gain(&self) -> f32 {
        if self.muted || self.scalar <= 0.0 { 0.0 } else { 10f32.powf(self.db / 20.0).min(1.0) }
    }
}

pub trait AudioHost: Send + Sync {
    fn now_ns(&self) -> Nanos;
    fn devices(&self, dir: Direction) -> Result<Vec<DeviceInfo>, AudioError>;
    fn open_output(&self, sel: &DeviceSelector, opts: OutputOptions, cb: Box<dyn RenderCallback>) -> Result<Stream, AudioError>;
    fn open_loopback(&self, sel: &DeviceSelector, opts: LoopbackOptions, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError>;
    fn open_input(&self, sel: &DeviceSelector, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError>;
    /// Time from a loopback capture timestamp until that sample reaches the device's DAC.
    fn probe_loopback_latency(&self, sel: &DeviceSelector) -> Result<Nanos, AudioError>;
    /// The device's own volume (the OS volume slider) and mute.
    fn endpoint_volume(&self, sel: &DeviceSelector) -> Result<EndpointVolume, AudioError>;
    /// Moves the device's own volume slider (0..=1).
    fn set_endpoint_volume(&self, sel: &DeviceSelector, scalar: f32) -> Result<(), AudioError>;
    /// Sets the device's own volume to a dB level (clamped to what the device can do).
    fn set_endpoint_volume_db(&self, sel: &DeviceSelector, db: f32) -> Result<(), AudioError>;
    /// Mutes or unmutes the device itself.
    fn set_endpoint_mute(&self, sel: &DeviceSelector, muted: bool) -> Result<(), AudioError>;
    /// Number of other processes' active render sessions on the endpoint.
    fn active_sessions(&self, sel: &DeviceSelector) -> Result<usize, AudioError>;
    /// Calls `on_event` on device changes until the returned guard is dropped.
    fn watch(&self, on_event: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Result<Box<dyn Send>, AudioError>;
}

/// The platform's audio host.
pub fn default_host() -> Result<Arc<dyn AudioHost>, AudioError> {
    #[cfg(windows)]
    {
        Ok(Arc::new(win::WinHost::new()?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Arc::new(mac::MacHost::new()?))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(AudioError::Unsupported("no audio backend for this platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_volume_gain_follows_decibels_not_the_slider() {
        // Windows' slider is a loudness curve: 50% is -10.5 dB, i.e. a gain of 0.30, not 0.5.
        let v = EndpointVolume { scalar: 0.5, db: -10.5, muted: false };
        assert!((v.gain() - 0.2985).abs() < 1e-3, "{}", v.gain());
        assert_eq!(EndpointVolume { scalar: 1.0, db: 0.0, muted: false }.gain(), 1.0);
        assert_eq!(EndpointVolume { scalar: 1.0, db: 0.0, muted: true }.gain(), 0.0);
        assert_eq!(EndpointVolume { scalar: 0.0, db: -96.0, muted: false }.gain(), 0.0, "0% is silence");
    }
}
