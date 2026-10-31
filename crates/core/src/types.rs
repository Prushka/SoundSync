use serde::{Deserialize, Serialize};

/// Nanoseconds on a node's monotonic clock (QPC on Windows, `mach_absolute_time` on macOS).
pub type Nanos = i64;

/// Random 64-bit identity of a node, persisted in its config file.
pub type NodeId = u64;

/// PCM stream format. Samples are always interleaved `f32`.
///
/// `channel_mask` uses WAVEFORMATEXTENSIBLE speaker bits (see [`crate::layout::speaker`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_mask: u32,
}

impl StreamFormat {
    /// More channels than WAVEFORMATEXTENSIBLE can name.
    pub const MAX_CHANNELS: u16 = 32;

    /// A format SoundSync can buffer and resample (formats from the network are checked).
    pub fn is_valid(&self) -> bool {
        (8_000..=384_000).contains(&self.sample_rate) && (1..=Self::MAX_CHANNELS).contains(&self.channels)
    }
}

/// How a Windows master captures audio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MasterMode {
    /// Apps play to VB-CABLE; SoundSync loopback-captures it and plays its own stream in sync.
    #[default]
    Virtual,
    /// Loopback-capture a real output device; Windows plays it directly (lowest latency, partial sync).
    Direct,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    #[default]
    Receiver,
    Master(MasterMode),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Os {
    Windows,
    MacOs,
    Other,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Other
        }
    }
}
