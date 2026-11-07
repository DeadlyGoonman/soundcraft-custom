//! Pure (platform-free) shading for the editor: the walnut veneer as per-line colour runs. `editor` blits these with GDI.

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn hash(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA6B).rotate_left(13);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^ (h >> 15)
}

fn unit(h: u32) -> f32 {
    (h & 0xFFFF) as f32 / 65535.0
}

/// One line of the walnut veneer: runs of `(start, end, 0xRRGGBB-as-GDI-colour)` as fractions of
/// the strip length. `line` of `count` lines across the thickness (0 = lit edge). Deterministic
/// for a `seed`.
pub fn wood_runs(seed: u32, line: usize, count: usize) -> Vec<(f32, f32, [u8; 3])> {
    let t = line as f32 / count.max(2).saturating_sub(1) as f32;
    // Walnut: lighter and warmer at the lit edge, deep brown at the far edge.
    let base = [mix(0x62 as f32, 0x30 as f32, t.powf(0.8)), mix(0x3D as f32, 0x1E as f32, t.powf(0.8)), mix(0x24 as f32, 0x12 as f32, t.powf(0.8))];
    // Glossy veneer: a soft bright band near the lit edge, darker edge lines.
    let gloss = 22.0 * clamp01(1.0 - (t - 0.12).abs() / 0.12);
    let edge = if line == 0 || line + 1 == count { -26.0 } else { 0.0 };
    let l = line as u32;
    let mut line_off = (unit(hash(seed, l)) - 0.5) * 9.0;
    if hash(seed ^ 0x55, l).is_multiple_of(7) {
        line_off -= 9.0;
    }
    let n = 3 + (hash(seed ^ 0xA5, l) % 3) as usize;
    let mut cuts: Vec<f32> = (1..n).map(|k| unit(hash(seed ^ 0x77, l * 8 + k as u32))).collect();
    cuts.push(0.0);
    cuts.push(1.0);
    cuts.sort_by(f32::total_cmp);
    cuts.windows(2)
        .enumerate()
        .map(|(k, w)| {
            let run = (unit(hash(seed ^ 0x31, l * 8 + k as u32)) - 0.5) * 5.0;
            let off = gloss + edge + line_off + run;
            (w[0], w[1], [0, 1, 2].map(|i| (base[i] + off * (1.0 - 0.15 * i as f32)).clamp(0.0, 255.0).round() as u8))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wood_is_deterministic_and_covers_the_line() {
        let a = wood_runs(7, 3, 20);
        assert_eq!(a, wood_runs(7, 3, 20));
        assert_eq!(a.first().map(|r| r.0), Some(0.0));
        assert_eq!(a.last().map(|r| r.1), Some(1.0));
    }
}
