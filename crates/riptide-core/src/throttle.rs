// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Speed limiting and speed measurement.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
// The bucket reads tokio's clock rather than the OS clock so that rate limiting can be
// tested under a paused runtime instead of by sleeping in real time.
use tokio::time::Instant;

/// A token bucket shared by every connection under one limit.
///
/// Workers ask for permission before reading from the socket, so the cap holds across all
/// segments of a download rather than per connection. One bucket can also be shared by every
/// download in the app to impose a global ceiling.
///
/// A limit of `None` means unlimited and short-circuits without taking the lock.
#[derive(Debug, Clone)]
pub struct Throttle {
    inner: Option<Arc<Mutex<Bucket>>>,
}

#[derive(Debug)]
struct Bucket {
    /// Bytes per second.
    rate: u64,
    /// Tokens currently available, in bytes.
    tokens: f64,
    /// Ceiling on accumulated tokens, so a long idle period cannot produce an unbounded burst.
    capacity: f64,
    last: Instant,
}

impl Throttle {
    /// An unlimited throttle, which adds no measurable cost to the read loop.
    pub fn unlimited() -> Self {
        Self { inner: None }
    }

    /// A throttle capped at `bytes_per_sec`. A rate of 0 is treated as unlimited.
    pub fn new(bytes_per_sec: u64) -> Self {
        if bytes_per_sec == 0 {
            return Self::unlimited();
        }
        // Allow up to a quarter second of burst so that bursty TCP reads are not chopped into
        // pathologically small pieces.
        let capacity = (bytes_per_sec as f64 / 4.0).max(64.0 * 1024.0);
        Self {
            inner: Some(Arc::new(Mutex::new(Bucket {
                rate: bytes_per_sec,
                tokens: capacity,
                capacity,
                last: Instant::now(),
            }))),
        }
    }

    pub fn is_limited(&self) -> bool {
        self.inner.is_some()
    }

    pub async fn limit(&self) -> Option<u64> {
        match &self.inner {
            Some(b) => Some(b.lock().await.rate),
            None => None,
        }
    }

    /// Change the cap at runtime without disturbing downloads in flight.
    pub async fn set_limit(&self, bytes_per_sec: u64) {
        if let Some(b) = &self.inner {
            let mut b = b.lock().await;
            b.rate = bytes_per_sec;
            b.capacity = (bytes_per_sec as f64 / 4.0).max(64.0 * 1024.0);
            b.tokens = b.tokens.min(b.capacity);
        }
    }

    /// Wait until at most `want` bytes may be read, and return how many are allowed.
    ///
    /// The return value can be smaller than `want`, in which case the caller should read that
    /// much and come back. It is never zero, so a caller cannot spin.
    pub async fn acquire(&self, want: u64) -> u64 {
        let Some(bucket) = &self.inner else {
            return want;
        };
        loop {
            let wait = {
                let mut b = bucket.lock().await;
                b.refill();
                if b.rate == 0 {
                    return want;
                }
                if b.tokens >= 1.0 {
                    let granted = (b.tokens.floor() as u64).min(want).max(1);
                    b.tokens -= granted as f64;
                    return granted;
                }
                // Sleep just long enough for one token, with a floor to avoid busy waiting.
                Duration::from_micros(((1.0 - b.tokens) / b.rate as f64 * 1_000_000.0) as u64)
                    .max(Duration::from_millis(1))
            };
            tokio::time::sleep(wait).await;
        }
    }
}

impl Bucket {
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.rate as f64).min(self.capacity);
            self.last = now;
        }
    }
}

/// Sliding-window transfer rate estimate.
///
/// Samples are kept for [`SpeedMeter::window`] and the rate is the total over the span they
/// actually cover, so the figure settles quickly after a stall instead of decaying slowly the
/// way a plain exponential average does.
#[derive(Debug)]
pub struct SpeedMeter {
    samples: VecDeque<(Instant, u64)>,
    window: Duration,
    total: u64,
}

impl SpeedMeter {
    pub fn new() -> Self {
        Self::with_window(Duration::from_secs(5))
    }

    pub fn with_window(window: Duration) -> Self {
        Self { samples: VecDeque::new(), window, total: 0 }
    }

    pub fn record(&mut self, bytes: u64) {
        self.record_at(bytes, Instant::now());
    }

    fn record_at(&mut self, bytes: u64, now: Instant) {
        self.total += bytes;
        self.samples.push_back((now, bytes));
        self.expire(now);
    }

    fn expire(&mut self, now: Instant) {
        while let Some(&(t, _)) = self.samples.front() {
            if now.saturating_duration_since(t) > self.window {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Bytes per second over the window, or 0 before there is enough history to judge.
    pub fn rate(&self) -> u64 {
        self.rate_at(Instant::now())
    }

    fn rate_at(&self, now: Instant) -> u64 {
        let Some(&(first, _)) = self.samples.front() else {
            return 0;
        };
        // Measure against the span the samples cover, extended to now so that a transfer that
        // has gone quiet reports a falling rate rather than a stale one.
        let span = now.saturating_duration_since(first).as_secs_f64();
        if span < 0.05 {
            return 0;
        }
        let bytes: u64 = self.samples.iter().map(|&(_, b)| b).sum();
        (bytes as f64 / span) as u64
    }

    /// Everything recorded since this meter was created.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Seconds until `remaining` bytes would arrive at the current rate.
    pub fn eta(&self, remaining: u64) -> Option<Duration> {
        self.eta_at(remaining, Instant::now())
    }

    fn eta_at(&self, remaining: u64, now: Instant) -> Option<Duration> {
        let rate = self.rate_at(now);
        if rate == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(remaining as f64 / rate as f64))
    }
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_grants_everything_without_waiting() {
        let t = Throttle::unlimited();
        assert!(!t.is_limited());
        // No runtime needed: an unlimited acquire never awaits anything.
        let got = futures_util::future::FutureExt::now_or_never(t.acquire(1 << 30));
        assert_eq!(got, Some(1 << 30));
    }

    #[tokio::test(start_paused = true)]
    async fn a_limited_bucket_holds_the_average_rate() {
        let rate = 1_000_000u64; // 1 MB/s, so the burst allowance is rate/4 = 250 kB
        let t = Throttle::new(rate);
        let start = Instant::now();

        let mut taken = 0u64;
        while taken < rate * 2 {
            taken += t.acquire(16 * 1024).await;
        }

        // 2 MB at 1 MB/s, less the 250 kB granted up front, is 1.75s of enforced waiting.
        let elapsed = start.elapsed();
        assert!(
            (Duration::from_millis(1600)..Duration::from_millis(2100)).contains(&elapsed),
            "2 MB at 1 MB/s took {elapsed:?}, expected about 1.75s"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_never_grants_zero_so_a_caller_cannot_spin() {
        let t = Throttle::new(1); // pathologically slow
                                  // Spend the initial burst allowance.
        let burst = t.acquire(u64::MAX).await;
        assert!(burst > 0);
        // With the bucket empty, the next grant must still be at least one byte rather than
        // zero, or a reader loop would spin without making progress.
        assert_eq!(t.acquire(10_000).await, 1);
    }

    #[tokio::test]
    async fn raising_the_limit_takes_effect_immediately() {
        let t = Throttle::new(1000);
        assert_eq!(t.limit().await, Some(1000));
        t.set_limit(50_000).await;
        assert_eq!(t.limit().await, Some(50_000));
    }

    #[test]
    fn speed_is_measured_over_the_window() {
        let mut m = SpeedMeter::with_window(Duration::from_secs(4));
        let t0 = Instant::now();
        // 1 MiB spread evenly across two seconds.
        for i in 0..20 {
            m.record_at(52_429, t0 + Duration::from_millis(i * 100));
        }
        let rate = m.rate_at(t0 + Duration::from_millis(2000));
        // Roughly 512 kB/s, allowing for the half-open sample span.
        assert!((400_000..=620_000).contains(&rate), "unexpected rate {rate}");
        assert_eq!(m.total(), 52_429 * 20);
    }

    #[test]
    fn old_samples_leave_the_window() {
        let mut m = SpeedMeter::with_window(Duration::from_secs(1));
        let t0 = Instant::now();
        m.record_at(1_000_000, t0);
        // Long after the window, the burst no longer counts toward the rate.
        m.record_at(10, t0 + Duration::from_secs(30));
        let rate = m.rate_at(t0 + Duration::from_secs(30));
        assert!(rate < 1000, "a stale burst still inflates the rate: {rate}");
        // The lifetime total is unaffected by expiry.
        assert_eq!(m.total(), 1_000_010);
    }

    #[test]
    fn eta_is_unavailable_until_a_rate_is_known() {
        let m = SpeedMeter::new();
        assert!(m.eta(1 << 30).is_none());
    }

    #[test]
    fn eta_follows_the_measured_rate() {
        let mut m = SpeedMeter::with_window(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..10 {
            m.record_at(100_000, t0 + Duration::from_millis(i * 100));
        }
        // Around 1 MB/s, so 10 MB should be about 10 seconds.
        let eta = m.eta_at(10_000_000, t0 + Duration::from_millis(1000)).unwrap();
        assert!(eta.as_secs() >= 5 && eta.as_secs() <= 20, "unexpected eta {eta:?}");
    }
}
