//! Frame-indexed jitter buffer: packets land at their frame position, so a late or reordered
//! packet still fills its hole as long as it arrives before that frame is read.

/// Result of storing a span of frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    Stored,
    /// Entirely older than what the buffer still holds (already read, discarded, or evicted).
    TooOld,
    /// So far ahead of the buffered data that everything was cleared first (stream restart/gap).
    Reset,
}

/// Ring of `capacity_frames` interleaved frames with one presence flag per frame.
/// All allocation happens in [`JitterBuffer::new`].
#[derive(Clone, Debug)]
pub struct JitterBuffer {
    channels: usize,
    capacity: usize,
    pub(crate) data: Vec<f32>,
    present: Vec<bool>,
    end: Option<u64>,
    floor: u64,
}

impl JitterBuffer {
    pub fn new(channels: usize, capacity_frames: usize) -> Self {
        assert!(channels > 0 && capacity_frames > 0);
        Self {
            channels,
            capacity: capacity_frames,
            data: vec![0.0; channels * capacity_frames],
            present: vec![false; capacity_frames],
            end: None,
            floor: 0,
        }
    }

    pub fn insert(&mut self, frame_index: u64, samples: &[f32]) -> Insert {
        self.store(frame_index, samples.len() / self.channels, Some(samples))
    }

    pub fn insert_silence(&mut self, frame_index: u64, frames: usize) -> Insert {
        self.store(frame_index, frames, None)
    }

    /// Oldest frame index the ring can still hold.
    fn lower_bound(&self) -> u64 {
        let end = self.end.unwrap_or(0);
        self.floor.max(end.saturating_sub(self.capacity as u64))
    }

    fn store(&mut self, frame_index: u64, frames: usize, samples: Option<&[f32]>) -> Insert {
        let mut result = Insert::Stored;
        if let Some(end) = self.end
            && frame_index >= end + self.capacity as u64
        {
            self.clear();
            result = Insert::Reset;
        }
        let span_end = frame_index + frames as u64;
        if self.end.is_some() && span_end <= self.lower_bound() {
            return Insert::TooOld;
        }
        // Advancing `end` evicts the frames one capacity behind; their slots become holes.
        let old_end = self.end.unwrap_or(frame_index);
        if span_end > old_end {
            let first_new = old_end.max(span_end.saturating_sub(self.capacity as u64));
            for f in first_new..span_end {
                self.present[(f % self.capacity as u64) as usize] = false;
            }
            self.end = Some(span_end);
        } else if self.end.is_none() {
            self.end = Some(span_end);
        }
        let lower = self.lower_bound();
        for (i, f) in (frame_index..span_end).enumerate() {
            if f < lower {
                continue;
            }
            let slot = (f % self.capacity as u64) as usize;
            let dst = &mut self.data[slot * self.channels..(slot + 1) * self.channels];
            match samples {
                Some(s) => dst.copy_from_slice(&s[i * self.channels..(i + 1) * self.channels]),
                None => dst.fill(0.0),
            }
            self.present[slot] = true;
        }
        result
    }

    /// Copies `out.len() / channels` frames starting at `frame_index`; returns how many were
    /// missing (those are zero-filled).
    pub fn read(&mut self, frame_index: u64, out: &mut [f32]) -> usize {
        let frames = out.len() / self.channels;
        let lower = self.lower_bound();
        let end = self.end.unwrap_or(0);
        let mut missing = 0;
        for (i, f) in (frame_index..frame_index + frames as u64).enumerate() {
            let dst = &mut out[i * self.channels..(i + 1) * self.channels];
            let slot = (f % self.capacity as u64) as usize;
            if self.end.is_some() && f >= lower && f < end && self.present[slot] {
                dst.copy_from_slice(&self.data[slot * self.channels..(slot + 1) * self.channels]);
            } else {
                dst.fill(0.0);
                missing += 1;
            }
        }
        missing
    }

    pub fn discard_before(&mut self, frame_index: u64) {
        self.floor = self.floor.max(frame_index);
    }

    /// One past the newest stored frame.
    pub fn end_frame(&self) -> Option<u64> {
        self.end
    }

    pub fn clear(&mut self) {
        self.present.fill(false);
        self.end = None;
        self.floor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stereo frames whose samples encode their frame index: (f, -f).
    fn frames(start: u64, n: usize) -> Vec<f32> {
        (start..start + n as u64).flat_map(|f| [f as f32, -(f as f32)]).collect()
    }

    #[test]
    fn in_order_read_returns_samples() {
        let mut jb = JitterBuffer::new(2, 4800);
        assert!(matches!(jb.insert(0, &frames(0, 43)), Insert::Stored));
        assert!(matches!(jb.insert(43, &frames(43, 43)), Insert::Stored));
        assert_eq!(jb.end_frame(), Some(86));
        let mut out = vec![9.0; 86 * 2];
        assert_eq!(jb.read(0, &mut out), 0);
        assert_eq!(out, frames(0, 86));
    }

    #[test]
    fn late_packet_fills_hole_before_read() {
        let mut jb = JitterBuffer::new(2, 4800);
        jb.insert(0, &frames(0, 43));
        jb.insert(86, &frames(86, 43));
        jb.insert(43, &frames(43, 43));
        let mut out = vec![0.0; 129 * 2];
        assert_eq!(jb.read(0, &mut out), 0);
        assert_eq!(out, frames(0, 129));
    }

    #[test]
    fn missing_frames_are_zero_and_counted() {
        let mut jb = JitterBuffer::new(2, 4800);
        jb.insert(0, &frames(0, 43));
        jb.insert(86, &frames(86, 43));
        let mut out = vec![7.0; 129 * 2];
        assert_eq!(jb.read(0, &mut out), 43);
        assert!(out[43 * 2..86 * 2].iter().all(|&x| x == 0.0));
        assert_eq!(&out[..43 * 2], &frames(0, 43)[..]);
        assert_eq!(&out[86 * 2..], &frames(86, 43)[..]);
    }

    #[test]
    fn too_old_insert_is_rejected() {
        let mut jb = JitterBuffer::new(2, 4800);
        jb.insert(1000, &frames(1000, 43));
        jb.discard_before(1000);
        assert!(matches!(jb.insert(500, &frames(500, 43)), Insert::TooOld));
        // Older than the ring can hold is also too old.
        jb.insert(10_000, &frames(10_000, 43));
        assert!(matches!(jb.insert(5_000, &frames(5_000, 43)), Insert::TooOld));
    }

    #[test]
    fn ring_wraparound_does_not_leak_stale_frames() {
        let mut jb = JitterBuffer::new(2, 100);
        jb.insert(0, &frames(0, 50));
        // Jump ahead by less than capacity past end: frames 50..150 are a hole, not stale 0..50 data.
        jb.insert(150, &frames(150, 10));
        let mut out = vec![0.0; 10 * 2];
        assert_eq!(jb.read(100, &mut out), 10, "slots of evicted frames must read as missing");
    }

    #[test]
    fn far_future_insert_resets() {
        let mut jb = JitterBuffer::new(2, 4800);
        jb.insert(0, &frames(0, 43));
        assert!(matches!(jb.insert(10_000_000, &frames(10_000_000, 43)), Insert::Reset));
        assert_eq!(jb.end_frame(), Some(10_000_043));
        let mut out = vec![0.0; 43 * 2];
        assert_eq!(jb.read(0, &mut out), 43);
        assert_eq!(jb.read(10_000_000, &mut out), 0);
        assert_eq!(out, frames(10_000_000, 43));
    }

    #[test]
    fn silence_insert_marks_present_zero() {
        let mut jb = JitterBuffer::new(2, 4800);
        jb.insert(0, &frames(0, 43));
        assert!(matches!(jb.insert_silence(43, 480), Insert::Stored));
        let mut out = vec![5.0; 480 * 2];
        assert_eq!(jb.read(43, &mut out), 0);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(jb.end_frame(), Some(523));
    }

    #[test]
    fn no_allocation_after_new() {
        let mut jb = JitterBuffer::new(8, 9600);
        let ptr = jb.data.as_ptr();
        let mut out = vec![0.0; 43 * 8];
        for i in 0..10_000u64 {
            jb.insert(i * 43, &vec![0.5; 43 * 8]);
            jb.read(i * 43, &mut out);
        }
        assert_eq!(jb.data.as_ptr(), ptr);
    }
}
