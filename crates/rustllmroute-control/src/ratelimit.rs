use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderValue};
use moka::sync::Cache;
use parking_lot::Mutex;
use rustllmroute_types::{GatewayError, RateLimitScope};
use tokio::time::Instant;

const WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    Key,
    Team,
    User,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Team => "team",
            Self::User => "user",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Descriptor {
    pub scope: Scope,
    pub id: Arc<str>,
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
    pub max_parallel: Option<u32>,
}

impl Descriptor {
    pub fn has_limits(&self) -> bool {
        self.rpm.is_some() || self.tpm.is_some() || self.max_parallel.is_some()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LimitStatus {
    pub limit_requests: Option<u32>,
    pub remaining_requests: Option<u32>,
    pub limit_tokens: Option<u32>,
    pub remaining_tokens: Option<u32>,
    pub reset_in: Duration,
}

impl LimitStatus {
    pub fn headers(&self, out: &mut HeaderMap) {
        let mut set = |name: &'static str, value: Option<u32>| {
            if let Some(value) = value
                && let Ok(header) = HeaderValue::from_str(&value.to_string())
            {
                out.insert(name, header);
            }
        };
        set("x-ratelimit-limit-requests", self.limit_requests);
        set("x-ratelimit-remaining-requests", self.remaining_requests);
        set("x-ratelimit-limit-tokens", self.limit_tokens);
        set("x-ratelimit-remaining-tokens", self.remaining_tokens);
    }

    /// Keeps the most restrictive numbers across descriptors.
    fn merge(&mut self, other: LimitStatus) {
        fn tighter(a: Option<u32>, b: Option<u32>) -> Option<u32> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        if other.remaining_requests.is_some()
            && (self.remaining_requests.is_none()
                || other.remaining_requests < self.remaining_requests)
        {
            self.limit_requests = other.limit_requests;
            self.remaining_requests = other.remaining_requests;
        } else {
            self.limit_requests = tighter(self.limit_requests, other.limit_requests);
        }
        if other.remaining_tokens.is_some()
            && (self.remaining_tokens.is_none() || other.remaining_tokens < self.remaining_tokens)
        {
            self.limit_tokens = other.limit_tokens;
            self.remaining_tokens = other.remaining_tokens;
        } else {
            self.limit_tokens = tighter(self.limit_tokens, other.limit_tokens);
        }
        self.reset_in = self.reset_in.max(other.reset_in);
    }
}

#[derive(Debug)]
struct Window {
    started: Instant,
    requests: u32,
    tokens: i64,
    parallel: u32,
}

impl Window {
    fn roll(&mut self) {
        if self.started.elapsed() >= WINDOW {
            self.started = Instant::now();
            self.requests = 0;
            self.tokens = 0;
        }
    }

    fn reset_in(&self) -> Duration {
        WINDOW.saturating_sub(self.started.elapsed())
    }
}

type WindowKey = (Scope, Arc<str>);

/// A reservation taken by `try_acquire`; settle it with the actual token count when the call ends.
#[derive(Debug)]
pub struct Reservation {
    entries: Vec<(WindowKey, u32)>,
    pub status: LimitStatus,
}

#[async_trait::async_trait]
pub trait RateLimitStore: Send + Sync + 'static {
    /// All-or-nothing: every descriptor is checked first, then every counter is incremented.
    async fn try_acquire(
        &self,
        descriptors: &[Descriptor],
        estimated_tokens: u32,
    ) -> Result<Reservation, GatewayError>;
    fn settle(&self, reservation: Reservation, actual_tokens: Option<u32>);
}

/// Fixed 60-second windows per descriptor, measured with the monotonic clock. Correct for one
/// gateway instance; a Redis store behind the same trait is needed for several.
pub struct MemoryRateLimiter {
    windows: Cache<WindowKey, Arc<Mutex<Window>>>,
}

impl Default for MemoryRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRateLimiter {
    pub fn new() -> Self {
        Self {
            windows: Cache::builder()
                .time_to_idle(Duration::from_secs(600))
                .max_capacity(100_000)
                .build(),
        }
    }

    fn window(&self, key: &WindowKey) -> Arc<Mutex<Window>> {
        self.windows.get_with(key.clone(), || {
            Arc::new(Mutex::new(Window {
                started: Instant::now(),
                requests: 0,
                tokens: 0,
                parallel: 0,
            }))
        })
    }

    pub fn entry_count(&self) -> u64 {
        self.windows.entry_count()
    }
}

#[async_trait::async_trait]
impl RateLimitStore for MemoryRateLimiter {
    async fn try_acquire(
        &self,
        descriptors: &[Descriptor],
        estimated_tokens: u32,
    ) -> Result<Reservation, GatewayError> {
        let mut active: Vec<(&Descriptor, Arc<Mutex<Window>>)> = descriptors
            .iter()
            .filter(|d| d.has_limits())
            .map(|d| (d, self.window(&(d.scope, d.id.clone()))))
            .collect();
        active.sort_by(|a, b| (a.0.scope.label(), &a.0.id).cmp(&(b.0.scope.label(), &b.0.id)));
        let mut guards: Vec<_> = active.iter().map(|(_, w)| w.lock()).collect();
        let mut status = LimitStatus::default();
        for (index, (descriptor, _)) in active.iter().enumerate() {
            let window = &mut guards[index];
            window.roll();
            let reset_in = window.reset_in();
            let exceeded = |what: &str| GatewayError::RateLimited {
                message: format!(
                    "{} rate limit exceeded for this {}; retry in {}s",
                    what,
                    descriptor.scope.label(),
                    reset_in.as_secs().max(1)
                ),
                retry_after: Some(reset_in.max(Duration::from_secs(1))),
                scope: RateLimitScope::Key,
            };
            if let Some(rpm) = descriptor.rpm
                && window.requests >= rpm
            {
                return Err(exceeded("requests"));
            }
            if let Some(tpm) = descriptor.tpm
                && window.tokens + i64::from(estimated_tokens) > i64::from(tpm)
            {
                return Err(exceeded("tokens"));
            }
            if let Some(max) = descriptor.max_parallel
                && window.parallel >= max
            {
                return Err(GatewayError::RateLimited {
                    message: format!(
                        "max parallel requests ({max}) reached for this {}",
                        descriptor.scope.label()
                    ),
                    retry_after: Some(Duration::from_secs(1)),
                    scope: RateLimitScope::Key,
                });
            }
            status.merge(LimitStatus {
                limit_requests: descriptor.rpm,
                remaining_requests: descriptor
                    .rpm
                    .map(|rpm| rpm.saturating_sub(window.requests + 1)),
                limit_tokens: descriptor.tpm,
                remaining_tokens: descriptor.tpm.map(|tpm| {
                    (i64::from(tpm) - window.tokens - i64::from(estimated_tokens)).max(0) as u32
                }),
                reset_in,
            });
        }
        let mut entries = Vec::with_capacity(active.len());
        for (index, (descriptor, _)) in active.iter().enumerate() {
            let window = &mut guards[index];
            window.requests += 1;
            window.tokens += i64::from(estimated_tokens);
            window.parallel += 1;
            entries.push(((descriptor.scope, descriptor.id.clone()), estimated_tokens));
        }
        Ok(Reservation { entries, status })
    }

    fn settle(&self, reservation: Reservation, actual_tokens: Option<u32>) {
        for (key, estimated) in reservation.entries {
            let window = self.window(&key);
            let mut window = window.lock();
            if let Some(actual) = actual_tokens {
                window.tokens = (window.tokens + i64::from(actual) - i64::from(estimated)).max(0);
            }
            window.parallel = window.parallel.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(rpm: Option<u32>, tpm: Option<u32>, parallel: Option<u32>) -> Descriptor {
        Descriptor {
            scope: Scope::Key,
            id: Arc::from("k"),
            rpm,
            tpm,
            max_parallel: parallel,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rpm_window_counts_and_resets() {
        let limiter = MemoryRateLimiter::new();
        let descriptors = vec![descriptor(Some(2), None, None)];
        let first = limiter.try_acquire(&descriptors, 0).await.unwrap();
        assert_eq!(first.status.remaining_requests, Some(1));
        limiter.settle(first, None);
        limiter.settle(limiter.try_acquire(&descriptors, 0).await.unwrap(), None);
        let error = limiter.try_acquire(&descriptors, 0).await.unwrap_err();
        assert!(matches!(error, GatewayError::RateLimited { .. }));
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(limiter.try_acquire(&descriptors, 0).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn tokens_are_reserved_then_settled_to_actual() {
        let limiter = MemoryRateLimiter::new();
        let descriptors = vec![descriptor(None, Some(100), None)];
        let reservation = limiter.try_acquire(&descriptors, 80).await.unwrap();
        assert_eq!(reservation.status.remaining_tokens, Some(20));
        assert!(
            limiter.try_acquire(&descriptors, 30).await.is_err(),
            "80 reserved + 30 > 100"
        );
        limiter.settle(reservation, Some(10));
        assert!(
            limiter.try_acquire(&descriptors, 30).await.is_ok(),
            "settled to 10 actual"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn parallel_slots_are_released_on_settle() {
        let limiter = MemoryRateLimiter::new();
        let descriptors = vec![descriptor(None, None, Some(1))];
        let held = limiter.try_acquire(&descriptors, 0).await.unwrap();
        assert!(limiter.try_acquire(&descriptors, 0).await.is_err());
        limiter.settle(held, None);
        assert!(limiter.try_acquire(&descriptors, 0).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn all_or_nothing_across_descriptors() {
        let limiter = MemoryRateLimiter::new();
        let key = descriptor(Some(10), None, None);
        let team = Descriptor {
            scope: Scope::Team,
            id: Arc::from("t"),
            rpm: Some(1),
            tpm: None,
            max_parallel: None,
        };
        limiter.settle(
            limiter
                .try_acquire(&[key.clone(), team.clone()], 0)
                .await
                .unwrap(),
            None,
        );
        assert!(limiter.try_acquire(&[key.clone(), team], 0).await.is_err());
        let key_only = limiter.try_acquire(&[key], 0).await.unwrap();
        assert_eq!(
            key_only.status.remaining_requests,
            Some(8),
            "the failed attempt did not consume key quota"
        );
    }
}
