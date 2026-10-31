mod common;

use std::thread::sleep;
use std::time::Duration;

use common::{Spec, spawn, spawn_linked, spawn_with, wait_until};
use soundsync_audio::AudioHost;
use soundsync_audio::fake::FakeClock;
use soundsync_core::config::Config;

#[test]
fn two_engines_handshake_and_sync_clocks() {
    let a = spawn("a", FakeClock { offset_ns: 0, skew_ppm: 0.0 }, |_| {});
    let b = spawn_linked("b", FakeClock { offset_ns: 3_700_000, skew_ppm: 60.0 }, &[&a], |_| {});
    assert!(wait_until(Duration::from_secs(6), || a
        .engine
        .status()
        .peers
        .iter()
        .any(|p| p.name == "b" && p.clock_offset_ns.is_some())));
    // b also learned about a (from a's HELLO reply), without a static entry.
    assert!(wait_until(Duration::from_secs(3), || b.engine.status().peers.iter().any(|p| p.name == "a")));
    sleep(Duration::from_secs(3));
    let off = a.engine.status().peers[0].clock_offset_ns.unwrap();
    let truth = (b.host.now_ns() - a.host.now_ns()) as f64;
    assert!((off - truth).abs() < 100_000.0, "offset {off} vs truth {truth}");
    let rtt = a.engine.status().peers[0].rtt_ns.unwrap();
    assert!((0..10_000_000).contains(&rtt), "loopback rtt {rtt}");
    b.shutdown();
    a.shutdown();
}

#[test]
fn mismatched_group_key_never_peers() {
    let a = spawn_with(Spec::new("a"), |c: &mut Config| c.network.group = "home".into(), |_| {});
    let b = spawn_with(Spec { peers: &[&a], ..Spec::new("b") }, |c: &mut Config| c.network.group = "office".into(), |_| {});
    sleep(Duration::from_secs(3));
    assert!(a.engine.status().peers.is_empty());
    assert!(b.engine.status().peers.is_empty());
    b.shutdown();
    a.shutdown();
}

#[test]
fn duplicate_node_id_regenerates() {
    let a = spawn_with(Spec::new("a"), |c: &mut Config| c.node.id = 4242, |_| {});
    let b = spawn_with(Spec { peers: &[&a], ..Spec::new("b") }, |c: &mut Config| c.node.id = 4242, |_| {});
    assert!(wait_until(Duration::from_secs(4), || a.engine.status().node_id != b.engine.status().node_id));
    let warned = |n: &common::TestNode| n.engine.status().warnings.iter().any(|w| w.contains("duplicate node id"));
    assert!(wait_until(Duration::from_secs(2), || warned(&a) || warned(&b)));
    assert!(!(warned(&a) && warned(&b)), "only the node that regenerated warns");
    let changed = if warned(&a) { &a } else { &b };
    let on_disk = Config::load_or_create(&changed.config_path()).unwrap().config.node.id;
    assert_eq!(on_disk, changed.engine.status().node_id);
    assert_ne!(on_disk, 4242);
    // And they peer normally afterwards.
    assert!(wait_until(Duration::from_secs(4), || a.engine.status().peers.len() == 1 && b.engine.status().peers.len() == 1));
    b.shutdown();
    a.shutdown();
}

#[test]
fn peer_expires_after_5s() {
    let a = spawn("a", FakeClock::default(), |_| {});
    let b = spawn_linked("b", FakeClock::default(), &[&a], |_| {});
    assert!(wait_until(Duration::from_secs(4), || a.engine.status().peers.len() == 1));
    b.shutdown();
    assert!(wait_until(Duration::from_millis(6500), || a.engine.status().peers.is_empty()));
    a.shutdown();
}
