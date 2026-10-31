//! A SoundSync node without UI: discovery, clock sync, streaming and calibration.
//!
//! [`Engine::start`] spawns the network receive thread and the engine thread; the UI drives it
//! with [`Command`]s and reads [`Status`] snapshots.

pub mod chunk;
pub mod command;
pub mod defaults;
pub mod discovery;
pub mod leases;
pub mod net;
pub mod peers;
pub mod receiver;
pub mod sender;
pub mod status;

mod streams;

mod calibration;
mod devices;
mod node;
mod power;
mod synctest;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use anyhow::Context;
use arc_swap::ArcSwap;
use crossbeam_channel::Sender;
use soundsync_audio::AudioHost;
use soundsync_core::config::Config;

pub use command::Command;
pub use net::NetImpair;
pub use status::{CalibStatus, DeviceStatus, PeerStatus, Status, StreamStatus};

pub struct EngineOptions {
    pub config: Config,
    /// Where the config is saved when commands change it (None: never saved).
    pub config_path: Option<PathBuf>,
    pub state_path: Option<PathBuf>,
    pub host: Arc<dyn AudioHost>,
    pub enable_mdns: bool,
    /// Address to bind (default 0.0.0.0).
    pub bind_ip: Option<Ipv4Addr>,
    pub impair: Option<NetImpair>,
    /// Lets SoundSync switch the OS default output (Windows); None in tests and on macOS.
    pub default_devices: Option<Arc<dyn defaults::DefaultDeviceApi>>,
}

pub struct Engine {
    commands: Sender<command::Command>,
    status: Arc<ArcSwap<Status>>,
    config_fingerprint: Arc<AtomicU64>,
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    routes: Sender<net::RxCmd>,
    threads: Vec<JoinHandle<()>>,
}

impl Engine {
    pub fn start(o: EngineOptions) -> anyhow::Result<Engine> {
        let mut config = o.config;
        if config.node.id == 0 {
            config.regenerate_id();
        }
        let node_id = Arc::new(AtomicU64::new(config.node.id));
        let ip = o.bind_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let transport = net::Transport::bind(ip, config.network.port, config.group_key(), node_id.clone(), o.impair)
            .with_context(|| format!("cannot open UDP port {} on {ip}", config.network.port))?;
        let local_addr = transport.local_addr();
        let (inbound_tx, inbound_rx) = crossbeam_channel::unbounded();
        let (routes_tx, routes_rx) = crossbeam_channel::unbounded();
        let (commands_tx, commands_rx) = crossbeam_channel::unbounded();
        let counters = Arc::new(net::RxCounters::default());
        let status = Arc::new(ArcSwap::from_pointee(Status::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let rx_thread = net::spawn_rx(transport.clone(), o.host.clone(), inbound_tx, routes_rx, counters.clone());
        let mut node = node::Node::new(
            config,
            o.config_path,
            o.host,
            transport,
            node_id,
            status.clone(),
            routes_tx.clone(),
            counters,
        );
        if let (Some(api), Some(state_path)) = (o.default_devices, o.state_path.clone()) {
            // Crash recovery runs on the engine thread (it uses COM; see node::run).
            node.defaults = Some(defaults::DefaultSwitchGuard::new(api, state_path));
        }
        node.local_nets = discovery::local_networks(&node.config.network.interface);
        if o.enable_mdns {
            let nets = node.local_nets.clone();
            if nets.is_empty() {
                node.warn(format!("no network interface matches \"{}\"; automatic discovery is off", node.config.network.interface));
            } else {
                tracing::info!("mDNS on {}", nets.iter().map(|n| format!("{} ({})", n.name, n.ip)).collect::<Vec<_>>().join(", "));
                match discovery::Discovery::start(&nets, node.id(), node.session, &node.config.node.name, &node.config.network.group, local_addr.port(), soundsync_core::Role::Receiver) {
                    Ok(d) => node.discovery = Some(d),
                    Err(e) => node.warn(format!("automatic discovery unavailable: {e}")),
                }
            }
        }
        let config_fingerprint = node.config_fingerprint.clone();
        if let Some(bytes) = node.config_path.as_ref().and_then(|p| std::fs::read(p).ok()) {
            config_fingerprint.store(Config::fingerprint(&bytes), Ordering::Release);
        }
        let first = node.base_status(node.host.now_ns());
        status.store(Arc::new(first));
        let s = stop.clone();
        let engine_thread = std::thread::Builder::new()
            .name("soundsync-engine".into())
            .spawn(move || node::run(node, inbound_rx, commands_rx, s))
            .context("spawn engine thread")?;
        Ok(Engine { commands: commands_tx, status, config_fingerprint, local_addr, stop, routes: routes_tx, threads: vec![rx_thread, engine_thread] })
    }

    pub fn command(&self, c: Command) {
        let _ = self.commands.send(c);
    }

    pub fn status(&self) -> Arc<Status> {
        self.status.load_full()
    }

    /// [`Config::fingerprint`] of the config file as this engine last loaded or wrote it; a
    /// file with another fingerprint was edited by someone else.
    pub fn config_fingerprint(&self) -> u64 {
        self.config_fingerprint.load(Ordering::Acquire)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shutdown(self) {
        drop(self);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.routes.send(net::RxCmd::Stop);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
