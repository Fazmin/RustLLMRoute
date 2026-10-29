use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};

use crate::health::HealthState;

pub fn current_minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 60)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MinuteWindow {
    pub epoch_min: u64,
    pub rpm: u32,
    pub tpm: u64,
    pub successes: u32,
    pub fails: u32,
}

impl MinuteWindow {
    fn roll(&mut self, minute: u64) {
        if self.epoch_min != minute {
            *self = Self {
                epoch_min: minute,
                ..Self::default()
            };
        }
    }

    pub fn total(&self) -> u32 {
        self.successes + self.fails
    }
}

#[derive(Debug)]
struct LatencyRing {
    per_token: VecDeque<f32>,
    ttft: VecDeque<f32>,
    cap: usize,
}

impl LatencyRing {
    fn push(queue: &mut VecDeque<f32>, cap: usize, value: f32) {
        if queue.len() >= cap {
            queue.pop_front();
        }
        queue.push_back(value);
    }

    fn mean(queue: &VecDeque<f32>) -> f32 {
        if queue.is_empty() {
            return 0.0;
        }
        queue.iter().sum::<f32>() / queue.len() as f32
    }
}

/// Per-deployment counters. Cheap to update on every attempt; read in batches at selection time.
#[derive(Debug)]
pub struct DeploymentStats {
    pub in_flight: AtomicU32,
    minute: Mutex<MinuteWindow>,
    latency: Mutex<LatencyRing>,
    pub health: RwLock<HealthState>,
}

impl DeploymentStats {
    pub fn new(latency_samples: usize) -> Self {
        Self {
            in_flight: AtomicU32::new(0),
            minute: Mutex::new(MinuteWindow::default()),
            latency: Mutex::new(LatencyRing {
                per_token: VecDeque::with_capacity(latency_samples),
                ttft: VecDeque::with_capacity(latency_samples),
                cap: latency_samples.max(1),
            }),
            health: RwLock::new(HealthState::default()),
        }
    }

    pub fn minute(&self, minute: u64) -> MinuteWindow {
        let mut window = self.minute.lock();
        window.roll(minute);
        *window
    }

    pub fn record_request(&self, minute: u64) {
        let mut window = self.minute.lock();
        window.roll(minute);
        window.rpm = window.rpm.saturating_add(1);
    }

    pub fn record_result(&self, minute: u64, ok: bool, tokens: u64) {
        let mut window = self.minute.lock();
        window.roll(minute);
        if ok {
            window.successes = window.successes.saturating_add(1);
            window.tpm = window.tpm.saturating_add(tokens);
        } else {
            window.fails = window.fails.saturating_add(1);
        }
    }

    pub fn record_latency(&self, per_output_token: Duration, ttft: Option<Duration>) {
        let mut ring = self.latency.lock();
        let cap = ring.cap;
        LatencyRing::push(&mut ring.per_token, cap, per_output_token.as_secs_f32());
        if let Some(ttft) = ttft {
            LatencyRing::push(&mut ring.ttft, cap, ttft.as_secs_f32());
        }
    }

    /// Mean seconds per output token, or mean time-to-first-token for streams when samples exist.
    /// Zero when nothing was measured yet, so new deployments receive traffic.
    pub fn mean_latency(&self, stream: bool) -> f32 {
        let ring = self.latency.lock();
        if stream && !ring.ttft.is_empty() {
            return LatencyRing::mean(&ring.ttft);
        }
        LatencyRing::mean(&ring.per_token)
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_roll_and_rings_cap() {
        let stats = DeploymentStats::new(2);
        stats.record_request(1);
        stats.record_result(1, true, 10);
        stats.record_result(1, false, 0);
        let window = stats.minute(1);
        assert_eq!(
            (window.rpm, window.tpm, window.successes, window.fails),
            (1, 10, 1, 1)
        );
        assert_eq!(
            stats.minute(2),
            MinuteWindow {
                epoch_min: 2,
                ..MinuteWindow::default()
            }
        );
        for ms in [100, 200, 300] {
            stats.record_latency(Duration::from_millis(ms), None);
        }
        assert!(
            (stats.mean_latency(false) - 0.25).abs() < 1e-6,
            "only the last two samples count"
        );
        assert_eq!(
            stats.mean_latency(true),
            stats.mean_latency(false),
            "no TTFT samples falls back"
        );
    }
}
