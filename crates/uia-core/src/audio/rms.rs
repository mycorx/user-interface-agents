// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

/// Root-mean-square amplitude of a PCM frame, normalised to 0.0..=1.0.
/// Feeds both the HUD level meter and the barge-in detector.
pub fn rms(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = frame
        .iter()
        .map(|&s| {
            let v = s as f64;
            v * v
        })
        .sum();
    let mean_sq = sum_sq / frame.len() as f64;
    (mean_sq.sqrt() / i16::MAX as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_zero() {
        assert_eq!(rms(&[0, 0, 0, 0]), 0.0);
    }

    #[test]
    fn empty_frame_is_zero_not_nan() {
        // A division-by-zero here would poison every downstream comparison.
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn full_scale_square_wave_is_one() {
        let frame = [i16::MAX, i16::MIN, i16::MAX, i16::MIN];
        assert!((rms(&frame) - 1.0).abs() < 0.001, "got {}", rms(&frame));
    }

    #[test]
    fn half_scale_is_about_half() {
        let half = i16::MAX / 2;
        let frame = [half, -half, half, -half];
        assert!((rms(&frame) - 0.5).abs() < 0.01, "got {}", rms(&frame));
    }
}
