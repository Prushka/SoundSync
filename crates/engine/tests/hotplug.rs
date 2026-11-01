mod common;

use std::thread::sleep;
use std::time::Duration;

use common::*;
use soundsync_audio::fake::FakeClock;
use soundsync_core::Nanos;
use soundsync_core::config::Config;
use soundsync_engine::{Command, Status};

const S: Nanos = 1_000_000_000;

fn virtual_master(name: &str, monitor: &str) -> TestNode {
    spawn_with(
        Spec::new(name),
        |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = monitor.into();
        },
        |h| {
            h.add_source(pulse_source("CABLE Input", 0));
            h.add_output(output(monitor, 50.0, 15, 0));
        },
    )
}

fn receiver(name: &str, master: &TestNode, device: &str, ppm: f64, reported_ms: i64) -> TestNode {
    let dev = device.to_string();
    spawn_with(
        Spec { peers: &[master], ..Spec::new(name) },
        move |c: &mut Config| c.playback.output_device = dev,
        |h| h.add_output(output(device, ppm, reported_ms, 0)),
    )
}

/// Sleeps until `secs` after `t0` (true time).
fn sleep_until(t0: Nanos, secs: f64) {
    let left = at(t0, secs) - now();
    if left > 0 {
        sleep(Duration::from_nanos(left as u64));
    }
}

fn has_warning(st: &Status, text: &str) -> bool {
    st.warnings.iter().any(|w| w.contains(text))
}

#[test]
fn output_removed_then_readded_resumes_playback() {
    let t0 = now();
    let m = virtual_master("m", "TV");
    let r1 = receiver("r1", &m, "Laptop", -120.0, 40);
    m.engine.command(Command::SetBroadcast(true));
    sleep_until(t0, 4.0);
    r1.host.remove_device("Laptop");
    assert!(
        wait_until(Duration::from_secs(1), || has_warning(&r1.engine.status(), "Laptop disconnected — retrying")),
        "{:?}",
        r1.engine.status().warnings
    );
    assert!(r1.engine.status().output.is_none(), "no output while the only device is gone");
    sleep_until(t0, 6.0);
    r1.host.readd_device("Laptop");
    let delays = DelayLog::record(&m, Duration::from_nanos((at(t0, 9.3) - now()) as u64));
    assert_in_sync(&m, "CABLE Input", &[(&r1, "Laptop", 0)], &delays.during(at(t0, 7.5), at(t0, 9.0)), at(t0, 7.5), at(t0, 9.0), MS, false);
    let st = r1.engine.status();
    assert_eq!(st.output.as_ref().map(|o| o.name.as_str()), Some("Laptop"));
    assert!(!has_warning(&st, "disconnected"), "{:?}", st.warnings);
    assert!(has_warning(&st, "Output changed — run Sync ▸ Auto-calibrate again for best sync"), "{:?}", st.warnings);
    r1.shutdown();
    m.shutdown();
}

#[test]
fn missing_output_falls_back_to_default_and_returns() {
    let t0 = now();
    let m = virtual_master("m", "TV");
    let dev = "Kitchen".to_string();
    let r1 = spawn_with(
        Spec { peers: &[&m], ..Spec::new("r1") },
        move |c: &mut Config| c.playback.output_device = dev,
        |h| {
            h.add_output(output("Speakers", 0.0, 10, 0)); // the default (first) output
            h.add_output(output("Kitchen", 0.0, 10, 0));
        },
    );
    m.engine.command(Command::SetBroadcast(true));
    sleep_until(t0, 3.0);
    r1.host.remove_device("Kitchen");
    assert!(
        wait_until(Duration::from_secs(1), || r1.engine.status().output.as_ref().is_some_and(|o| o.name == "Speakers")),
        "plays on the default output while Kitchen is gone"
    );
    assert!(has_warning(&r1.engine.status(), "Kitchen not found — using default output"), "{:?}", r1.engine.status().warnings);
    sleep_until(t0, 4.0);
    r1.host.readd_device("Kitchen");
    assert!(
        wait_until(Duration::from_secs(1), || r1.engine.status().output.as_ref().is_some_and(|o| o.name == "Kitchen")),
        "back on Kitchen once it returns"
    );
    let st = r1.engine.status();
    assert!(!has_warning(&st, "not found"), "{:?}", st.warnings);
    assert!(has_warning(&st, "Output changed"), "{:?}", st.warnings);
    r1.shutdown();
    m.shutdown();
}

#[test]
fn capture_device_removed_stops_stream_and_recovers() {
    let t0 = now();
    let m = virtual_master("m", "TV");
    let r1 = receiver("r1", &m, "Laptop", 0.0, 10);
    m.engine.command(Command::SetBroadcast(true));
    sleep_until(t0, 4.0);
    assert_eq!(r1.engine.status().streams.len(), 1);
    m.host.remove_device("CABLE Input");
    assert!(wait_until(Duration::from_secs(3), || r1.engine.status().streams.is_empty()), "receivers drop the dead stream");
    assert!(has_warning(&m.engine.status(), "CABLE Input disconnected — retrying"), "{:?}", m.engine.status().warnings);
    sleep_until(t0, 6.0);
    m.host.readd_device("CABLE Input");
    let delays = DelayLog::record(&m, Duration::from_nanos((at(t0, 10.0) - now()) as u64));
    let st = m.engine.status();
    assert!(st.broadcasting);
    assert!(!has_warning(&st, "disconnected"), "{:?}", st.warnings);
    assert_in_sync(&m, "CABLE Input", &[(&m, "TV", 0), (&r1, "Laptop", 0)], &delays.during(at(t0, 8.0), at(t0, 9.5)), at(t0, 8.0), at(t0, 9.5), MS, false);
    r1.shutdown();
    m.shutdown();
}

#[test]
fn tick_gap_resets_clocks() {
    let t0 = now();
    let m = virtual_master("m", "TV");
    let r1 = receiver("r1", &m, "Laptop", 0.0, 10);
    let r2 = spawn_with(
        Spec { peers: &[&m], clock: FakeClock { offset_ns: 3 * MS, skew_ppm: 20.0 }, ..Spec::new("r2") },
        |c: &mut Config| c.playback.output_device = "Speaker".into(),
        |h| h.add_output(output("Speaker", 50.0, 5, 0)),
    );
    m.engine.command(Command::SetBroadcast(true));
    sleep_until(t0, 5.0);
    // r2 "sleeps": its clock leaps 30 s ahead of everyone else's.
    r2.host.jump_clock(30 * S);
    let jumped = now();
    let delays = DelayLog::record(&m, Duration::from_millis(5500));
    let window = (jumped + 3 * S, jumped + 4_500_000_000);
    assert_in_sync(&m, "CABLE Input", &[(&r1, "Laptop", 0), (&r2, "Speaker", 0)], &delays.during(window.0, window.1), window.0, window.1, MS, false);
    assert_eq!(m.engine.status().peers.len(), 2, "the master still knows both receivers");
    let st = r2.engine.status();
    assert_eq!(st.peers.len(), 1, "r2 only knows the master");
    assert!(st.peers.iter().all(|p| p.clock_offset_ns.is_some()), "r2 re-synchronised its peers' clocks");
    r2.shutdown();
    r1.shutdown();
    m.shutdown();
}

#[test]
fn device_changes_are_counted_for_the_device_lists() {
    // The UI refreshes its device pickers when this count moves.
    let n = spawn("n", FakeClock::default(), |h| {
        h.add_output(output("TV", 0.0, 10, 0));
        h.add_output(output("Laptop", 0.0, 10, 0));
    });
    sleep(Duration::from_millis(300));
    let before = n.engine.status().device_events;
    n.host.set_default_render("Laptop");
    assert!(wait_until(Duration::from_secs(2), || n.engine.status().device_events > before));
    n.shutdown();
}
