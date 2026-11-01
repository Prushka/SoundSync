//! This node's Volume slider: the output device's own volume, or SoundSync's level for outputs
//! without one. Changed from the UI, from other nodes, and by calibration's volume matching.

use std::sync::atomic::Ordering;

use soundsync_audio::{AudioError, DeviceSelector};
use soundsync_core::NodeId;
use soundsync_core::protocol::{Control, VolumeChange};

use crate::node::Node;
use crate::status::VolumeStatus;

fn report(result: Result<(), AudioError>, what: &str) {
    if let Err(e) = result {
        tracing::warn!("cannot {what}: {e}");
    }
}

impl Node {
    /// Reads the output device's own volume and keeps SoundSync's software level out of the way
    /// while the device has one (the Volume slider is the device's volume then).
    pub(crate) fn poll_output_volume(&mut self) {
        self.media.output_volume = self.media.output.as_ref().and_then(|o| self.host.endpoint_volume(&DeviceSelector::Id(o.id.clone())).ok());
        let (volume, muted) = match self.media.output_volume {
            Some(_) => (1.0, false),
            None => (self.config.playback.volume, self.config.playback.muted),
        };
        self.media.mixer.volume_bits.store(volume.to_bits(), Ordering::Relaxed);
        self.media.mixer.muted.store(muted, Ordering::Relaxed);
    }

    /// The output whose own volume the Volume slider controls, if it has one.
    fn device_volume_selector(&self) -> Option<DeviceSelector> {
        self.media.output_volume.and(self.media.output.as_ref()).map(|o| DeviceSelector::Id(o.id.clone()))
    }

    /// This node's Volume slider, as shown here and reported to peers.
    pub(crate) fn own_volume(&self) -> VolumeStatus {
        match self.media.output_volume {
            Some(v) => VolumeStatus { volume: v.scalar, muted: v.muted, device: true },
            None => VolumeStatus { volume: self.config.playback.volume, muted: self.config.playback.muted, device: false },
        }
    }

    /// Changes any node's volume: this one directly, a peer by asking it.
    pub(crate) fn change_volume(&mut self, node: NodeId, change: VolumeChange) {
        if node == self.id() {
            self.change_own_volume(change);
        } else if let Some(p) = self.peers.get(node) {
            self.send(p.addr, &Control::SetVolume { target: node, change });
        }
    }

    pub(crate) fn change_own_volume(&mut self, change: VolumeChange) {
        match (change, self.device_volume_selector()) {
            (VolumeChange::Set(v), Some(sel)) => report(self.host.set_endpoint_volume(&sel, v.clamp(0.0, 1.0)), "set the output's volume"),
            (VolumeChange::Muted(m), Some(sel)) => report(self.host.set_endpoint_mute(&sel, m), "mute the output"),
            (VolumeChange::TurnDown { db }, Some(sel)) => match self.host.endpoint_volume(&sel) {
                Ok(v) => report(self.host.set_endpoint_volume_db(&sel, v.db - db.max(0.0)), "turn the output down"),
                Err(e) => tracing::warn!("cannot read the output's volume: {e}"),
            },
            (VolumeChange::Set(v), None) => self.config.playback.volume = v.clamp(0.0, 1.0),
            (VolumeChange::Muted(m), None) => self.config.playback.muted = m,
            (VolumeChange::TurnDown { db }, None) => self.config.playback.volume *= 10f32.powf(-db.max(0.0) / 20.0),
        }
        if self.device_volume_selector().is_none() {
            self.save_config();
        }
        self.poll_output_volume();
        self.send_state(); // the other nodes' volume columns update at once
    }
}
