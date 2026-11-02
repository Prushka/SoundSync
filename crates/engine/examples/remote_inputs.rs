//! Read a peer's microphone list without opening audio devices or changing its settings.
//! Usage: cargo run -p soundsync-engine --example remote_inputs -- <host:port> [config.toml]

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use soundsync_core::config::{Config, Paths};
use soundsync_core::protocol::{Control, GroupKey, Hello, MAX_DATAGRAM, Packet, decode, encode};
use soundsync_core::Os;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let peer: SocketAddr = args.next().ok_or("expected peer host:port")?.parse()?;
    let path = args.next().map(Into::into).unwrap_or_else(|| Paths::for_instance(None).config_file);
    let (config, _) = Config::parse(&std::fs::read(path)?)?;
    let key = GroupKey::derive(&config.network.group, &config.network.key);
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    let id = rand::random::<u64>();
    let hello = Control::Hello(Hello {
        reply: false,
        name: "SoundSync microphone diagnostic".into(),
        os: Os::current(),
        app_version: soundsync_core::version::version().into(),
        group: config.network.group,
        session: rand::random(),
    });
    let send = |control| -> Result<(), Box<dyn std::error::Error>> {
        let mut buf = [0; MAX_DATAGRAM];
        let n = encode(id, &Packet::Control(control), &key, &mut buf)?;
        socket.send_to(&buf[..n], peer)?;
        Ok(())
    };
    let start = Instant::now();
    let mut next_request = Duration::ZERO;
    let mut connected = false;
    let mut buf = [0; 65536];
    while start.elapsed() < Duration::from_secs(12) {
        if start.elapsed() >= next_request {
            send(if connected { Control::ListInputs } else { hello.clone() })?;
            next_request = start.elapsed() + Duration::from_secs(1);
        }
        match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == peer => match decode(&buf[..n], &key) {
                Ok((_, Packet::Control(Control::Hello(h)))) => {
                    println!("{}: {:?}, {}", h.name, h.os, h.app_version);
                    connected = true;
                    send(Control::ListInputs)?;
                }
                Ok((_, Packet::Control(Control::Inputs(list)))) => {
                    println!("Default: {:?}\nMicrophones: {:?}", list.default, list.devices);
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => eprintln!("Reply from {from}: {e}"),
            },
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err(if connected { "peer connected but never returned its microphone list" } else { "peer did not answer the handshake" }.into())
}
