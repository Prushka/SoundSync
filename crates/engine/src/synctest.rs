//! Sync test: every node clicks once per second at the same group time, each at its own pitch,
//! so you can hear (and fix with the latency sliders) any speaker that is early or late.

use soundsync_core::calib::click;
use soundsync_core::protocol::Control;
use soundsync_core::{Nanos, NodeId};

use crate::node::Node;

const LOOKAHEAD_NS: Nanos = 2_000_000_000;
const CLICK_EVERY_NS: Nanos = 1_000_000_000;
const MIN_LEAD_NS: Nanos = 50_000_000;

pub(crate) struct SyncTestState {
    /// Local time of click 0.
    epoch: Nanos,
    next: i64,
}

impl Node {
    fn click_pitch(&self) -> f64 {
        let mut ids: Vec<NodeId> = self.peers.iter().map(|p| p.id).collect();
        ids.push(self.id());
        ids.sort_unstable();
        let rank = ids.iter().position(|i| *i == self.id()).unwrap_or(0);
        600.0 + 200.0 * (rank % 6) as f64
    }

    fn set_sync_test(&mut self, epoch_local: Option<Nanos>) {
        self.sync_test = epoch_local.map(|epoch| SyncTestState { epoch, next: 0 });
        self.update_paused();
    }

    pub(crate) fn sync_test_command(&mut self, on: bool) {
        let now = self.host.now_ns();
        let epoch = now + 1_000_000_000;
        for p in self.peers.iter() {
            self.send(p.addr, &Control::SyncTest { active: on, epoch_ns: epoch });
        }
        self.set_sync_test(on.then_some(epoch));
    }

    pub(crate) fn on_sync_test(&mut self, sender: NodeId, active: bool, epoch_ns: Nanos) {
        if !active {
            self.set_sync_test(None);
            return;
        }
        if let Some(map) = self.peers.get(sender).and_then(|p| p.clock_map()) {
            self.set_sync_test(Some(map.remote_to_local(epoch_ns)));
        }
    }

    pub(crate) fn sync_test_tick(&mut self, now: Nanos) {
        let Some(rate) = self.output_rate() else { return };
        let pitch = self.click_pitch();
        loop {
            let Some(st) = &mut self.sync_test else { return };
            let at = st.epoch + st.next * CLICK_EVERY_NS;
            if at > now + LOOKAHEAD_NS {
                return;
            }
            st.next += 1;
            if at >= now + MIN_LEAD_NS {
                self.play_signal(click(rate, pitch), at);
            }
        }
    }
}
