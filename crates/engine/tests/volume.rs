mod common;

use std::time::Duration;

use common::*;
use soundsync_audio::fake::FakeClock;
use soundsync_audio::{AudioHost, DeviceSelector};
use soundsync_core::config::Config;
use soundsync_core::protocol::VolumeChange;
use soundsync_engine::Command;

fn tv() -> DeviceSelector {
    DeviceSelector::Name("TV".into())
}

#[test]
fn volume_slider_is_the_output_devices_volume() {
    let n = spawn_with(Spec::new("n"), |c: &mut Config| c.playback.output_device = "TV".into(), |h| h.add_output(output("TV", 0.0, 10, 0)));
    assert!(wait_until(Duration::from_secs(2), || n.engine.status().output.is_some()));
    // SoundSync's slider moves the device's own (Windows) volume...
    n.engine.command(Command::SetVolume(0.3));
    assert!(wait_until(Duration::from_secs(1), || (n.host.endpoint_volume(&tv()).unwrap().scalar - 0.3).abs() < 1e-6));
    assert!(wait_until(Duration::from_secs(1), || (n.engine.status().volume - 0.3).abs() < 1e-6));
    // ...and SoundSync shows changes made elsewhere (media keys, Sound settings).
    n.host.set_endpoint_volume(&tv(), 0.7).unwrap();
    assert!(wait_until(Duration::from_secs(1), || (n.engine.status().volume - 0.7).abs() < 1e-6), "{}", n.engine.status().volume);
    n.engine.command(Command::SetMuted(true));
    assert!(wait_until(Duration::from_secs(1), || n.host.endpoint_volume(&tv()).unwrap().muted));
    assert!(wait_until(Duration::from_secs(1), || n.engine.status().muted));
    n.host.set_endpoint_mute(&tv(), false).unwrap();
    assert!(wait_until(Duration::from_secs(1), || !n.engine.status().muted));
    // No separate SoundSync level on top: the saved software level stays at 100%.
    let saved = Config::parse(&std::fs::read(n.config_path()).unwrap()).unwrap().0;
    assert_eq!((saved.playback.volume, saved.playback.muted), (1.0, false));
    n.shutdown();
}

#[test]
fn null_output_keeps_a_software_volume() {
    let n = spawn("n", FakeClock::default(), |_| {}); // plays to the null output
    n.engine.command(Command::SetVolume(0.4));
    assert!(wait_until(Duration::from_secs(1), || (n.engine.status().volume - 0.4).abs() < 1e-6));
    let saved = Config::parse(&std::fs::read(n.config_path()).unwrap()).unwrap().0;
    assert_eq!(saved.playback.volume, 0.4);
    n.shutdown();
}

#[test]
fn receivers_follow_the_masters_volume_in_decibels() {
    // The master's CABLE Input at 50%: Windows plays that 10.5 dB quieter (gain 0.30), not at
    // half amplitude; receivers following the master must match.
    let m = spawn_with(
        Spec::new("m"),
        |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = "null".into();
        },
        |h| h.add_source(pulse_source("CABLE Input", 0)),
    );
    let dev = "Laptop".to_string();
    let r = spawn_with(Spec { peers: &[&m], ..Spec::new("r") }, move |c: &mut Config| c.playback.output_device = dev, |h| h.add_output(output("Laptop", 0.0, 10, 0)));
    m.host.set_endpoint_volume(&DeviceSelector::Name("CABLE Input".into()), 0.5).unwrap();
    m.engine.command(Command::SetBroadcast(true));
    let gain = || r.engine.status().streams.first().map(|s| s.master_volume);
    assert!(wait_until(Duration::from_secs(5), || gain().is_some_and(|g| g < 0.99)), "{:?}", gain());
    let g = gain().unwrap();
    assert!((g - 0.297).abs() < 0.01, "followed gain {g}");
    r.shutdown();
    m.shutdown();
}

#[test]
fn any_node_changes_another_nodes_volume() {
    let a = spawn("a", FakeClock::default(), |_| {});
    let b = spawn_with(Spec { peers: &[&a], ..Spec::new("b") }, |c: &mut Config| c.playback.output_device = "TV".into(), |h| h.add_output(output("TV", 0.0, 10, 0)));
    let b_id = b.engine.status().node_id;
    let seen_by_a = || a.engine.status().peers.iter().find(|p| p.id == b_id).and_then(|p| p.volume);
    assert!(wait_until(Duration::from_secs(5), || seen_by_a().is_some_and(|v| v.device && v.volume == 1.0)), "{:?}", seen_by_a());
    // a moves b's Volume slider: b's own device volume follows, and a sees the new value.
    a.engine.command(Command::ChangeVolume { node: b_id, change: VolumeChange::Set(0.3) });
    assert!(wait_until(Duration::from_secs(2), || (b.host.endpoint_volume(&tv()).unwrap().scalar - 0.3).abs() < 1e-6));
    assert!(wait_until(Duration::from_secs(2), || seen_by_a().is_some_and(|v| (v.volume - 0.3).abs() < 1e-6)), "{:?}", seen_by_a());
    a.engine.command(Command::ChangeVolume { node: b_id, change: VolumeChange::Muted(true) });
    assert!(wait_until(Duration::from_secs(2), || b.host.endpoint_volume(&tv()).unwrap().muted));
    assert!(wait_until(Duration::from_secs(2), || seen_by_a().is_some_and(|v| v.muted)));
    // Volume matching turns b down by 6 dB from wherever it is.
    let before = b.host.endpoint_volume(&tv()).unwrap().db;
    a.engine.command(Command::ChangeVolume { node: b_id, change: VolumeChange::TurnDown { db: 6.0 } });
    assert!(wait_until(Duration::from_secs(2), || (b.host.endpoint_volume(&tv()).unwrap().db - (before - 6.0)).abs() < 0.01));
    b.shutdown();
    a.shutdown();
}

#[test]
fn turning_down_a_node_without_a_device_volume_lowers_its_soundsync_level() {
    let n = spawn("n", FakeClock::default(), |_| {}); // null output
    let id = n.engine.status().node_id;
    n.engine.command(Command::ChangeVolume { node: id, change: VolumeChange::TurnDown { db: 6.0 } });
    assert!(wait_until(Duration::from_secs(1), || (n.engine.status().volume - 0.501).abs() < 0.001), "{}", n.engine.status().volume);
    n.shutdown();
}
