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
