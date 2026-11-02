//! Text shown in the window and tray.

use soundsync_core::calib::CalibKind;
use soundsync_core::player::PlayState;
use soundsync_core::{MasterMode, Nanos, Role};
use soundsync_engine::{Status, StreamStatus};

use std::time::SystemTime;

use crate::update::UpdateState;

pub fn fmt_ms(ns: Nanos) -> String {
    format!("{:.1} ms", ns as f64 / 1e6)
}

pub fn fmt_opt_ms(ns: Option<Nanos>) -> String {
    ns.map_or_else(|| "—".to_string(), fmt_ms)
}

pub fn sync_label(s: &StreamStatus) -> String {
    match s.state {
        PlayState::Waiting => "Waiting…".into(),
        PlayState::Playing => format!("In sync (±{:.1} ms)", s.error_ns.abs() as f64 / 1e6),
        PlayState::Behind => format!("Behind {:.0} ms", s.behind_ns as f64 / 1e6),
    }
}

/// A measurement's signal-to-noise for display.
pub fn fmt_snr(db: f32) -> String {
    if db > 60.0 { "> 60 dB".into() } else { format!("{db:.0} dB") }
}

/// A speaker's level in calibration results: how much louder than the quietest one it arrived.
pub fn fmt_level(db: f32) -> String {
    if db < 0.05 { "quietest".into() } else { format!("+{db:.1} dB") }
}

/// Note for one device's calibration result and whether it looks good. A check run (`check`)
/// shows what is left, which is fine within the 1 ms and 1 dB people notice.
pub fn calib_note(r: &soundsync_core::calib::CalibResult, kind: CalibKind, check: bool) -> (String, bool) {
    let mut notes = Vec::new();
    if check && kind == CalibKind::Latency && r.delta_ns.abs() > 1_000_000 {
        notes.push(format!("off by {:.1} ms (Apply latency fixes it)", r.delta_ns.abs() as f64 / 1e6));
    }
    if check && kind == CalibKind::Volume && r.level_db > 1.0 {
        notes.push(format!("{:.1} dB louder (Apply volume turns it down)", r.level_db));
    }
    notes.extend(r.problems(kind));
    if notes.is_empty() {
        return ((match (kind, check) {
            (_, false) => "✓",
            (CalibKind::Latency, true) => "✓ in sync",
            (CalibKind::Volume, true) => "✓ matched",
        }).into(), true);
    }
    (notes.join("; "), false)
}

pub fn role_label(r: Role) -> &'static str {
    match r {
        Role::Receiver => "Listener",
        Role::Master(MasterMode::Virtual) => "Master (VB-CABLE)",
        Role::Master(MasterMode::Direct) => "Master (direct)",
    }
}

pub fn tray_status_line(st: &Status) -> String {
    if st.broadcasting {
        let listeners = st.peers.iter().filter(|p| p.rx.iter().any(|r| r.master == st.node_id)).count();
        return format!("Broadcasting · {listeners} listener{}", if listeners == 1 { "" } else { "s" });
    }
    if let Some(first) = st.streams.iter().find(|s| s.master != st.node_id) {
        let name = st.peers.iter().find(|p| p.id == first.master).map_or("another PC", |p| p.name.as_str());
        let others = st.streams.iter().filter(|s| s.master != st.node_id).count() - 1;
        return if others == 0 { format!("Playing from {name}") } else { format!("Playing from {name} + {others} more") };
    }
    if st.peers.is_empty() { "Idle · no other devices".into() } else { format!("Idle · {} devices", st.peers.len() + 1) }
}

fn percent(received: u64, total: u64) -> u64 {
    (received * 100).checked_div(total).unwrap_or(0).min(100)
}

/// The update line under the header (every tab), when there is something to act on.
pub fn update_line(s: &UpdateState) -> Option<String> {
    match s {
        UpdateState::Available(r) => {
            Some(format!("SoundSync build {} is available. Update every device in the group: different versions may not be able to talk.", r.build))
        }
        UpdateState::Downloading { release, received, total } => Some(format!("Downloading build {}… {}%", release.build, percent(*received, *total))),
        UpdateState::Ready { release, .. } | UpdateState::Restarting(release) => Some(format!("Restarting into build {}…", release.build)),
        UpdateState::Failed { error, release: Some(r) } => Some(format!("Update to build {} failed: {error}", r.build)),
        _ => None,
    }
}

/// What Settings ▸ Updates says.
pub fn update_status(s: &UpdateState, now: SystemTime) -> String {
    match s {
        UpdateState::Unavailable(why) => why.clone(),
        UpdateState::Idle => "Not checked yet".into(),
        UpdateState::Checking => "Checking for updates…".into(),
        UpdateState::UpToDate { checked } => {
            let mins = now.duration_since(*checked).unwrap_or_default().as_secs() / 60;
            match mins {
                0 => "Up to date (checked just now)".into(),
                1..60 => format!("Up to date (checked {mins} min ago)"),
                _ => format!("Up to date (checked {} h ago)", mins / 60),
            }
        }
        UpdateState::Available(r) => format!("Build {} is available", r.build),
        UpdateState::Failed { error, .. } => error.clone(),
        busy => update_line(busy).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::release::{Asset, Release};

    fn build(n: u32) -> Release {
        let asset = Asset { name: "SoundSync-windows-x64.zip".into(), url: "https://example/x.zip".into(), size: 10 << 20, sha256: [0; 32] };
        Release { tag: format!("v0.1.0-build.{n}"), title: format!("SoundSync 0.1.0 (build {n})"), build: n, page_url: "https://example/release".into(), asset }
    }

    #[test]
    fn update_line_names_the_build_and_the_group() {
        assert_eq!(
            update_line(&UpdateState::Available(build(9))).as_deref(),
            Some("SoundSync build 9 is available. Update every device in the group: different versions may not be able to talk.")
        );
        let downloading = UpdateState::Downloading { release: build(9), received: 5 << 20, total: 10 << 20 };
        assert_eq!(update_line(&downloading).as_deref(), Some("Downloading build 9… 50%"));
        let failed = UpdateState::Failed { error: "GitHub answered 500".into(), release: Some(build(9)) };
        assert_eq!(update_line(&failed).as_deref(), Some("Update to build 9 failed: GitHub answered 500"));
        assert_eq!(update_line(&UpdateState::Restarting(build(9))).as_deref(), Some("Restarting into build 9…"));
        for quiet in [UpdateState::Idle, UpdateState::Checking, UpdateState::UpToDate { checked: SystemTime::UNIX_EPOCH }, UpdateState::Unavailable("dev".into())] {
            assert_eq!(update_line(&quiet), None, "{quiet:?}");
        }
        assert_eq!(update_line(&UpdateState::Failed { error: "offline".into(), release: None }), None, "a failed check is shown in Settings only");
    }

    #[test]
    fn update_status_says_what_the_updater_is_doing() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10_000);
        let ago = |secs: u64| UpdateState::UpToDate { checked: now - std::time::Duration::from_secs(secs) };
        assert_eq!(update_status(&ago(20), now), "Up to date (checked just now)");
        assert_eq!(update_status(&ago(180), now), "Up to date (checked 3 min ago)");
        assert_eq!(update_status(&ago(7_300), now), "Up to date (checked 2 h ago)");
        assert_eq!(update_status(&UpdateState::Unavailable("This is a development build; it doesn't update itself.".into()), now), "This is a development build; it doesn't update itself.");
        assert_eq!(update_status(&UpdateState::Checking, now), "Checking for updates…");
        assert_eq!(update_status(&UpdateState::Idle, now), "Not checked yet");
        assert_eq!(update_status(&UpdateState::Available(build(9)), now), "Build 9 is available");
        assert_eq!(update_status(&UpdateState::Failed { error: "offline".into(), release: None }, now), "offline");
    }

    fn result(delta_ms: f64, snr_db: f32, spread_ms: f64, heard: u8, sweeps: u8) -> soundsync_core::calib::CalibResult {
        soundsync_core::calib::CalibResult {
            node: 1,
            delta_ns: (delta_ms * 1e6) as Nanos,
            snr_db,
            spread_ns: (spread_ms * 1e6) as Nanos,
            heard,
            sweeps,
            level_db: 0.0,
            peak_dbfs: -20.0,
            level_spread_db: 0.0,
        }
    }

    #[test]
    fn levels_and_what_a_check_leaves() {
        assert_eq!(fmt_level(0.0), "quietest");
        assert_eq!(fmt_level(4.06), "+4.1 dB");
        // A check: within 1 dB counts as matched, like 1 ms for timing.
        let louder = soundsync_core::calib::CalibResult { level_db: 2.4, ..result(0.2, 40.0, 0.0, 1, 1) };
        assert_eq!(calib_note(&louder, CalibKind::Volume, true), ("2.4 dB louder (Apply volume turns it down)".to_string(), false));
        assert_eq!(calib_note(&louder, CalibKind::Latency, true), ("✓ in sync".to_string(), true));
        let close = soundsync_core::calib::CalibResult { level_db: 0.8, ..result(0.2, 40.0, 0.0, 1, 1) };
        assert_eq!(calib_note(&close, CalibKind::Volume, true), ("✓ matched".to_string(), true));
        // In a calibration the Level column says it; Apply turns it down.
        assert_eq!(calib_note(&louder, CalibKind::Volume, false), ("✓".to_string(), true));
        assert_eq!(calib_note(&result(25.0, 40.0, 2.0, 3, 3), CalibKind::Volume, true), ("✓ matched".to_string(), true));
    }

    #[test]
    fn calibration_result_labels() {
        assert_eq!(fmt_snr(41.6), "42 dB");
        assert_eq!(fmt_snr(f32::INFINITY), "> 60 dB");
        // Calibration: corrections with their doubts.
        assert_eq!(calib_note(&result(12.0, 41.0, 0.1, 3, 3), CalibKind::Latency, false), ("✓".to_string(), true));
        assert_eq!(calib_note(&result(12.0, 24.0, 2.0, 2, 3), CalibKind::Latency, false), ("heard 2 of 3 sweeps; sweeps disagree by 2.0 ms; weak signal (24 dB)".to_string(), false));
        // Check run: what is left.
        assert_eq!(calib_note(&result(0.4, 40.0, 0.0, 1, 1), CalibKind::Latency, true), ("✓ in sync".to_string(), true));
        assert_eq!(calib_note(&result(-2.5, 40.0, 0.0, 1, 1), CalibKind::Latency, true), ("off by 2.5 ms (Apply latency fixes it)".to_string(), false));
        assert_eq!(calib_note(&result(-2.5, 22.0, 0.0, 1, 1), CalibKind::Latency, true), ("off by 2.5 ms (Apply latency fixes it); weak signal (22 dB)".to_string(), false));
    }
    use soundsync_core::player::PlayState;
    use soundsync_core::protocol::RxStat;
    use soundsync_core::{MasterMode, Os, StreamFormat};
    use soundsync_engine::PeerStatus;

    fn stream(state: PlayState, error_ns: Nanos, behind_ns: Nanos) -> StreamStatus {
        StreamStatus {
            master: 7,
            stream_id: 1,
            format: StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 3 },
            state,
            error_ns,
            behind_ns,
            lost_frames: 0,
            layout_warning: None,
            master_volume: 1.0,
        }
    }

    fn peer(id: u64, name: &str, rx_from: Option<u64>) -> PeerStatus {
        PeerStatus {
            id,
            name: name.into(),
            os: Os::Windows,
            addr: "192.168.1.2:47810".parse().unwrap(),
            role: Role::Receiver,
            rtt_ns: None,
            jitter_ns: None,
            clock_offset_ns: None,
            extra_latency_ns: 0,
            output: None,
            app_version: "0.1.0 (dev)".into(),
            volume: None,
            inputs: None,
            rx: rx_from.map(|m| vec![RxStat { master: m, stream_id: 1, sync_error_ns: 0, behind_ns: 0, lost_frames: 0 }]).unwrap_or_default(),
        }
    }

    #[test]
    fn fmt_ms_rounds_to_tenths() {
        assert_eq!(fmt_ms(12_345_678), "12.3 ms");
        assert_eq!(fmt_ms(-1_260_000), "-1.3 ms");
        assert_eq!(fmt_opt_ms(None), "—");
        assert_eq!(fmt_opt_ms(Some(500_000)), "0.5 ms");
    }

    #[test]
    fn sync_label_states() {
        assert_eq!(sync_label(&stream(PlayState::Waiting, 0, 0)), "Waiting…");
        assert_eq!(sync_label(&stream(PlayState::Playing, -180_000, 0)), "In sync (±0.2 ms)");
        assert_eq!(sync_label(&stream(PlayState::Behind, 0, 45_000_000)), "Behind 45 ms");
    }

    #[test]
    fn role_labels() {
        assert_eq!(role_label(Role::Receiver), "Listener");
        assert_eq!(role_label(Role::Master(MasterMode::Virtual)), "Master (VB-CABLE)");
        assert_eq!(role_label(Role::Master(MasterMode::Direct)), "Master (direct)");
    }

    #[test]
    fn tray_status_line_variants() {
        let mut st = Status { node_id: 1, broadcasting: true, ..Status::default() };
        st.peers = vec![peer(2, "Kitchen", Some(1)), peer(3, "TV", Some(1)), peer(4, "Mac", None)];
        assert_eq!(tray_status_line(&st), "Broadcasting · 2 listeners");
        st.peers.truncate(1);
        assert_eq!(tray_status_line(&st), "Broadcasting · 1 listener");
        let mut st = Status { node_id: 1, ..Status::default() };
        st.peers = vec![peer(7, "Living Room", None), peer(8, "TV", None)];
        st.streams = vec![stream(PlayState::Playing, 0, 0)];
        assert_eq!(tray_status_line(&st), "Playing from Living Room");
        st.streams.clear();
        assert_eq!(tray_status_line(&st), "Idle · 3 devices");
        st.peers.clear();
        assert_eq!(tray_status_line(&st), "Idle · no other devices");
    }
}
