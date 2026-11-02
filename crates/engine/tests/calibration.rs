mod common;

use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use common::*;
use soundsync_audio::fake::{FakeClock, FakeHost, FakeRoom};
use soundsync_audio::{AudioHost, DeviceSelector};
use soundsync_core::calib::{CalibKind, click};
use soundsync_core::protocol::InputList;
use soundsync_core::config::Config;
use soundsync_core::{Nanos, NodeId};
use soundsync_engine::{CalibStatus, Command};

const HIDDEN_MS: [i64; 3] = [0, 35, 180];

/// Node 0: master (CABLE source, monitor "Spk") with the mic; nodes 1, 2: receivers. All three
/// speakers sit in one room with the given hidden latencies.
fn room_setup(room: &Arc<FakeRoom>) -> Vec<TestNode> {
    room_setup_with(room, |_, _| {})
}

/// [`room_setup`], with `extra(i, host)` adding to node i's devices.
fn room_setup_with(room: &Arc<FakeRoom>, extra: impl Fn(usize, &FakeHost)) -> Vec<TestNode> {
    let mut nodes: Vec<TestNode> = Vec::new();
    for (i, hidden) in HIDDEN_MS.iter().enumerate() {
        let room = room.clone();
        let extra = &extra;
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
                extra(i, h);
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

fn apply_calibration(n: &TestNode, expected: CalibKind) {
    let CalibStatus::Done { session, kind, .. } = n.engine.status().calibration else { panic!("no result to apply") };
    assert_eq!(kind, expected);
    n.engine.command(Command::ApplyCalibration { session, kind });
}

/// Whether a speaker underran since true time `since`. A machine that stalls a speaker's thread
/// past its buffer (CI runners do) cuts a hole into a sweep or delays its rest, which the run
/// rightly reports as a doubtful sweep: only results that survive one bad sweep can be checked.
fn speakers_underran_since(nodes: &[TestNode], since: Nanos) -> bool {
    nodes.iter().any(|n| n.host.underruns("Spk").iter().any(|t| *t >= since))
}

#[test]
fn calibration_recovers_hidden_latencies() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    let mic_node = id(&nodes[0]);
    let started = now();
    nodes[0].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), kind: CalibKind::Latency, check: false });
    assert!(wait_until(Duration::from_secs(4), || matches!(nodes[1].engine.status().calibration, CalibStatus::Running { .. })
        || nodes[1].engine.status().streams_paused));
    assert!(wait_until(Duration::from_secs(16), || matches!(nodes[0].engine.status().calibration, CalibStatus::Done { .. })), "{:?}", nodes[0].engine.status().calibration);
    // Three sweeps per device, all heard clearly and consistently.
    let CalibStatus::Done { results, check, .. } = nodes[0].engine.status().calibration.clone() else { unreachable!() };
    assert!(!check);
    assert_eq!(results.len(), 3);
    if !speakers_underran_since(&nodes, started) {
        for r in &results {
            assert_eq!((r.heard, r.sweeps), (3, 3), "{r:?}");
            assert!(r.snr_db >= 30.0 && r.spread_ns < 500_000 && r.problems(CalibKind::Latency).is_empty(), "{r:?}");
        }
    }
    apply_calibration(&nodes[0], CalibKind::Latency);
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
    let started = now();
    nodes[0].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), kind: CalibKind::Latency, check: true });
    assert!(wait_until(Duration::from_secs(10), || matches!(nodes[0].engine.status().calibration, CalibStatus::Done { check: true, .. })), "{:?}", nodes[0].engine.status().calibration);
    let CalibStatus::Done { results, .. } = nodes[0].engine.status().calibration.clone() else { unreachable!() };
    for r in &results {
        assert_eq!(r.sweeps, 1);
        assert!(r.delta_ns.abs() <= 500_000 || speakers_underran_since(&nodes, started), "left over after calibration: {r:?}");
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

fn spk() -> DeviceSelector {
    DeviceSelector::Name("Spk".into())
}

fn run_calibration(nodes: &[TestNode], kind: CalibKind, check: bool) -> Vec<soundsync_core::calib::CalibResult> {
    let mic_node = id(&nodes[0]);
    let previous = match nodes[0].engine.status().calibration { CalibStatus::Done { session, .. } => Some(session), _ => None };
    nodes[0].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), kind, check });
    let done = || matches!(&nodes[0].engine.status().calibration, CalibStatus::Done { session, kind: k, check: c, .. } if *c == check && *k == kind && Some(*session) != previous);
    assert!(wait_until(Duration::from_secs(20), done), "{:?}", nodes[0].engine.status().calibration);
    let CalibStatus::Done { results, .. } = nodes[0].engine.status().calibration.clone() else { unreachable!() };
    results
}

#[test]
fn volume_calibration_matches_loudness_without_changing_latencies() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    // Device volumes 60%, 80%, 100%: -7.76, -3.39 and 0 dB on the fake volume curve.
    for (n, v) in nodes.iter().zip([0.6, 0.8, 1.0]) {
        n.host.set_endpoint_volume(&spk(), v).unwrap();
    }
    let offsets = [5 * MS, 15 * MS, 50 * MS];
    for (n, ns) in nodes.iter().zip(offsets) {
        n.engine.command(Command::SetExtraLatency { node: id(n), ns });
    }
    assert!(wait_until(Duration::from_secs(3), || nodes.iter().map(extra_of).eq(offsets)));
    let db = |n: &TestNode| n.host.endpoint_volume(&spk()).unwrap().db;
    let want: Vec<f32> = nodes.iter().map(|n| db(n) - db(&nodes[0])).collect();
    let results = run_calibration(&nodes, CalibKind::Volume, false);
    assert!(results.iter().any(|r| r.delta_ns.abs() > MS), "there must be timing errors to leave alone");
    for (n, want) in nodes.iter().zip(&want) {
        let r = results.iter().find(|r| r.node == id(n)).unwrap();
        assert!((r.level_db - want).abs() < 0.3, "{}: level {} dB, want {want} dB", n.engine.status().name, r.level_db);
    }
    apply_calibration(&nodes[0], CalibKind::Volume);
    let quietest = db(&nodes[0]);
    assert!(wait_until(Duration::from_secs(3), || nodes.iter().all(|n| (db(n) - quietest).abs() < 0.3)), "{:?}", nodes.iter().map(db).collect::<Vec<_>>());
    sleep(Duration::from_millis(500));
    // A check now finds the speakers equally loud.
    let started = now();
    assert_eq!(nodes.iter().map(extra_of).collect::<Vec<_>>(), offsets);
    for r in run_calibration(&nodes, CalibKind::Volume, true) {
        assert!(r.level_db < 0.5 || speakers_underran_since(&nodes, started), "still louder after matching: {r:?}");
    }
    apply_calibration(&nodes[0], CalibKind::Volume);
    assert!(wait_until(Duration::from_secs(3), || nodes[0].engine.status().calibration == CalibStatus::Idle));
    for (n, expected) in nodes.iter().zip(offsets) {
        assert_eq!(extra_of(n), expected, "volume Check/Apply must also leave latency alone");
        assert_eq!(Config::load_or_create(&n.config_path()).unwrap().config.extra_latency_ns(), expected);
    }
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[test]
fn latency_calibration_leaves_volumes_alone() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    nodes[2].host.set_endpoint_volume(&spk(), 0.5).unwrap();
    let results = run_calibration(&nodes, CalibKind::Latency, false);
    assert!(results.iter().any(|r| r.level_db > 5.0), "{results:?}");
    apply_calibration(&nodes[0], CalibKind::Latency);
    assert!(wait_until(Duration::from_secs(3), || extra_of(&nodes[2]) != 0));
    sleep(Duration::from_millis(300));
    assert_eq!(nodes.iter().map(|n| n.host.endpoint_volume(&spk()).unwrap().scalar).collect::<Vec<_>>(), vec![1.0, 1.0, 0.5]);
    run_calibration(&nodes, CalibKind::Latency, true);
    apply_calibration(&nodes[0], CalibKind::Latency);
    assert!(wait_until(Duration::from_secs(3), || nodes[0].engine.status().calibration == CalibStatus::Idle));
    assert_eq!(nodes.iter().map(|n| n.host.endpoint_volume(&spk()).unwrap().scalar).collect::<Vec<_>>(), vec![1.0, 1.0, 0.5]);
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[test]
fn a_speaker_that_sleeps_after_silence_still_plays_its_first_sweep() {
    // Like the LG TV: after a while of digital silence it mutes, and the first 400 ms of sound
    // after that are lost. Calibration must wake it before the first sweep.
    let room = FakeRoom::new(48_000);
    let nodes = room_setup_with(&room, |i, h| {
        if i == 1 {
            h.make_sleepy("Spk", 4_000 * MS, 400 * MS);
        }
    });
    sleep(Duration::from_millis(4_500)); // silent long enough to fall asleep
    let results = run_calibration(&nodes, CalibKind::Latency, false);
    for r in &results {
        assert_eq!((r.heard, r.sweeps), (3, 3), "{r:?}");
    }
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[test]
fn remote_microphones_load_and_refresh_without_ui_commands() {
    let a = spawn("a", FakeClock::default(), |_| {});
    let b = spawn_linked("b", FakeClock::default(), &[&a], |h| {
        h.add_mic("USB Mic", 0);
        h.add_mic("Line In", 0);
    });
    let b_id = id(&b);
    assert!(wait_until(Duration::from_secs(5), || a.engine.status().peers.iter().any(|p| p.id == b_id)));
    let inputs = || a.engine.status().peers.iter().find(|p| p.id == b_id).and_then(|p| p.inputs.clone());
    assert!(wait_until(Duration::from_secs(3), || inputs().is_some()));
    assert_eq!(inputs().unwrap(), InputList { default: Some("USB Mic".into()), devices: vec!["USB Mic".into(), "Line In".into()] });
    assert!(wait_until(Duration::from_secs(3), || b.engine.status().peers.iter().any(|p| p.id == id(&a) && p.inputs == Some(InputList::default()))));
    // Even a host without device notifications is refreshed, with no visible UI or selection.
    b.host.add_mic("New Mic", 0);
    assert!(wait_until(Duration::from_secs(7), || inputs().is_some_and(|list| list.devices.contains(&"New Mic".to_string()))));
    b.shutdown();
    a.shutdown();
}

#[test]
fn remote_microphone_discovery_retries_after_packet_loss() {
    let a = spawn("a", FakeClock::default(), |_| {});
    let b = spawn_with(
        Spec { peers: &[&a], impair: Some(soundsync_engine::NetImpair { loss: 0.5, seed: 41, ..Default::default() }), ..Spec::new("b") },
        |_| {},
        |h| h.add_mic("USB Mic", 0),
    );
    assert!(wait_until(Duration::from_secs(10), || a.engine.status().peers.iter().any(|p| {
        p.id == id(&b) && p.inputs.as_ref().is_some_and(|list| list.devices == ["USB Mic"])
    })), "discovery must retry without any UI commands");
    b.shutdown();
    a.shutdown();
}

#[test]
fn calibration_fails_cleanly_without_signal() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    nodes[2].engine.command(Command::SetVolume(0.0));
    let mic_node = id(&nodes[0]);
    nodes[0].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), kind: CalibKind::Latency, check: false });
    assert!(wait_until(Duration::from_secs(16), || matches!(nodes[0].engine.status().calibration, CalibStatus::Failed { .. })));
    match &nodes[0].engine.status().calibration {
        CalibStatus::Failed { message, .. } => assert!(message.contains("no signal from n2"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(wait_until(Duration::from_secs(3), || nodes.iter().all(|n| !n.engine.status().streams_paused)));
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[test]
fn remote_mic_node_preserves_volume_calibration_kind() {
    let room = FakeRoom::new(48_000);
    let nodes = room_setup(&room);
    let mic_node = id(&nodes[0]);
    nodes[2].host.set_endpoint_volume(&spk(), 0.5).unwrap();
    nodes[2].engine.command(Command::StartCalibration { mic_node, mic_device: "Mic".into(), kind: CalibKind::Volume, check: false });
    assert!(wait_until(Duration::from_secs(18), || nodes.iter().all(|n| matches!(n.engine.status().calibration, CalibStatus::Done { kind: CalibKind::Volume, .. }))), "{:?}", nodes[2].engine.status().calibration);
    apply_calibration(&nodes[2], CalibKind::Volume);
    let quietest = nodes[2].host.endpoint_volume(&spk()).unwrap().db;
    assert!(wait_until(Duration::from_secs(3), || nodes.iter().all(|n| (n.host.endpoint_volume(&spk()).unwrap().db - quietest).abs() < 0.3)));
    assert!(nodes.iter().all(|n| extra_of(n) == 0));
    for n in nodes.into_iter().rev() {
        n.shutdown();
    }
}

#[test]
fn remote_microphone_open_failure_reaches_the_requester() {
    let a = spawn("a", FakeClock::default(), |_| {});
    let b = spawn_linked("b", FakeClock::default(), &[&a], |_| {});
    assert!(wait_until(Duration::from_secs(5), || a.engine.status().peers.len() == 1));
    a.engine.command(Command::StartCalibration { mic_node: id(&b), mic_device: "Missing Mic".into(), kind: CalibKind::Volume, check: false });
    assert!(wait_until(Duration::from_secs(3), || matches!(&a.engine.status().calibration,
        CalibStatus::Failed { kind: CalibKind::Volume, message } if message.contains("cannot open microphone"))));
    assert!(!a.engine.status().streams_paused && !b.engine.status().streams_paused);
    b.shutdown();
    a.shutdown();
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

