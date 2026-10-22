//! Sample conversion and buffer-size choice for the ASIO host. Platform independent and safe.
//!
//! ASIO names each channel's sample format with a type code (the `INT16_LSB`… constants). Samples
//! are little-endian on the wire. Nothing here panics: short or odd-sized slices are truncated,
//! unknown types read as silence and write zero bytes.

/// 16-bit integer, little-endian.
pub const INT16_LSB: i32 = 16;
/// 24-bit integer (3 bytes), little-endian, sign-extended.
pub const INT24_LSB: i32 = 17;
/// 32-bit integer, little-endian.
pub const INT32_LSB: i32 = 18;
/// 32-bit IEEE float, little-endian.
pub const FLOAT32_LSB: i32 = 19;
/// 64-bit IEEE float, little-endian.
pub const FLOAT64_LSB: i32 = 20;
/// 32-bit container holding 16 significant bits (LSB-aligned).
pub const INT32_LSB16: i32 = 24;
/// 32-bit container holding 18 significant bits.
pub const INT32_LSB18: i32 = 25;
/// 32-bit container holding 20 significant bits.
pub const INT32_LSB20: i32 = 26;
/// 32-bit container holding 24 significant bits.
pub const INT32_LSB24: i32 = 27;

/// How one sample type is stored and scaled.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Format {
    /// Integer in `bytes` bytes: reads divide the stored value by `div`, writes multiply by `max`.
    Int {
        bytes: usize,
        div: f64,
        max: f64,
    },
    F32,
    F64,
}

impl Format {
    fn size(self) -> usize {
        match self {
            Format::Int { bytes, .. } => bytes,
            Format::F32 => 4,
            Format::F64 => 8,
        }
    }
}

fn format(t: i32) -> Option<Format> {
    Some(match t {
        INT16_LSB => Format::Int { bytes: 2, div: 32768.0, max: 32767.0 },
        INT24_LSB => Format::Int { bytes: 3, div: 8388608.0, max: 8388607.0 },
        INT32_LSB => Format::Int { bytes: 4, div: 2147483648.0, max: 2147483647.0 },
        FLOAT32_LSB => Format::F32,
        FLOAT64_LSB => Format::F64,
        INT32_LSB16 => Format::Int { bytes: 4, div: 32768.0, max: 32767.0 },
        INT32_LSB18 => Format::Int { bytes: 4, div: 131072.0, max: 131071.0 },
        INT32_LSB20 => Format::Int { bytes: 4, div: 524288.0, max: 524287.0 },
        INT32_LSB24 => Format::Int { bytes: 4, div: 8388608.0, max: 8388607.0 },
        _ => return None,
    })
}

/// Bytes one sample of type `t` occupies, `None` for a type this host does not handle.
pub fn bytes_per_sample(t: i32) -> Option<usize> {
    format(t).map(Format::size)
}

/// The first `N` bytes of `src` as an array; zeros when `src` is shorter.
fn le<const N: usize>(src: &[u8]) -> [u8; N] {
    src.get(..N).and_then(|s| <[u8; N]>::try_from(s).ok()).unwrap_or([0; N])
}

/// Copies `v` into the front of `dst` (as many bytes as fit).
fn put<const N: usize>(dst: &mut [u8], v: [u8; N]) {
    if let Some(d) = dst.get_mut(..N) {
        for (d, s) in d.iter_mut().zip(v) {
            *d = s;
        }
    }
}

/// Non-finite values read as silence.
fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// One sample from `src` (exactly `f.size()` bytes) as `f32`.
fn decode(src: &[u8], f: Format) -> f32 {
    match f {
        Format::Int { bytes, div, .. } => {
            let v: i32 = match bytes {
                2 => i32::from(i16::from_le_bytes(le::<2>(src))),
                // Sign-extend the 24-bit value: move it to the top three bytes, then shift back.
                3 => {
                    let [a, b, c] = le::<3>(src);
                    (i32::from_le_bytes([a, b, c, 0]) << 8) >> 8
                }
                _ => i32::from_le_bytes(le::<4>(src)),
            };
            finite((f64::from(v) / div) as f32)
        }
        Format::F32 => finite(f32::from_le_bytes(le::<4>(src))),
        Format::F64 => finite(f64::from_le_bytes(le::<8>(src)) as f32),
    }
}

/// One sample `v` written into `dst` (exactly `f.size()` bytes) in format `f`.
fn encode(v: f32, f: Format, dst: &mut [u8]) {
    let v = if v.is_nan() { 0.0 } else { v.clamp(-1.0, 1.0) };
    match f {
        Format::Int { bytes, max, .. } => {
            // |q| <= max < 2^31, so the cast cannot wrap.
            let q = (f64::from(v) * max).round() as i32;
            match bytes {
                2 => put(dst, (q as i16).to_le_bytes()),
                3 => {
                    let [a, b, c, _] = q.to_le_bytes();
                    put(dst, [a, b, c]);
                }
                _ => put(dst, q.to_le_bytes()),
            }
        }
        Format::F32 => put(dst, v.to_le_bytes()),
        Format::F64 => put(dst, f64::from(v).to_le_bytes()),
    }
}

/// The samples of channel `ch` in an interleaved buffer of width `stride`, at most `frames` long.
fn channel_slots(dst: &mut [f32], ch: usize, stride: usize, frames: usize) -> impl Iterator<Item = &mut f32> {
    dst.iter_mut().skip(ch).step_by(stride.max(1)).take(frames)
}

/// Channel `ch` of the interleaved `dst` (width `stride`) from one driver buffer `src` of type `t`.
///
/// Stops at whichever of `src` (in whole samples) and `frames` is shorter. An unknown type writes
/// silence to the channel.
pub fn read_channel(src: &[u8], t: i32, dst: &mut [f32], ch: usize, stride: usize, frames: usize) {
    let Some(f) = format(t) else {
        for d in channel_slots(dst, ch, stride, frames) {
            *d = 0.0;
        }
        return;
    };
    for (s, d) in src.chunks_exact(f.size()).zip(channel_slots(dst, ch, stride, frames)) {
        *d = decode(s, f);
    }
}

/// One driver buffer of type `t` from channel `ch` of the interleaved `src` (width `stride`).
///
/// Stops at whichever of `dst` (in whole samples) and `frames` is shorter. An unknown type fills
/// `dst` with zero bytes.
pub fn write_channel(src: &[f32], ch: usize, stride: usize, t: i32, dst: &mut [u8], frames: usize) {
    let Some(f) = format(t) else {
        dst.fill(0);
        return;
    };
    let samples = src.iter().skip(ch).step_by(stride.max(1)).take(frames);
    for (d, &v) in dst.chunks_exact_mut(f.size()).zip(samples) {
        encode(v, f, d);
    }
}

/// Whether `want` (already within `[min, max]`) satisfies the driver's granularity rule.
fn allowed(want: u32, min: u32, granularity: i32) -> bool {
    let off = want - min;
    match granularity {
        -1 => want.is_power_of_two(),
        0 => want == min,
        n if n > 0 => u32::try_from(n).is_ok_and(|n| off.is_multiple_of(n)),
        _ => true,
    }
}

/// The buffer size to open: `want` when it lies in `[min, max]` and the granularity allows it,
/// else `preferred` (clamped to `[min, max]` when `min <= max`), and at least 1.
pub fn choose_buffer(want: Option<u32>, min: u32, max: u32, preferred: u32, granularity: i32) -> u32 {
    if let Some(w) = want
        && (min..=max).contains(&w)
        && w >= 1
        && allowed(w, min, granularity)
    {
        return w;
    }
    let p = if min <= max { preferred.clamp(min, max) } else { preferred };
    p.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [i32; 9] = [INT16_LSB, INT24_LSB, INT32_LSB, FLOAT32_LSB, FLOAT64_LSB, INT32_LSB16, INT32_LSB18, INT32_LSB20, INT32_LSB24];

    /// Round-trips one value through `write_channel` and `read_channel` in format `t`.
    fn round_trip(t: i32, v: f32) -> f32 {
        let bytes = bytes_per_sample(t).unwrap_or(0);
        let mut buf = vec![0u8; bytes];
        write_channel(&[v], 0, 1, t, &mut buf, 1);
        let mut out = [0.0f32; 1];
        read_channel(&buf, t, &mut out, 0, 1, 1);
        out[0]
    }

    #[test]
    fn sizes_match_the_asio_types() {
        let sizes: Vec<Option<usize>> = ALL.iter().map(|&t| bytes_per_sample(t)).collect();
        assert_eq!(sizes, [Some(2), Some(3), Some(4), Some(4), Some(8), Some(4), Some(4), Some(4), Some(4)]);
        assert_eq!(bytes_per_sample(0), None);
        assert_eq!(bytes_per_sample(99), None);
    }

    #[test]
    fn round_trip_is_within_one_lsb() {
        // (type, tolerance): one LSB of the format's own scale, with slack for the rounding.
        let cases: [(i32, f32); 9] = [
            (INT16_LSB, 2.0 / 32768.0),
            (INT24_LSB, 2.0 / 8388608.0),
            (INT32_LSB, 2.0 / 2147483648.0),
            (FLOAT32_LSB, 1e-6),
            (FLOAT64_LSB, 1e-6),
            (INT32_LSB16, 2.0 / 32768.0),
            (INT32_LSB18, 2.0 / 131072.0),
            (INT32_LSB20, 2.0 / 524288.0),
            (INT32_LSB24, 2.0 / 8388608.0),
        ];
        for (t, tol) in cases {
            for v in [0.5f32, -0.5, 0.0] {
                let back = round_trip(t, v);
                assert!((back - v).abs() <= tol, "type {t}: {v} came back as {back}");
            }
        }
    }

    #[test]
    fn clipping_and_nan() {
        for t in [INT16_LSB, INT24_LSB, INT32_LSB, FLOAT32_LSB, FLOAT64_LSB] {
            let hi = round_trip(t, 2.0);
            assert!(hi > 0.999 && hi <= 1.0, "type {t}: 2.0 read back as {hi}");
            let lo = round_trip(t, -2.0);
            assert!((-1.0..-0.999).contains(&lo), "type {t}: -2.0 read back as {lo}");
            assert_eq!(round_trip(t, f32::NAN), 0.0, "type {t}: NaN must write silence");
        }
    }

    #[test]
    fn int24_sign_extends() {
        // -1 stored as three bytes is 0xFFFFFF; it must read back as a small negative value.
        let v = read_one(&[0xFF, 0xFF, 0xFF], INT24_LSB);
        assert!(v < 0.0 && v > -1e-6, "got {v}");
        // Most negative 24-bit value is -1.0 exactly.
        assert_eq!(read_one(&[0x00, 0x00, 0x80], INT24_LSB), -1.0);
        // Positive full scale.
        assert!((read_one(&[0xFF, 0xFF, 0x7F], INT24_LSB) - 1.0).abs() < 1e-6);
    }

    fn read_one(bytes: &[u8], t: i32) -> f32 {
        let mut out = [0.0f32; 1];
        read_channel(bytes, t, &mut out, 0, 1, 1);
        out[0]
    }

    #[test]
    fn non_finite_floats_read_as_zero() {
        assert_eq!(read_one(&f32::NAN.to_le_bytes(), FLOAT32_LSB), 0.0);
        assert_eq!(read_one(&f32::INFINITY.to_le_bytes(), FLOAT32_LSB), 0.0);
        assert_eq!(read_one(&f64::NAN.to_le_bytes(), FLOAT64_LSB), 0.0);
    }

    #[test]
    fn stride_interleaves_three_channels() {
        // Two frames of three interleaved channels: [.25,.5,.75] [-.25,-.5,-.75] (within [-1, 1]).
        let src = [0.25f32, 0.5, 0.75, -0.25, -0.5, -0.75];
        let mut buf = [0u8; 8];
        write_channel(&src, 1, 3, FLOAT32_LSB, &mut buf, 2);
        let mut back = [0.0f32; 6];
        read_channel(&buf, FLOAT32_LSB, &mut back, 1, 3, 2);
        // Channel 1 lands at positions 1 and 4 only.
        assert_eq!(back, [0.0, 0.5, 0.0, 0.0, -0.5, 0.0]);
    }

    #[test]
    fn unknown_type_reads_zeros_and_writes_zero_bytes() {
        let mut out = [1.0f32; 4];
        read_channel(&[9, 9, 9, 9], 99, &mut out, 0, 1, 4);
        assert_eq!(out, [0.0; 4]);
        let mut buf = [0xABu8; 4];
        write_channel(&[0.5; 4], 0, 1, 99, &mut buf, 4);
        assert_eq!(buf, [0u8; 4]);
    }

    #[test]
    fn short_slices_do_not_panic() {
        let mut out = [0.0f32; 8];
        read_channel(&[1, 2, 3], INT32_LSB, &mut out, 0, 0, 8);
        let mut buf = [0u8; 3];
        write_channel(&[0.5; 1], 5, 0, INT32_LSB, &mut buf, 9);
    }

    #[test]
    fn choose_buffer_table() {
        // (want, min, max, preferred, granularity) -> expected
        let table: [((Option<u32>, u32, u32, u32, i32), u32); 9] = [
            ((Some(512), 64, 2048, 1024, -1), 512),
            ((Some(500), 64, 2048, 1024, -1), 1024),
            ((Some(96), 32, 512, 64, 32), 96),
            ((Some(100), 32, 512, 64, 32), 64),
            ((None, 64, 2048, 1024, -1), 1024),
            ((Some(64), 64, 64, 64, 0), 64),
            ((Some(128), 64, 64, 64, 0), 64),
            ((None, 64, 512, 2000, -1), 512),
            ((None, 0, 0, 0, -1), 1),
        ];
        for ((want, min, max, pref, gran), expected) in table {
            assert_eq!(choose_buffer(want, min, max, pref, gran), expected, "want {want:?} in {min}..={max} gran {gran}");
        }
    }
}
