//! Known peers: identity from HELLO, liveness, clock estimate and latest gossiped state.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use soundsync_core::clock::{ClockEstimator, ClockMap};
use soundsync_core::protocol::{Hello, InputList, NodeState};
use soundsync_core::{Nanos, NodeId, Os};

pub const PEER_TIMEOUT_NS: Nanos = 5_000_000_000;
const FAST_PING_WINDOW_NS: Nanos = 2_000_000_000;
const FAST_PING_NS: Nanos = 50_000_000;
const PING_NS: Nanos = 250_000_000;

pub struct Peer {
    pub id: NodeId,
    pub addr: SocketAddr,
    pub name: String,
    pub os: Os,
    pub session: u64,
    pub app_version: String,
    pub first_seen: Nanos,
    pub last_rx: Nanos,
    pub clock: ClockEstimator,
    pub state: Option<NodeState>,
    /// Its microphones, from its last `Inputs` answer.
    pub inputs: Option<InputList>,
    /// Engine-owned polling: device lists must not depend on a visible Sync tab.
    pub next_inputs: Nanos,
    pub next_ping: Nanos,
}

impl Peer {
    pub fn ping_interval(&self, now: Nanos) -> Nanos {
        if now - self.first_seen < FAST_PING_WINDOW_NS { FAST_PING_NS } else { PING_NS }
    }

    /// Peer clock − local clock at local time `now`.
    pub fn clock_offset_ns(&self, now: Nanos) -> Option<f64> {
        let m = self.clock.map()?;
        Some(m.offset_ns + m.skew * (now - m.ref_local) as f64)
    }

    pub fn clock_map(&self) -> Option<ClockMap> {
        self.clock.map()
    }
}

#[derive(Default)]
pub struct PeerTable {
    peers: BTreeMap<NodeId, Peer>,
}

impl PeerTable {
    /// Records a HELLO. Returns true for a new peer or a restarted one (new session), whose clock
    /// and state start over.
    pub fn upsert_hello(&mut self, id: NodeId, addr: SocketAddr, hello: &Hello, now: Nanos) -> bool {
        match self.peers.get_mut(&id) {
            Some(p) if p.session == hello.session => {
                p.addr = addr;
                p.name = hello.name.clone();
                p.last_rx = now;
                false
            }
            _ => {
                self.peers.insert(
                    id,
                    Peer {
                        id,
                        addr,
                        name: hello.name.clone(),
                        os: hello.os,
                        session: hello.session,
                        app_version: hello.app_version.clone(),
                        first_seen: now,
                        last_rx: now,
                        clock: ClockEstimator::new(),
                        state: None,
                        inputs: None,
                        next_inputs: now,
                        next_ping: now,
                    },
                );
                true
            }
        }
    }

    pub fn get(&self, id: NodeId) -> Option<&Peer> {
        self.peers.get(&id)
    }

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Peer> {
        self.peers.get_mut(&id)
    }

    pub fn touch(&mut self, id: NodeId, now: Nanos) -> bool {
        match self.peers.get_mut(&id) {
            Some(p) => {
                p.last_rx = now;
                true
            }
            None => false,
        }
    }

    pub fn has_addr(&self, addr: SocketAddr) -> bool {
        self.peers.values().any(|p| p.addr == addr)
    }

    /// Removes peers silent for longer than the timeout and returns them.
    pub fn expire(&mut self, now: Nanos) -> Vec<Peer> {
        let dead: Vec<NodeId> = self.peers.values().filter(|p| now - p.last_rx > PEER_TIMEOUT_NS).map(|p| p.id).collect();
        dead.into_iter().filter_map(|id| self.peers.remove(&id)).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Peer> {
        self.peers.values_mut()
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn reset_clocks(&mut self) {
        for p in self.peers.values_mut() {
            p.clock.reset();
        }
    }
}
