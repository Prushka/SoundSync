mod common;

use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use common::*;
use soundsync_audio::fake::{FakeClock, FakeHost, FakeRoom};
use soundsync_core::calib::click;
use soundsync_core::config::Config;
use soundsync_core::{Nanos, NodeId};
use soundsync_engine::{CalibStatus, Command};

const HIDDEN_MS: [i64; 3] = [0, 35, 180];

/// Node 0: master (CABLE source, monitor "Spk") with the mic; nodes 1, 2: receivers. All three
/// speakers sit in one room with the given hidden latencies.
fn room_setup(room: &Arc<FakeRoom>) -> Vec<TestNode> {
    let mut nodes: Vec<TestNode> = Vec::new();
    for (i, hidden) in HIDDEN_MS.iter().enumerate() {
        let room = room.clone();
        let hidden = *hidden;
        let peers: Vec<&TestNode> = nodes.iter().collect();
        let node = spawn_with(
            Spec { peers: &peers, ..Spec::new(["n0", "n1", "n2"][i]) },
            |c: &mut Config| {
                c.playback.output_device = "Spk".into();
                c.master.monitor_device = "Spk".into();
                c.master.capture_device = "CABLE Input".into();
            },
            move |h: &FakeHost| {
                h.attach_room(room);
                h.add_output(output("Spk", 0.0, 10, hidden));
                if i == 0 {
                    h.add_source(pulse_source("CABLE Input", 0));
                    h.add_mic("Mic", 12 * MS);
                }
            },
        );
        nodes.push(node);
    }
    assert!(wait_until(Duration::from_secs(6), || nodes.iter().all(|n| {
        let st = n.engine.status();
        st.peers.len() == 2 && st.peers.iter().all(|p| p.clock_offset_ns.is_some())
    })));
    nodes
}

fn id(n: &TestNode) -> NodeId {
    n.engine.status().node_id
}

fn extra_of(n: &TestNode) -> Nanos {
    n.engine.status().extra_latency_ns
}

#[test]
fn calibration_recovers_hidden_latencies() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    let mic_node = id(&nodes[0]);
    nodes[0].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), check: false });
    assert!(wait_until(Duration::from_secs(4), || matches!(nodes[1].engine.status().calibration, CalibStatus::Running { .. })
        || nodes[1].engine.status().streams_paused));
    assert!(wait_until(Duration::from_secs(16), || matches!(nodes[0].engine.status().calibration, CalibStatus::Done { .. })), "{:?}", nodes[0].engine.status().calibration);
    // Three sweeps per device, all heard clearly and consistently.
    let CalibStatus::Done { results, check, .. } = nodes[0].engine.status().calibration.clone() else { unreachable!() };
    assert!(!check);
    assert_eq!(results.len(), 3);
    for r in &results {
        assert_eq!((r.heard, r.sweeps), (3, 3), "{r:?}");
        assert!(r.snr_db >= 30.0 && r.spread_ns < 500_000 && r.problems().is_empty(), "{r:?}");
    }
    nodes[0].engine.command(Command::ApplyCalibration);
    assert!(wait_until(Duration::from_secs(3), || extra_of(&nodes[2]) != 0 && extra_of(&nodes[0]) != 0));
    sleep(Duration::from_millis(300));
    for a in 0..3 {
        for b in 0..3 {
            let got = extra_of(&nodes[a]) - extra_of(&nodes[b]);
            let want = (HIDDEN_MS[a] - HIDDEN_MS[b]) * MS;
            assert!((got - want).abs() <= 500_000, "E{a}-E{b} = {} ms, want {} ms", got as f64 / 1e6, want as f64 / 1e6);
        }
    }
    assert!(nodes.iter().all(|n| !n.engine.status().streams_paused));
    // A quick check (one sweep each) measures what is left: nothing worth correcting.
    nodes[0].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), check: true });
    assert!(wait_until(Duration::from_secs(10), || matches!(nodes[0].engine.status().calibration, CalibStatus::Done { check: true, .. })), "{:?}", nodes[0].engine.status().calibration);
    let CalibStatus::Done { results, .. } = nodes[0].engine.status().calibration.clone() else { unreachable!() };
    for r in &results {
        assert_eq!(r.sweeps, 1);
        assert!(r.delta_ns.abs() <= 500_000, "left over after calibration: {r:?}");
    }
    assert!(wait_until(Duration::from_secs(3), || nodes.iter().all(|n| !n.engine.status().streams_paused)));
    // With the calibrated offsets every speaker now sounds together (all h_ref = 35 ms late).
    let t0 = now();
    nodes[0].engine.command(Command::SetBroadcast(true));
    let delays = DelayLog::record(&nodes[0], Duration::from_secs(9));
    let outs: Vec<(&TestNode, &str, Nanos)> = nodes.iter().map(|n| (n, "Spk", 35 * MS)).collect();
    assert_in_sync(&nodes[0], "CABLE Input", &outs, &delays.during(at(t0, 6.0), at(t0, 8.5)), at(t0, 6.0), at(t0, 8.5), MS, false);
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[test]
fn calibration_fails_cleanly_without_signal() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    nodes[2].engine.command(Command::SetVolume(0.0));
    let mic_node = id(&nodes[0]);
    nodes[0].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), check: false });
    assert!(wait_until(Duration::from_secs(16), || matches!(nodes[0].engine.status().calibration, CalibStatus::Failed(_))));
    match &nodes[0].engine.status().calibration {
        CalibStatus::Failed(msg) => assert!(msg.contains("no signal from n2"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert!(wait_until(Duration::from_secs(3), || nodes.iter().all(|n| !n.engine.status().streams_paused)));
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[test]
fn remote_mic_node_runs_calibration() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    let mic_node = id(&nodes[0]);
    nodes[2].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), check: false });
    assert!(wait_until(Duration::from_secs(18), || matches!(nodes[2].engine.status().calibration, CalibStatus::Done { .. })), "{:?}", nodes[2].engine.status().calibration);
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[test]
fn sync_test_clicks_coincide() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    // Correct calibration by hand: E = hidden - h_ref (35 ms).
    for (i, n) in nodes.iter().enumerate() {
        nodes[0].engine.command(Command::SetExtraLatency { node: id(n), ns: (HIDDEN_MS[i] - 35) * MS });
    }
    assert!(wait_until(Duration::from_secs(3), || extra_of(&nodes[2]) == 145 * MS && extra_of(&nodes[0]) == -35 * MS));
    nodes[1].engine.command(Command::SyncTest(true));
    assert!(wait_until(Duration::from_secs(3), || nodes.iter().all(|n| n.engine.status().sync_test)));
    sleep(Duration::from_secs(5));
    nodes[1].engine.command(Command::SyncTest(false));
    assert!(wait_until(Duration::from_secs(3), || nodes.iter().all(|n| !n.engine.status().sync_test && !n.engine.status().streams_paused)));
    // Each node clicks at its own pitch; compare onsets minus the pitch's onset offset.
    let mut ids: Vec<NodeId> = nodes.iter().map(id).collect();
    ids.sort_unstable();
    let mut series: Vec<Vec<Nanos>> = Vec::new();
    for n in &nodes {
        let rank = ids.iter().position(|x| *x == id(n)).unwrap();
        let wave = click(48_000, 600.0 + 200.0 * (rank % 6) as f64);
        let first = wave.iter().position(|x| *x > 0.25).unwrap() as Nanos * 1_000_000_000 / 48_000;
        series.push(n.host.onsets("Spk").into_iter().map(|t| t - first).collect());
    }
    assert!(series.iter().all(|s| s.len() >= 3), "clicks per node: {:?}", series.iter().map(|s| s.len()).collect::<Vec<_>>());
    for t in &series[0] {
        for other in &series[1..] {
            let nearest = other.iter().min_by_key(|o| (**o - t).abs()).unwrap();
            assert!((nearest - t).abs() < MS, "click at {t} vs {nearest}: {} us apart", (nearest - t) / 1000);
        }
    }
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[allow(dead_code)]
fn unused(_: FakeClock) {}
