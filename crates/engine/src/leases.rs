//! Subscription leases: a master streams to a receiver only while it keeps renewing.

use std::net::SocketAddr;

use soundsync_core::Nanos;

pub const LEASE_TTL_NS: Nanos = 3_000_000_000;

#[derive(Clone, Debug, Default)]
pub struct Leases {
    ttl_ns: Nanos,
    entries: Vec<(SocketAddr, Nanos)>,
}

impl Leases {
    pub fn new(ttl_ns: Nanos) -> Self {
        Self { ttl_ns, entries: Vec::new() }
    }

    pub fn renew(&mut self, addr: SocketAddr, now: Nanos) {
        match self.entries.iter_mut().find(|(a, _)| *a == addr) {
            Some(e) => e.1 = now,
            None => self.entries.push((addr, now)),
        }
    }

    pub fn remove(&mut self, addr: SocketAddr) {
        self.entries.retain(|(a, _)| *a != addr);
    }

    /// Unexpired subscribers, in subscription order (expired ones are dropped).
    pub fn active(&mut self, now: Nanos) -> Vec<SocketAddr> {
        let ttl = self.ttl_ns;
        self.entries.retain(|(_, t)| now - t <= ttl);
        self.entries.iter().map(|(a, _)| *a).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_expire_after_3s() {
        let s = 1_000_000_000;
        let a: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let mut l = Leases::new(3 * s);
        l.renew(a, 10 * s);
        l.renew(b, 11 * s);
        assert_eq!(l.active(12 * s), vec![a, b]);
        assert_eq!(l.active(13 * s + 1), vec![b], "a expired");
        l.renew(a, 13 * s + 2);
        let mut both = l.active(13 * s + 3);
        both.sort();
        assert_eq!(both, vec![a, b], "renewal revives");
        assert!(l.active(20 * s).is_empty());
        l.remove(a);
        l.renew(b, 30 * s);
        assert_eq!(l.active(30 * s), vec![b]);
    }
}
