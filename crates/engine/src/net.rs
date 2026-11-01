//! UDP transport: one socket per node, authenticated packets, and the network receive thread.
//!
//! The receive thread decodes every datagram. Audio goes straight into the ring of the stream it
//! belongs to (registered via [`RxCmd`]); control messages go to the engine thread.

use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use socket2::{Domain, Protocol, Socket, Type};
use soundsync_audio::AudioHost;
use soundsync_core::protocol::{Control, DecodeError, GroupKey, MAX_DATAGRAM, Packet, decode, encode};
use soundsync_core::{Nanos, NodeId};

use crate::chunk::AudioChunk;

const RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// Simulated network trouble on packets *arriving* at a node (tests only).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetImpair {
    pub loss: f64,
    pub reorder: f64,
    pub jitter_ns: Nanos,
    pub seed: u64,
}

/// Something that can put a datagram on the wire.
pub trait PacketSink: Send + Sync {
    fn send_datagram(&self, to: SocketAddr, bytes: &[u8]);
}

pub struct Transport {
    socket: UdpSocket,
    key: GroupKey,
    node_id: Arc<AtomicU64>,
    impair: Option<NetImpair>,
}

impl Transport {
    pub fn bind(ip: Ipv4Addr, port: u16, key: GroupKey, node_id: Arc<AtomicU64>, impair: Option<NetImpair>) -> io::Result<Arc<Transport>> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        let _ = socket.set_recv_buffer_size(RECV_BUFFER_BYTES);
        socket.bind(&SocketAddr::V4(SocketAddrV4::new(ip, port)).into())?;
        socket.set_read_timeout(Some(READ_TIMEOUT))?;
        Ok(Arc::new(Transport { socket: socket.into(), key, node_id, impair }))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().expect("bound socket has an address")
    }

    pub fn key(&self) -> &GroupKey {
        &self.key
    }

    pub fn node_id(&self) -> NodeId {
        self.node_id.load(Ordering::Relaxed)
    }

    pub fn send_control(&self, to: SocketAddr, c: &Control) {
        let mut buf = [0u8; MAX_DATAGRAM];
        match encode(self.node_id(), &Packet::Control(c.clone()), &self.key, &mut buf) {
            Ok(n) => self.send_datagram(to, &buf[..n]),
            Err(e) => tracing::warn!("cannot encode control message: {e}"),
        }
    }
}

impl PacketSink for Transport {
    fn send_datagram(&self, to: SocketAddr, bytes: &[u8]) {
        if let Err(e) = self.socket.send_to(bytes, to) {
            tracing::trace!("send to {to} failed: {e}");
        }
    }
}

pub struct Inbound {
    pub from: SocketAddr,
    pub sender: NodeId,
    pub arrival_ns: Nanos,
    pub control: Control,
}

pub enum RxCmd {
    AddRoute { master: NodeId, stream_id: u32, tx: rtrb::Producer<AudioChunk> },
    RemoveRoute { master: NodeId, stream_id: u32 },
    Stop,
}

/// Counters maintained by the receive thread.
#[derive(Default)]
pub struct RxCounters {
    pub bad_packets: AtomicU64,
    pub dropped_chunks: AtomicU64,
    pub unrouted_audio: AtomicU64,
    /// Last time (host clock) any authenticated packet arrived.
    pub last_authenticated_ns: std::sync::atomic::AtomicI64,
    /// The last datagram from a SoundSync speaking another protocol version.
    pub other_version: std::sync::Mutex<Option<OtherVersion>>,
}

impl RxCounters {
    /// When a SoundSync datagram last got in: an authenticated one, or one from another version
    /// (unreadable, but it got past the firewall all the same).
    pub fn last_heard_ns(&self) -> Option<Nanos> {
        let authenticated = Some(self.last_authenticated_ns.load(Ordering::Relaxed)).filter(|t| *t != 0);
        let other = self.other_version.lock().unwrap().map(|o| o.at_ns);
        authenticated.max(other)
    }
}

/// A SoundSync datagram this version cannot read (unauthenticated: only a hint for the user).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OtherVersion {
    pub from: std::net::IpAddr,
    pub version: u8,
    pub at_ns: Nanos,
}

struct Delayed {
    due: Nanos,
    seq: u64,
    from: SocketAddr,
    bytes: Vec<u8>,
}

impl PartialEq for Delayed {
    fn eq(&self, o: &Self) -> bool {
        (self.due, self.seq) == (o.due, o.seq)
    }
}
impl Eq for Delayed {}
impl PartialOrd for Delayed {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Delayed {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        (o.due, o.seq).cmp(&(self.due, self.seq)) // min-heap
    }
}

struct Rng(u64);
impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Rx {
    transport: Arc<Transport>,
    host: Arc<dyn AudioHost>,
    ctrl: Sender<Inbound>,
    routes: HashMap<(NodeId, u32), rtrb::Producer<AudioChunk>>,
    counters: Arc<RxCounters>,
}

impl Rx {
    fn deliver(&mut self, buf: &[u8], from: SocketAddr, arrival_ns: Nanos) {
        let (sender, packet) = match decode(buf, &self.transport.key) {
            Ok(v) => v,
            Err(e) => {
                self.counters.bad_packets.fetch_add(1, Ordering::Relaxed);
                if let DecodeError::BadVersion(version) = e {
                    *self.counters.other_version.lock().unwrap() = Some(OtherVersion { from: from.ip(), version, at_ns: arrival_ns });
                }
                return;
            }
        };
        self.counters.last_authenticated_ns.store(arrival_ns, Ordering::Relaxed);
        let chunk = match packet {
            Packet::Control(control) => {
                let _ = self.ctrl.send(Inbound { from, sender, arrival_ns, control });
                return;
            }
            Packet::Audio { header, payload } => AudioChunk::from_payload(header, payload, arrival_ns),
            Packet::Silent { header } => AudioChunk::silent(header, arrival_ns),
        };
        match self.routes.get_mut(&(sender, chunk.header.stream_id)) {
            Some(tx) => {
                if tx.push(chunk).is_err() {
                    self.counters.dropped_chunks.fetch_add(1, Ordering::Relaxed);
                }
            }
            None => {
                self.counters.unrouted_audio.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

pub fn spawn_rx(
    transport: Arc<Transport>,
    host: Arc<dyn AudioHost>,
    ctrl: Sender<Inbound>,
    routes: Receiver<RxCmd>,
    counters: Arc<RxCounters>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("soundsync-net-rx".into())
        .spawn(move || {
            let impair = transport.impair;
            let mut rng = Rng(impair.map_or(1, |i| i.seed.max(1)));
            let mut delayed: BinaryHeap<Delayed> = BinaryHeap::new();
            let mut seq = 0u64;
            let mut rx = Rx { transport: transport.clone(), host, ctrl, routes: HashMap::new(), counters };
            // Any UDP datagram fits, so junk is read and rejected by decode() instead of failing
            // the receive (on Windows an oversized datagram is an error, WSAEMSGSIZE).
            let mut buf = vec![0u8; 65_536];
            let mut last_error_log: Option<std::time::Instant> = None;
            // set_read_timeout is a setsockopt call: only make it when the timeout changes (the
            // socket was created with READ_TIMEOUT), not once per packet.
            let mut current_timeout = READ_TIMEOUT;
            loop {
                for cmd in routes.try_iter() {
                    match cmd {
                        RxCmd::AddRoute { master, stream_id, tx } => {
                            rx.routes.insert((master, stream_id), tx);
                        }
                        RxCmd::RemoveRoute { master, stream_id } => {
                            rx.routes.remove(&(master, stream_id));
                        }
                        RxCmd::Stop => return,
                    }
                }
                let now = rx.host.now_ns();
                while delayed.peek().is_some_and(|d| d.due <= now) {
                    let d = delayed.pop().expect("peeked");
                    rx.deliver(&d.bytes, d.from, d.due);
                }
                let timeout = match delayed.peek() {
                    Some(d) => Duration::from_nanos((d.due - now).clamp(100_000, 50_000_000) as u64),
                    None => READ_TIMEOUT,
                };
                if timeout != current_timeout && transport.socket.set_read_timeout(Some(timeout)).is_ok() {
                    current_timeout = timeout;
                }
                match transport.socket.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        let arrival = rx.host.now_ns();
                        match impair {
                            None => rx.deliver(&buf[..n], from, arrival),
                            Some(i) => {
                                if rng.next_f64() < i.loss {
                                    continue;
                                }
                                let mut delay = (rng.next_f64() * i.jitter_ns as f64) as Nanos;
                                if rng.next_f64() < i.reorder {
                                    delay += 3_000_000;
                                }
                                seq += 1;
                                delayed.push(Delayed { due: arrival + delay, seq, from, bytes: buf[..n].to_vec() });
                            }
                        }
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    // Windows reports ICMP "port unreachable" from an earlier send as a receive error.
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                    Err(e) => {
                        if last_error_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
                            tracing::warn!("network receive failed: {e}");
                            last_error_log = Some(std::time::Instant::now());
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        })
        .expect("spawn network thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_datagram_from_another_version_proves_packets_get_in() {
        // Inbound reachability counts any SoundSync datagram: one we cannot read still got past
        // the firewall (blaming the firewall for a version mismatch would send people astray).
        let c = RxCounters::default();
        assert_eq!(c.last_heard_ns(), None);
        *c.other_version.lock().unwrap() = Some(OtherVersion { from: [192, 168, 1, 157].into(), version: 1, at_ns: 5_000 });
        assert_eq!(c.last_heard_ns(), Some(5_000));
        c.last_authenticated_ns.store(7_000, Ordering::Relaxed);
        assert_eq!(c.last_heard_ns(), Some(7_000));
    }
}
