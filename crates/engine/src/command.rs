//! Commands from the UI (or tests) to the engine. Each one also updates the saved config.

use soundsync_core::calib::CalibKind;
use soundsync_core::delay::DelaySetting;
use soundsync_core::protocol::VolumeChange;
use soundsync_core::{MasterMode, Nanos, NodeId};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    SetBroadcast(bool),
    SetMasterMode(MasterMode),
    SetCaptureDevice(String),
    SetMonitorDevice(String),
    SetOutputDevice(String),
    /// This node's Volume slider and mute.
    SetVolume(f32),
    SetMuted(bool),
    /// Any node's Volume slider or mute (this node or a peer).
    ChangeVolume { node: NodeId, change: VolumeChange },
    SetFollowMasterVolume(bool),
    SetPlayRemote(bool),
    /// Extra output latency of `node` (this node or a peer).
    SetExtraLatency { node: NodeId, ns: Nanos },
    SetDelay(DelaySetting),
    SetSwitchDefault(bool),
    SyncTest(bool),
    /// `check`: a quick run (one sweep per device) that only measures what is left.
    StartCalibration { mic_node: NodeId, mic_device: String, kind: CalibKind, check: bool },
    /// Applies the displayed result only if both its session and kind still match.
    ApplyCalibration { session: u32, kind: CalibKind },
    CancelCalibration,
    /// Refreshes a peer's microphones now (also refreshed automatically by the engine).
    ListInputs(NodeId),
}
