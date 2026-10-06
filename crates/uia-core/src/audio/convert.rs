// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

/// Convert normalised float samples to signed 16-bit PCM, clamping rather
/// than wrapping so an overshooting device buffer cannot flip a sample's sign.
pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| {
            (s.clamp(-1.0, 1.0) * 32768.0)
                .round()
                .clamp(i16::MIN as f32, i16::MAX as f32) as i16
        })
        .collect()
}

/// Convert signed 16-bit PCM to normalised floats — the inverse of
/// [`f32_to_i16`], which the Opus codec needs because it works in floats while
/// every audio port in this workspace carries `i16`.
///
/// Divides by 32768 rather than 32767 so the two directions are exact inverses:
/// `f32_to_i16` multiplies by 32768 and clamps, so `i16::MIN` maps to `-1.0`
/// and back without drift.
pub fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// Average interleaved channels down to mono. Accumulates in i32 because
/// summing two i16 maxima overflows i16.
pub fn downmix_to_mono(interleaved: &[i16], channels: u16) -> Vec<i16> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let n = channels as usize;
    interleaved
        .chunks_exact(n)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&s| s as i32).sum();
            (sum / n as i32) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i16_converts_to_f32_full_scale() {
        // The Opus codec works in normalised floats while every port in this
        // workspace carries i16, so this is the seam the codec crosses twice
        // per frame. i16::MIN must map to exactly -1.0.
        assert_eq!(i16_to_f32(&[0]), vec![0.0]);
        assert_eq!(i16_to_f32(&[i16::MIN]), vec![-1.0]);
    }

    #[test]
    fn i16_to_f32_round_trips_through_f32_to_i16() {
        let original = vec![0i16, 1, -1, 12_345, -12_345, i16::MAX, i16::MIN];
        assert_eq!(f32_to_i16(&i16_to_f32(&original)), original);
    }

    #[test]
    fn f32_converts_to_i16_full_scale() {
        assert_eq!(f32_to_i16(&[0.0]), vec![0]);
        assert_eq!(f32_to_i16(&[1.0]), vec![i16::MAX]);
        assert_eq!(f32_to_i16(&[-1.0]), vec![i16::MIN]);
    }

    #[test]
    fn f32_clamps_out_of_range_input() {
        // Device buffers can overshoot; wrapping instead of clamping would
        // turn a loud sample into a loud sample of the opposite sign.
        assert_eq!(f32_to_i16(&[2.0]), vec![i16::MAX]);
        assert_eq!(f32_to_i16(&[-2.0]), vec![i16::MIN]);
    }

    #[test]
    fn stereo_downmixes_to_mono_by_averaging() {
        assert_eq!(downmix_to_mono(&[100, 300, -50, 50], 2), vec![200, 0]);
    }

    #[test]
    fn mono_downmix_is_a_passthrough() {
        assert_eq!(downmix_to_mono(&[1, 2, 3], 1), vec![1, 2, 3]);
    }

    #[test]
    fn downmix_averaging_does_not_overflow() {
        // i16::MAX + i16::MAX overflows i16; the average must be computed wider.
        assert_eq!(downmix_to_mono(&[i16::MAX, i16::MAX], 2), vec![i16::MAX]);
    }
}
