mod common;

use std::thread::sleep;
use std::time::Duration;

use common::*;
use soundsync_audio::fake::{FakeClock, FakeSignal, FakeSourceSpec};
use soundsync_core::config::Config;
use soundsync_core::{MasterMode, StreamFormat};
use soundsync_engine::Command;

fn virtual_master(name: &str, monitor: &str) -> TestNode {
    spawn_with(
        Spec::new(name),
        |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = monitor.into();
        },
        |h| {
            h.add_source(pulse_source("CABLE Input", 0));
            h.add_output(output("TV", 50.0, 15, 0));
        },
    )
}

fn receiver(name: &str, clock: FakeClock, master: &TestNode, device: &str, ppm: f64, reported_ms: i64, impair: Option<soundsync_engine::NetImpair>) -> TestNode {
    let dev = device.to_string();
    spawn_with(
        Spec { clock, peers: &[master], impair, ..Spec::new(name) },
        move |c: &mut Config| c.playback.output_device = dev,
        |h| h.add_output(output(device, ppm, reported_ms, 0)),
    )
}

#[test]
fn virtual_mode_three_outputs_in_sync() {
    let t0 = now();
    let m = virtual_master("m", "TV");
    let r1 = receiver("r1", FakeClock::default(), &m, "Laptop", -120.0, 40, None);
    let r2 = receiver("r2", FakeClock { offset_ns: 7 * MS, skew_ppm: 0.0 }, &m, "Speaker", 80.0, 5, None);
    m.engine.command(Command::SetBroadcast(true));
    sleep(Duration::from_secs(6));
    let d = m.engine.status().delay_ns.expect("group delay chosen");
    let delays = DelayLog::record(&m, Duration::from_secs(4));
    let st = m.engine.status();
    assert!(st.broadcasting);
    // A brief timer probe at the end cannot prove that capture and device threads stayed
    // punctual throughout this window. Auto delay may grow with their requirements, but
    // must not shrink. Stability for unchanged requirements is tested in DelayPolicy.
    assert!(st.delay_ns.expect("broadcast has a group delay") >= d, "auto delay must not shrink within a stream");
    assert_eq!(d % (5 * MS), 0);
    assert!(d >= 45 * MS, "D {} ms", d / MS);
    let outputs = [(&m, "TV", 0), (&r1, "Laptop", 0), (&r2, "Speaker", 0)];
    assert_in_sync(&m, "CABLE Input", &outputs, &delays.during(at(t0, 6.5), at(t0, 9.5)), at(t0, 6.5), at(t0, 9.5), MS, false);
    r2.shutdown();
    r1.shutdown();
    m.shutdown();
}

#[test]
fn stream_start_delay_covers_known_receivers() {
    // Receivers subscribe only after hearing about the new stream; the first group delay must
    // already cover them, or they play behind until the next gossip round raises it.
    let m = virtual_master("m", "TV");
    let slow = receiver("slow", FakeClock::default(), &m, "Bluetooth", 0.0, 100, None);
    assert!(wait_until(Duration::from_secs(5), || m.engine.status().peers.iter().any(|p| p.output.is_some())), "the receiver's state arrived");
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(1), || m.engine.status().delay_ns.is_some()));
    let first = m.engine.status().delay_ns.unwrap();
    assert!(first >= 100 * MS, "initial D {} ms ignores the 100 ms receiver", first / MS);
    slow.shutdown();
    m.shutdown();
}

/// Whether this machine's timers fire on time. Tests that compare exact group delays need it:
/// the very first stream's delay is estimated assuming the master captures on the usual ~10 ms
/// cadence, and measured requirements stay steady. Where timers oversleep (GitHub's macOS runners,
/// SS_FAKE_MIN_SLEEP_MS) capture arrives late, the first stream legitimately needs one delay
/// increase, and requirements drift a step or two at any moment.
fn timers_are_punctual() -> bool {
    if std::env::var_os("SS_FAKE_MIN_SLEEP_MS").is_some() {
        return false;
    }
    let t = std::time::Instant::now();
    for _ in 0..10 {
        sleep(Duration::from_millis(2));
    }
    t.elapsed() < Duration::from_millis(60)
}

#[test]
fn first_stream_starts_with_the_delay_the_receivers_need() {
    // Before a receiver has played anything it can only estimate its requirement; the estimate
    // must not undershoot what it then measures, or the very first broadcast starts behind.
    if !timers_are_punctual() {
        eprintln!("skipped: this machine's timers oversleep, so capture cadence differs from the estimate's assumption");
        return;
    }
    let m = virtual_master("m", "TV");
    let r1 = receiver("r1", FakeClock::default(), &m, "Laptop", 0.0, 40, None);
    assert!(wait_until(Duration::from_secs(5), || m.engine.status().peers.iter().any(|p| p.output.is_some())));
    sleep(Duration::from_millis(1500)); // r1's state with its output reached the master
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(1), || m.engine.status().delay_ns.is_some()));
    let first = m.engine.status().delay_ns.unwrap();
    sleep(Duration::from_secs(4));
    let later = m.engine.status().delay_ns.unwrap();
    assert_eq!(later, first, "the group delay had to grow from {} ms to {} ms after the start", first / MS, later / MS);
    r1.shutdown();
    m.shutdown();
}

#[test]
fn restarted_stream_starts_with_the_delay_the_receivers_needed() {
    // Between streams a receiver has nothing to measure; it must keep reporting what it
    // measured, or a restarted broadcast begins with a too-small delay and plays behind.
    if !timers_are_punctual() {
        // Late timers make requirements drift a step or two at any moment (seen on GitHub's macOS
        // runner: the receiver's requirement rose after "settled" was read), so exact delays
        // before and after the restart cannot be compared there.
        eprintln!("skipped: this machine's timers oversleep, so requirements drift between the readings");
        return;
    }
    let m = virtual_master("m", "TV");
    let r1 = receiver("r1", FakeClock::default(), &m, "Laptop", 0.0, 40, None);
    m.engine.command(Command::SetBroadcast(true));
    sleep(Duration::from_secs(4));
    let settled = m.engine.status().delay_ns.unwrap();
    m.engine.command(Command::SetBroadcast(false));
    assert!(wait_until(Duration::from_secs(3), || r1.engine.status().streams.is_empty()));
    sleep(Duration::from_millis(1500)); // r1's next STATE goes out without a stream
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(1), || m.engine.status().delay_ns.is_some()));
    let first = m.engine.status().delay_ns.unwrap();
    assert!(first <= settled, "a restart never needs more than the last stream: {} vs {} ms", first / MS, settled / MS);
    sleep(Duration::from_secs(3));
    let later = m.engine.status().delay_ns.unwrap();
    assert_eq!(later, first, "the restarted stream's delay had to grow from {} ms to {} ms", first / MS, later / MS);
    r1.shutdown();
    m.shutdown();
}

#[test]
fn receivers_start_playing_promptly() {
    // A new stream is announced and subscribed to at once, not on the next 1 s gossip round.
    // A continuous tone lets us measure the first audio, without waiting for a 200 ms pulse.
    let m = spawn_with(
        Spec::new("m"),
        |c| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = "TV".into();
        },
        |h| {
            h.add_source(FakeSourceSpec { signal: FakeSignal::Sine { hz: 440.0, amplitude: 0.8 }, ..pulse_source("CABLE Input", 0) });
            h.add_output(output("TV", 50.0, 15, 0));
        },
    );
    let r1 = receiver("r1", FakeClock::default(), &m, "Laptop", 0.0, 10, None);
    assert!(wait_until(Duration::from_secs(3), || r1.engine.status().peers.iter().all(|p| p.clock_offset_ns.is_some()) && !r1.engine.status().peers.is_empty()));
    for round in 0..3 {
        let prior_onsets = r1.host.onsets("Laptop").len();
        let start = now();
        m.engine.command(Command::SetBroadcast(true));
        let mut first_audio = None;
        let playing = || {
            let status = r1.engine.status();
            // Behind also plays audio; Waiting does not. Timing correction can need a later
            // gossip round on a coarse timer, but it must not count as delayed subscription.
            let active = status.streams.iter().any(|s| matches!(s.state,
                soundsync_core::player::PlayState::Playing | soundsync_core::player::PlayState::Behind));
            first_audio = r1.host.onsets("Laptop").into_iter().skip(prior_onsets).find(|t| *t >= start);
            active && first_audio.is_some()
        };
        assert!(wait_until(Duration::from_secs(3), playing), "round {round}: never played");
        let took = first_audio.unwrap() - start;
        assert!(took < 600 * MS, "round {round}: first audio after {} ms", took as f64 / 1e6);
        m.engine.command(Command::SetBroadcast(false));
        assert!(wait_until(Duration::from_secs(3), || r1.engine.status().streams.is_empty()), "round {round}: stream not dropped");
    }
    r1.shutdown();
    m.shutdown();
}

#[test]
fn sync_recovers_after_a_simulated_output_stall() {
    let m = virtual_master("m", "TV");
    let r = receiver("r", FakeClock::default(), &m, "Laptop", 0.0, 10, None);
    m.engine.command(Command::SetDelay(soundsync_core::delay::DelaySetting::Manual(250.0)));
    m.engine.command(Command::SetBroadcast(true));
    sleep(Duration::from_secs(3));
    let from = now();
    let next = m.host.source_pulses("CABLE Input").last().copied().unwrap() + 200 * MS + 250 * MS;
    // Leave room for the fake device's extra buffering on coarse timers, then stall across
    // this pulse well after warm-up. Subsequent pulses must recover within the usual 1 ms.
    assert!(wait_until(Duration::from_secs(1), || now() >= next - 120 * MS));
    r.host.stall_output("Laptop", Duration::from_millis(300));
    sleep(Duration::from_secs(3));
    let gaps = r.host.output_gaps("Laptop");
    assert!(gaps.iter().any(|gap| gap.contains(&next)), "the planned pulse must fall in the gap: {gaps:?}");
    assert_in_sync(&m, "CABLE Input", &[(&r, "Laptop", 0)], &[250 * MS], from, now() - 350 * MS, MS, false);
    r.shutdown();
    m.shutdown();
}

#[test]
fn virtual_mode_survives_loss_and_reorder() {
    let t0 = now();
    let m = virtual_master("m", "TV");
    let impair = soundsync_engine::NetImpair { loss: 0.01, reorder: 0.02, jitter_ns: 2 * MS, seed: 9 };
    let r1 = receiver("r1", FakeClock::default(), &m, "Laptop", -120.0, 40, Some(impair));
    m.engine.command(Command::SetBroadcast(true));
    let delays = DelayLog::record(&m, Duration::from_secs(10));
    assert_in_sync(&m, "CABLE Input", &[(&r1, "Laptop", 0)], &delays.during(at(t0, 6.5), at(t0, 9.5)), at(t0, 6.5), at(t0, 9.5), MS, true);
    let st = r1.engine.status();
    assert_eq!(st.streams.len(), 1);
    assert!(st.streams[0].lost_frames > 0, "1% loss must show up in the stats");
    r1.shutdown();
    m.shutdown();
}

#[test]
fn direct_mode_reports_behind() {
    let t0 = now();
    // A TV-like master: Windows plays the captured audio 120 ms after its loopback timestamp (long
    // enough for the fast receiver even where simulated devices buffer ~50 ms on slow CI machines).
    let m = spawn_with(
        Spec::new("m"),
        |c: &mut Config| {
            c.master.mode = MasterMode::Direct;
            c.master.capture_device = "Speakers".into();
            c.playback.output_device = "Speakers".into();
        },
        |h| h.add_source(pulse_source("Speakers", 120 * MS)),
    );
    let fast = receiver("fast", FakeClock::default(), &m, "Kitchen", 0.0, 5, None);
    let slow = receiver("slow", FakeClock::default(), &m, "Headphones", 0.0, 200, None);
    m.engine.command(Command::SetBroadcast(true));
    sleep(Duration::from_secs(8));
    let st = m.engine.status();
    assert_eq!(st.delay_ns, Some(120 * MS), "direct mode presents at the master's own output time");
    assert!(st.warnings.iter().any(|w| w.contains("echo guard")), "{:?}", st.warnings);
    // The fast receiver matches what Windows plays on the master.
    let emitted: Vec<_> = m.host.onsets("Speakers");
    assert!(emitted.len() > 10);
    assert_in_sync(&m, "Speakers", &[(&fast, "Kitchen", 0)], &[120 * MS], at(t0, 4.5), at(t0, 7.5), MS, false);
    let behind = slow.engine.status().streams[0].behind_ns;
    assert!((80 * MS..=180 * MS).contains(&behind), "slow receiver behind {} ms", behind / MS);
    assert_eq!(fast.engine.status().streams[0].behind_ns, 0);
    slow.shutdown();
    fast.shutdown();
    m.shutdown();
}

#[test]
fn auto_delay_increases_when_slow_receiver_joins() {
    let m = virtual_master("m", "TV");
    let fast = receiver("fast", FakeClock::default(), &m, "Speaker", 0.0, 5, None);
    m.engine.command(Command::SetBroadcast(true));
    sleep(Duration::from_secs(4));
    let before = m.engine.status().delay_ns.unwrap();
    let slow = receiver("slow", FakeClock::default(), &m, "Bluetooth", 0.0, 150, None);
    assert!(wait_until(Duration::from_secs(8), || {
        let d = m.engine.status().delay_ns.unwrap();
        d > before && d >= 155 * MS
    }), "D stayed at {} ms (before {} ms)", m.engine.status().delay_ns.unwrap() / MS, before / MS);
    // The first estimate comes from the device lead; measured requirements can still grow
    // later on a slow runner. Track the whole window instead of assuming the delay settles.
    let t0 = now();
    let delays = DelayLog::record(&m, Duration::from_secs(4));
    let from = at(t0, 1.0);
    let to = now() - m.engine.status().delay_ns.unwrap() - 50 * MS;
    assert_in_sync(&m, "CABLE Input", &[(&m, "TV", 0), (&fast, "Speaker", 0), (&slow, "Bluetooth", 0)], &delays.during(from, to), from, to, MS, false);
    slow.shutdown();
    fast.shutdown();
    m.shutdown();
}

#[test]
fn master_leaving_removes_stream() {
    let m = virtual_master("m", "TV");
    let r = receiver("r", FakeClock::default(), &m, "Laptop", 0.0, 10, None);
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(6), || r.engine.status().streams.len() == 1));
    m.engine.command(Command::SetBroadcast(false));
    assert!(wait_until(Duration::from_secs(4), || r.engine.status().streams.is_empty()));
    assert!(!m.engine.status().broadcasting);
    r.shutdown();
    m.shutdown();
}

#[test]
fn remote_set_offset_applies_and_persists() {
    let m = virtual_master("m", "TV");
    let r = receiver("r", FakeClock::default(), &m, "Laptop", 0.0, 10, None);
    assert!(wait_until(Duration::from_secs(4), || m.engine.status().peers.len() == 1));
    let rid = r.engine.status().node_id;
    m.engine.command(Command::SetExtraLatency { node: rid, ns: 25 * MS });
    assert!(wait_until(Duration::from_secs(2), || r.engine.status().extra_latency_ns == 25 * MS));
    let saved = Config::load_or_create(&r.config_path()).unwrap().config;
    assert_eq!(saved.playback.extra_latency_ms, 25.0);
    // And it is gossiped back to the master.
    assert!(wait_until(Duration::from_secs(3), || m.engine.status().peers[0].extra_latency_ns == 25 * MS));
    r.shutdown();
    m.shutdown();
}

#[test]
fn layout_warning_clears_when_the_cable_is_reconfigured() {
    // Broadcasting a stereo CABLE Input to a 7.1 monitor warns; after the user sets the cable to
    // 7.1 (Windows ends the capture, SoundSync reopens it) the warning must go away.
    let seven_one = StreamFormat { sample_rate: 48_000, channels: 8, channel_mask: 0x63F };
    let m = spawn_with(
        Spec::new("m"),
        |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = "TV".into();
        },
        |h| {
            h.add_source(pulse_source("CABLE Input", 0));
            let mut tv = output("TV", 0.0, 10, 0);
            tv.format = seven_one;
            h.add_output(tv);
        },
    );
    let mismatch = |n: &TestNode| n.engine.status().warnings.iter().any(|w| w.contains("Stereo → 7.1"));
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(3), || mismatch(&m)), "{:?}", m.engine.status().warnings);
    m.host.remove_device("CABLE Input");
    m.host.set_source_format("CABLE Input", seven_one);
    m.host.readd_device("CABLE Input");
    assert!(wait_until(Duration::from_secs(3), || m.engine.status().capture.as_ref().is_some_and(|c| c.format.channels == 8)));
    sleep(Duration::from_millis(300));
    let st = m.engine.status();
    assert!(!st.warnings.iter().any(|w| w.contains(" → ")), "stale layout warning: {:?}", st.warnings);
    m.shutdown();
}

#[test]
fn layout_mismatch_warns() {
    let m = spawn_with(
        Spec::new("m"),
        |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = "null".into();
        },
        |h| {
            let mut s = pulse_source("CABLE Input", 0);
            s.format = StreamFormat { sample_rate: 48_000, channels: 6, channel_mask: 0x3F };
            h.add_source(s);
        },
    );
    let r = receiver("r", FakeClock::default(), &m, "Laptop", 0.0, 10, None);
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(6), || r.engine.status().streams.first().is_some_and(|s| s.layout_warning.is_some())));
    let st = r.engine.status();
    assert_eq!(st.streams[0].layout_warning.as_deref(), Some("5.1 → Stereo"));
    assert!(st.warnings.iter().any(|w| w.contains("5.1 → Stereo")), "{:?}", st.warnings);
    r.shutdown();
    m.shutdown();
}


#[test]
fn spoofed_subscribe_gets_no_audio() {
    // A SUBSCRIBE claiming a known peer's id from another address must not redirect the stream
    // there (it would make the master an amplifier: ~12 Mbit/s for a few bytes).
    use soundsync_core::protocol::{Control, GroupKey, MAX_DATAGRAM, Packet, encode};
    let m = virtual_master("m", "TV");
    let r1 = receiver("r1", FakeClock::default(), &m, "Laptop", 0.0, 10, None);
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(5), || r1.engine.status().streams.iter().any(|s| s.state == soundsync_core::player::PlayState::Playing)));
    let stream_id = r1.engine.status().streams[0].stream_id;
    let victim = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    victim.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let key = GroupKey::derive("test", ""); // the test nodes' group
    let mut buf = [0u8; MAX_DATAGRAM];
    let n = encode(r1.engine.status().node_id, &Packet::Control(Control::Subscribe { stream_id }), &key, &mut buf).unwrap();
    let mut received = 0;
    let end = std::time::Instant::now() + Duration::from_millis(1500);
    let mut rx = [0u8; 2048];
    while std::time::Instant::now() < end {
        victim.send_to(&buf[..n], m.engine.local_addr()).unwrap();
        while victim.recv_from(&mut rx).is_ok() {
            received += 1;
        }
    }
    assert_eq!(received, 0, "the master streamed to an address that never handshook");
    r1.shutdown();
    m.shutdown();
}

#[test]
fn oversized_datagrams_do_not_stall_the_receiver() {
    // Anyone on the LAN can send junk; datagrams larger than SoundSync ever sends must be
    // dropped without slowing down the receive loop.
    let m = virtual_master("m", "TV");
    let r1 = receiver("r1", FakeClock::default(), &m, "Laptop", 0.0, 10, None);
    m.engine.command(Command::SetBroadcast(true));
    let playing = |n: &TestNode| n.engine.status().streams.iter().any(|s| s.state == soundsync_core::player::PlayState::Playing);
    assert!(wait_until(Duration::from_secs(5), || playing(&r1)));
    sleep(Duration::from_millis(500));
    let lost_before = r1.engine.status().streams[0].lost_frames;
    let junk = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let big = vec![0u8; 8_000];
    for _ in 0..400 {
        let _ = junk.send_to(&big, r1.engine.local_addr());
        sleep(Duration::from_millis(2));
    }
    sleep(Duration::from_millis(500));
    let st = r1.engine.status();
    let lost = st.streams[0].lost_frames - lost_before;
    assert!(lost < 4_800, "{lost} frames lost while junk arrived");
    assert!(playing(&r1), "{:?}", st.streams);
    r1.shutdown();
    m.shutdown();
}

#[test]
fn a_node_running_another_version_is_reported() {
    let n = spawn("n", FakeClock::default(), |_| {});
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    // A version 1 datagram: our magic, then the old version byte.
    let mut old = b"SSYN".to_vec();
    old.push(1);
    old.extend([1u8; 40]);
    sock.send_to(&old, n.engine.local_addr()).unwrap();
    let warned = || n.engine.status().warnings.iter().any(|w| w.contains("127.0.0.1") && w.contains("different version"));
    assert!(wait_until(Duration::from_secs(3), warned), "{:?}", n.engine.status().warnings);
    n.shutdown();
}
