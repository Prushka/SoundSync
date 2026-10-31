// Procedural app icon in the style of the sibling apps (Sparkle, Toolbox Apps): a gold speaker
// with two sound waves, #f5c542 shading to #d7a626, outlined in dark brown #523d0c, on a
// transparent background. Used for the tray, the window, the .exe and the macOS bundle.
// Shared with build.rs via include!, so it must stay dependency-free.

const GOLD: [f32; 3] = [245.0, 197.0, 66.0];
const GOLD_SHADE: [f32; 3] = [215.0, 166.0, 38.0];
const OUTLINE: [f32; 3] = [82.0, 61.0, 12.0];
const GLINT: [f32; 3] = [255.0, 228.0, 147.0];

// Artwork coordinates run 0..256 (like Sparkle's), y down.
/// Half the outline pen width.
const OUTLINE_HALF: f32 = 3.5;
/// Speaker (box + cone); its signed distance is offset by `SPEAKER_ROUND` to round the corners.
const SPEAKER: [(f32, f32); 6] = [(42.0, 104.0), (92.0, 104.0), (138.0, 64.0), (138.0, 192.0), (92.0, 152.0), (42.0, 152.0)];
const SPEAKER_ROUND: f32 = 8.0;
const WAVE_CENTRE: (f32, f32) = (136.0, 128.0);
const WAVE_RADII: [f32; 2] = [50.0, 94.0];
const WAVE_HALF_ANGLE: f32 = 0.85;
const WAVE_HALF_WIDTH: f32 = 10.0;

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1)).clamp(0.0, 1.0);
    distance(pa, (ba.0 * h, ba.1 * h))
}

/// Signed distance to a polygon (negative inside).
fn polygon_sdf(p: (f32, f32), v: &[(f32, f32)]) -> f32 {
    let mut d = f32::MAX;
    let mut inside = false;
    for i in 0..v.len() {
        let (a, b) = (v[i], v[(i + 1) % v.len()]);
        d = d.min(segment_distance(p, a, b));
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0) {
            inside = !inside;
        }
    }
    if inside { -d } else { d }
}

/// Signed distance to a round-capped arc of radius `r` around `c`, spanning ±`half_angle`
/// from the +x axis, stroked `half_width` either side.
fn arc_sdf(p: (f32, f32), c: (f32, f32), r: f32, half_angle: f32, half_width: f32) -> f32 {
    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
    let d = if dy.atan2(dx).abs() <= half_angle {
        (distance(p, c) - r).abs()
    } else {
        distance(p, (c.0 + r * half_angle.cos(), c.1 + r * half_angle.sin() * dy.signum()))
    };
    d - half_width
}

/// Signed distance to a rounded rectangle.
fn round_rect_sdf(p: (f32, f32), (x0, y0, x1, y1): (f32, f32, f32, f32), r: f32) -> f32 {
    let (qx, qy) = ((p.0 - (x0 + x1) / 2.0).abs() - ((x1 - x0) / 2.0 - r), (p.1 - (y0 + y1) / 2.0).abs() - ((y1 - y0) / 2.0 - r));
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

/// Colour of one artwork point, or None where transparent. Like Sparkle: each shape is filled
/// with a vertical gold gradient and outlined by a pen centred on its edge; later shapes cover
/// earlier ones.
fn sample(p: (f32, f32), glint: bool) -> Option<[f32; 3]> {
    let t = ((p.1 - 44.0) / 168.0).clamp(0.0, 1.0);
    let fill = [0, 1, 2].map(|i| GOLD[i] + (GOLD_SHADE[i] - GOLD[i]) * t);
    let shapes = [
        arc_sdf(p, WAVE_CENTRE, WAVE_RADII[0], WAVE_HALF_ANGLE, WAVE_HALF_WIDTH),
        arc_sdf(p, WAVE_CENTRE, WAVE_RADII[1], WAVE_HALF_ANGLE, WAVE_HALF_WIDTH),
        polygon_sdf(p, &SPEAKER) - SPEAKER_ROUND,
    ];
    let mut colour = None;
    for d in shapes {
        if d.abs() <= OUTLINE_HALF {
            colour = Some(OUTLINE);
        } else if d < 0.0 {
            colour = Some(fill);
        }
    }
    // A small highlight on the box, only where there are pixels enough to show it.
    if glint && colour == Some(fill) && round_rect_sdf(p, (46.0, 112.0, 70.0, 126.0), 6.0) <= 0.0 {
        colour = Some(GLINT);
    }
    colour
}

/// RGBA8 pixels of a `size`×`size` icon (4×4 supersampled).
pub fn icon_rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let scale = 256.0 / size as f32;
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut sum, mut hits) = ([0.0f32; 3], 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let p = ((x as f32 + (sx as f32 + 0.5) / SS as f32) * scale, (y as f32 + (sy as f32 + 0.5) / SS as f32) * scale);
                    if let Some(c) = sample(p, size >= 32) {
                        for i in 0..3 {
                            sum[i] += c[i];
                        }
                        hits += 1;
                    }
                }
            }
            let n = hits.max(1) as f32;
            let alpha = (hits as f32 / (SS * SS) as f32 * 255.0).round() as u8;
            px.extend_from_slice(&[(sum[0] / n).round() as u8, (sum[1] / n).round() as u8, (sum[2] / n).round() as u8, alpha]);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_rgba_has_expected_size_and_transparent_corners() {
        for size in [16u32, 32, 48, 256] {
            let px = icon_rgba(size);
            assert_eq!(px.len(), (size * size * 4) as usize);
            assert_eq!(px[3], 0, "top-left corner is transparent");
            let mid = ((size / 2) * size + size / 2) as usize * 4;
            assert_eq!(px[mid + 3], 255, "centre is opaque");
        }
    }

    #[test]
    fn icon_is_gold_with_a_dark_outline_like_the_sibling_apps() {
        // Sparkle and Toolbox Apps: #f5c542 gold shading to #d7a626, outlined in #523d0c.
        let size = 256u32;
        let px = icon_rgba(size);
        let at = |x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            (px[i], px[i + 1], px[i + 2], px[i + 3])
        };
        let (r, g, b, a) = at(108, 128); // inside the speaker cone
        assert_eq!(a, 255);
        assert!((200..=250).contains(&r) && (150..=205).contains(&g) && b <= 80, "gold fill, got {:?}", (r, g, b));
        let dark = (0..size).flat_map(|x| (0..size).map(move |y| (x, y))).map(|(x, y)| at(x, y)).filter(|&(r, g, b, a)| a == 255 && r < 110 && g < 85 && b < 40).count();
        assert!(dark > 1_000, "dark brown outline pixels: {dark}");
        let blue = (0..size * size).filter(|i| {
            let i = (*i * 4) as usize;
            px[i + 3] > 0 && px[i + 2] > px[i] && px[i + 2] > 120
        });
        assert_eq!(blue.count(), 0, "no blue tile any more");
        assert_eq!(at(250, 6).3, 0, "no background tile: the corners stay transparent");
    }
}

