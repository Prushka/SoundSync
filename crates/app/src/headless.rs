//! `soundsync run`: a node without the GUI, printing its state every 2 s.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use soundsync_core::config::{Config, Paths};
use soundsync_core::{MasterMode, Nanos};
use soundsync_engine::{Engine, EngineOptions, Status};

pub struct RunArgs {
    pub broadcast: bool,
    pub mode: Option<MasterMode>,
    pub capture: Option<String>,
    pub monitor: Option<String>,
    pub output: Option<String>,
    pub port: Option<u16>,
    pub bind: Option<Ipv4Addr>,
    pub peers: Vec<String>,
    pub no_mdns: bool,
    pub seconds: Option<f64>,
}

fn ms(ns: Nanos) -> f64 {
    ns as f64 / 1e6
}

pub fn status_lines(st: &Status, elapsed_s: f64) -> Vec<String> {
    let mut head = format!("[{elapsed_s:6.1}s] {}: {}", st.name, if st.broadcasting { "broadcasting" } else { "listening" });
    if let Some(d) = st.delay_ns {
        head += &format!(", D {:.1} ms", ms(d));
    }
    let mut lines = vec![head];
    for p in &st.peers {
        let rtt = p.rtt_ns.map_or("?".to_string(), |r| format!("{:.2} ms", ms(r)));
        let off = p.clock_offset_ns.map_or("?".to_string(), |o| format!("{:+.3} ms", o / 1e6));
        lines.push(format!("  peer {} ({}): rtt {rtt}, clock offset {off}", p.name, p.addr));
    }
    for s in &st.streams {
        let from = if s.master == st.node_id {
            "this PC".to_string()
        } else {
            st.peers.iter().find(|p| p.id == s.master).map_or_else(|| format!("{:016x}", s.master), |p| p.name.clone())
        };
        lines.push(format!(
            "  stream from {from}: {:?}, error {:+.2} ms, behind {:.1} ms, lost {} frames",
            s.state,
            ms(s.error_ns),
            ms(s.behind_ns),
            s.lost_frames
        ));
    }
    lines.extend(st.warnings.iter().map(|w| format!("  ! {w}")));
    lines
}

pub fn run(args: RunArgs, config_path: Option<PathBuf>, instance: Option<&str>) -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).with_writer(std::io::stderr).try_init();
    let path = config_path.unwrap_or_else(|| Paths::for_instance(instance).config_file);
    let loaded = Config::load_or_create(&path)?;
    for w in &loaded.warnings {
        eprintln!("config: {w}");
    }
    let mut config = loaded.config;
    let overridden = args.broadcast || args.mode.is_some() || args.capture.is_some() || args.monitor.is_some() || args.output.is_some() || args.port.is_some() || !args.peers.is_empty();
    if args.broadcast {
        config.master.enabled = true;
    }
    if let Some(m) = args.mode {
        config.master.mode = m;
    }
    if let Some(c) = args.capture {
        config.master.capture_device = c;
    }
    if let Some(m) = args.monitor {
        config.master.monitor_device = m;
    }
    if let Some(o) = args.output {
        config.playback.output_device = o;
    }
    if let Some(p) = args.port {
        config.network.port = p;
    }
    config.network.static_peers.extend(args.peers);
    if overridden {
        eprintln!("command-line overrides are active; settings changed while running are not saved");
    }
    let host = soundsync_audio::default_host()?;
    let state_path = Paths::for_instance(instance).state_file;
    let engine = Engine::start(EngineOptions {
        config,
        config_path: (!overridden).then_some(path),
        state_path: Some(state_path),
        host,
        enable_mdns: !args.no_mdns,
        bind_ip: args.bind,
        impair: None,
        default_devices: crate::default_devices_api(),
    })?;
    eprintln!("listening on {}", engine.local_addr());

    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    ctrlc::set_handler(move || s.store(true, Ordering::Release))?;
    let start = Instant::now();
    let mut next_print = Duration::from_secs(2);
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(50));
        let elapsed = start.elapsed();
        if args.seconds.is_some_and(|limit| elapsed.as_secs_f64() >= limit) {
            break;
        }
        if elapsed >= next_print {
            next_print += Duration::from_secs(2);
            for line in status_lines(&engine.status(), elapsed.as_secs_f64()) {
                println!("{line}");
            }
        }
    }
    engine.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundsync_core::player::PlayState;
    use soundsync_core::{Os, Role, StreamFormat};
    use soundsync_engine::{PeerStatus, Status, StreamStatus};

    #[test]
    fn status_lines_show_peers_and_streams() {
        let st = Status {
            node_id: 0xabcd,
            name: "a".into(),
            broadcasting: true,
            delay_ns: Some(45_000_000),
            peers: vec![PeerStatus {
                id: 0x1234,
                name: "b".into(),
                os: Os::Windows,
                addr: "127.0.0.1:47811".parse().unwrap(),
                role: Role::Receiver,
                rtt_ns: Some(310_000),
                jitter_ns: Some(20_000),
                clock_offset_ns: Some(12_500.0),
                extra_latency_ns: 0,
                output: None,
                rx: vec![],
                app_version: "0.1.0 (dev)".into(),
                volume: None,
                inputs: None,
            }],
            streams: vec![StreamStatus {
                master: 0x1234,
                stream_id: 9,
                format: StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 3 },
                state: PlayState::Playing,
                error_ns: 120_000,
                behind_ns: 0,
                lost_frames: 3,
                layout_warning: None,
                master_volume: 1.0,
            }],
            ..Status::default()
        };
        let lines = status_lines(&st, 2.0);
        assert_eq!(lines[0], "[   2.0s] a: broadcasting, D 45.0 ms");
        assert_eq!(lines[1], "  peer b (127.0.0.1:47811): rtt 0.31 ms, clock offset +0.013 ms");
        assert_eq!(lines[2], "  stream from b: Playing, error +0.12 ms, behind 0.0 ms, lost 3 frames");
    }
}
