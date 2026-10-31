//! Speaker layouts (WAVEFORMATEXTENSIBLE channel masks) and channel mapping between layouts.
//!
//! Channel order within an interleaved frame follows ascending mask bits, as on Windows.

use crate::types::StreamFormat;

pub mod speaker {
    pub const FL: u32 = 0x1;
    pub const FR: u32 = 0x2;
    pub const FC: u32 = 0x4;
    pub const LFE: u32 = 0x8;
    pub const BL: u32 = 0x10;
    pub const BR: u32 = 0x20;
    pub const FLC: u32 = 0x40;
    pub const FRC: u32 = 0x80;
    pub const BC: u32 = 0x100;
    pub const SL: u32 = 0x200;
    pub const SR: u32 = 0x400;
}

use speaker::*;

const HALF_POWER: f32 = std::f32::consts::FRAC_1_SQRT_2;

pub fn default_mask(channels: u16) -> u32 {
    match channels {
        1 => FC,
        2 => FL | FR,
        4 => FL | FR | BL | BR,
        6 => 0x3F,
        8 => 0x63F,
        n => ((1u64 << n.min(32)) - 1) as u32,
    }
}

/// The format's mask, or the default for its channel count when the mask is missing or wrong.
pub fn effective_mask(f: StreamFormat) -> u32 {
    if f.channel_mask != 0 && f.channel_mask.count_ones() == f.channels as u32 {
        f.channel_mask
    } else {
        default_mask(f.channels)
    }
}

pub fn describe(f: StreamFormat) -> String {
    let mask = effective_mask(f);
    match mask {
        0x4 => "Mono".into(),
        0x3 => "Stereo".into(),
        0x33 | 0x603 => "Quad".into(),
        0x3F | 0x60F => "5.1".into(),
        0x63F => "7.1".into(),
        0xFF => "7.1 (wide)".into(),
        _ => format!("{} ch (0x{:X})", f.channels, mask),
    }
}

/// Maps CoreAudio `AudioChannelLabel` values to a WAVEFORMATEXTENSIBLE mask.
pub fn mask_from_coreaudio_labels(labels: &[u32]) -> u32 {
    let has_rear = labels.iter().any(|&l| l == 33 || l == 34);
    labels.iter().fold(0, |mask, &label| {
        mask | match label {
            1 => FL,
            2 => FR,
            3 => FC,
            4 => LFE,
            5 if has_rear => SL,
            6 if has_rear => SR,
            5 => BL,
            6 => BR,
            7 => FLC,
            8 => FRC,
            9 => BC,
            10 => SL,
            11 => SR,
            33 => BL,
            34 => BR,
            _ => 0,
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapKind {
    Identity,
    /// Same speakers except side↔back surrounds swapped 1:1 — not a real mismatch.
    Substituted,
    /// Some source speakers folded into others (or LFE dropped).
    Downmix,
    /// All source speakers placed; some destination speakers stay silent.
    Upmix,
}

/// Matrix from a source layout to a destination layout.
#[derive(Clone, Debug)]
pub struct ChannelMap {
    src_channels: usize,
    dst_channels: usize,
    kind: MapKind,
    matrix: Vec<f32>,
    /// Non-zero entries as (dst, src, coeff), for the apply loop.
    taps: Vec<(usize, usize, f32)>,
}

fn channel_of(mask: u32, speaker: u32) -> Option<usize> {
    (mask & speaker != 0).then(|| (mask & (speaker - 1)).count_ones() as usize)
}

impl ChannelMap {
    pub fn new(src: StreamFormat, dst: StreamFormat) -> ChannelMap {
        let (sm, dm) = (effective_mask(src), effective_mask(dst));
        let (sc, dc) = (src.channels as usize, dst.channels as usize);
        let mut matrix = vec![0.0f32; sc * dc];
        let mut folded = false;
        let mut substituted = false;

        for bit in 0..32 {
            let spk = 1u32 << bit;
            let Some(s) = channel_of(sm, spk) else { continue };
            let mut set = |target: u32, c: f32| -> bool {
                match channel_of(dm, target) {
                    Some(d) => {
                        matrix[d * sc + s] += c;
                        true
                    }
                    None => false,
                }
            };
            if set(spk, 1.0) {
                continue;
            }
            let swap = match spk {
                SL => BL,
                BL => SL,
                SR => BR,
                BR => SR,
                _ => 0,
            };
            if swap != 0 && dm & spk == 0 && set(swap, 1.0) {
                substituted = true;
                continue;
            }
            folded = true;
            match spk {
                LFE => {}
                FC => {
                    set(FL, HALF_POWER);
                    set(FR, HALF_POWER);
                }
                FLC => {
                    if !set(FL, 1.0) {
                        set(FC, HALF_POWER);
                    }
                }
                FRC => {
                    if !set(FR, 1.0) {
                        set(FC, HALF_POWER);
                    }
                }
                FL | FR => {
                    set(FC, HALF_POWER);
                }
                SL | BL => {
                    if !set(FL, HALF_POWER) {
                        set(FC, 0.5);
                    }
                }
                SR | BR => {
                    if !set(FR, HALF_POWER) {
                        set(FC, 0.5);
                    }
                }
                BC => {
                    let surround = (set(SL, 0.5) | set(SR, 0.5)) || (set(BL, 0.5) | set(BR, 0.5));
                    if !surround {
                        set(FL, 0.5);
                        set(FR, 0.5);
                    }
                }
                _ => {}
            }
        }

        let dst_covered = (0..dc).all(|d| (0..sc).any(|s| matrix[d * sc + s] != 0.0));
        let kind = if folded {
            MapKind::Downmix
        } else if !dst_covered {
            MapKind::Upmix
        } else if substituted || sm != dm {
            MapKind::Substituted
        } else {
            MapKind::Identity
        };
        let taps = (0..dc)
            .flat_map(|d| (0..sc).map(move |s| (d, s)))
            .filter_map(|(d, s)| {
                let c = matrix[d * sc + s];
                (c != 0.0).then_some((d, s, c))
            })
            .collect();
        ChannelMap { src_channels: sc, dst_channels: dc, kind, matrix, taps }
    }

    pub fn kind(&self) -> MapKind {
        self.kind
    }

    pub fn is_mismatch(&self) -> bool {
        matches!(self.kind, MapKind::Downmix | MapKind::Upmix)
    }

    pub fn coeff(&self, dst_ch: usize, src_ch: usize) -> f32 {
        self.matrix[dst_ch * self.src_channels + src_ch]
    }

    /// `dst += M · src · gain` over interleaved frames.
    pub fn apply_add(&self, src: &[f32], dst: &mut [f32], gain: f32) {
        let frames = (src.len() / self.src_channels.max(1)).min(dst.len() / self.dst_channels.max(1));
        if self.kind == MapKind::Identity {
            let n = frames * self.src_channels;
            for (d, s) in dst[..n].iter_mut().zip(&src[..n]) {
                *d += s * gain;
            }
            return;
        }
        for f in 0..frames {
            let s = &src[f * self.src_channels..(f + 1) * self.src_channels];
            let d = &mut dst[f * self.dst_channels..(f + 1) * self.dst_channels];
            for &(dc, sc, c) in &self.taps {
                d[dc] += s[sc] * c * gain;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;

    fn fmt(channels: u16, mask: u32) -> StreamFormat {
        StreamFormat { sample_rate: 48_000, channels, channel_mask: mask }
    }

    /// Index of `speaker` among the set bits of `mask` (channel order = ascending bits).
    fn idx(mask: u32, speaker: u32) -> usize {
        assert!(mask & speaker != 0);
        (mask & (speaker - 1)).count_ones() as usize
    }

    #[test]
    fn identical_masks_are_identity() {
        let m = ChannelMap::new(fmt(8, 0x63F), fmt(8, 0x63F));
        assert_eq!(m.kind(), MapKind::Identity);
        assert!(!m.is_mismatch());
        let src: Vec<f32> = (0..8 * 10).map(|i| (i as f32 * 0.37).sin() * 0.9).collect();
        let mut dst = vec![0.0f32; src.len()];
        m.apply_add(&src, &mut dst, 1.0);
        assert_eq!(dst.iter().map(|x| x.to_bits()).collect::<Vec<_>>(), src.iter().map(|x| x.to_bits()).collect::<Vec<_>>());
    }

    #[test]
    fn side_and_back_substitute() {
        let m = ChannelMap::new(fmt(6, 0x3F), fmt(6, 0x60F));
        assert_eq!(m.kind(), MapKind::Substituted);
        assert!(!m.is_mismatch());
        assert_eq!(m.coeff(idx(0x60F, SL), idx(0x3F, BL)), 1.0);
        assert_eq!(m.coeff(idx(0x60F, SR), idx(0x3F, BR)), 1.0);
        assert_eq!(m.coeff(idx(0x60F, FC), idx(0x3F, FC)), 1.0);

        let m = ChannelMap::new(fmt(8, 0xFF & !(FLC | FRC) | SL | SR), fmt(8, 0x63F));
        // 0x63F has SL/SR and BL/BR; source FL FR FC LFE BL BR SL SR is identical as a set.
        assert_eq!(m.kind(), MapKind::Identity);
    }

    #[test]
    fn wide_71_to_71_is_not_a_mismatch_for_surrounds() {
        // 0xFF (FL FR FC LFE BL BR FLC FRC) → 0x63F: FLC/FRC fold into FL/FR, which is a downmix.
        let m = ChannelMap::new(fmt(8, 0xFF), fmt(8, 0x63F));
        assert_eq!(m.coeff(idx(0x63F, FL), idx(0xFF, FLC)), 1.0);
        assert_eq!(m.coeff(idx(0x63F, BL), idx(0xFF, BL)), 1.0);
    }

    #[test]
    fn surround_to_stereo_downmix_itu() {
        let m = ChannelMap::new(fmt(6, 0x3F), fmt(2, 0x3));
        assert_eq!(m.kind(), MapKind::Downmix);
        assert!(m.is_mismatch());
        let fl = idx(0x3, FL);
        assert_eq!(m.coeff(fl, idx(0x3F, FL)), 1.0);
        assert!((m.coeff(fl, idx(0x3F, FC)) - H).abs() < 1e-6);
        assert!((m.coeff(fl, idx(0x3F, BL)) - H).abs() < 1e-6);
        assert_eq!(m.coeff(fl, idx(0x3F, LFE)), 0.0);
        assert_eq!(m.coeff(fl, idx(0x3F, BR)), 0.0);
        // Applied: FC-only content lands in both fronts at -3 dB.
        let mut src = vec![0.0f32; 6];
        src[idx(0x3F, FC)] = 0.5;
        let mut dst = vec![0.0f32; 2];
        m.apply_add(&src, &mut dst, 1.0);
        assert!((dst[0] - 0.5 * H).abs() < 1e-6 && (dst[1] - 0.5 * H).abs() < 1e-6);
    }

    #[test]
    fn stereo_to_mono_sums_fronts() {
        let m = ChannelMap::new(fmt(2, 0x3), fmt(1, FC));
        assert_eq!(m.kind(), MapKind::Downmix);
        assert!((m.coeff(0, 0) - H).abs() < 1e-6 && (m.coeff(0, 1) - H).abs() < 1e-6);
    }

    #[test]
    fn stereo_to_71_places_front_only() {
        let m = ChannelMap::new(fmt(2, 0x3), fmt(8, 0x63F));
        assert_eq!(m.kind(), MapKind::Upmix);
        assert!(m.is_mismatch());
        for d in 0..8 {
            for s in 0..2 {
                let want = if (d, s) == (idx(0x63F, FL), 0) || (d, s) == (idx(0x63F, FR), 1) { 1.0 } else { 0.0 };
                assert_eq!(m.coeff(d, s), want, "d={d} s={s}");
            }
        }
    }

    #[test]
    fn gain_scales_output() {
        let m = ChannelMap::new(fmt(2, 0x3), fmt(2, 0x3));
        let mut dst = vec![0.1f32, 0.2];
        m.apply_add(&[0.5, -0.5], &mut dst, 0.5);
        assert!((dst[0] - 0.35).abs() < 1e-7 && (dst[1] + 0.05).abs() < 1e-7);
    }

    #[test]
    fn zero_mask_uses_default_for_channel_count() {
        assert_eq!(effective_mask(fmt(8, 0)), 0x63F);
        assert_eq!(effective_mask(fmt(6, 0)), 0x3F);
        assert_eq!(effective_mask(fmt(2, 0x7)), 0x3, "popcount mismatch falls back to default");
        assert_eq!(effective_mask(fmt(6, 0x60F)), 0x60F);
        assert_eq!(ChannelMap::new(fmt(8, 0), fmt(8, 0x63F)).kind(), MapKind::Identity);
    }

    #[test]
    fn describe_names_common_layouts() {
        assert_eq!(describe(fmt(8, 0x63F)), "7.1");
        assert_eq!(describe(fmt(6, 0x3F)), "5.1");
        assert_eq!(describe(fmt(6, 0x60F)), "5.1");
        assert_eq!(describe(fmt(2, 0x3)), "Stereo");
        assert_eq!(describe(fmt(1, 0x4)), "Mono");
        assert_eq!(describe(fmt(3, 0x7)), "3 ch (0x7)");
    }

    #[test]
    fn coreaudio_51_and_71_labels() {
        assert_eq!(mask_from_coreaudio_labels(&[1, 2, 3, 4, 5, 6]), 0x3F);
        assert_eq!(mask_from_coreaudio_labels(&[1, 2, 3, 4, 5, 6, 33, 34]), 0x63F);
        assert_eq!(mask_from_coreaudio_labels(&[1, 2]), 0x3);
    }
}
