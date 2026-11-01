//! Text shown in the window and tray.

use soundsync_core::player::PlayState;
use soundsync_core::{MasterMode, Nanos, Role};
use soundsync_engine::{Status, StreamStatus};

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
pub fn calib_note(r: &soundsync_core::calib::CalibResult, check: bool) -> (String, bool) {
    let mut notes = Vec::new();
    if check && r.delta_ns.abs() > 1_000_000 {
        notes.push(format!("off by {:.1} ms (Correct these fixes it)", r.delta_ns.abs() as f64 / 1e6));
    }
    if check && r.level_db > 1.0 {
        notes.push(format!("{:.1} dB louder (Correct these turns it down)", r.level_db));
    }
    notes.extend(r.problems());
    if notes.is_empty() {
        return ((if check { "✓ in sync" } else { "✓" }).into(), true);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(calib_note(&louder, true), ("2.4 dB louder (Correct these turns it down)".to_string(), false));
        let close = soundsync_core::calib::CalibResult { level_db: 0.8, ..result(0.2, 40.0, 0.0, 1, 1) };
        assert_eq!(calib_note(&close, true), ("✓ in sync".to_string(), true));
        // In a calibration the Level column says it; Apply turns it down.
        assert_eq!(calib_note(&louder, false), ("✓".to_string(), true));
    }

    #[test]
    fn calibration_result_labels() {
        assert_eq!(fmt_snr(41.6), "42 dB");
        assert_eq!(fmt_snr(f32::INFINITY), "> 60 dB");
        // Calibration: corrections with their doubts.
        assert_eq!(calib_note(&result(12.0, 41.0, 0.1, 3, 3), false), ("✓".to_string(), true));
        assert_eq!(calib_note(&result(12.0, 24.0, 2.0, 2, 3), false), ("heard 2 of 3 sweeps; sweeps disagree by 2.0 ms; weak signal (24 dB)".to_string(), false));
        // Check run: what is left.
        assert_eq!(calib_note(&result(0.4, 40.0, 0.0, 1, 1), true), ("✓ in sync".to_string(), true));
        assert_eq!(calib_note(&result(-2.5, 40.0, 0.0, 1, 1), true), ("off by 2.5 ms (Correct these fixes it)".to_string(), false));
        assert_eq!(calib_note(&result(-2.5, 22.0, 0.0, 1, 1), true), ("off by 2.5 ms (Correct these fixes it); weak signal (22 dB)".to_string(), false));
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
