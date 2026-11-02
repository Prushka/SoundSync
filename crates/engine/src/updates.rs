//! Authenticated update discovery, requests and acknowledgements, separate from audio peers.
//! Downloads and restarts stay in the app; this module only transports explicit user requests.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use crossbeam_channel::Sender;
use soundsync_core::protocol::PROTOCOL_VERSION;
use soundsync_core::remote_update::{UpdateAction, UpdateInfo, UpdateMessage, UpdateState, bounded};
use soundsync_core::{Nanos, NodeId, Os};

use crate::net::Transport;

const SECOND: Nanos = 1_000_000_000;
const ONLINE_FOR: Nanos = 10 * SECOND;
const REQUEST_TIMEOUT: Nanos = 8 * SECOND;
const UPDATE_TIMEOUT: Nanos = 360 * SECOND;

#[derive(Clone, Debug, PartialEq)]
pub struct UpdatePeerStatus {
    pub id: NodeId,
    pub addr: SocketAddr,
    pub name: String,
    pub os: Os,
    pub app_version: String,
    pub build: Option<u32>,
    pub protocol: u8,
    pub state: UpdateState,
    pub online: bool,
    pub pending: bool,
    pub requested: Option<UpdateAction>,
    pub notice: Option<String>,
}

impl UpdatePeerStatus {
    pub fn installable(&self) -> Option<u32> {
        if !self.online || self.pending || self.requested.is_some() {
            return None;
        }
        self.state.installable().filter(|b| self.build.is_none_or(|current| *b > current))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteUpdateRequest {
    pub sender: NodeId,
    pub name: String,
    pub action: UpdateAction,
}

struct Pending {
    number: u64,
    action: UpdateAction,
    started: Nanos,
    next_send: Nanos,
}

struct Peer {
    addr: SocketAddr,
    session: u64,
    info: UpdateInfo,
    seen: Nanos,
    pending: Option<Pending>,
    operation: Option<(UpdateAction, Nanos)>,
    notice: Option<String>,
}

pub(crate) struct Updates {
    state: UpdateState,
    peers: BTreeMap<NodeId, Peer>,
    next_announce: Nanos,
    next_request: u64,
    /// Kept for this process's whole session, even if a peer disconnects and is rediscovered.
    handled: BTreeMap<(NodeId, u64), (u64, Option<String>)>,
    pub requests: Option<Sender<RemoteUpdateRequest>>,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            state: UpdateState::Unavailable("Open the SoundSync app on this device to enable updates.".into()),
            peers: BTreeMap::new(), next_announce: 0, next_request: 1, handled: BTreeMap::new(), requests: None,
        }
    }
}

impl Updates {
    pub fn publish(&mut self, state: UpdateState) {
        if self.state != state {
            self.state = state;
            self.next_announce = 0;
        }
    }

    fn announcement(&self, name: &str, reply: bool) -> UpdateMessage {
        UpdateMessage::Status { reply, info: UpdateInfo {
            name: bounded(name, 128), os: Os::current(), version: bounded(soundsync_core::version::version(), 96),
            build: soundsync_core::version::build_number().and_then(|b| b.parse().ok()), protocol: PROTOCOL_VERSION, state: self.state.clone(),
        } }
    }

    pub fn request(&mut self, transport: &Transport, session: u64, node: NodeId, action: UpdateAction, now: Nanos) {
        let Some(p) = self.peers.get_mut(&node) else { return };
        if now - p.seen >= ONLINE_FOR || p.pending.is_some() || p.operation.is_some() {
            return;
        }
        if let UpdateAction::Install { build } = action
            && (p.info.state.installable() != Some(build) || p.info.build.is_some_and(|current| build <= current))
        {
            return;
        }
        let number = self.next_request;
        self.next_request = self.next_request.saturating_add(1);
        p.notice = None;
        p.operation = Some((action, now));
        p.pending = Some(Pending { number, action, started: now, next_send: now + SECOND });
        transport.send_update(p.addr, session, &UpdateMessage::Request { target: node, session: p.session, request: number, action });
    }

    #[allow(clippy::too_many_arguments)] // transport envelope plus the local identity
    pub fn receive(&mut self, transport: &Transport, own_session: u64, name: &str, from: SocketAddr, sender: NodeId, session: u64, message: UpdateMessage, now: Nanos) {
        match message {
            UpdateMessage::Status { reply, info } => {
                if !self.peers.contains_key(&sender) && self.peers.len() >= 128 {
                    return;
                }
                let p = self.peers.entry(sender).or_insert_with(|| Peer {
                    addr: from, session, info: info.clone(), seen: now,
                    pending: None, operation: None, notice: None,
                });
                if p.session != session {
                    p.pending = None; // a request for the old process must never be resent to the new one
                    if let Some((UpdateAction::Install { build }, _)) = p.operation {
                        p.notice = Some(if info.build.is_some_and(|b| b >= build) {
                            format!("Updated to build {}", info.build.unwrap())
                        } else {
                            "The device restarted without completing the update; try again.".into()
                        });
                    }
                    p.operation = None;
                }
                p.addr = from;
                p.session = session;
                p.info = info;
                p.seen = now;
                if p.pending.is_none() {
                    match p.operation {
                        Some((UpdateAction::Check, _)) if !p.info.state.busy() => p.operation = None,
                        Some((UpdateAction::Install { .. }, _)) if matches!(p.info.state, UpdateState::Failed { .. } | UpdateState::Unavailable(_)) => p.operation = None,
                        _ => {}
                    }
                }
                if !reply {
                    transport.send_update(from, own_session, &self.announcement(name, true));
                }
            }
            UpdateMessage::Request { target, session: target_session, request, action } => {
                if target != transport.node_id() || target_session != own_session {
                    return;
                }
                let Some(p) = self.peers.get_mut(&sender).filter(|p| p.session == session && p.addr == from && now - p.seen < ONLINE_FOR) else { return };
                let previous = self.handled.get(&(sender, session));
                let error = if previous.is_some_and(|(last, _)| request == *last) {
                    previous.unwrap().1.clone() // retry after a lost ACK: do not perform the action twice
                } else if previous.is_some_and(|(last, _)| request < *last) {
                    Some("This update request has expired; try again.".into())
                } else if previous.is_none() && self.handled.len() >= 1024 {
                    Some("Too many update sessions; restart this device to accept more.".into())
                } else {
                    let error = match (&self.state, action) {
                        (UpdateState::Unavailable(reason), _) => Some(reason.clone()),
                        (state, _) if state.busy() => Some("This device is already checking or installing an update.".into()),
                        (state, UpdateAction::Install { build }) if state.installable() != Some(build) => Some("The available release changed; check for updates again.".into()),
                        _ => {
                            let event = RemoteUpdateRequest { sender, name: p.info.name.clone(), action };
                            if self.requests.as_ref().is_none_or(|tx| tx.try_send(event).is_err()) {
                                Some("The app is not ready to update; try again.".into())
                            } else { None }
                        }
                    };
                    self.handled.insert((sender, session), (request, error.clone()));
                    error
                };
                transport.send_update(from, own_session, &UpdateMessage::Ack { target: sender, session, request, error });
            }
            UpdateMessage::Ack { target, session: target_session, request, error } => {
                if target != transport.node_id() || target_session != own_session {
                    return;
                }
                let Some(p) = self.peers.get_mut(&sender).filter(|p| p.session == session && p.addr == from) else { return };
                if p.pending.as_ref().is_some_and(|r| r.number == request) {
                    p.pending = None;
                    if error.is_some() {
                        p.operation = None;
                    }
                    p.notice = error;
                }
            }
        }
    }

    pub fn tick(&mut self, transport: &Transport, session: u64, name: &str, targets: &[SocketAddr], now: Nanos) {
        if now >= self.next_announce {
            self.next_announce = now + SECOND;
            let mut to: BTreeSet<_> = targets.iter().copied().collect();
            to.extend(self.peers.values().filter(|p| now - p.seen < ONLINE_FOR || p.operation.is_some()).map(|p| p.addr));
            let message = self.announcement(name, false);
            for addr in to {
                transport.send_update(addr, session, &message);
            }
        }
        for (id, p) in &mut self.peers {
            if let Some(r) = &mut p.pending {
                if now - r.started >= REQUEST_TIMEOUT {
                    p.notice = Some("No acknowledgement from this device; check its connection and try again.".into());
                    p.pending = None;
                    p.operation = None;
                } else if now >= r.next_send {
                    r.next_send = now + SECOND;
                    transport.send_update(p.addr, session, &UpdateMessage::Request { target: *id, session: p.session, request: r.number, action: r.action });
                }
            }
            if p.operation.is_some_and(|(_, started)| now - started >= UPDATE_TIMEOUT) {
                p.notice = Some("The update did not finish within six minutes; check this device.".into());
                p.operation = None;
            }
        }
        self.peers.retain(|_, p| now - p.seen < if p.notice.is_some() { 300 * SECOND } else { 60 * SECOND } || p.operation.is_some());
    }

    pub fn status(&self, now: Nanos) -> Vec<UpdatePeerStatus> {
        self.peers.iter().map(|(id, p)| UpdatePeerStatus {
            id: *id, addr: p.addr, name: p.info.name.clone(), os: p.info.os, app_version: p.info.version.clone(), build: p.info.build,
            protocol: p.info.protocol, state: p.info.state.clone(), online: now - p.seen < ONLINE_FOR, pending: p.pending.is_some(),
            requested: p.operation.map(|(action, _)| action), notice: p.notice.clone(),
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::{Arc, atomic::AtomicU64};
    use soundsync_core::protocol::GroupKey;

    #[test]
    fn remote_installs_require_the_current_session_address_and_advertised_build_and_are_not_replayed() {
        let transport = Transport::bind(Ipv4Addr::LOCALHOST, 0, GroupKey::derive("test", ""), Arc::new(AtomicU64::new(1)), None).unwrap();
        let from = "127.0.0.1:47811".parse().unwrap();
        let (tx, rx) = crossbeam_channel::bounded(8);
        let mut updates = Updates { requests: Some(tx), ..Updates::default() };
        updates.publish(UpdateState::Available { build: 21 });
        let status = UpdateMessage::Status { reply: true, info: UpdateInfo {
            name: "controller".into(), os: Os::Windows, version: "test".into(), build: Some(20), protocol: 200, state: UpdateState::Idle,
        } };
        let install = |session, request, build| UpdateMessage::Request { target: 1, session, request, action: UpdateAction::Install { build } };
        updates.receive(&transport, 10, "receiver", from, 2, 20, install(10, 1, 21), 0);
        assert!(rx.is_empty(), "must exchange authenticated update status first");
        updates.receive(&transport, 10, "receiver", from, 2, 20, status.clone(), 0);
        updates.receive(&transport, 10, "receiver", from, 2, 20, install(9, 1, 21), 1);
        updates.receive(&transport, 10, "receiver", from, 2, 19, install(10, 1, 21), 1);
        updates.receive(&transport, 10, "receiver", "127.0.0.1:47812".parse().unwrap(), 2, 20, install(10, 1, 21), 1);
        updates.receive(&transport, 10, "receiver", from, 2, 20, install(10, 1, 22), 1);
        assert!(rx.is_empty(), "wrong sessions, address or unadvertised build cannot install");
        updates.receive(&transport, 10, "receiver", from, 2, 20, install(10, 2, 21), 2);
        assert_eq!(rx.try_recv().unwrap().action, UpdateAction::Install { build: 21 });
        updates.receive(&transport, 10, "receiver", from, 2, 20, install(10, 2, 21), 3);
        updates.receive(&transport, 10, "receiver", from, 2, 20, install(10, 1, 21), 3);
        assert!(rx.is_empty(), "retransmission or replay must not enqueue another restart");
        updates.tick(&transport, 10, "receiver", &[], 61 * SECOND);
        assert!(updates.peers.is_empty());
        updates.receive(&transport, 10, "receiver", from, 2, 20, status, 62 * SECOND);
        updates.receive(&transport, 10, "receiver", from, 2, 20, install(10, 2, 21), 62 * SECOND);
        assert!(rx.is_empty(), "rediscovery must not make an old request new again");
    }

    #[test]
    fn lost_acknowledgements_time_out_and_a_restart_only_succeeds_when_the_build_changed() {
        let transport = Transport::bind(Ipv4Addr::LOCALHOST, 0, GroupKey::derive("test", ""), Arc::new(AtomicU64::new(1)), None).unwrap();
        let from = "127.0.0.1:47811".parse().unwrap();
        let mut updates = Updates::default();
        let status = |build| UpdateMessage::Status { reply: true, info: UpdateInfo {
            name: "receiver".into(), os: Os::MacOs, version: format!("0.1.0 (build {build})"), build: Some(build), protocol: 200,
            state: if build < 21 { UpdateState::Available { build: 21 } } else { UpdateState::Idle },
        } };
        updates.receive(&transport, 10, "controller", from, 2, 20, status(20), 0);
        updates.request(&transport, 10, 2, UpdateAction::Install { build: 21 }, 1);
        assert!(updates.status(1)[0].pending);
        updates.tick(&transport, 10, "controller", &[], SECOND + 1);
        assert_eq!(updates.peers[&2].pending.as_ref().unwrap().number, 1, "a retry keeps its request number");
        updates.tick(&transport, 10, "controller", &[], REQUEST_TIMEOUT + 1);
        assert!(updates.status(REQUEST_TIMEOUT + 1)[0].notice.as_ref().unwrap().contains("No acknowledgement"));
        assert!(updates.status(REQUEST_TIMEOUT + 1)[0].requested.is_none());
        updates.receive(&transport, 10, "controller", from, 2, 20, status(20), 9 * SECOND);
        updates.request(&transport, 10, 2, UpdateAction::Install { build: 21 }, 9 * SECOND);
        updates.receive(&transport, 10, "controller", from, 2, 20, UpdateMessage::Ack { target: 1, session: 10, request: 2, error: None }, 9 * SECOND);
        assert!(!updates.status(9 * SECOND)[0].pending);
        updates.receive(&transport, 10, "controller", from, 2, 21, status(20), 10 * SECOND);
        assert!(updates.status(10 * SECOND)[0].notice.as_ref().unwrap().contains("without completing"));
        updates.request(&transport, 10, 2, UpdateAction::Install { build: 21 }, 10 * SECOND);
        updates.receive(&transport, 10, "controller", from, 2, 22, status(21), 11 * SECOND);
        let complete = &updates.status(11 * SECOND)[0];
        assert_eq!(complete.notice.as_deref(), Some("Updated to build 21"));
        assert!(complete.requested.is_none());
        assert!(complete.installable().is_none(), "the new copy cannot be updated to the same build again");
    }

    #[test]
    fn bulk_updates_only_select_online_idle_devices_with_a_newer_release() {
        let available = UpdatePeerStatus {
            id: 2, addr: "127.0.0.1:47811".parse().unwrap(), name: "receiver".into(), os: Os::MacOs,
            app_version: "0.1.0 (build 20)".into(), build: Some(20), protocol: 200, state: UpdateState::Available { build: 21 },
            online: true, pending: false, requested: None, notice: None,
        };
        assert_eq!(available.installable(), Some(21));
        for blocked in [
            UpdatePeerStatus { online: false, ..available.clone() },
            UpdatePeerStatus { pending: true, ..available.clone() },
            UpdatePeerStatus { requested: Some(UpdateAction::Check), ..available.clone() },
            UpdatePeerStatus { build: Some(21), ..available.clone() },
            UpdatePeerStatus { build: Some(22), ..available.clone() },
            UpdatePeerStatus { state: UpdateState::UpToDate, ..available.clone() },
            UpdatePeerStatus { state: UpdateState::Unavailable("development build".into()), ..available.clone() },
            UpdatePeerStatus { state: UpdateState::Downloading { build: 21, received: 1, total: 2 }, ..available.clone() },
        ] {
            assert!(blocked.installable().is_none(), "{blocked:?}");
        }
        let retry = UpdatePeerStatus { state: UpdateState::Failed { error: "temporarily offline".into(), build: Some(21) }, ..available };
        assert_eq!(retry.installable(), Some(21));
    }
}
