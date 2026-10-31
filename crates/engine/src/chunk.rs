//! Fixed-size audio packet copy passed from the network thread to the render thread through a
//! lock-free ring (no allocation per packet).

use soundsync_core::Nanos;
use soundsync_core::protocol::{AudioHeader, MAX_AUDIO_PAYLOAD, le_bytes_to_f32};

pub const CHUNK_SAMPLES: usize = MAX_AUDIO_PAYLOAD / 4;

#[derive(Clone, Copy)]
pub struct AudioChunk {
    pub header: AudioHeader,
    pub arrival_ns: Nanos,
    /// Valid samples in `samples`; 0 for a silent span of `header.frames` frames.
    pub len: u16,
    pub samples: [f32; CHUNK_SAMPLES],
}

impl AudioChunk {
    pub fn silent(header: AudioHeader, arrival_ns: Nanos) -> Self {
        AudioChunk { header, arrival_ns, len: 0, samples: [0.0; CHUNK_SAMPLES] }
    }

    pub fn from_payload(header: AudioHeader, payload: &[u8], arrival_ns: Nanos) -> Self {
        let mut c = AudioChunk::silent(header, arrival_ns);
        c.len = le_bytes_to_f32(payload, &mut c.samples) as u16;
        c
    }

    pub fn from_samples(header: AudioHeader, samples: &[f32], arrival_ns: Nanos) -> Self {
        let mut c = AudioChunk::silent(header, arrival_ns);
        let n = samples.len().min(CHUNK_SAMPLES);
        c.samples[..n].copy_from_slice(&samples[..n]);
        c.len = n as u16;
        c
    }

    pub fn samples(&self) -> &[f32] {
        &self.samples[..self.len as usize]
    }
}
