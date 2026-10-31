//! Peer discovery with mDNS DNS-SD (`_soundsync._udp.local.`), limited to physical LAN
//! interfaces so announcements never go out over WSL/Hyper-V/VPN adapters.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, unbounded};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use soundsync_core::{MasterMode, Nanos, NodeId, Os, Role};

pub const SERVICE_TYPE: &str = "_soundsync._udp.local.";
const INBOUND_GRACE_NS: Nanos = 10_000_000_000;

const EXCLUDED_NAMES: [&str; 14] = [
    "vethernet",
    "wsl",
    "hyper-v",
    "docker",
    "virtualbox",
    "vmware",
    "tailscale",
    "zerotier",
    "wireguard",
    "openvpn",
    "tap-",
    "tun",
    "loopback",
    "npcap",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalNet {
    pub name: String,
    pub ip: Ipv4Addr,
    pub prefix: u8,
}

/// Physical LAN interfaces only: no loopback, link-local, CGNAT/Tailscale (100.64/10) or
/// virtual/VPN adapters.
pub fn iface_allowed(name: &str, ip: Ipv4Addr) -> bool {
    let lower = name.to_lowercase();
    let o = ip.octets();
    let cgnat = o[0] == 100 && (64..128).contains(&o[1]);
    !(ip.is_loopback() || ip.is_link_local() || cgnat || ip.is_unspecified() || EXCLUDED_NAMES.iter().any(|x| lower.contains(x)))
}

/// `"auto"`/empty: allowed interfaces. Otherwise an exact IPv4 address, or a case-insensitive
/// interface-name substring — an explicit choice bypasses the filter.
pub fn filter_networks(ifaces: &[(String, Ipv4Addr, u8)], cfg_interface: &str) -> Vec<LocalNet> {
    let cfg = cfg_interface.trim();
    let to_net = |(name, ip, prefix): &(String, Ipv4Addr, u8)| LocalNet { name: name.clone(), ip: *ip, prefix: *prefix };
    if cfg.is_empty() || cfg.eq_ignore_ascii_case("auto") {
        return ifaces.iter().filter(|(n, ip, _)| iface_allowed(n, *ip)).map(to_net).collect();
    }
    if let Ok(ip) = cfg.parse::<Ipv4Addr>() {
        return ifaces.iter().filter(|(_, i, _)| *i == ip).map(to_net).collect();
    }
    let needle = cfg.to_lowercase();
    ifaces.iter().filter(|(n, _, _)| n.to_lowercase().contains(&needle)).map(to_net).collect()
}

/// IPv4 interfaces of this machine, filtered per [`filter_networks`].
pub fn local_networks(cfg_interface: &str) -> Vec<LocalNet> {
    let ifaces: Vec<(String, Ipv4Addr, u8)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some((i.name, v4.ip, u32::from(v4.netmask).count_ones() as u8)),
            _ => None,
        })
        .collect();
    filter_networks(&ifaces, cfg_interface)
}

pub fn addr_on_local_net(addr: Ipv4Addr, nets: &[LocalNet]) -> bool {
    nets.iter().any(|n| {
        let mask = if n.prefix == 0 { 0 } else { u32::MAX << (32 - n.prefix.min(32) as u32) };
        u32::from(addr) & mask == u32::from(n.ip) & mask
    })
}

/// True when we contacted a peer that is demonstrably running (found via mDNS) at least 10 s
/// ago and have not received a single authenticated packet since: inbound UDP is probably
/// blocked (Windows Firewall, macOS Local Network permission).
/// Who may become a peer (and so a stream target): another instance on this PC, a device on a
/// LAN we use, or a configured static peer.
pub fn hello_allowed(from: IpAddr, nets: &[LocalNet], static_ips: &[IpAddr]) -> bool {
    match from {
        IpAddr::V4(v4) => v4.is_loopback() || addr_on_local_net(v4, nets) || static_ips.contains(&from),
        IpAddr::V6(_) => static_ips.contains(&from),
    }
}

pub fn inbound_suspect(now: Nanos, first_contact_attempt: Option<Nanos>, last_authenticated_rx: Option<Nanos>) -> bool {
    match first_contact_attempt {
        None => false,
        Some(t) => now - t >= INBOUND_GRACE_NS && last_authenticated_rx.is_none_or(|rx| rx < t),
    }
}

/// Whether an announcement (TXT group / sid / id) comes from another running node of our group.
/// Our own is recognised by session, not by node id: a config copied to another PC shares the
/// id, and that PC must still be contacted so the duplicate-id handshake can rename one of them.
pub fn is_other_node(group: Option<&str>, sid: Option<&str>, id: Option<&str>, own_group: &str, own_sid: &str, own_id: &str) -> bool {
    group == Some(own_group)
        && match sid {
            Some(sid) => sid != own_sid,
            None => id != Some(own_id),
        }
}

fn role_txt(role: Role) -> &'static str {
    match role {
        Role::Receiver => "receiver",
        Role::Master(MasterMode::Virtual) => "master-virtual",
        Role::Master(MasterMode::Direct) => "master-direct",
    }
}

pub struct Discovery {
    daemon: ServiceDaemon,
    info: ServiceInfo,
    found: Receiver<SocketAddr>,
    thread: Option<JoinHandle<()>>,
}

impl Discovery {
    pub fn start(nets: &[LocalNet], node_id: NodeId, session: u64, name: &str, group: &str, port: u16, role: Role) -> anyhow::Result<Discovery> {
        let daemon = ServiceDaemon::new()?;
        daemon.disable_interface(IfKind::All)?;
        for n in nets {
            daemon.enable_interface(IfKind::Addr(IpAddr::V4(n.ip)))?;
        }
        // Unique per run (not per node id), so two PCs sharing a copied id do not collide.
        let instance = format!("{}-{session:016x}", name.chars().filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-').collect::<String>());
        let host = format!("soundsync-{session:016x}.local.");
        let ips: Vec<IpAddr> = nets.iter().map(|n| IpAddr::V4(n.ip)).collect();
        let os = match Os::current() {
            Os::Windows => "windows",
            Os::MacOs => "macos",
            Os::Other => "other",
        };
        let id_hex = format!("{node_id:016x}");
        let sid_hex = format!("{session:016x}");
        let props = [("id", id_hex.as_str()), ("sid", sid_hex.as_str()), ("group", group), ("ver", env!("CARGO_PKG_VERSION")), ("os", os), ("role", role_txt(role))];
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, &ips[..], port, &props[..])?;
        daemon.register(info.clone())?;
        let events = daemon.browse(SERVICE_TYPE)?;
        let (tx, found) = unbounded();
        let (group, nets, own_id, own_sid) = (group.to_string(), nets.to_vec(), id_hex, sid_hex);
        let thread = std::thread::Builder::new().name("soundsync-mdns".into()).spawn(move || {
            while let Ok(ev) = events.recv() {
                let ServiceEvent::ServiceResolved(svc) = ev else { continue };
                let txt = |k: &str| svc.get_property_val_str(k);
                if !is_other_node(txt("group"), txt("sid"), txt("id"), &group, &own_sid, &own_id) {
                    continue;
                }
                for ip in svc.get_addresses_v4() {
                    if addr_on_local_net(ip, &nets) {
                        let _ = tx.send(SocketAddr::new(IpAddr::V4(ip), svc.port));
                    }
                }
            }
        })?;
        Ok(Discovery { daemon, info, found, thread: Some(thread) })
    }

    /// Addresses of other nodes in our group, as they are resolved.
    pub fn found(&self) -> &Receiver<SocketAddr> {
        &self.found
    }

    pub fn update_role(&mut self, role: Role) {
        self.set_txt("role", role_txt(role));
    }

    /// Re-announces with a new node id (after a duplicate id was regenerated).
    pub fn update_id(&mut self, node_id: NodeId) {
        self.set_txt("id", &format!("{node_id:016x}"));
    }

    fn set_txt(&mut self, key: &str, value: &str) {
        let mut props: Vec<(String, String)> =
            self.info.get_properties().iter().map(|p| (p.key().to_string(), p.val_str().to_string())).collect();
        for p in props.iter_mut().filter(|(k, _)| k == key) {
            p.1 = value.to_string();
        }
        if let Ok(info) = ServiceInfo::new(
            self.info.get_type(),
            self.info.get_fullname().trim_end_matches(self.info.get_type()).trim_end_matches('.'),
            self.info.get_hostname(),
            &self.info.get_addresses().iter().copied().collect::<Vec<_>>()[..],
            self.info.get_port(),
            &props.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>()[..],
        ) {
            let _ = self.daemon.register(info.clone());
            self.info = info;
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(self.info.get_fullname());
        if let Ok(done) = self.daemon.shutdown() {
            let _ = done.recv_timeout(std::time::Duration::from_secs(1));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Nanos = 1_000_000_000;

    #[test]
    fn hello_only_from_this_lan_loopback_or_static_peers() {
        let nets = [LocalNet { name: "Ethernet".into(), ip: "192.168.1.236".parse().unwrap(), prefix: 24 }];
        let statics = ["10.8.0.5".parse().unwrap()];
        let ok = |a: &str| hello_allowed(a.parse().unwrap(), &nets, &statics);
        assert!(ok("192.168.1.20"));
        assert!(ok("127.0.0.1"), "another instance on this PC");
        assert!(ok("10.8.0.5"), "a configured static peer (e.g. over a VPN)");
        assert!(!ok("8.8.8.8"), "off-LAN senders cannot become peers (and stream targets)");
        assert!(!ok("192.168.2.20"));
    }

    #[test]
    fn own_announcement_is_recognised_by_session_not_id() {
        // A config copied to another PC carries the same node id: that PC must still be heard,
        // or the duplicate-id handshake that renames one of them never happens.
        assert!(is_other_node(Some("home"), Some("a1"), Some("42"), "home", "b2", "42"), "same id, other session");
        assert!(!is_other_node(Some("home"), Some("b2"), Some("42"), "home", "b2", "42"), "our own announcement");
        assert!(!is_other_node(Some("work"), Some("a1"), Some("7"), "home", "b2", "42"), "another group");
        assert!(!is_other_node(Some("home"), None, Some("42"), "home", "b2", "42"), "no session: fall back to the id");
        assert!(is_other_node(Some("home"), None, Some("7"), "home", "b2", "42"));
    }

    #[test]
    fn inbound_suspect_after_10s_without_rx() {
        let t0 = 100 * S;
        assert!(!inbound_suspect(t0 + 9 * S, Some(t0), None));
        assert!(inbound_suspect(t0 + 10 * S, Some(t0), None));
        assert!(inbound_suspect(t0 + 11 * S, Some(t0), Some(t0 - S)), "rx from before the attempt doesn't count");
        assert!(!inbound_suspect(t0 + 11 * S, Some(t0), Some(t0 + S)));
        assert!(!inbound_suspect(t0 + 60 * S, None, None), "no attempt, nothing to suspect");
    }

    #[test]
    fn filters_virtual_adapters() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        for (name, addr, want) in [
            ("vEthernet (WSL (Hyper-V firewall))", "172.28.32.1", false),
            ("vEthernet (Default Switch)", "172.17.0.1", false),
            ("Ethernet", "192.168.1.50", true),
            ("Wi-Fi", "10.0.0.7", true),
            ("Tailscale", "100.101.1.2", false),
            ("ZeroTier One [abc]", "10.147.17.3", false),
            ("Ethernet 2", "100.101.1.2", false),
            ("Loopback Pseudo-Interface 1", "127.0.0.1", false),
            ("Ethernet 3", "169.254.10.10", false),
            ("OpenVPN Data Channel Offload", "10.8.0.2", false),
            ("Docker Desktop", "192.168.65.1", false),
        ] {
            assert_eq!(iface_allowed(name, ip(addr)), want, "{name} {addr}");
        }
    }

    fn sample_ifaces() -> Vec<(String, Ipv4Addr, u8)> {
        vec![
            ("Ethernet".into(), Ipv4Addr::new(192, 168, 1, 50), 24),
            ("vEthernet (WSL)".into(), Ipv4Addr::new(172, 28, 32, 1), 20),
            ("Loopback Pseudo-Interface 1".into(), Ipv4Addr::LOCALHOST, 8),
        ]
    }

    #[test]
    fn auto_selects_physical_interfaces() {
        let nets = filter_networks(&sample_ifaces(), "auto");
        assert_eq!(nets.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), vec!["Ethernet"]);
        assert_eq!(filter_networks(&sample_ifaces(), ""), nets);
    }

    #[test]
    fn allows_named_interface_override() {
        let by_name = filter_networks(&sample_ifaces(), "vethernet (wsl)");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].ip, Ipv4Addr::new(172, 28, 32, 1));
        let by_ip = filter_networks(&sample_ifaces(), "127.0.0.1");
        assert_eq!(by_ip.len(), 1);
        assert_eq!(by_ip[0].prefix, 8);
        assert!(filter_networks(&sample_ifaces(), "nothing-like-this").is_empty());
    }

    #[test]
    fn local_net_membership() {
        let nets = vec![LocalNet { name: "Ethernet".into(), ip: Ipv4Addr::new(192, 168, 1, 50), prefix: 24 }];
        assert!(addr_on_local_net(Ipv4Addr::new(192, 168, 1, 7), &nets));
        assert!(!addr_on_local_net(Ipv4Addr::new(192, 168, 2, 7), &nets));
        assert!(!addr_on_local_net(Ipv4Addr::new(10, 0, 0, 1), &[]));
        let slash16 = vec![LocalNet { name: "x".into(), ip: Ipv4Addr::new(10, 1, 2, 3), prefix: 16 }];
        assert!(addr_on_local_net(Ipv4Addr::new(10, 1, 200, 9), &slash16));
    }
}
