mod common;

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::*;
use soundsync_audio::fake::{FakeClock, FakeHost};
use soundsync_audio::{AudioError, AudioHost, CaptureSink, DeviceEvent, DeviceInfo, DeviceSelector, Direction, EndpointVolume, LoopbackOptions, OutputOptions, RenderCallback, Stream};
use soundsync_core::Nanos;
use soundsync_core::config::Config;
use soundsync_engine::{Command, Engine, EngineOptions};

struct InventoryHost {
    inner: Arc<FakeHost>,
    delay: Duration,
    fail: AtomicBool,
    queries: AtomicUsize,
    finished: AtomicUsize,
    opens: AtomicUsize,
}

impl InventoryHost {
    fn new(delay: Duration) -> Arc<Self> {
        let inner = FakeHost::new(FakeClock::default());
        inner.add_mic("USB Mic", 0);
        Arc::new(Self { inner, delay, fail: AtomicBool::new(false), queries: AtomicUsize::new(0), finished: AtomicUsize::new(0), opens: AtomicUsize::new(0) })
    }

    fn start(self: &Arc<Self>) -> Engine {
        let mut config = Config::default();
        config.regenerate_id();
        config.network.group = "test".into();
        config.network.port = 0;
        config.playback.output_device = "null".into();
        Engine::start(EngineOptions {
            config, config_path: None, state_path: None, host: self.clone(), enable_mdns: false,
            bind_ip: Some(Ipv4Addr::LOCALHOST), impair: None, default_devices: None,
        }).unwrap()
    }
}

impl AudioHost for InventoryHost {
    fn now_ns(&self) -> Nanos { self.inner.now_ns() }
    fn devices(&self, dir: Direction) -> Result<Vec<DeviceInfo>, AudioError> {
        if dir == Direction::Capture {
            self.queries.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.finished.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(AudioError::Os("temporary inventory failure".into()));
            }
        }
        self.inner.devices(dir)
    }
    fn open_output(&self, sel: &DeviceSelector, opts: OutputOptions, cb: Box<dyn RenderCallback>) -> Result<Stream, AudioError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.inner.open_output(sel, opts, cb)
    }
    fn open_loopback(&self, sel: &DeviceSelector, opts: LoopbackOptions, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> { self.inner.open_loopback(sel, opts, sink) }
    fn open_input(&self, sel: &DeviceSelector, sink: Box<dyn CaptureSink>) -> Result<Stream, AudioError> { self.inner.open_input(sel, sink) }
    fn probe_loopback_latency(&self, sel: &DeviceSelector) -> Result<Nanos, AudioError> { self.inner.probe_loopback_latency(sel) }
    fn endpoint_volume(&self, sel: &DeviceSelector) -> Result<EndpointVolume, AudioError> { self.inner.endpoint_volume(sel) }
    fn set_endpoint_volume(&self, sel: &DeviceSelector, value: f32) -> Result<(), AudioError> { self.inner.set_endpoint_volume(sel, value) }
    fn set_endpoint_volume_db(&self, sel: &DeviceSelector, value: f32) -> Result<(), AudioError> { self.inner.set_endpoint_volume_db(sel, value) }
    fn set_endpoint_mute(&self, sel: &DeviceSelector, value: bool) -> Result<(), AudioError> { self.inner.set_endpoint_mute(sel, value) }
    fn active_sessions(&self, sel: &DeviceSelector) -> Result<usize, AudioError> { self.inner.active_sessions(sel) }
    fn watch(&self, callback: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Result<Box<dyn Send>, AudioError> { self.inner.watch(callback) }
}

fn observer(name: &str, engine: &Engine) -> TestNode {
    spawn_with(Spec::new(name), |c| c.network.static_peers = vec![engine.local_addr().to_string()], |_| {})
}

#[test]
fn slow_inventory_does_not_block_clocks_commands_or_reset_audio() {
    // Longer than the 2 s resume detector threshold: enumeration on the engine thread used
    // to reset clocks and reopen output here, merely because another node polled its inputs.
    let host = InventoryHost::new(Duration::from_millis(2250));
    let engine = host.start();
    let b = observer("b", &engine);
    let c = observer("c", &engine);
    assert!(wait_until(Duration::from_secs(2), || host.queries.load(Ordering::SeqCst) == 1));
    engine.command(Command::SetVolume(0.7));
    assert!(wait_until(Duration::from_secs(1), || {
        let status = engine.status();
        status.volume == 0.7 && status.peers.len() == 2 && status.peers.iter().all(|p| p.clock_offset_ns.is_some())
    }), "clock sync and commands must progress while the probe is blocked");
    assert_eq!(host.finished.load(Ordering::SeqCst), 0, "probe must still be in progress");
    assert!(wait_until(Duration::from_secs(4), || [&b, &c].iter().all(|n| {
        n.engine.status().peers.iter().any(|p| p.inputs.as_ref().is_some_and(|l| l.devices == ["USB Mic"]))
    })));
    assert_eq!(host.queries.load(Ordering::SeqCst), 1, "overlapping peers and retries must share the probe");
    assert_eq!(host.opens.load(Ordering::SeqCst), 1, "inventory must not cause a false resume/reset");
    assert!(engine.status().peers.iter().all(|p| p.clock_offset_ns.is_some()));
    c.shutdown();
    b.shutdown();
    engine.shutdown();
}

#[test]
fn inventory_errors_retry_without_publishing_an_empty_list() {
    let host = InventoryHost::new(Duration::ZERO);
    host.fail.store(true, Ordering::SeqCst);
    let engine = host.start();
    let b = observer("b", &engine);
    assert!(wait_until(Duration::from_secs(4), || host.finished.load(Ordering::SeqCst) >= 2));
    let inputs = || b.engine.status().peers.iter().find(|p| p.id == engine.status().node_id).and_then(|p| p.inputs.clone());
    assert_eq!(inputs(), None, "failed enumeration is not a successful empty inventory");
    host.fail.store(false, Ordering::SeqCst);
    assert!(wait_until(Duration::from_secs(3), || inputs().is_some_and(|l| l.devices == ["USB Mic"])));
    host.fail.store(true, Ordering::SeqCst);
    let before = host.finished.load(Ordering::SeqCst);
    assert!(wait_until(Duration::from_secs(7), || host.finished.load(Ordering::SeqCst) > before));
    std::thread::sleep(Duration::from_millis(200)); // let the response, if any, reach the observer
    assert_eq!(inputs().unwrap().devices, ["USB Mic"], "a failed refresh must preserve the last list");
    b.shutdown();
    engine.shutdown();
}

#[test]
fn shutdown_does_not_wait_for_a_stalled_inventory_probe() {
    let host = InventoryHost::new(Duration::from_millis(2250));
    let engine = host.start();
    let b = observer("b", &engine);
    assert!(wait_until(Duration::from_secs(2), || host.queries.load(Ordering::SeqCst) == 1));
    let start = Instant::now();
    engine.shutdown();
    assert!(start.elapsed() < Duration::from_secs(1));
    b.shutdown();
    assert!(wait_until(Duration::from_secs(4), || Arc::strong_count(&host) == 1), "worker must release its host after the OS call finishes");
}
