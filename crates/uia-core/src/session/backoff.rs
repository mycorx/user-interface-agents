// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use std::time::Duration;

const BASE_MS: u64 = 500;
const CAP: Duration = Duration::from_secs(30);

/// Exponential backoff with full jitter. Jitter matters even for a single
/// client: without it, a provider blip plus a fixed schedule reconnects in
/// lockstep with every other instance.
pub struct Backoff {
    attempt: u32,
    jitter: bool,
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            attempt: 0,
            jitter: true,
        }
    }

    /// Jitter disabled, for assertable tests.
    pub fn new_deterministic() -> Self {
        Self {
            attempt: 0,
            jitter: false,
        }
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn next_delay(&mut self) -> Duration {
        let nominal =
            Duration::from_millis(BASE_MS.saturating_mul(1u64 << self.attempt.min(20))).min(CAP);
        self.attempt = self.attempt.saturating_add(1);
        if !self.jitter {
            return nominal;
        }
        // Full jitter: uniform in [0, nominal]. Cheap PRNG, no rand dependency.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        let frac = (seed % 1000) as f64 / 1000.0;
        nominal.mul_f64(frac)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_from_the_base() {
        let mut b = Backoff::new_deterministic();
        assert_eq!(b.next_delay(), Duration::from_millis(500));
        assert_eq!(b.next_delay(), Duration::from_millis(1000));
        assert_eq!(b.next_delay(), Duration::from_millis(2000));
    }

    #[test]
    fn delays_are_capped_at_thirty_seconds() {
        let mut b = Backoff::new_deterministic();
        for _ in 0..20 {
            b.next_delay();
        }
        assert_eq!(b.next_delay(), Duration::from_secs(30));
    }

    #[test]
    fn reset_returns_to_the_base_delay() {
        let mut b = Backoff::new_deterministic();
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(500));
    }

    #[test]
    fn jitter_stays_within_the_nominal_delay() {
        let mut b = Backoff::new();
        let d = b.next_delay();
        assert!(
            d <= Duration::from_millis(500),
            "jitter must not exceed nominal"
        );
    }
}
