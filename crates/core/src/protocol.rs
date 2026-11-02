//! Wire format.
//!
//! Every datagram is `[MAGIC 4][version 1][type 1][sender u64 LE 8][body][tag 8]`, where the tag is
//! the first 8 bytes of HMAC-SHA256(group key, everything before the tag).
//!
//! * type 1 (Control): body is the postcard encoding of [`Control`].
//! * type 16 (Audio): body is a fixed 49-byte little-endian [`AudioHeader`] followed by raw
//!   interleaved `f32` LE samples, exactly as captured.
//! * type 17 (Silent): header only; `frames` counts digital-silence frames.
//!
//! Times inside control messages are in the sender's clock.

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::calib::{CalibKind, CalibResult};
use crate::types::{MasterMode, Nanos, NodeId, Os, Role, StreamFormat};

pub const MAGIC: [u8; 4] = *b"SSYN";
/// 3: separate latency and volume calibration kinds in requests, sessions and results.
pub const PROTOCOL_VERSION: u8 = 3;
pub const HEADER_LEN: usize = 14;
pub const AUDIO_HEADER_LEN: usize = 49;
pub const TAG_LEN: usize = 8;
pub const MAX_DATAGRAM: usize = 1472;
pub const MAX_AUDIO_PAYLOAD: usize = 1400;
pub const DEFAULT_PORT: u16 = 47810;

/// Presentation offsets beyond this are nonsense (the group delay tops out at 2 s).
const MAX_OFFSET_NS: Nanos = 10_000_000_000;

const TYPE_CONTROL: u8 = 1;
const TYPE_AUDIO: u8 = 16;
const TYPE_SILENT: u8 = 17;

type HmacSha256 = Hmac<Sha256>;

/// 256-bit key shared by every node in a group; derived from the group name and optional secret.
#[derive(Clone, PartialEq, Eq)]
pub struct GroupKey([u8; 32]);

impl GroupKey {
    pub fn derive(group: &str, secret: &str) -> GroupKey {
        let mut h = Sha256::new();
        h.update(b"soundsync-v1|");
        h.update(group.as_bytes());
        h.update(b"|");
        h.update(secret.as_bytes());
        GroupKey(h.finalize().into())
    }

    fn mac(&self) -> HmacSha256 {
        <HmacSha256 as KeyInit>::new_from_slice(&self.0).expect("HMAC accepts any key length")
    }
}

impl std::fmt::Debug for GroupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupKey(..)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioHeader {
    /// Random per capture session; changes when the capture format changes.
    pub stream_id: u32,
    pub seq: u32,
    /// Index of the first frame in this packet, counted from the start of the stream.
    pub frame_index: u64,
    pub frames: u16,
    /// Master-clock time at which the first frame was captured (mixed).
    pub capture_time_ns: Nanos,
    /// Intended time from capture to acoustic emission on every node.
    pub presentation_offset_ns: Nanos,
    pub format: StreamFormat,
    /// Master endpoint volume (scalar 0..=1), for receivers that follow master volume.
    pub volume: f32,
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub reply: bool,
    pub name: String,
    pub os: Os,
    pub app_version: String,
    pub group: String,
    /// Random per process start; distinguishes restarts and node-id collisions.
    pub session: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub stream_id: u32,
    pub format: StreamFormat,
    pub mode: MasterMode,
    pub presentation_offset_ns: Nanos,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub device: String,
    pub format: StreamFormat,
    pub device_latency_ns: Nanos,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RxStat {
    pub master: NodeId,
    pub stream_id: u32,
    pub sync_error_ns: Nanos,
    pub behind_ns: Nanos,
    pub lost_frames: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    pub name: String,
    pub os: Os,
    pub session: u64,
    pub role: Role,
    pub stream: Option<StreamInfo>,
    pub output: Option<OutputInfo>,
    pub extra_latency_ns: Nanos,
    pub required_delay_ns: Nanos,
    /// Plays other masters' streams (so a new stream's group delay must already cover it).
    pub plays_remote: bool,
    /// The node's Volume slider (0..=1) and mute.
    pub volume: f32,
    pub muted: bool,
    /// The slider is the output device's own volume (else SoundSync's level on that node).
    pub device_volume: bool,
    pub rx: Vec<RxStat>,
}

/// A change to a node's volume, made from any node.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum VolumeChange {
    /// Moves the Volume slider (0..=1).
    Set(f32),
    Muted(bool),
    /// Turns the node down by this many dB from wherever it is (volume matching).
    TurnDown { db: f32 },
}

/// A node's connected microphones (and line inputs).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputList {
    pub default: Option<String>,
    pub devices: Vec<String>,
}

/// Largest single turn-down volume matching asks for.
pub const MAX_TURN_DOWN_DB: f32 = 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CalibSignal {
    Sweep,
    Click { freq_hz: u16 },
    /// Soft noise that wakes TVs and soundbars muted by silence before the sweeps.
    Wake,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CalibMsg {
    /// `check`: one sweep per device, to measure what is left after applying corrections.
    Request { mic_device: String, kind: CalibKind, check: bool },
    Begin { session: u32, until_ns: Nanos, kind: CalibKind },
    Play { session: u32, at_ns: Nanos, signal: CalibSignal },
    End { session: u32 },
    Result { session: u32, results: Vec<CalibResult>, kind: CalibKind, check: bool, error: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Control {
    Hello(Hello),
    Ping { t1: Nanos },
    Pong { t1: Nanos, t2: Nanos, t3: Nanos },
    State(NodeState),
    Subscribe { stream_id: u32 },
    Unsubscribe { stream_id: u32 },
    SetOffset { target: NodeId, extra_latency_ns: Nanos },
    SetVolume { target: NodeId, change: VolumeChange },
    /// Asks a node for its microphones; it answers with `Inputs`.
    ListInputs,
    Inputs(InputList),
    Calib(CalibMsg),
    SyncTest { active: bool, epoch_ns: Nanos },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Packet<'a> {
    Control(Control),
    Audio { header: AudioHeader, payload: &'a [u8] },
    Silent { header: AudioHeader },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("packet does not fit in a datagram")]
    TooLarge,
    #[error("control message failed to serialize")]
    Serialize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("datagram too short")]
    TooShort,
    #[error("not a SoundSync datagram")]
    BadMagic,
    #[error("unsupported protocol version {0}")]
    BadVersion(u8),
    #[error("authentication tag mismatch (wrong group or key, or corrupted)")]
    BadTag,
    #[error("unknown packet type {0}")]
    UnknownType(u8),
    #[error("malformed packet body")]
    Malformed,
}

/// Whether `c` fits in one datagram.
pub fn control_fits(c: &Control) -> bool {
    let mut room = [0u8; MAX_DATAGRAM - HEADER_LEN - TAG_LEN];
    postcard::to_slice(c, &mut room).is_ok()
}

/// Frames of `channels`-channel f32 audio that fit in one packet.
pub fn frames_per_packet(channels: u16) -> u16 {
    (MAX_AUDIO_PAYLOAD / (channels.max(1) as usize * 4)) as u16
}

/// Writes `src` as little-endian f32 into `dst`; returns bytes written.
pub fn f32_to_le_bytes(src: &[f32], dst: &mut [u8]) -> usize {
    let n = src.len().min(dst.len() / 4);
    for (s, d) in src[..n].iter().zip(dst.as_chunks_mut::<4>().0) {
        *d = s.to_le_bytes();
    }
    n * 4
}

/// Reads little-endian f32 from `src` into `dst`; returns samples written.
pub fn le_bytes_to_f32(src: &[u8], dst: &mut [f32]) -> usize {
    let n = (src.len() / 4).min(dst.len());
    for (d, s) in dst[..n].iter_mut().zip(src.as_chunks::<4>().0) {
        *d = f32::from_le_bytes(*s);
    }
    n
}

pub fn encode(sender: NodeId, p: &Packet<'_>, key: &GroupKey, buf: &mut [u8; MAX_DATAGRAM]) -> Result<usize, EncodeError> {
    buf[..4].copy_from_slice(&MAGIC);
    buf[4] = PROTOCOL_VERSION;
    buf[6..HEADER_LEN].copy_from_slice(&sender.to_le_bytes());
    let body_end = match p {
        Packet::Control(c) => {
            buf[5] = TYPE_CONTROL;
            let room = &mut buf[HEADER_LEN..MAX_DATAGRAM - TAG_LEN];
            let used = postcard::to_slice(c, room).map_err(|e| match e {
                postcard::Error::SerializeBufferFull => EncodeError::TooLarge,
                _ => EncodeError::Serialize,
            })?;
            HEADER_LEN + used.len()
        }
        Packet::Audio { header, payload } => {
            if payload.len() > MAX_AUDIO_PAYLOAD {
                return Err(EncodeError::TooLarge);
            }
            buf[5] = TYPE_AUDIO;
            write_audio_header(header, &mut buf[HEADER_LEN..HEADER_LEN + AUDIO_HEADER_LEN]);
            let start = HEADER_LEN + AUDIO_HEADER_LEN;
            buf[start..start + payload.len()].copy_from_slice(payload);
            start + payload.len()
        }
        Packet::Silent { header } => {
            buf[5] = TYPE_SILENT;
            write_audio_header(header, &mut buf[HEADER_LEN..HEADER_LEN + AUDIO_HEADER_LEN]);
            HEADER_LEN + AUDIO_HEADER_LEN
        }
    };
    let mut mac = key.mac();
    mac.update(&buf[..body_end]);
    let tag = mac.finalize().into_bytes();
    buf[body_end..body_end + TAG_LEN].copy_from_slice(&tag[..TAG_LEN]);
    Ok(body_end + TAG_LEN)
}

pub fn decode<'a>(buf: &'a [u8], key: &GroupKey) -> Result<(NodeId, Packet<'a>), DecodeError> {
    if buf.len() < HEADER_LEN + TAG_LEN {
        return Err(DecodeError::TooShort);
    }
    if buf[..4] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    if buf[4] != PROTOCOL_VERSION {
        return Err(DecodeError::BadVersion(buf[4]));
    }
    let body_end = buf.len() - TAG_LEN;
    let mut mac = key.mac();
    mac.update(&buf[..body_end]);
    mac.verify_truncated_left(&buf[body_end..]).map_err(|_| DecodeError::BadTag)?;

    let sender = u64::from_le_bytes(buf[6..HEADER_LEN].try_into().expect("8 bytes"));
    let body = &buf[HEADER_LEN..body_end];
    let packet = match buf[5] {
        TYPE_CONTROL => {
            let control: Control = postcard::from_bytes(body).map_err(|_| DecodeError::Malformed)?;
            if !control_ok(&control) {
                return Err(DecodeError::Malformed);
            }
            Packet::Control(control)
        }
        TYPE_AUDIO => {
            let header = read_audio_header(body)?;
            let payload = &body[AUDIO_HEADER_LEN..];
            let frame_bytes = header.format.channels as usize * 4;
            if payload.len() != header.frames as usize * frame_bytes {
                return Err(DecodeError::Malformed);
            }
            Packet::Audio { header, payload }
        }
        TYPE_SILENT => {
            if body.len() != AUDIO_HEADER_LEN {
                return Err(DecodeError::Malformed);
            }
            Packet::Silent { header: read_audio_header(body)? }
        }
        t => return Err(DecodeError::UnknownType(t)),
    };
    Ok((sender, packet))
}

fn write_audio_header(h: &AudioHeader, out: &mut [u8]) {
    let mut w = Writer { buf: out, pos: 0 };
    w.put(&h.stream_id.to_le_bytes());
    w.put(&h.seq.to_le_bytes());
    w.put(&h.frame_index.to_le_bytes());
    w.put(&h.frames.to_le_bytes());
    w.put(&h.capture_time_ns.to_le_bytes());
    w.put(&h.presentation_offset_ns.to_le_bytes());
    w.put(&h.format.sample_rate.to_le_bytes());
    w.put(&h.format.channels.to_le_bytes());
    w.put(&h.format.channel_mask.to_le_bytes());
    w.put(&h.volume.to_le_bytes());
    w.put(&[h.muted as u8]);
    debug_assert_eq!(w.pos, AUDIO_HEADER_LEN);
}

fn read_audio_header(body: &[u8]) -> Result<AudioHeader, DecodeError> {
    if body.len() < AUDIO_HEADER_LEN {
        return Err(DecodeError::Malformed);
    }
    let mut r = Reader { buf: body, pos: 0 };
    let h = AudioHeader {
        stream_id: u32::from_le_bytes(r.take()),
        seq: u32::from_le_bytes(r.take()),
        frame_index: u64::from_le_bytes(r.take()),
        frames: u16::from_le_bytes(r.take()),
        capture_time_ns: i64::from_le_bytes(r.take()),
        presentation_offset_ns: i64::from_le_bytes(r.take()),
        format: StreamFormat {
            sample_rate: u32::from_le_bytes(r.take()),
            channels: u16::from_le_bytes(r.take()),
            channel_mask: u32::from_le_bytes(r.take()),
        },
        volume: f32::from_le_bytes(r.take()),
        muted: {
            let [flags] = r.take::<1>();
            flags & 1 != 0
        },
    };
    if !h.format.is_valid() || !(0.0..=1.0).contains(&h.volume) || h.presentation_offset_ns.abs() > MAX_OFFSET_NS {
        return Err(DecodeError::Malformed);
    }
    Ok(h)
}

/// Values in gossiped state that later size buffers or enter time arithmetic.
fn control_ok(c: &Control) -> bool {
    match c {
        Control::State(s) => {
            s.stream.as_ref().is_none_or(|i| i.format.is_valid() && i.presentation_offset_ns.abs() <= MAX_OFFSET_NS)
                && s.output.as_ref().is_none_or(|o| o.format.is_valid())
                && (0.0..=1.0).contains(&s.volume)
        }
        Control::SetVolume { change: VolumeChange::Set(v), .. } => (0.0..=1.0).contains(v),
        Control::SetVolume { change: VolumeChange::TurnDown { db }, .. } => (0.0..=MAX_TURN_DOWN_DB).contains(db),
        Control::Calib(CalibMsg::Result { results, .. }) => {
            results.iter().all(|r| !r.snr_db.is_nan() && r.level_db.is_finite() && r.level_spread_db.is_finite() && !r.peak_dbfs.is_nan() && r.heard <= r.sweeps)
        }
        _ => true,
    }
}

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) {
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let out: [u8; N] = self.buf[self.pos..self.pos + N].try_into().expect("length checked by caller");
        self.pos += N;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MasterMode, Os, Role, StreamFormat};

    fn hdr() -> AudioHeader {
        AudioHeader {
            stream_id: 7,
            seq: 42,
            frame_index: 1_000_000,
            frames: 43,
            capture_time_ns: 123_456_789,
            presentation_offset_ns: 80_000_000,
            format: StreamFormat { sample_rate: 48_000, channels: 8, channel_mask: 0x63F },
            volume: 0.5,
            muted: false,
        }
    }

    fn encode_audio(key: &GroupKey, samples: &[f32], buf: &mut [u8; MAX_DATAGRAM]) -> usize {
        let mut bytes = vec![0u8; samples.len() * 4];
        assert_eq!(f32_to_le_bytes(samples, &mut bytes), bytes.len());
        encode(99, &Packet::Audio { header: hdr(), payload: &bytes }, key, buf).unwrap()
    }

    #[test]
    fn audio_roundtrip_preserves_header_and_payload() {
        let key = GroupKey::derive("home", "");
        let samples: Vec<f32> = (0..43 * 8).map(|i| i as f32 * 1e-3 - 0.1).collect();
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = encode_audio(&key, &samples, &mut buf);
        assert_eq!(n, HEADER_LEN + AUDIO_HEADER_LEN + samples.len() * 4 + TAG_LEN);
        let (sender, p) = decode(&buf[..n], &key).unwrap();
        assert_eq!(sender, 99);
        let Packet::Audio { header, payload } = p else { panic!("expected audio, got {p:?}") };
        assert_eq!(header, hdr());
        let mut out = vec![0f32; samples.len()];
        assert_eq!(le_bytes_to_f32(payload, &mut out), samples.len());
        let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&out), bits(&samples));
    }

    #[test]
    fn wrong_key_is_rejected() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = encode_audio(&GroupKey::derive("home", ""), &[0.25; 43 * 8], &mut buf);
        assert_eq!(decode(&buf[..n], &GroupKey::derive("office", "")).unwrap_err(), DecodeError::BadTag);
    }

    #[test]
    fn secret_changes_key() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = encode_audio(&GroupKey::derive("home", "s3cret"), &[0.25; 43 * 8], &mut buf);
        assert_eq!(decode(&buf[..n], &GroupKey::derive("home", "")).unwrap_err(), DecodeError::BadTag);
    }

    #[test]
    fn tampered_byte_is_rejected() {
        let key = GroupKey::derive("home", "");
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = encode_audio(&key, &[0.25; 43 * 8], &mut buf);
        buf[20] ^= 0x01;
        assert_eq!(decode(&buf[..n], &key).unwrap_err(), DecodeError::BadTag);
    }

    #[test]
    fn control_messages_roundtrip() {
        let key = GroupKey::derive("home", "");
        let fmt = StreamFormat { sample_rate: 48_000, channels: 2, channel_mask: 0x3 };
        let all = vec![
            Control::Hello(Hello {
                reply: true,
                name: "Living Room".into(),
                os: Os::Windows,
                app_version: "0.1.0".into(),
                group: "home".into(),
                session: 0xDEAD_BEEF,
            }),
            Control::Ping { t1: -5 },
            Control::Pong { t1: 1, t2: 2, t3: 3 },
            Control::State(NodeState {
                name: "TV".into(),
                os: Os::MacOs,
                session: 9,
                role: Role::Master(MasterMode::Virtual),
                stream: Some(StreamInfo {
                    stream_id: 77,
                    format: fmt,
                    mode: MasterMode::Direct,
                    presentation_offset_ns: 45_000_000,
                }),
                output: Some(OutputInfo {
                    device: "LG TV SSCR2 (NVIDIA High Definition Audio)".into(),
                    format: fmt,
                    device_latency_ns: 12_000_000,
                }),
                extra_latency_ns: -3_000_000,
                required_delay_ns: 61_000_000,
                plays_remote: true,
                volume: 0.72,
                muted: true,
                device_volume: true,
                rx: vec![
                    RxStat { master: 1, stream_id: 2, sync_error_ns: -150, behind_ns: 0, lost_frames: 4 },
                    RxStat { master: 3, stream_id: 4, sync_error_ns: 90, behind_ns: 45_000_000, lost_frames: 0 },
                ],
            }),
            Control::Subscribe { stream_id: 5 },
            Control::Unsubscribe { stream_id: 6 },
            Control::SetOffset { target: 42, extra_latency_ns: 25_000_000 },
            Control::SetVolume { target: 42, change: VolumeChange::Set(0.35) },
            Control::SetVolume { target: 42, change: VolumeChange::Muted(true) },
            Control::SetVolume { target: 42, change: VolumeChange::TurnDown { db: 4.5 } },
            Control::ListInputs,
            Control::Inputs(InputList { default: Some("MacBook Pro Microphone".into()), devices: vec!["MacBook Pro Microphone".into(), "AirPods".into()] }),
            Control::Calib(CalibMsg::Request { mic_device: "Microphone".into(), kind: CalibKind::Latency, check: true }),
            Control::Calib(CalibMsg::Request { mic_device: "Microphone".into(), kind: CalibKind::Volume, check: false }),
            Control::Calib(CalibMsg::Begin { session: 1, until_ns: 10, kind: CalibKind::Latency }),
            Control::Calib(CalibMsg::Begin { session: 2, until_ns: 10, kind: CalibKind::Volume }),
            Control::Calib(CalibMsg::Play { session: 1, at_ns: 11, signal: CalibSignal::Sweep }),
            Control::Calib(CalibMsg::Play { session: 1, at_ns: 12, signal: CalibSignal::Click { freq_hz: 800 } }),
            Control::Calib(CalibMsg::Play { session: 1, at_ns: 13, signal: CalibSignal::Wake }),
            Control::Calib(CalibMsg::End { session: 1 }),
            Control::Calib(CalibMsg::Result {
                session: 1,
                results: vec![
                    CalibResult { node: 1, delta_ns: -35_000_000, snr_db: 41.5, spread_ns: 120_000, heard: 3, sweeps: 3, level_db: 0.0, peak_dbfs: -12.5, level_spread_db: 0.4 },
                    CalibResult { node: 2, delta_ns: 145_000_000, snr_db: 27.0, spread_ns: 0, heard: 1, sweeps: 3, level_db: 4.5, peak_dbfs: -3.1, level_spread_db: 0.0 },
                ],
                kind: CalibKind::Volume,
                check: false,
                error: Some("no signal from TV".into()),
            }),
            Control::SyncTest { active: true, epoch_ns: 1_000 },
        ];
        for c in all {
            let mut buf = [0u8; MAX_DATAGRAM];
            let n = encode(3, &Packet::Control(c.clone()), &key, &mut buf).unwrap();
            let (sender, p) = decode(&buf[..n], &key).unwrap();
            assert_eq!(sender, 3);
            assert_eq!(p, Packet::Control(c));
        }
    }

    #[test]
    fn out_of_range_formats_and_values_are_rejected() {
        // Formats from the network size buffers and resamplers: a bad one must never get that far.
        let key = GroupKey::derive("home", "");
        let mut buf = [0u8; MAX_DATAGRAM];
        let state = |format: StreamFormat, offset: Nanos| {
            Control::State(NodeState {
                name: "x".into(),
                os: Os::Windows,
                session: 1,
                role: Role::Master(MasterMode::Virtual),
                stream: Some(StreamInfo { stream_id: 1, format, mode: MasterMode::Virtual, presentation_offset_ns: offset }),
                output: None,
                extra_latency_ns: 0,
                required_delay_ns: 0,
                plays_remote: true,
                volume: 1.0,
                muted: false,
                device_volume: true,
                rx: Vec::new(),
            })
        };
        let good = StreamFormat { sample_rate: 48_000, channels: 8, channel_mask: 0x63F };
        let n = encode(5, &Packet::Control(state(good, 80_000_000)), &key, &mut buf).unwrap();
        assert!(decode(&buf[..n], &key).is_ok());
        for (format, offset) in [
            (StreamFormat { sample_rate: 0, ..good }, 80_000_000),
            (StreamFormat { sample_rate: 10_000_000, ..good }, 80_000_000),
            (StreamFormat { channels: 0, ..good }, 80_000_000),
            (StreamFormat { channels: 65_535, ..good }, 80_000_000),
            (good, Nanos::MAX),
        ] {
            let n = encode(5, &Packet::Control(state(format, offset)), &key, &mut buf).unwrap();
            assert_eq!(decode(&buf[..n], &key), Err(DecodeError::Malformed), "{format:?} offset {offset}");
        }
        let Control::State(good_state) = state(good, 80_000_000) else { unreachable!() };
        let calib_result = |level_db: f32| CalibResult { node: 1, delta_ns: 0, snr_db: 40.0, spread_ns: 0, heard: 3, sweeps: 3, level_db, peak_dbfs: -12.0, level_spread_db: 0.0 };
        for control in [
            Control::State(NodeState { volume: 1.5, ..good_state.clone() }),
            Control::State(NodeState { volume: f32::NAN, ..good_state }),
            Control::SetVolume { target: 1, change: VolumeChange::Set(-0.1) },
            Control::SetVolume { target: 1, change: VolumeChange::Set(f32::NAN) },
            Control::SetVolume { target: 1, change: VolumeChange::TurnDown { db: -3.0 } },
            Control::SetVolume { target: 1, change: VolumeChange::TurnDown { db: f32::INFINITY } },
            Control::Calib(CalibMsg::Result { session: 1, results: vec![calib_result(f32::NAN)], kind: CalibKind::Latency, check: false, error: None }),
        ] {
            let n = encode(5, &Packet::Control(control.clone()), &key, &mut buf).unwrap();
            assert_eq!(decode(&buf[..n], &key), Err(DecodeError::Malformed), "{control:?}");
        }
        for header in [
            AudioHeader { format: StreamFormat { channels: 4_000, ..good }, ..hdr() },
            AudioHeader { volume: 2.0, ..hdr() },
            AudioHeader { volume: -0.5, ..hdr() },
            AudioHeader { presentation_offset_ns: -Nanos::MAX, ..hdr() },
        ] {
            let n = encode(5, &Packet::Silent { header }, &key, &mut buf).unwrap();
            assert_eq!(decode(&buf[..n], &key), Err(DecodeError::Malformed), "{header:?}");
        }
    }

    #[test]
    fn another_protocol_version_is_reported_as_such() {
        // Older versions cannot distinguish latency runs from volume runs. Reject them clearly
        // instead of allowing an old client to apply both corrections to a volume-only result.
        let key = GroupKey::derive("home", "");
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = encode(5, &Packet::Control(Control::Ping { t1: 1 }), &key, &mut buf).unwrap();
        assert_eq!(PROTOCOL_VERSION, 3);
        for old in [1, 2] {
            buf[4] = old;
            assert_eq!(decode(&buf[..n], &key), Err(DecodeError::BadVersion(old)));
        }
    }

    #[test]
    fn control_fits_tells_whether_a_message_fits_a_datagram() {
        let list = |n: usize| Control::Inputs(InputList { default: None, devices: vec!["Microphone (USB Audio Device)".into(); n] });
        assert!(control_fits(&list(3)));
        assert!(!control_fits(&list(100)));
        let key = GroupKey::derive("home", "");
        let mut buf = [0u8; MAX_DATAGRAM];
        let largest = (1..100).take_while(|&n| control_fits(&list(n))).last().unwrap();
        assert!(encode(1, &Packet::Control(list(largest)), &key, &mut buf).is_ok());
        assert_eq!(encode(1, &Packet::Control(list(largest + 1)), &key, &mut buf), Err(EncodeError::TooLarge));
    }

    #[test]
    fn silent_roundtrip() {
        let key = GroupKey::derive("home", "");
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = encode(5, &Packet::Silent { header: hdr() }, &key, &mut buf).unwrap();
        assert_eq!(n, 14 + 49 + 8);
        assert_eq!(decode(&buf[..n], &key).unwrap(), (5, Packet::Silent { header: hdr() }));
    }

    #[test]
    fn frames_per_packet_fits_payload() {
        for ch in 1..=8u16 {
            assert!(frames_per_packet(ch) as usize * ch as usize * 4 <= MAX_AUDIO_PAYLOAD, "ch={ch}");
        }
        assert_eq!(frames_per_packet(8), 43);
        assert_eq!(frames_per_packet(2), 175);
    }

    #[test]
    fn oversized_payload_fails_to_encode() {
        let key = GroupKey::derive("home", "");
        let payload = vec![0u8; 1404];
        let mut buf = [0u8; MAX_DATAGRAM];
        assert_eq!(
            encode(1, &Packet::Audio { header: hdr(), payload: &payload }, &key, &mut buf).unwrap_err(),
            EncodeError::TooLarge
        );
    }

    #[test]
    fn audio_with_inconsistent_length_is_malformed() {
        // Correctly signed, but the payload length doesn't match frames * channels * 4.
        let key = GroupKey::derive("home", "");
        let payload = vec![0u8; 100];
        let mut buf = [0u8; MAX_DATAGRAM];
        let n = encode(1, &Packet::Audio { header: hdr(), payload: &payload }, &key, &mut buf).unwrap();
        assert_eq!(decode(&buf[..n], &key).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn garbage_is_rejected_without_panic() {
        let key = GroupKey::derive("home", "");
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for i in 0..10_000 {
            let len = (next() % 2000) as usize;
            let mut buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if i % 2 == 0 && len >= HEADER_LEN {
                // Valid prefix so the decoder has to get past magic/version.
                buf[..4].copy_from_slice(&MAGIC);
                buf[4] = PROTOCOL_VERSION;
            }
            assert!(decode(&buf, &key).is_err(), "iteration {i} decoded garbage");
        }
    }
}
