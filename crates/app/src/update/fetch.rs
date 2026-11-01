//! Talks to GitHub: the latest release's metadata, and its download, checked against the size and
//! SHA-256 the release states. Uses the OS's own TLS and certificate store.

use std::io::Read;
use std::time::Duration;

use sha2::{Digest, Sha256};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use super::release::{Asset, Release, parse_latest};

/// GitHub's latest release of SoundSync (no drafts, no pre-releases).
pub const LATEST_URL: &str = "https://api.github.com/repos/Prushka/SoundSync/releases/latest";

/// Downloads larger than this are refused (a release zip is about 15 MB).
const MAX_DOWNLOAD: u64 = 256 << 20;

pub struct Client {
    agent: ureq::Agent,
}

fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(code @ (403 | 429)) => format!("GitHub answered {code}: too many checks from this network, try again later"),
        ureq::Error::StatusCode(code) => format!("GitHub answered {code}"),
        e => format!("cannot reach GitHub: {e}"),
    }
}

impl Client {
    pub fn new() -> Client {
        let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
        let config = ureq::Agent::config_builder()
            .tls_config(tls)
            .user_agent(format!("SoundSync/{}", soundsync_core::version::version()))
            .timeout_global(Some(Duration::from_secs(120)))
            .build();
        Client { agent: config.into() }
    }

    /// The latest release and its `asset` (see [`parse_latest`]).
    pub fn latest(&self, url: &str, asset: &str) -> Result<Release, String> {
        let mut reply = self.agent.get(url).header("Accept", "application/vnd.github+json").call().map_err(describe)?;
        let json = reply.body_mut().read_to_string().map_err(describe)?;
        parse_latest(&json, asset)
    }

    /// Downloads `asset`, reporting the bytes received so far, and checks its size and SHA-256.
    pub fn download(&self, asset: &Asset, progress: &mut dyn FnMut(u64)) -> Result<Vec<u8>, String> {
        if asset.size > MAX_DOWNLOAD {
            return Err(format!("{} is {} MB, too large for an update", asset.name, asset.size >> 20));
        }
        let mut reply = self.agent.get(&asset.url).call().map_err(describe)?;
        let mut reader = reply.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
        let mut bytes = Vec::with_capacity(asset.size as usize);
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let n = reader.read(&mut chunk).map_err(|e| format!("the download of {} broke off: {e}", asset.name))?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            progress(bytes.len() as u64);
        }
        if bytes.len() as u64 != asset.size {
            return Err(format!("the download has {} bytes, but the release states a size of {}", bytes.len(), asset.size));
        }
        if Sha256::digest(&bytes)[..] != asset.sha256[..] {
            return Err(format!("{} doesn't match the release's SHA-256 checksum", asset.name));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const LATEST: &str = include_str!("testdata/latest-build-8.json");

    /// Answers one HTTP request per `(status, body)` on 127.0.0.1, in order; returns its base URL
    /// and the paths requested so far.
    pub(in crate::update) fn serve(responses: Vec<(u16, Vec<u8>)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                log.lock().unwrap().push(line.split_whitespace().nth(1).unwrap_or("").to_string());
                loop {
                    line.clear();
                    if !reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                        break; // the blank line after the headers (or a closed connection)
                    }
                }
                let mut stream = reader.into_inner();
                let head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(&body));
            }
        });
        (url, seen)
    }

    fn asset(url: String, bytes: &[u8]) -> Asset {
        Asset { name: "x.zip".into(), url, size: bytes.len() as u64, sha256: Sha256::digest(bytes).into() }
    }

    #[test]
    fn latest_release_is_read_from_the_api() {
        let (url, seen) = serve(vec![(200, LATEST.as_bytes().to_vec())]);
        let r = Client::new().latest(&format!("{url}/repos/x/releases/latest"), "SoundSync-windows-x64.zip").unwrap();
        assert_eq!(r.build, 8);
        assert_eq!(*seen.lock().unwrap(), vec!["/repos/x/releases/latest".to_string()]);
    }

    #[test]
    fn downloads_are_checked_against_the_release_checksum() {
        let (url, _) = serve(vec![(200, b"abc".to_vec()), (200, b"abc".to_vec()), (200, b"abc".to_vec())]);
        let good = asset(format!("{url}/x.zip"), b"abc");
        let mut received = 0;
        assert_eq!(Client::new().download(&good, &mut |n| received = n).unwrap(), b"abc");
        assert_eq!(received, 3, "progress reports the bytes received");
        let tampered = Asset { sha256: [0; 32], ..good.clone() };
        let e = Client::new().download(&tampered, &mut |_| {}).unwrap_err();
        assert!(e.contains("checksum"), "{e}");
        let longer = Asset { size: 4, ..good };
        let e = Client::new().download(&longer, &mut |_| {}).unwrap_err();
        assert!(e.contains("size"), "{e}");
    }

    #[test]
    fn api_errors_are_reported() {
        let (url, _) = serve(vec![(403, b"rate limited".to_vec())]);
        let e = Client::new().latest(&format!("{url}/latest"), "SoundSync-windows-x64.zip").unwrap_err();
        assert!(e.contains("403"), "{e}");
    }
}
