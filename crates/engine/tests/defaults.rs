mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use soundsync_audio::fake::{FakeSignal, FakeSourceSpec};
use soundsync_core::config::Config;
use soundsync_engine::Command;
use soundsync_engine::defaults::{DefaultDeviceApi, DefaultDevices, DefaultRole};

struct MockApi(Mutex<DefaultDevices>);

impl DefaultDeviceApi for MockApi {
    fn read(&self) -> Result<DefaultDevices, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn set(&self, id: &str, role: DefaultRole) -> Result<(), String> {
        let mut d = self.0.lock().unwrap();
        match role {
            DefaultRole::Console => d.console = id.into(),
            DefaultRole::Multimedia => d.multimedia = id.into(),
            DefaultRole::Communications => d.communications = id.into(),
        }
        Ok(())
    }
    fn exists(&self, _id: &str) -> bool {
        true
    }
}

fn speakers() -> DefaultDevices {
    DefaultDevices { console: "fake:TV".into(), multimedia: "fake:TV".into(), communications: "fake:TV".into() }
}

#[test]
fn virtual_broadcast_switches_default_and_restores() {
    let api = Arc::new(MockApi(Mutex::new(speakers())));
    let m = spawn_with(
        Spec { default_devices: Some(api.clone()), ..Spec::new("m") },
        |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = "TV".into();
            c.master.switch_default = true;
        },
        |h| {
            h.add_source(pulse_source("CABLE Input", 0));
            h.add_output(output("TV", 0.0, 10, 0));
        },
    );
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(3), || api.0.lock().unwrap().console == "fake:CABLE Input"));
    assert_eq!(api.0.lock().unwrap().communications, "fake:CABLE Input");
    m.engine.command(Command::SetBroadcast(false));
    assert!(wait_until(Duration::from_secs(3), || *api.0.lock().unwrap() == speakers()));
    // Turning it on again and quitting while switched restores too.
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(3), || api.0.lock().unwrap().console == "fake:CABLE Input"));
    m.shutdown();
    assert_eq!(*api.0.lock().unwrap(), speakers());
}

#[test]
fn silence_watchdog_warns_when_apps_play_silence() {
    let m = spawn_with(
        Spec::new("m"),
        |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = "null".into();
        },
        |h| {
            h.add_source(FakeSourceSpec {
                name: "CABLE Input".into(),
                format: stereo(),
                ppm: 0.0,
                signal: FakeSignal::Silence,
                probe_offset_ns: 0,
                hidden_latency_ns: 0,
            });
            h.set_active_sessions("CABLE Input", 1);
        },
    );
    m.engine.command(Command::SetBroadcast(true));
    let warned = |n: &TestNode| n.engine.status().warnings.iter().any(|w| w.contains("hears silence"));
    assert!(wait_until(Duration::from_secs(12), || warned(&m)), "{:?}", m.engine.status().warnings);
    m.host.set_active_sessions("CABLE Input", 0);
    assert!(wait_until(Duration::from_secs(4), || !warned(&m)), "warning clears when no app plays");
    m.shutdown();
}

fn cable_master(api: Option<Arc<MockApi>>, switch_default: bool) -> TestNode {
    spawn_with(
        Spec { default_devices: api.map(|a| a as Arc<dyn DefaultDeviceApi>), ..Spec::new("m") },
        move |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = String::new(); // "Default"
            c.master.switch_default = switch_default;
        },
        |h| {
            h.add_source(pulse_source("CABLE Input", 0));
            h.add_output(output("TV", 0.0, 10, 0));
        },
    )
}

#[test]
fn monitor_never_plays_into_the_captured_cable() {
    // The user made CABLE Input the Windows default and left the monitor on "Default": playing
    // the monitor there would be captured again and loop through every speaker.
    let m = cable_master(None, false);
    m.host.set_default_render("CABLE Input");
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(3), || m.engine.status().broadcasting));
    std::thread::sleep(Duration::from_millis(300));
    let st = m.engine.status();
    assert_ne!(st.output.as_ref().map(|o| o.name.as_str()), Some("CABLE Input"), "monitor opened on the captured device");
    assert!(st.warnings.iter().any(|w| w.contains("Monitor output")), "{:?}", st.warnings);
    // Making the speakers the default again brings the monitor back.
    m.host.set_default_render("TV");
    assert!(wait_until(Duration::from_secs(2), || m.engine.status().output.as_ref().is_some_and(|o| o.name == "TV")), "{:?}", m.engine.status().output);
    assert!(!m.engine.status().warnings.iter().any(|w| w.contains("Monitor output")));
    m.shutdown();
}

#[test]
fn monitor_uses_the_default_from_before_the_switch() {
    let api = Arc::new(MockApi(Mutex::new(speakers())));
    let m = cable_master(Some(api.clone()), true);
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(3), || api.0.lock().unwrap().console == "fake:CABLE Input"));
    // Windows now resolves "Default" to the cable; any reopen (resume, device change) must not follow it.
    m.host.set_default_render("CABLE Input");
    m.engine.command(Command::SetMonitorDevice(String::new()));
    std::thread::sleep(Duration::from_millis(400)); // the reopen happened
    assert_eq!(m.engine.status().output.as_ref().map(|o| o.name.as_str()), Some("TV"));
    m.shutdown();
}

#[test]
fn broadcast_off_during_a_capture_outage_restores_defaults() {
    let api = Arc::new(MockApi(Mutex::new(speakers())));
    let m = spawn_with(
        Spec { default_devices: Some(api.clone()), ..Spec::new("m") },
        |c: &mut Config| {
            c.master.capture_device = "CABLE Input".into();
            c.master.monitor_device = "TV".into();
            c.master.switch_default = true;
        },
        |h| {
            h.add_source(pulse_source("CABLE Input", 0));
            h.add_output(output("TV", 0.0, 10, 0));
        },
    );
    m.engine.command(Command::SetBroadcast(true));
    assert!(wait_until(Duration::from_secs(3), || api.0.lock().unwrap().console == "fake:CABLE Input"));
    m.host.remove_device("CABLE Input");
    assert!(wait_until(Duration::from_secs(2), || !m.engine.status().broadcasting), "capture lost");
    m.engine.command(Command::SetBroadcast(false));
    assert!(wait_until(Duration::from_secs(2), || *api.0.lock().unwrap() == speakers()), "defaults restored: {:?}", api.0.lock().unwrap());
    m.shutdown();
}
