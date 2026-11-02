//! Remote update requests cross the existing UDP transport without changing audio or files.

mod common;

use std::time::Duration;
use soundsync_core::remote_update::{self, UpdateAction, UpdateInfo, UpdateMessage, UpdateState};
use soundsync_core::protocol::{GroupKey, MAX_DATAGRAM};
use soundsync_core::Os;
use soundsync_engine::Command;
use common::{Spec, spawn_with, wait_until};

#[test]
fn one_device_can_check_and_update_another_without_updating_itself() {
    let a = spawn_with(Spec::new("controller"), |_| {}, |_| {});
    let b = spawn_with(Spec { peers: &[&a], ..Spec::new("receiver") }, |_| {}, |_| {});
    b.engine.command(Command::PublishUpdate(UpdateState::Available { build: 21 }));
    let id = b.engine.status().node_id;
    assert!(wait_until(Duration::from_secs(5), || a.engine.status().updates.iter().any(|p| p.id == id && p.state == UpdateState::Available { build: 21 })));
    a.engine.command(Command::RemoteUpdate { node: id, action: UpdateAction::Check });
    assert!(wait_until(Duration::from_secs(3), || b.engine.take_update_request().is_some_and(|r| r.action == UpdateAction::Check)));
    b.engine.command(Command::PublishUpdate(UpdateState::Checking));
    assert!(wait_until(Duration::from_secs(3), || a.engine.status().updates.iter().any(|p| p.id == id && p.state == UpdateState::Checking && !p.pending)));
    b.engine.command(Command::PublishUpdate(UpdateState::Available { build: 21 }));
    assert!(wait_until(Duration::from_secs(3), || a.engine.status().updates.iter().any(|p| p.id == id && p.state == UpdateState::Available { build: 21 } && p.requested.is_none())));
    a.engine.command(Command::RemoteUpdate { node: id, action: UpdateAction::Install { build: 21 } });
    assert!(wait_until(Duration::from_secs(3), || b.engine.take_update_request().is_some_and(|r| r.action == UpdateAction::Install { build: 21 })), "{:?}", a.engine.status().updates);
    assert!(a.engine.take_update_request().is_none(), "the controller was not asked to update");
    b.engine.command(Command::PublishUpdate(UpdateState::Failed { error: "folder is read-only".into(), build: Some(21) }));
    assert!(wait_until(Duration::from_secs(3), || a.engine.status().updates.iter().any(|p| matches!(&p.state, UpdateState::Failed { error, .. } if error == "folder is read-only"))));
    b.shutdown();
    a.shutdown();
}

#[test]
fn incompatible_audio_nodes_can_still_request_updates_and_wrong_group_nodes_cannot() {
    let node = spawn_with(Spec::new("receiver"), |_| {}, |_| {});
    node.engine.command(Command::PublishUpdate(UpdateState::Available { build: 21 }));
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let key = GroupKey::derive("test", "");
    let hello = UpdateMessage::Status { reply: false, info: UpdateInfo {
        name: "MacBook with a different audio protocol".into(), os: Os::MacOs, version: "0.1.0 (build 20)".into(),
        build: Some(20), protocol: 200, state: UpdateState::Available { build: 21 },
    } };
    let mut buf = [0; MAX_DATAGRAM];
    let n = remote_update::encode(42, 200, &hello, &key, &mut buf).unwrap();
    socket.send_to(&buf[..n], node.engine.local_addr()).unwrap();
    let mut session = None;
    assert!(wait_until(Duration::from_secs(3), || {
        if let Ok((n, _)) = socket.recv_from(&mut buf)
            && let Ok((_, s, UpdateMessage::Status { info, .. })) = remote_update::decode(&buf[..n], &key)
            && info.state == (UpdateState::Available { build: 21 })
        {
            session = Some(s);
        }
        session.is_some() && node.engine.status().updates.iter().any(|p| p.id == 42 && p.protocol == 200 && p.installable() == Some(21))
    }));
    assert!(node.engine.status().peers.is_empty(), "no compatible audio handshake was needed");
    let request = UpdateMessage::Request { target: node.engine.status().node_id, session: session.unwrap(), request: 1, action: UpdateAction::Install { build: 21 } };
    let n = remote_update::encode(42, 200, &request, &GroupKey::derive("other group", ""), &mut buf).unwrap();
    socket.send_to(&buf[..n], node.engine.local_addr()).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert!(node.engine.take_update_request().is_none());
    let n = remote_update::encode(42, 200, &request, &key, &mut buf).unwrap();
    socket.send_to(&buf[..n], node.engine.local_addr()).unwrap();
    assert!(wait_until(Duration::from_secs(3), || node.engine.take_update_request().is_some_and(|r| r.sender == 42 && r.action == UpdateAction::Install { build: 21 })));
    node.shutdown();
}

#[test]
fn legacy_protocol_mismatches_are_listed_without_claiming_remote_update_support() {
    let node = spawn_with(Spec::new("controller"), |_| {}, |_| {});
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let other = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut buf = [0; MAX_DATAGRAM];
    let n = soundsync_core::protocol::encode(42, &soundsync_core::protocol::Packet::Control(soundsync_core::protocol::Control::Ping { t1: 0 }), &GroupKey::derive("test", ""), &mut buf).unwrap();
    buf[4] = soundsync_core::protocol::PROTOCOL_VERSION - 1;
    socket.send_to(&buf[..n], node.engine.local_addr()).unwrap();
    other.send_to(&buf[..n], node.engine.local_addr()).unwrap();
    assert!(wait_until(Duration::from_secs(3), || node.engine.status().incompatible_peers.len() == 2));
    assert!(node.engine.status().updates.is_empty(), "unreadable audio is only a hint, not an authenticated update-capable device");
    node.shutdown();
}
