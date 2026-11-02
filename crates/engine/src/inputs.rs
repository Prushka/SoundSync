//! Microphone inventory work stays off the engine thread: endpoint probes can stall in drivers.

use std::collections::HashSet;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, bounded};
use soundsync_audio::{AudioError, AudioHost, Direction};
use soundsync_core::protocol::{Control, InputList, control_fits};
use soundsync_core::{Nanos, NodeId};

/// Nearby requests share one snapshot; later polls still discover changes without OS events.
const CACHE_NS: Nanos = 1_000_000_000;

pub(crate) struct InputInventory {
    requests: Sender<()>,
    results: Receiver<Result<InputList, AudioError>>,
    pending: HashSet<NodeId>,
    in_flight: bool,
    cached: Option<(Nanos, InputList)>,
}

impl InputInventory {
    pub fn spawn(host: Arc<dyn AudioHost>) -> std::io::Result<Self> {
        let (requests, rx) = bounded(1);
        let (tx, results) = bounded(1);
        std::thread::Builder::new().name("soundsync-inputs".into()).spawn(move || {
            while rx.recv().is_ok() {
                if tx.send(input_list(host.as_ref())).is_err() {
                    break;
                }
            }
        })?;
        // Dropping the channels stops the worker after its current OS call. Do not join it:
        // an unresponsive audio driver must not hold up engine shutdown either.
        Ok(Self { requests, results, pending: HashSet::new(), in_flight: false, cached: None })
    }

    /// Returns a fresh cached answer, or queues this peer for the single background probe.
    pub fn request(&mut self, peer: NodeId, now: Nanos) -> Option<InputList> {
        if let Some((at, list)) = &self.cached
            && (0..CACHE_NS).contains(&now.saturating_sub(*at))
        {
            return Some(list.clone());
        }
        self.pending.insert(peer);
        if !self.in_flight {
            match self.requests.try_send(()) {
                Ok(()) => self.in_flight = true,
                Err(e) => {
                    tracing::warn!("cannot request microphone inventory: {e}");
                    self.pending.clear();
                }
            }
        }
        None
    }

    pub fn invalidate(&mut self) {
        self.cached = None;
    }

    /// Never waits for the worker. Failed probes leave peers to retry, rather than claiming
    /// that no microphones exist or replacing a previously successful list with an empty one.
    pub fn poll(&mut self, now: Nanos) -> Option<(Vec<NodeId>, InputList)> {
        let result = self.results.try_recv().ok()?;
        self.in_flight = false;
        let peers = self.pending.drain().collect();
        match result {
            Ok(list) => {
                self.cached = Some((now, list.clone()));
                Some((peers, list))
            }
            Err(e) => {
                tracing::warn!("cannot list microphones: {e}");
                None
            }
        }
    }
}

/// This node's connected microphones, as many as fit in one answer.
fn input_list(host: &dyn AudioHost) -> Result<InputList, AudioError> {
    let devices = host.devices(Direction::Capture)?;
    let mut list = InputList {
        default: devices.iter().find(|d| d.is_default && d.active).map(|d| d.name.clone()),
        devices: devices.iter().filter(|d| d.active).map(|d| d.name.clone()).collect(),
    };
    while !list.devices.is_empty() && !control_fits(&Control::Inputs(list.clone())) {
        list.devices.pop();
    }
    // An unusually long endpoint name can make even the default alone exceed the packet.
    if !control_fits(&Control::Inputs(list.clone())) {
        list.default = None;
    }
    Ok(list)
}
