//! Commands from the UI (or tests) to the engine. Each one also updates the saved config.

use soundsync_core::delay::DelaySetting;
use soundsync_core::{MasterMode, Nanos, NodeId};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    SetBroadcast(bool),
    SetMasterMode(MasterMode),
    SetCaptureDevice(String),
    SetMonitorDevice(String),
    SetOutputDevice(String),
    SetVolume(f32),
    SetMuted(bool),
    SetFollowMasterVolume(bool),
    SetPlayRemote(bool),
    /// Extra output latency of `node` (this node or a peer).
    SetExtraLatency { node: NodeId, ns: Nanos },
    SetDelay(DelaySetting),
    SetSwitchDefault(bool),
    SyncTest(bool),
    /// `check`: a quick run (one sweep per device) that only measures what is left.
    StartCalibration { mic_node: NodeId, mic_device: String, check: bool },
    ApplyCalibration,
    CancelCalibration,
}
