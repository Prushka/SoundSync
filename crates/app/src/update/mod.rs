//! Self-update from GitHub releases: finds a newer build, downloads and checks it, and replaces
//! the copy that is running (the exe on Windows, SoundSync.app on macOS).

pub mod fetch;
pub mod install;
pub mod release;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use soundsync_core::Os;

use install::{Staged, Target};
use release::Release;

/// What the updater is doing, as the window and the tray show it.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    /// This copy can't update itself; says why.
    Unavailable(String),
    /// Not checked yet.
    Idle,
    Checking,
    UpToDate { checked: SystemTime },
    Available(Release),
    Downloading { release: Release, received: u64, total: u64 },
    /// Downloaded, unpacked and checked: the UI swaps it in and restarts.
    Ready { release: Release, staged: Staged },
    /// The UI took the staged build and is swapping it in and quitting.
    Restarting(Release),
    /// `release` is kept when there is one, so the user can retry or open its page.
    Failed { error: String, release: Option<Release> },
}

impl UpdateState {
    /// A bounded status for other devices; release URLs and file contents stay in this app.
    pub fn for_group(&self) -> soundsync_core::remote_update::UpdateState {
        use soundsync_core::remote_update::{UpdateState as Remote, MAX_TEXT, bounded};
        match self {
            Self::Unavailable(reason) => Remote::Unavailable(bounded(reason, MAX_TEXT)),
            Self::Idle => Remote::Idle,
            Self::Checking => Remote::Checking,
            Self::UpToDate { .. } => Remote::UpToDate,
            Self::Available(r) => Remote::Available { build: r.build },
            Self::Downloading { release, received, total } => Remote::Downloading { build: release.build, received: (*received).min(*total), total: *total },
            Self::Ready { release, .. } | Self::Restarting(release) => Remote::Restarting { build: release.build },
            Self::Failed { error, release } => Remote::Failed { error: bounded(error, MAX_TEXT), build: release.as_ref().map(|r| r.build) },
        }
    }

    /// The release an install would fetch now.
    pub fn installable(&self) -> Option<&Release> {
        match self {
            UpdateState::Available(r) | UpdateState::Failed { release: Some(r), .. } => Some(r),
            _ => None,
        }
    }

    fn busy(&self) -> bool {
        matches!(self, UpdateState::Checking | UpdateState::Downloading { .. } | UpdateState::Ready { .. } | UpdateState::Restarting(_))
    }
}

enum Request {
    Check,
    Install,
    InstallBuild(u32),
}

/// Checks GitHub on a background thread (after `first_check`, then every `every`, and when
/// asked) and downloads and stages a release when told to.
pub struct Updater {
    state: Arc<Mutex<UpdateState>>,
    requests: Option<Sender<Request>>,
}

struct Worker {
    state: Arc<Mutex<UpdateState>>,
    on_change: Box<dyn Fn() + Send>,
    target: Target,
    build: u32,
    api_url: String,
    asset: &'static str,
    client: fetch::Client,
}

impl Updater {
    pub fn start(target: Result<Target, String>, current_build: Option<u32>, api_url: String, first_check: Duration, every: Duration, on_change: Box<dyn Fn() + Send>) -> Updater {
        let ready = target.and_then(|t| {
            let build = current_build.ok_or_else(|| install::DEV_BUILD.to_string())?;
            let asset = release::asset_name(Os::current()).ok_or_else(|| "SoundSync updates itself only on Windows and macOS.".to_string())?;
            Ok((t, build, asset))
        });
        let (target, build, asset) = match ready {
            Ok(r) => r,
            Err(why) => return Updater { state: Arc::new(Mutex::new(UpdateState::Unavailable(why))), requests: None },
        };
        let state = Arc::new(Mutex::new(UpdateState::Idle));
        let (tx, rx) = unbounded();
        let worker = Worker { state: state.clone(), on_change, target, build, api_url, asset, client: fetch::Client::new() };
        let spawned = std::thread::Builder::new().name("soundsync-update".into()).spawn(move || worker.run(rx, first_check, every));
        if let Err(e) = spawned {
            *state.lock().unwrap() = UpdateState::Unavailable(format!("cannot start the updater: {e}"));
        }
        Updater { state, requests: Some(tx) }
    }

    pub fn state(&self) -> UpdateState {
        self.state.lock().unwrap().clone()
    }

    pub fn check_now(&self) {
        if let Some(tx) = &self.requests {
            let _ = tx.send(Request::Check);
        }
    }

    /// Downloads and stages the available release; does nothing when there is none.
    pub fn install(&self) {
        if let (Some(tx), true) = (&self.requests, self.state().installable().is_some()) {
            let _ = tx.send(Request::Install);
        }
    }

    /// A remote click selects an advertised build. Recheck GitHub before installing it.
    pub fn install_build(&self, build: u32) {
        if let Some(tx) = &self.requests {
            let _ = tx.send(Request::InstallBuild(build));
        }
    }

    /// The staged build, once: the state moves on to `Restarting`.
    pub fn take_ready(&self) -> Option<Staged> {
        let mut state = self.state.lock().unwrap();
        let UpdateState::Ready { release, staged } = state.clone() else { return None };
        *state = UpdateState::Restarting(release);
        Some(staged)
    }

    /// The UI couldn't swap the staged build in.
    pub fn swap_failed(&self, error: String) {
        let mut state = self.state.lock().unwrap();
        if let UpdateState::Restarting(r) = state.clone() {
            *state = UpdateState::Failed { error, release: Some(r) };
        }
    }
}

impl Worker {
    fn set(&self, s: UpdateState) {
        *self.state.lock().unwrap() = s;
        (self.on_change)();
    }

    fn run(self, requests: Receiver<Request>, first_check: Duration, every: Duration) {
        let mut next = Instant::now() + first_check;
        loop {
            match requests.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(Request::Check) => self.check(true),
                Ok(Request::Install) => self.install(),
                Ok(Request::InstallBuild(build)) => self.install_latest(build),
                Err(RecvTimeoutError::Timeout) => {
                    self.check(false);
                    next = Instant::now() + every;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// An automatic check that fails (offline, rate-limited) leaves the state as it was; a check
    /// the user asked for shows the error.
    fn check(&self, asked: bool) {
        let before = self.state.lock().unwrap().clone();
        if before.busy() {
            return;
        }
        self.set(UpdateState::Checking);
        match self.client.latest(&self.api_url, self.asset) {
            Ok(r) if r.newer_than(self.build) => {
                tracing::info!("update: {} is available", r.title);
                self.set(UpdateState::Available(r));
            }
            Ok(_) => self.set(UpdateState::UpToDate { checked: SystemTime::now() }),
            Err(error) => {
                tracing::warn!("update check failed: {error}");
                let release = before.installable().cloned();
                self.set(if asked { UpdateState::Failed { error, release } } else { before });
            }
        }
    }

    fn install(&self) {
        let Some(release) = self.state.lock().unwrap().installable().cloned() else { return };
        tracing::info!("update: downloading {}", release.title);
        let total = release.asset.size;
        self.set(UpdateState::Downloading { release: release.clone(), received: 0, total });
        let mut shown = Instant::now();
        let bytes = self.client.download(&release.asset, &mut |received| {
            if shown.elapsed() >= Duration::from_millis(100) {
                shown = Instant::now();
                self.set(UpdateState::Downloading { release: release.clone(), received, total });
            }
        });
        let staged = bytes.and_then(|zip| install::stage(&self.target, &zip, release.build));
        match staged {
            Ok(staged) => {
                tracing::info!("update: {} is ready at {}", release.title, staged.new.display());
                self.set(UpdateState::Ready { release, staged });
            }
            Err(error) => {
                tracing::warn!("update to {} failed: {error}", release.title);
                self.set(UpdateState::Failed { error, release: Some(release) });
            }
        }
    }

    fn install_latest(&self, build: u32) {
        if self.state.lock().unwrap().busy() {
            return;
        }
        self.check(true);
        let state = self.state.lock().unwrap().clone();
        match state {
            UpdateState::Available(r) if r.build == build => self.install(),
            UpdateState::Available(r) => self.set(UpdateState::Failed {
                error: format!("The latest release changed to build {}; choose Update again to install it.", r.build), release: Some(r),
            }),
            UpdateState::UpToDate { .. } => self.set(UpdateState::Failed {
                error: format!("Build {build} is no longer a newer published release; this device is up to date."), release: None,
            }),
            _ => {} // a failed check already explains the failure; never fall back to a cached download
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use fetch::tests::serve;
    use install::Target;

    const LATEST: &str = include_str!("testdata/latest-build-8.json");
    const HOUR: Duration = Duration::from_secs(3600);

    fn wait_for(updater: &Updater, ok: impl Fn(&UpdateState) -> bool) -> UpdateState {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let s = updater.state();
            if ok(&s) || Instant::now() > deadline {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn start(api: &str, build: u32, first_check: Duration) -> Updater {
        let exe = std::env::temp_dir().join("soundsync-updater-test").join("soundsync.exe");
        Updater::start(Ok(Target::Exe(exe)), Some(build), format!("{api}/latest"), first_check, HOUR, Box::new(|| {}))
    }

    #[test]
    fn a_newer_release_becomes_available() {
        let (url, _) = serve(vec![(200, LATEST.as_bytes().to_vec())]);
        let updater = start(&url, 7, Duration::ZERO);
        let s = wait_for(&updater, |s| matches!(s, UpdateState::Available(_)));
        assert!(matches!(&s, UpdateState::Available(r) if r.build == 8), "{s:?}");
    }

    #[test]
    fn the_same_build_is_up_to_date() {
        let (url, _) = serve(vec![(200, LATEST.as_bytes().to_vec())]);
        let updater = start(&url, 8, Duration::ZERO);
        let s = wait_for(&updater, |s| matches!(s, UpdateState::UpToDate { .. }));
        assert!(matches!(s, UpdateState::UpToDate { .. }), "{s:?}");
    }

    #[test]
    fn failed_automatic_check_stays_quiet() {
        let (url, seen) = serve(vec![(403, b"rate limited".to_vec()), (403, b"rate limited".to_vec())]);
        let updater = start(&url, 7, Duration::ZERO);
        let s = wait_for(&updater, |s| seen.lock().unwrap().len() == 1 && *s == UpdateState::Idle);
        assert_eq!(s, UpdateState::Idle, "an automatic check that fails shows nothing");
        updater.check_now();
        let s = wait_for(&updater, |s| matches!(s, UpdateState::Failed { .. }));
        assert!(matches!(&s, UpdateState::Failed { error, release: None } if error.contains("403")), "a check the user asked for says why it failed: {s:?}");
    }

    #[test]
    fn install_without_an_available_release_does_nothing() {
        let (url, seen) = serve(vec![(200, LATEST.as_bytes().to_vec())]);
        let updater = start(&url, 7, HOUR);
        updater.install();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(updater.state(), UpdateState::Idle);
        assert!(seen.lock().unwrap().is_empty(), "nothing was downloaded: {:?}", seen.lock().unwrap());
    }

    #[test]
    fn copies_that_cannot_update_never_check() {
        let (url, seen) = serve(vec![(200, LATEST.as_bytes().to_vec())]);
        let updater = Updater::start(Err("Only SoundSync.app updates itself.".into()), Some(7), format!("{url}/latest"), Duration::ZERO, HOUR, Box::new(|| {}));
        updater.check_now();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(updater.state(), UpdateState::Unavailable("Only SoundSync.app updates itself.".into()));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_remote_install_rechecks_latest_and_does_not_install_a_changed_release() {
        let (url, seen) = serve(vec![(200, LATEST.as_bytes().to_vec())]);
        let updater = start(&url, 6, HOUR);
        updater.install_build(7);
        let s = wait_for(&updater, |s| matches!(s, UpdateState::Failed { .. }));
        assert!(matches!(&s, UpdateState::Failed { error, release: Some(r) } if r.build == 8 && error.contains("changed")), "{s:?}");
        assert_eq!(*seen.lock().unwrap(), ["/latest"], "only metadata was fetched; no stale build or download supplied by another node");
    }

    #[test]
    fn a_remote_install_downloads_through_the_existing_verification_and_staging() {
        use sha2::{Digest, Sha256};
        let zip = b"not a valid zip";
        let (download, downloaded) = serve(vec![(200, zip.to_vec())]);
        let mut release: serde_json::Value = serde_json::from_str(LATEST).unwrap();
        for asset in release["assets"].as_array_mut().unwrap() {
            asset["browser_download_url"] = format!("{download}/update.zip").into();
            asset["size"] = zip.len().into();
            let digest: String = Sha256::digest(zip).iter().map(|b| format!("{b:02x}")).collect();
            asset["digest"] = format!("sha256:{digest}").into();
        }
        let (url, _) = serve(vec![(200, serde_json::to_vec(&release).unwrap())]);
        let updater = start(&url, 7, HOUR);
        updater.install_build(8);
        let s = wait_for(&updater, |s| matches!(s, UpdateState::Failed { .. }));
        assert!(matches!(&s, UpdateState::Failed { error, release: Some(r) } if r.build == 8 && error.contains("zip")), "{s:?}");
        assert_eq!(*downloaded.lock().unwrap(), ["/update.zip"]);
    }
}
