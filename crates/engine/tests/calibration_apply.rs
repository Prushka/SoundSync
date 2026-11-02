mod common;

use std::net::UdpSocket;
use std::time::Duration;

use common::*;
use soundsync_core::Os;
use soundsync_core::calib::{CalibKind, CalibResult};
use soundsync_core::config::Config;
use soundsync_core::protocol::{CalibMsg, Control, Hello, MAX_DATAGRAM, Packet, encode};
use soundsync_engine::{CalibStatus, Command};

#[test]
fn apply_rejects_stale_sessions_wrong_kinds_and_duplicate_clicks() {
    let node = spawn_with(Spec::new("apply"), |c| c.playback.volume = 0.8, |_| {});
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let key = Config::load_or_create(&node.config_path()).unwrap().config.group_key();
    let send = |control| {
        let mut bytes = [0; MAX_DATAGRAM];
        let len = encode(930002, &Packet::Control(control), &key, &mut bytes).unwrap();
        peer.send_to(&bytes[..len], node.engine.local_addr()).unwrap();
    };
    send(Control::Hello(Hello { reply: false, name: "mic node".into(), os: Os::Other, app_version: "test".into(), group: "test".into(), session: 1 }));
    assert!(wait_until(Duration::from_secs(3), || node.engine.status().peers.len() == 1));
    let result = CalibResult {
        node: node.engine.status().node_id, delta_ns: 120 * MS, snr_db: 40.0, spread_ns: 0,
        heard: 3, sweeps: 3, level_db: 6.0, peak_dbfs: -20.0, level_spread_db: 0.0,
    };
    let deliver = |session, kind| {
        send(Control::Calib(CalibMsg::Result { session, kind, results: vec![result.clone()], check: false, error: None }));
        assert!(wait_until(Duration::from_secs(2), || matches!(node.engine.status().calibration,
            CalibStatus::Done { session: s, kind: k, .. } if s == session && k == kind)));
    };
    deliver(20, CalibKind::Volume);
    let shown = node.engine.status(); // the UI holds this snapshot until its next refresh
    deliver(19, CalibKind::Latency); // a delayed packet replaces the result before the click
    let CalibStatus::Done { session, kind, .. } = shown.calibration else { unreachable!() };
    let unchanged = std::fs::read(node.config_path()).unwrap();
    let rejected = || node.engine.status().warnings.iter().any(|w| w.contains("changed before Apply"));
    for command in [
        Command::ApplyCalibration { session, kind },
        Command::ApplyCalibration { session: 19, kind: CalibKind::Volume }, // wrong kind alone
        Command::ApplyCalibration { session: 20, kind: CalibKind::Latency }, // wrong session alone
    ] {
        node.engine.command(command);
        assert!(wait_until(Duration::from_secs(2), rejected));
        let status = node.engine.status();
        assert_eq!((status.extra_latency_ns, status.volume), (0, 0.8));
        assert!(matches!(status.calibration, CalibStatus::Done { session: 19, kind: CalibKind::Latency, .. }));
        assert_eq!(std::fs::read(node.config_path()).unwrap(), unchanged, "rejection must not write settings");
        node.engine.command(Command::CancelCalibration);
        assert!(wait_until(Duration::from_secs(2), || node.engine.status().calibration == CalibStatus::Idle && !rejected()));
        deliver(19, CalibKind::Latency);
    }
    node.engine.command(Command::ApplyCalibration { session: 19, kind: CalibKind::Latency });
    assert!(wait_until(Duration::from_secs(2), || node.engine.status().calibration == CalibStatus::Idle));
    assert_eq!((node.engine.status().extra_latency_ns, node.engine.status().volume), (120 * MS, 0.8));
    node.engine.command(Command::ApplyCalibration { session: 19, kind: CalibKind::Latency });
    assert!(wait_until(Duration::from_secs(2), rejected));
    assert_eq!(node.engine.status().extra_latency_ns, 120 * MS, "a duplicate click must not add the offset twice");
    deliver(20, CalibKind::Volume);
    node.engine.command(Command::ApplyCalibration { session: 20, kind: CalibKind::Volume });
    assert!(wait_until(Duration::from_secs(2), || node.engine.status().calibration == CalibStatus::Idle && !rejected()));
    assert_eq!(node.engine.status().extra_latency_ns, 120 * MS);
    assert!((node.engine.status().volume - 0.8 * 10f32.powf(-6.0 / 20.0)).abs() < 0.001);
    node.shutdown();
}
