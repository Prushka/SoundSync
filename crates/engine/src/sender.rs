//! Master side: turns captured audio into packets and sends them to every subscriber (and, in
//! virtual mode, to this node's own mixer).
//!
//! Runs on the capture thread: after the first buffer of a format it neither allocates nor
//! locks. Packets carry `frames_per_packet(channels)` frames; leftover frames wait for the next
//! buffer. After 2 s of digital silence only small `Silent` packets go out, which keeps every
//! receiver's timeline running without ~12 Mbit/s of zeros.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use arc_swap::ArcSwap;
use soundsync_audio::{CaptureInfo, CaptureSink};
use soundsync_core::protocol::{AudioHeader, GroupKey, MAX_AUDIO_PAYLOAD, MAX_DATAGRAM, Packet, encode, f32_to_le_bytes, frames_per_packet};
use soundsync_core::{Nanos, StreamFormat};

use crate::chunk::AudioChunk;
use crate::net::PacketSink;

const SILENCE_AFTER_S: u64 = 2;

/// State shared between the capture thread (sender) and the engine thread.
pub struct SenderShared {
    pub subscribers: ArcSwap<Vec<SocketAddr>>,
    pub presentation_offset_ns: AtomicI64,
    pub volume_bits: AtomicU32,
    pub muted: AtomicBool,
    pub stream_id: AtomicU32,
    pub format: ArcSwap<Option<StreamFormat>>,
    /// Currently sending only `Silent` packets.
    pub silent: AtomicBool,
    pub packets_sent: AtomicU64,
}

impl SenderShared {
    pub fn new() -> Self {
        SenderShared {
            subscribers: ArcSwap::from_pointee(Vec::new()),
            presentation_offset_ns: AtomicI64::new(0),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            stream_id: AtomicU32::new(0),
            format: ArcSwap::from_pointee(None),
            silent: AtomicBool::new(false),
            packets_sent: AtomicU64::new(0),
        }
    }
}

impl Default for SenderShared {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CaptureSender {
    node_id: Arc<AtomicU64>,
    key: GroupKey,
    sink: Arc<dyn PacketSink>,
    shared: Arc<SenderShared>,
    local: Option<rtrb::Producer<AudioChunk>>,
    /// New producers for the local stream (the engine rebuilds the local slot when the output
    /// is reopened); the newest one replaces `local`.
    local_updates: Option<rtrb::Consumer<rtrb::Producer<AudioChunk>>>,
    now: Arc<dyn Fn() -> Nanos + Send + Sync>,
    format: Option<StreamFormat>,
    stream_id: u32,
    frame_index: u64,
    seq: u32,
    carry: Vec<f32>,
    carry_frames: usize,
    carry_time: Nanos,
    silent_run: u64,
    payload: [u8; MAX_AUDIO_PAYLOAD],
    datagram: Box<[u8; MAX_DATAGRAM]>,
}

impl CaptureSender {
    pub fn new(
        node_id: Arc<AtomicU64>,
        key: GroupKey,
        sink: Arc<dyn PacketSink>,
        shared: Arc<SenderShared>,
        local: Option<rtrb::Producer<AudioChunk>>,
        now: Arc<dyn Fn() -> Nanos + Send + Sync>,
    ) -> Self {
        CaptureSender {
            node_id,
            key,
            sink,
            shared,
            local,
            local_updates: None,
            now,
            format: None,
            stream_id: 0,
            frame_index: 0,
            seq: 0,
            carry: Vec::new(),
            carry_frames: 0,
            carry_time: 0,
            silent_run: 0,
            payload: [0; MAX_AUDIO_PAYLOAD],
            datagram: Box::new([0; MAX_DATAGRAM]),
        }
    }

    /// Lets the engine hand over a new producer for the local (monitor) stream at any time.
    pub fn with_local_updates(mut self, updates: rtrb::Consumer<rtrb::Producer<AudioChunk>>) -> Self {
        self.local_updates = Some(updates);
        self
    }

    fn start_stream(&mut self, format: StreamFormat) {
        let mut id = rand::random::<u32>();
        while id == 0 || id == self.stream_id {
            id = rand::random::<u32>();
        }
        self.stream_id = id;
        self.frame_index = 0;
        self.seq = 0;
        self.carry_frames = 0;
        self.silent_run = 0;
        self.carry = vec![0.0; frames_per_packet(format.channels) as usize * format.channels as usize];
        self.format = Some(format);
        self.shared.format.store(Arc::new(Some(format)));
        self.shared.stream_id.store(id, Ordering::Release);
        tracing::info!("capture stream {id:08x}: {} ch, {} Hz", format.channels, format.sample_rate);
    }

    fn header(&self, frames: usize, capture_time_ns: Nanos) -> AudioHeader {
        AudioHeader {
            stream_id: self.stream_id,
            seq: self.seq,
            frame_index: self.frame_index,
            frames: frames as u16,
            capture_time_ns,
            presentation_offset_ns: self.shared.presentation_offset_ns.load(Ordering::Relaxed),
            format: self.format.expect("format set before sending"),
            volume: f32::from_bits(self.shared.volume_bits.load(Ordering::Relaxed)),
            muted: self.shared.muted.load(Ordering::Relaxed),
        }
    }

    fn send(&mut self, packet: &Packet<'_>) {
        let node = self.node_id.load(Ordering::Relaxed);
        let Ok(n) = encode(node, packet, &self.key, &mut self.datagram) else { return };
        let subscribers = self.shared.subscribers.load();
        for addr in subscribers.iter() {
            self.sink.send_datagram(*addr, &self.datagram[..n]);
        }
        self.shared.packets_sent.fetch_add(1, Ordering::Relaxed);
    }

    fn emit_audio(&mut self, samples: &[f32], frames: usize, capture_time_ns: Nanos) {
        let header = self.header(frames, capture_time_ns);
        let bytes = f32_to_le_bytes(samples, &mut self.payload);
        let payload = self.payload;
        self.send(&Packet::Audio { header, payload: &payload[..bytes] });
        if let Some(local) = &mut self.local {
            let _ = local.push(AudioChunk::from_samples(header, samples, (self.now)()));
        }
        self.frame_index += frames as u64;
        self.seq = self.seq.wrapping_add(1);
    }

    fn emit_silent(&mut self, frames: usize, capture_time_ns: Nanos) {
        let header = self.header(frames, capture_time_ns);
        self.send(&Packet::Silent { header });
        if let Some(local) = &mut self.local {
            let _ = local.push(AudioChunk::silent(header, (self.now)()));
        }
        self.frame_index += frames as u64;
        self.seq = self.seq.wrapping_add(1);
    }

    fn flush_carry(&mut self) {
        if self.carry_frames > 0 {
            let ch = self.format.map_or(1, |f| f.channels as usize);
            let carry = std::mem::take(&mut self.carry);
            self.emit_audio(&carry[..self.carry_frames * ch], self.carry_frames, self.carry_time);
            self.carry = carry;
            self.carry_frames = 0;
        }
    }
}

impl CaptureSink for CaptureSender {
    fn on_capture(&mut self, data: &[f32], format: StreamFormat, info: CaptureInfo) {
        if let Some(updates) = &mut self.local_updates {
            while let Ok(p) = updates.pop() {
                self.local = Some(p);
            }
        }
        if self.format != Some(format) {
            self.flush_carry();
            self.start_stream(format);
        }
        let ch = format.channels as usize;
        let rate = format.sample_rate as u64;
        let frames = (data.len() / ch).min(info.frames.max(data.len() / ch));
        let time_of = |i: usize| info.time_ns + (i as u128 * 1_000_000_000 / rate as u128) as Nanos;
        if info.discontinuity {
            self.flush_carry();
        }

        let is_silent = info.silent || data.iter().all(|&x| x == 0.0);
        let suppress = is_silent && self.silent_run >= SILENCE_AFTER_S * rate;
        self.silent_run = if is_silent { self.silent_run + frames as u64 } else { 0 };
        self.shared.silent.store(suppress, Ordering::Relaxed);
        if suppress {
            self.flush_carry();
            let step = (rate / 100) as usize;
            let mut i = 0;
            while i < frames {
                let n = step.min(frames - i);
                self.emit_silent(n, time_of(i));
                i += n;
            }
            return;
        }

        let fpp = frames_per_packet(format.channels) as usize;
        let mut i = 0;
        if self.carry_frames > 0 {
            let take = (fpp - self.carry_frames).min(frames);
            self.carry[self.carry_frames * ch..(self.carry_frames + take) * ch].copy_from_slice(&data[..take * ch]);
            self.carry_frames += take;
            i = take;
            if self.carry_frames == fpp {
                self.flush_carry();
            }
        }
        while i + fpp <= frames {
            self.emit_audio(&data[i * ch..(i + fpp) * ch], fpp, time_of(i));
            i += fpp;
        }
        if i < frames {
            let rest = frames - i;
            self.carry[..rest * ch].copy_from_slice(&data[i * ch..frames * ch]);
            self.carry_frames = rest;
            self.carry_time = time_of(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use soundsync_core::protocol::{Packet, decode};

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(SocketAddr, Vec<u8>)>>);
    impl PacketSink for RecordingSink {
        fn send_datagram(&self, to: SocketAddr, bytes: &[u8]) {
            self.0.lock().unwrap().push((to, bytes.to_vec()));
        }
    }

    struct Decoded {
        header: AudioHeader,
        samples: Option<Vec<f32>>,
    }

    impl RecordingSink {
        fn decoded(&self, key: &GroupKey) -> Vec<Decoded> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(_, b)| match decode(b, key).unwrap().1 {
                    Packet::Audio { header, payload } => {
                        let mut s = vec![0.0; payload.len() / 4];
                        soundsync_core::protocol::le_bytes_to_f32(payload, &mut s);
                        Decoded { header, samples: Some(s) }
                    }
                    Packet::Silent { header } => Decoded { header, samples: None },
                    other => panic!("unexpected {other:?}"),
                })
                .collect()
        }
    }

    const T0: Nanos = 50_000_000_000;

    fn fmt(channels: u16) -> StreamFormat {
        StreamFormat { sample_rate: 48_000, channels, channel_mask: if channels == 8 { 0x63F } else { 0x3 } }
    }

    fn setup(local: bool) -> (CaptureSender, Arc<RecordingSink>, Arc<SenderShared>, GroupKey, Option<rtrb::Consumer<AudioChunk>>) {
        let sink = Arc::new(RecordingSink::default());
        let shared = Arc::new(SenderShared::new());
        shared.subscribers.store(Arc::new(vec!["127.0.0.1:9".parse().unwrap()]));
        shared.presentation_offset_ns.store(60_000_000, Ordering::Relaxed);
        let key = GroupKey::derive("test", "");
        let (tx, rx) = rtrb::RingBuffer::new(4096);
        let (tx, rx) = if local { (Some(tx), Some(rx)) } else { (None, None) };
        let s = CaptureSender::new(Arc::new(AtomicU64::new(77)), key.clone(), sink.clone(), shared.clone(), tx, Arc::new(|| T0 + 1_000_000));
        (s, sink, shared, key, rx)
    }

    fn info(time_ns: Nanos, frames: usize) -> CaptureInfo {
        CaptureInfo { time_ns, raw_time_ns: time_ns, frames, silent: false, discontinuity: false }
    }

    fn ramp(start: usize, frames: usize, ch: usize) -> Vec<f32> {
        (start * ch..(start + frames) * ch).map(|i| (i % 1000) as f32 * 1e-3 + 1e-4).collect()
    }

    #[test]
    fn packetizes_8ch_into_43_frame_packets() {
        let (mut s, sink, _, key, _) = setup(false);
        s.on_capture(&ramp(0, 480, 8), fmt(8), info(T0, 480));
        let p = sink.decoded(&key);
        assert_eq!(p.len(), 11);
        assert!(p.iter().all(|d| d.header.frames == 43 && d.header.format == fmt(8)));
        assert_eq!(p.iter().map(|d| d.header.frame_index).collect::<Vec<_>>(), (0..11).map(|k| k * 43).collect::<Vec<u64>>());
        assert_eq!(p.iter().map(|d| d.header.seq).collect::<Vec<_>>(), (0..11).collect::<Vec<u32>>());
        let all: Vec<f32> = p.iter().flat_map(|d| d.samples.clone().unwrap()).collect();
        assert_eq!(all, ramp(0, 473, 8), "payload bits are exact");
        s.on_capture(&ramp(480, 480, 8), fmt(8), info(T0 + 10_000_000, 480));
        let p = sink.decoded(&key);
        assert_eq!(p[11].header.frame_index, 473, "the 7 carried frames start the next packet");
        let all: Vec<f32> = p.iter().flat_map(|d| d.samples.clone().unwrap()).collect();
        assert_eq!(all, ramp(0, all.len() / 8, 8));
        assert_eq!(p[0].header.presentation_offset_ns, 60_000_000);
    }

    #[test]
    fn capture_times_follow_info_time() {
        let (mut s, sink, _, key, _) = setup(false);
        s.on_capture(&ramp(0, 480, 8), fmt(8), info(T0, 480));
        s.on_capture(&ramp(480, 480, 8), fmt(8), info(T0 + 10_000_000, 480));
        for d in sink.decoded(&key) {
            let want = T0 + (d.header.frame_index as f64 * 1e9 / 48_000.0).round() as Nanos;
            assert!((d.header.capture_time_ns - want).abs() <= 1, "frame {}: {} vs {want}", d.header.frame_index, d.header.capture_time_ns);
        }
    }

    #[test]
    fn silence_suppressed_after_2s() {
        let (mut s, sink, _, key, _) = setup(false);
        let zeros = vec![0.0f32; 480 * 2];
        for k in 0..250 {
            s.on_capture(&zeros, fmt(2), info(T0 + k * 10_000_000, 480));
        }
        let p = sink.decoded(&key);
        let total: u64 = p.iter().map(|d| d.header.frames as u64).sum();
        let silent_frames: u64 = p.iter().filter(|d| d.samples.is_none()).map(|d| d.header.frames as u64).sum();
        assert!((total as i64 - 120_000).abs() < 175, "frames {total}");
        assert!((23_000..=24_200).contains(&silent_frames), "silent frames {silent_frames}");
        assert!(p.iter().take_while(|d| d.samples.is_some()).count() > 500, "the first 2 s still go out as audio");
        let mut expect = 0;
        for d in &p {
            assert_eq!(d.header.frame_index, expect, "silent packets keep the timeline continuous");
            expect += d.header.frames as u64;
        }
    }

    #[test]
    fn audio_resumes_immediately_after_silence() {
        let (mut s, sink, _, key, _) = setup(false);
        let zeros = vec![0.0f32; 480 * 2];
        for k in 0..300 {
            s.on_capture(&zeros, fmt(2), info(T0 + k * 10_000_000, 480));
        }
        let before = sink.decoded(&key).len();
        s.on_capture(&ramp(0, 480, 2), fmt(2), info(T0 + 300 * 10_000_000, 480));
        let p = sink.decoded(&key);
        assert!(p[before..].iter().all(|d| d.samples.is_some()), "audio packets right away");
        assert!(p[before..].iter().map(|d| d.header.frames as usize).sum::<usize>() >= 350);
    }

    #[test]
    fn format_change_starts_new_stream() {
        let (mut s, sink, shared, key, _) = setup(false);
        s.on_capture(&ramp(0, 480, 2), fmt(2), info(T0, 480));
        let first = shared.stream_id.load(Ordering::Relaxed);
        s.on_capture(&ramp(0, 480, 8), fmt(8), info(T0 + 10_000_000, 480));
        let second = shared.stream_id.load(Ordering::Relaxed);
        assert_ne!(first, second);
        assert_eq!(**shared.format.load(), Some(fmt(8)));
        let p = sink.decoded(&key);
        let new: Vec<_> = p.iter().filter(|d| d.header.stream_id == second).collect();
        assert_eq!(new[0].header.frame_index, 0);
        assert_eq!(new[0].header.format, fmt(8));
        assert!(p.iter().filter(|d| d.header.stream_id == first).all(|d| d.header.format == fmt(2)));
    }

    #[test]
    fn local_producer_receives_same_chunks() {
        let (mut s, sink, _, key, rx) = setup(true);
        let mut rx = rx.unwrap();
        s.on_capture(&ramp(0, 480, 8), fmt(8), info(T0, 480));
        let sent = sink.decoded(&key);
        let mut local = Vec::new();
        while let Ok(c) = rx.pop() {
            local.push(c);
        }
        assert_eq!(local.len(), sent.len());
        for (c, d) in local.iter().zip(&sent) {
            assert_eq!(c.header, d.header);
            assert_eq!(c.samples(), &d.samples.clone().unwrap()[..]);
            assert_eq!(c.arrival_ns, T0 + 1_000_000);
        }
    }

    #[test]
    fn local_producer_can_be_replaced() {
        let (s, sink, _, key, _) = setup(false);
        let (mut upd_tx, upd_rx) = rtrb::RingBuffer::new(2);
        let mut s = s.with_local_updates(upd_rx);
        let (tx, mut rx) = rtrb::RingBuffer::new(64);
        assert!(upd_tx.push(tx).is_ok());
        s.on_capture(&ramp(0, 175, 2), fmt(2), info(T0, 175));
        assert_eq!(rx.slots(), 1, "one chunk delivered to the new local ring");
        let (tx2, mut rx2) = rtrb::RingBuffer::new(64);
        assert!(upd_tx.push(tx2).is_ok());
        s.on_capture(&ramp(175, 175, 2), fmt(2), info(T0 + 3_645_833, 175));
        assert!(rx2.pop().is_ok(), "chunks now go to the replacement");
        assert_eq!(sink.decoded(&key).len(), 2);
        let _ = rx.pop();
    }

    #[test]
    fn sends_to_every_subscriber_and_follows_volume() {
        let (mut s, sink, shared, key, _) = setup(false);
        shared.subscribers.store(Arc::new(vec!["127.0.0.1:9".parse().unwrap(), "127.0.0.1:10".parse().unwrap()]));
        shared.volume_bits.store(0.25f32.to_bits(), Ordering::Relaxed);
        shared.muted.store(true, Ordering::Relaxed);
        s.on_capture(&ramp(0, 175, 2), fmt(2), info(T0, 175));
        let raw = sink.0.lock().unwrap().clone();
        assert_eq!(raw.len(), 2);
        assert_ne!(raw[0].0, raw[1].0);
        let p = sink.decoded(&key);
        assert_eq!(p[0].header.volume, 0.25);
        assert!(p[0].header.muted);
    }
}
