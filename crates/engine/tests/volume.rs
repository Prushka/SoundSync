mod common;

use std::time::Duration;

use common::*;
use soundsync_audio::fake::FakeClock;
use soundsync_audio::{AudioHost, DeviceSelector};
use soundsync_core::config::Config;
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
