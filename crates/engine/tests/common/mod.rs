//! Helpers for engine integration tests: in-process nodes on 127.0.0.1 backed by FakeHosts.
#![allow(dead_code)]

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use soundsync_audio::fake::{FakeClock, FakeHost};
use soundsync_core::config::Config;
use soundsync_engine::{Engine, EngineOptions, NetImpair};

pub struct TestNode {
    pub engine: Engine,
    pub host: Arc<FakeHost>,
    pub dir: PathBuf,
}

impl TestNode {
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    pub fn shutdown(self) {
        let TestNode { engine, dir, .. } = self;
        engine.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}

pub struct Spec<'a> {
    pub name: &'a str,
    pub clock: FakeClock,
    pub peers: &'a [&'a TestNode],
    pub impair: Option<NetImpair>,
    pub default_devices: Option<Arc<dyn soundsync_engine::defaults::DefaultDeviceApi>>,
}

impl<'a> Spec<'a> {
    pub fn new(name: &'a str) -> Self {
        Spec { name, clock: FakeClock::default(), peers: &[], impair: None, default_devices: None }
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("soundsync-engine-test-{}-{name}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts a node; `configure` edits its config, `setup` adds fake devices before start.
pub fn spawn_with(spec: Spec, configure: impl FnOnce(&mut Config), setup: impl FnOnce(&FakeHost)) -> TestNode {
    let host = FakeHost::new(spec.clock);
    setup(&host);
    let dir = scratch_dir(spec.name);
    let mut config = Config::default();
    config.regenerate_id();
    config.node.name = spec.name.to_string();
    config.network.group = "test".into();
    config.network.port = 0;
    config.playback.output_device = "null".into();
    config.network.static_peers = spec.peers.iter().map(|p| p.engine.local_addr().to_string()).collect();
    configure(&mut config);
    let config_path = dir.join("config.toml");
    config.save(&config_path).unwrap();
    let engine = Engine::start(EngineOptions {
        config,
        config_path: Some(config_path),
        state_path: Some(dir.join("state.toml")),
        host: host.clone(),
        enable_mdns: false,
        bind_ip: Some(Ipv4Addr::LOCALHOST),
        impair: spec.impair,
        default_devices: spec.default_devices,
    })
    .unwrap();
    TestNode { engine, host, dir }
}

pub fn spawn(name: &str, clock: FakeClock, setup: impl FnOnce(&FakeHost)) -> TestNode {
    spawn_with(Spec { clock, ..Spec::new(name) }, |_| {}, setup)
}

pub fn spawn_linked(name: &str, clock: FakeClock, peers: &[&TestNode], setup: impl FnOnce(&FakeHost)) -> TestNode {
    spawn_with(Spec { clock, peers, ..Spec::new(name) }, |_| {}, setup)
}

/// Polls `cond` every 20 ms until it holds or `timeout` passes.
pub fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

use soundsync_audio::fake::{FakeOutputSpec, FakeSignal, FakeSourceSpec, true_now};
use soundsync_core::{Nanos, StreamFormat};

pub const MS: Nanos = 1_000_000;

pub fn stereo() -> StreamFormat {
    StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 0x3 }
}

pub fn pulse_source(name: &str, probe_offset_ns: Nanos) -> FakeSourceSpec {
    FakeSourceSpec {
        name: name.into(),
        format: stereo(),
        ppm: 0.0,
        signal: FakeSignal::Pulses { every_ms: 200, amplitude: 0.8 },
        probe_offset_ns,
        hidden_latency_ns: 0,
    }
}

pub fn output(name: &str, ppm: f64, reported_ms: i64, hidden_ms: i64) -> FakeOutputSpec {
    FakeOutputSpec {
        name: name.into(),
        format: stereo(),
        ppm,
        reported_latency_ns: reported_ms * MS,
        hidden_latency_ns: hidden_ms * MS,
        period_ns: 10 * MS,
    }
}

/// Seconds since `t0` (true time) as true time.
pub fn at(t0: Nanos, secs: f64) -> Nanos {
    t0 + (secs * 1e9) as Nanos
}

pub fn now() -> Nanos {
    true_now()
}

/// Group delay of a master, sampled every 20 ms while a test waits. Auto D may step up mid-test
/// when a scheduling hiccup makes a receiver late (by design), so sync checks use every value
/// that was in effect rather than one read at the end.
pub struct DelayLog(Vec<(Nanos, Nanos)>);

impl DelayLog {
    pub fn record(node: &TestNode, dur: Duration) -> Self {
        let end = Instant::now() + dur;
        let mut samples = Vec::new();
        while Instant::now() < end {
            if let Some(d) = node.engine.status().delay_ns {
                samples.push((now(), d));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Self(samples)
    }

    /// Every group delay in effect while pulses captured in `[from, to)` played.
    pub fn during(&self, from: Nanos, to: Nanos) -> Vec<Nanos> {
        let max_d = self.0.iter().map(|s| s.1).max().unwrap_or(0);
        let mut ds: Vec<Nanos> = self.0.iter().filter(|(t, _)| (from..to + max_d + 100 * MS).contains(t)).map(|s| s.1).collect();
        ds.extend(self.0.iter().rev().find(|(t, _)| *t < from).map(|s| s.1));
        ds.sort_unstable();
        ds.dedup();
        ds
    }
}

/// Deviation of the onset nearest to `capture + d + extra` over the candidate delays.
fn nearest_deviation(onsets: &[Nanos], p: Nanos, delays: &[Nanos], extra: Nanos) -> Option<Nanos> {
    delays.iter().filter_map(|d| onsets.iter().map(|o| o - (p + d + extra)).min_by_key(|x| x.abs())).min_by_key(|x| x.abs())
}

/// Every pulse `source` generated with true capture time in `[from, to)` must appear on each
/// output within `tol` of `capture + d + extra` for one of the group delays `delays` in effect
/// (outputs: node, device, expected extra delay). With `allow_missing`, up to 20% of pulses may
/// be absent (lossy network: a pulse lives in a single packet).
#[allow(clippy::too_many_arguments)] // test assertion helper: every argument is part of the check
pub fn assert_in_sync(master: &TestNode, source: &str, outputs: &[(&TestNode, &str, Nanos)], delays: &[Nanos], from: Nanos, to: Nanos, tol: Nanos, allow_missing: bool) {
    assert!(!delays.is_empty(), "no group delay");
    let pulses: Vec<Nanos> = master.host.source_pulses(source).into_iter().filter(|p| (from..to).contains(p)).collect();
    assert!(pulses.len() >= 5, "only {} pulses in the window", pulses.len());
    let delays_ms: Vec<Nanos> = delays.iter().map(|d| d / MS).collect();
    if std::env::var("SS_DEBUG").is_ok() {
        for (node, device, extra) in outputs {
            let onsets = node.host.onsets(device);
            let devs: Vec<i64> = pulses.iter().map(|p| nearest_deviation(&onsets, *p, delays, *extra).unwrap_or(i64::MAX) / 1000).collect();
            eprintln!("DBG {device}: D={delays_ms:?} ms, deviations (us) {devs:?}");
            eprintln!("DBG {device}: streams {:?}", node.engine.status().streams);
        }
    }
    for (node, device, extra) in outputs {
        let onsets = node.host.onsets(device);
        let mut missing = 0;
        for p in &pulses {
            match nearest_deviation(&onsets, *p, delays, *extra) {
                Some(dev) if dev.abs() < 30 * MS => {
                    assert!(dev.abs() < tol, "{device}: pulse captured at {p} played {} us off (D {delays_ms:?} ms)", dev / 1000);
                }
                _ => missing += 1,
            }
        }
        let max_missing = if allow_missing { pulses.len() / 5 } else { 0 };
        assert!(missing <= max_missing, "{device}: {missing} of {} pulses missing", pulses.len());
    }
}
