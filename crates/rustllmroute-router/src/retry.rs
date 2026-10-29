use std::time::Duration;

use rand::Rng;
use rustllmroute_types::{ErrorKind, GatewayError};

use crate::config::RouterConfig;

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);
const MAX_JITTER: Duration = Duration::from_millis(750);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

pub struct RetryContext<'a> {
    pub cfg: &'a RouterConfig,
    pub group: &'a str,
    pub group_size: usize,
    pub healthy_siblings: usize,
}

/// Port of LiteLLM's `should_retry_this_error`, simplified.
pub fn should_retry(ctx: &RetryContext<'_>, error: &GatewayError) -> bool {
    match error.kind() {
        ErrorKind::ContextWindow if ctx.cfg.context_fallbacks(ctx.group).is_some() => return false,
        ErrorKind::ContentPolicy if ctx.cfg.content_fallbacks(ctx.group).is_some() => return false,
        ErrorKind::NotFound | ErrorKind::BadRequest => return false,
        ErrorKind::Auth => return ctx.group_size > 1 && ctx.healthy_siblings > 0,
        ErrorKind::RateLimit
            if ctx.healthy_siblings == 0 && !ctx.cfg.general_fallbacks(ctx.group).is_empty() =>
        {
            return false;
        }
        _ => {}
    }
    if ctx.group_size > 1 && ctx.healthy_siblings == 0 {
        return false;
    }
    error.retryable()
}

/// Zero when a healthy sibling exists; otherwise `Retry-After` (1..=60s) or capped exponential
/// backoff, each plus up to 750ms of jitter.
pub fn backoff(ctx: &RetryContext<'_>, error: &GatewayError, attempt: u32) -> Duration {
    if ctx.group_size > 1 && ctx.healthy_siblings > 0 {
        return Duration::ZERO;
    }
    let jitter = rand::rng().random_range(Duration::ZERO..=MAX_JITTER);
    if let Some(retry_after) = error.retry_after()
        && retry_after >= Duration::from_secs(1)
        && retry_after <= MAX_RETRY_AFTER
    {
        return retry_after + jitter;
    }
    let exponential = INITIAL_RETRY_DELAY
        .checked_mul(1u32 << attempt.min(8))
        .unwrap_or(MAX_RETRY_DELAY)
        .min(MAX_RETRY_DELAY)
        .max(ctx.cfg.retry_after_min);
    exponential + jitter
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustllmroute_types::RateLimitScope;

    use super::*;

    fn upstream(status: u16, kind: ErrorKind) -> GatewayError {
        GatewayError::Upstream {
            provider: "t",
            status,
            kind,
            code: None,
            message: String::new(),
            retry_after: None,
            body: None,
        }
    }

    fn ctx<'a>(
        cfg: &'a RouterConfig,
        group_size: usize,
        healthy_siblings: usize,
    ) -> RetryContext<'a> {
        RetryContext {
            cfg,
            group: "g",
            group_size,
            healthy_siblings,
        }
    }

    #[rstest]
    #[case::rate_limit_with_sibling(upstream(429, ErrorKind::RateLimit), 2, 1, true)]
    #[case::rate_limit_alone(upstream(429, ErrorKind::RateLimit), 1, 0, true)]
    #[case::server_error(upstream(503, ErrorKind::Upstream5xx), 1, 0, true)]
    #[case::auth_with_sibling(upstream(401, ErrorKind::Auth), 2, 1, true)]
    #[case::auth_alone(upstream(401, ErrorKind::Auth), 1, 0, false)]
    #[case::not_found(upstream(404, ErrorKind::NotFound), 2, 1, false)]
    #[case::bad_request(upstream(400, ErrorKind::BadRequest), 2, 1, false)]
    #[case::all_siblings_cooled(upstream(503, ErrorKind::Upstream5xx), 3, 0, false)]
    #[case::key_limit(GatewayError::RateLimited { message: "x".into(), retry_after: None, scope: RateLimitScope::Key }, 2, 1, false)]
    fn retry_decisions(
        #[case] error: GatewayError,
        #[case] group_size: usize,
        #[case] healthy: usize,
        #[case] expected: bool,
    ) {
        let cfg = RouterConfig::default();
        assert_eq!(
            should_retry(&ctx(&cfg, group_size, healthy), &error),
            expected
        );
    }

    #[rstest]
    fn context_window_defers_to_fallbacks_when_configured() {
        let mut cfg = RouterConfig::default();
        let error = GatewayError::Upstream {
            provider: "t",
            status: 400,
            kind: ErrorKind::ContextWindow,
            code: None,
            message: String::new(),
            retry_after: None,
            body: None,
        };
        assert!(!should_retry(&ctx(&cfg, 1, 0), &error), "400s never retry");
        cfg.context_window_fallbacks = vec![
            [("g".to_owned(), vec!["big".to_owned()])]
                .into_iter()
                .collect(),
        ];
        assert!(!should_retry(&ctx(&cfg, 2, 1), &error));
    }

    #[rstest]
    fn backoff_is_zero_with_siblings_and_bounded_otherwise() {
        let cfg = RouterConfig::default();
        let error = upstream(503, ErrorKind::Upstream5xx);
        assert_eq!(backoff(&ctx(&cfg, 2, 1), &error, 0), Duration::ZERO);
        for attempt in 0..6 {
            let wait = backoff(&ctx(&cfg, 1, 0), &error, attempt);
            let base = (INITIAL_RETRY_DELAY * (1 << attempt)).min(MAX_RETRY_DELAY);
            assert!(
                wait >= base && wait <= base + MAX_JITTER,
                "attempt {attempt}: {wait:?}"
            );
        }
        let mut limited = upstream(429, ErrorKind::RateLimit);
        if let GatewayError::Upstream { retry_after, .. } = &mut limited {
            *retry_after = Some(Duration::from_secs(7));
        }
        let wait = backoff(&ctx(&cfg, 1, 0), &limited, 0);
        assert!(wait >= Duration::from_secs(7) && wait <= Duration::from_secs(7) + MAX_JITTER);
    }
}
