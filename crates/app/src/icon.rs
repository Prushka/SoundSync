// Procedural app icon (rounded blue square, white speaker with two sound waves).
// Shared with build.rs via include!, so it must stay dependency-free.

fn inside_background(u: f32, v: f32) -> bool {
    const R: f32 = 0.22;
    let cx = u.clamp(R, 1.0 - R);
    let cy = v.clamp(R, 1.0 - R);
    (u - cx).powi(2) + (v - cy).powi(2) <= R * R && (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v)
}

fn inside_glyph(u: f32, v: f32) -> bool {
    let body = (0.20..=0.34).contains(&u) && (0.40..=0.60).contains(&v);
    // Cone: widens from the body (0.34) to x = 0.50.
    let cone = (0.34..=0.50).contains(&u) && {
        let t = (u - 0.34) / 0.16;
        let half = 0.10 + t * 0.14;
        (v - 0.5).abs() <= half
    };
    let (dx, dy) = (u - 0.50, v - 0.50);
    let r = (dx * dx + dy * dy).sqrt();
    let angle_ok = dx > 0.0 && dy.abs() <= dx * 1.2;
    let wave = angle_ok && ((r - 0.17).abs() <= 0.03 || (r - 0.28).abs() <= 0.03);
    body || cone || wave
}

/// RGBA8 pixels of a `size`×`size` icon.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut bg, mut fg) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if inside_background(u, v) {
                        bg += 1;
                        if inside_glyph(u, v) {
                            fg += 1;
                        }
                    }
                }
            }
            let n = (SS * SS) as f32;
            let white = fg as f32 / bg.max(1) as f32;
            let blend = |c: f32| (c + (255.0 - c) * white).round() as u8;
            px.extend_from_slice(&[blend(0x1E as f32), blend(0x88 as f32), blend(0xE5 as f32), (bg as f32 / n * 255.0).round() as u8]);
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
}
