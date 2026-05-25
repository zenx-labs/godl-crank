//! Running tallies for the reset race.
//!
//! `sample + reveal + reset` is contested — multiple bots rush each round — so
//! we distinguish three outcomes:
//!   * **won**    — our transaction confirmed and advanced the round (we earned
//!                  the crank reward).
//!   * **lost**   — the round advanced, but someone else's reset landed first.
//!   * **failed** — the round did not advance at all after our attempts
//!                  (entropy/RPC error, or we couldn't land in time).

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct ResetStats {
    won: AtomicU64,
    lost: AtomicU64,
    failed: AtomicU64,
}

impl ResetStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_win(&self) {
        self.won.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_loss(&self) {
        self.lost.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    /// `(won, lost, failed)`.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.won.load(Ordering::Relaxed),
            self.lost.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }

    /// One-line human summary, e.g.
    /// `won=12 lost=3 failed=1 | attempted=16, win-rate 75.0%, land-rate 80.0%`.
    pub fn summary(&self) -> String {
        let (won, lost, failed) = self.snapshot();
        let attempted = won + lost + failed;
        // win-rate: of every round we tried, how many we took.
        let win_rate = pct(won, attempted);
        // land-rate: of rounds that actually advanced, how many were ours
        // (excludes outright failures where nobody advanced it via our path).
        let land_rate = pct(won, won + lost);
        format!(
            "won={won} lost={lost} failed={failed} | attempted={attempted}, win-rate {win_rate:.1}%, land-rate {land_rate:.1}%"
        )
    }
}

fn pct(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 * 100.0 / den as f64
    }
}
