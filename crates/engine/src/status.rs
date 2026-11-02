//! Snapshot of the node's state, published for the UI every 100 ms.

use std::net::SocketAddr;

use soundsync_core::calib::CalibKind;
use soundsync_core::player::PlayState;
use soundsync_core::protocol::{InputList, OutputInfo, RxStat};
use soundsync_core::{MasterMode, Nanos, NodeId, Os, Role, StreamFormat};

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceStatus {
    pub name: String,
    pub format: StreamFormat,
    pub latency_ns: Nanos,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PeerStatus {
    pub id: NodeId,
    pub name: String,
    pub os: Os,
    pub addr: SocketAddr,
    pub role: Role,
    pub rtt_ns: Option<Nanos>,
    pub jitter_ns: Option<Nanos>,
    /// Peer clock − our clock, now.
    pub clock_offset_ns: Option<f64>,
    pub extra_latency_ns: Nanos,
    pub output: Option<OutputInfo>,
    pub rx: Vec<RxStat>,
    /// The SoundSync it runs, e.g. "0.1.0 (build 42, fcaec3d)".
    pub app_version: String,
    /// Its Volume slider, once it has reported it.
    pub volume: Option<VolumeStatus>,
    /// Its microphones, fetched automatically while connected, including with the UI hidden.
    pub inputs: Option<InputList>,
}

/// A node's Volume slider as it reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolumeStatus {
    pub volume: f32,
    pub muted: bool,
    /// The slider is the output device's own volume (else SoundSync's level on that node).
    pub device: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamStatus {
    pub master: NodeId,
    pub stream_id: u32,
    pub format: StreamFormat,
    pub state: PlayState,
    pub error_ns: Nanos,
    pub behind_ns: Nanos,
    pub lost_frames: u64,
    pub layout_warning: Option<String>,
    /// Gain followed from the master's volume (1.0 when not following).
    pub master_volume: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum CalibStatus {
    #[default]
    Idle,
    Running { kind: CalibKind, step: usize, total: usize },
    /// Per-device corrections (or, for a check run, what is left) with their quality.
    Done { session: u32, results: Vec<soundsync_core::calib::CalibResult>, kind: CalibKind, check: bool },
    Failed { kind: CalibKind, message: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub node_id: NodeId,
    pub name: String,
    pub group: String,
    pub os: Os,
    pub broadcasting: bool,
    pub master_mode: MasterMode,
    pub capture: Option<DeviceStatus>,
    pub output: Option<DeviceStatus>,
    pub delay_ns: Option<Nanos>,
    pub peers: Vec<PeerStatus>,
    pub streams: Vec<StreamStatus>,
    pub warnings: Vec<String>,
    /// Information about current conditions that needs no action.
    pub notes: Vec<String>,
    /// Counts device changes the OS reported (device pickers refresh when it moves).
    pub device_events: u64,
    pub calibration: CalibStatus,
    pub sync_test: bool,
    /// Stream playback is paused (calibration or sync test in progress).
    pub streams_paused: bool,
    pub inbound_ok: bool,
    /// The Volume slider: the output device's own volume, or SoundSync's level without one.
    pub volume: f32,
    /// The output whose own volume `volume` is (None: SoundSync's software level).
    pub volume_device: Option<String>,
    pub muted: bool,
    pub extra_latency_ns: Nanos,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            node_id: 0,
            name: String::new(),
            group: String::new(),
            os: Os::current(),
            broadcasting: false,
            master_mode: MasterMode::Virtual,
            capture: None,
            output: None,
            delay_ns: None,
            peers: Vec::new(),
            streams: Vec::new(),
            warnings: Vec::new(),
            notes: Vec::new(),
            device_events: 0,
            calibration: CalibStatus::Idle,
            sync_test: false,
            streams_paused: false,
            inbound_ok: true,
            volume: 1.0,
            volume_device: None,
            muted: false,
            extra_latency_ns: 0,
        }
    }
}
