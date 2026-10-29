use std::sync::Arc;
use std::time::Duration;

use rustllmroute_types::GatewayError;
use tokio::time::Instant;

use crate::config::RouterConfig;
use crate::deployment::RoutedDeployment;
use crate::state::CooldownEntry;
use crate::stats::MinuteWindow;

const MIN_REQUESTS_FOR_RATE_RULE: u32 = 5;
const FAILURE_RATE_THRESHOLD: f64 = 0.5;

/// Whether this failure should take the deployment out of rotation, and for how long.
/// Port of LiteLLM's `_should_cooldown_deployment` with the lone-deployment safety rule.
pub fn decide(
    cfg: &RouterConfig,
    error: &GatewayError,
    window: MinuteWindow,
    group_size: usize,
    has_fallback: bool,
    deployment_cooldown: Option<Duration>,
) -> Option<Duration> {
    if cfg.disable_cooldowns || !error.cooldown_eligible() {
        return None;
    }
    if group_size == 1 && !has_fallback {
        return None;
    }
    let status = error.upstream_status();
    let cool = match cfg.allowed_fails {
        Some(allowed) => window.fails > allowed,
        None => {
            (status == Some(429) && group_size > 1)
                || (group_size > 1
                    && window.total() >= MIN_REQUESTS_FOR_RATE_RULE
                    && f64::from(window.fails) / f64::from(window.total()) > FAILURE_RATE_THRESHOLD)
                || !error.retryable_status()
        }
    };
    if !cool {
        return None;
    }
    let ttl = deployment_cooldown
        .or_else(|| error.retry_after())
        .unwrap_or(cfg.cooldown_time);
    (!ttl.is_zero()).then_some(ttl)
}

pub fn entry_for<D: RoutedDeployment>(
    deployment: &Arc<D>,
    error: &GatewayError,
    ttl: Duration,
) -> CooldownEntry {
    let mut reason = error.to_string();
    if reason.len() > 200 {
        let mut end = 200;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
    }
    tracing::warn!(
        deployment = %deployment.id(),
        group = %deployment.model_group(),
        ttl_s = ttl.as_secs_f64(),
        status = ?error.upstream_status(),
        "deployment cooled down"
    );
    CooldownEntry {
        until: Instant::now() + ttl,
        status: error.upstream_status(),
        kind: error.kind(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustllmroute_types::ErrorKind;

    use super::*;

    fn upstream(status: u16, kind: ErrorKind, retry_after: Option<Duration>) -> GatewayError {
        GatewayError::Upstream {
            provider: "t",
            status,
            kind,
            code: None,
            message: "boom".into(),
            retry_after,
            body: None,
        }
    }

    fn window(fails: u32, successes: u32) -> MinuteWindow {
        MinuteWindow {
            epoch_min: 1,
            rpm: fails + successes,
            tpm: 0,
            successes,
            fails,
        }
    }

    #[rstest]
    #[case::rate_limited_with_sibling(
        upstream(429, ErrorKind::RateLimit, None),
        window(1, 0),
        2,
        false,
        true
    )]
    #[case::lone_without_fallback(
        upstream(429, ErrorKind::RateLimit, None),
        window(9, 0),
        1,
        false,
        false
    )]
    #[case::lone_with_fallback_rate_rule(
        upstream(503, ErrorKind::Upstream5xx, None),
        window(9, 1),
        1,
        true,
        false
    )]
    #[case::rate_rule(
        upstream(503, ErrorKind::Upstream5xx, None),
        window(3, 2),
        2,
        false,
        true
    )]
    #[case::rate_rule_below_threshold(
        upstream(503, ErrorKind::Upstream5xx, None),
        window(2, 3),
        2,
        false,
        false
    )]
    #[case::too_few_requests(
        upstream(503, ErrorKind::Upstream5xx, None),
        window(2, 1),
        2,
        false,
        false
    )]
    #[case::non_retryable_immediate(
        upstream(401, ErrorKind::Auth, None),
        window(1, 0),
        2,
        false,
        true
    )]
    #[case::bad_request_ineligible(
        upstream(400, ErrorKind::BadRequest, None),
        window(9, 0),
        2,
        true,
        false
    )]
    fn default_mode_decisions(
        #[case] error: GatewayError,
        #[case] window: MinuteWindow,
        #[case] group_size: usize,
        #[case] has_fallback: bool,
        #[case] expected: bool,
    ) {
        let cfg = RouterConfig::default();
        assert_eq!(
            decide(&cfg, &error, window, group_size, has_fallback, None).is_some(),
            expected
        );
    }

    #[rstest]
    fn legacy_allowed_fails_mode_counts_failures() {
        let cfg = RouterConfig {
            allowed_fails: Some(2),
            ..RouterConfig::default()
        };
        let error = upstream(503, ErrorKind::Upstream5xx, None);
        assert!(decide(&cfg, &error, window(2, 0), 2, false, None).is_none());
        assert!(decide(&cfg, &error, window(3, 0), 2, false, None).is_some());
    }

    #[rstest]
    fn ttl_precedence_is_deployment_then_header_then_router() {
        let cfg = RouterConfig {
            cooldown_time: Duration::from_secs(30),
            ..RouterConfig::default()
        };
        let with_header = upstream(429, ErrorKind::RateLimit, Some(Duration::from_secs(7)));
        assert_eq!(
            decide(
                &cfg,
                &with_header,
                window(1, 0),
                2,
                false,
                Some(Duration::from_secs(3))
            ),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            decide(&cfg, &with_header, window(1, 0), 2, false, None),
            Some(Duration::from_secs(7))
        );
        let plain = upstream(429, ErrorKind::RateLimit, None);
        assert_eq!(
            decide(&cfg, &plain, window(1, 0), 2, false, None),
            Some(Duration::from_secs(30))
        );
        let disabled = RouterConfig {
            disable_cooldowns: true,
            ..cfg
        };
        assert!(decide(&disabled, &plain, window(1, 0), 2, false, None).is_none());
    }
}
