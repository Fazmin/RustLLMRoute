use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Datelike, Months, TimeZone, Utc};
use regex::Regex;
use rustllmroute_db::{BudgetEntity, Db, now_ms};
use tokio_util::sync::CancellationToken;

use crate::cache::AuthCache;

static DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)(s|m|h|d|w|mo)$").expect("valid regex"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
}

pub fn parse_duration(text: &str) -> Option<(u32, Unit)> {
    let captures = DURATION.captures(text.trim())?;
    let amount: u32 = captures[1].parse().ok()?;
    if amount == 0 {
        return None;
    }
    let unit = match &captures[2] {
        "s" => Unit::Seconds,
        "m" => Unit::Minutes,
        "h" => Unit::Hours,
        "d" => Unit::Days,
        "w" => Unit::Weeks,
        "mo" => Unit::Months,
        _ => return None,
    };
    Some((amount, unit))
}

/// A plain `Duration` for retention-style settings ("90d", "12h").
pub fn to_duration(text: &str) -> Option<Duration> {
    let (amount, unit) = parse_duration(text)?;
    let amount = u64::from(amount);
    Some(match unit {
        Unit::Seconds => Duration::from_secs(amount),
        Unit::Minutes => Duration::from_secs(amount * 60),
        Unit::Hours => Duration::from_secs(amount * 3_600),
        Unit::Days => Duration::from_secs(amount * 86_400),
        Unit::Weeks => Duration::from_secs(amount * 7 * 86_400),
        Unit::Months => Duration::from_secs(amount * 30 * 86_400),
    })
}

/// Unaligned expiry, used for key `duration` ("30d" from now).
pub fn expires_at(now_ms: i64, text: &str) -> Option<i64> {
    let (amount, unit) = parse_duration(text)?;
    let amount = i64::from(amount);
    Some(match unit {
        Unit::Seconds => now_ms + amount * 1_000,
        Unit::Minutes => now_ms + amount * 60_000,
        Unit::Hours => now_ms + amount * 3_600_000,
        Unit::Days => now_ms + amount * 86_400_000,
        Unit::Weeks => now_ms + amount * 7 * 86_400_000,
        Unit::Months => {
            let at = Utc.timestamp_millis_opt(now_ms).single()?;
            at.checked_add_months(Months::new(amount as u32))?
                .timestamp_millis()
        }
    })
}

/// Aligned like LiteLLM's `get_next_standardized_reset_time`: seconds/minutes/hours to the next
/// multiple of the period, days/weeks to a UTC midnight, months to the first of a month.
pub fn next_reset_at(now_ms: i64, text: &str) -> Option<i64> {
    let (amount, unit) = parse_duration(text)?;
    let amount = i64::from(amount);
    let now: DateTime<Utc> = Utc.timestamp_millis_opt(now_ms).single()?;
    let period_ms = match unit {
        Unit::Seconds => Some(amount * 1_000),
        Unit::Minutes => Some(amount * 60_000),
        Unit::Hours => Some(amount * 3_600_000),
        Unit::Days => Some(amount * 86_400_000),
        Unit::Weeks => Some(amount * 7 * 86_400_000),
        Unit::Months => None,
    };
    match period_ms {
        Some(period) => Some(((now_ms / period) + 1) * period),
        None => {
            let first = Utc
                .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .single()?;
            Some(
                first
                    .checked_add_months(Months::new(amount as u32))?
                    .timestamp_millis(),
            )
        }
    }
}

pub struct ResetReport {
    pub keys: u64,
    pub teams: u64,
    pub users: u64,
}

/// One pass over every entity whose reset time has passed. Safe to repeat: the update is guarded
/// by the previous reset time.
pub async fn reset_due_budgets(
    db: &Db,
    cache: &AuthCache,
    now: i64,
) -> Result<ResetReport, rustllmroute_db::DbError> {
    let mut report = ResetReport {
        keys: 0,
        teams: 0,
        users: 0,
    };
    for entity in [BudgetEntity::Key, BudgetEntity::Team, BudgetEntity::User] {
        loop {
            let due = db.budgets_due(entity, now, 500).await?;
            if due.is_empty() {
                break;
            }
            let count = due.len();
            let resets: Vec<(String, i64, i64)> = due
                .into_iter()
                .filter_map(|row| {
                    let next = next_reset_at(now, &row.budget_duration)?;
                    Some((row.id, row.budget_reset_at, next))
                })
                .collect();
            if resets.is_empty() {
                tracing::warn!(
                    entity = entity.label(),
                    "budget rows due with unparseable durations; skipping"
                );
                break;
            }
            let updated = db.reset_budgets(entity, &resets).await?;
            for (id, _, _) in &resets {
                match entity {
                    BudgetEntity::Key => cache.invalidate_key(id).await,
                    BudgetEntity::Team => cache.invalidate_team(id),
                    BudgetEntity::User => cache.invalidate_user(id),
                }
            }
            match entity {
                BudgetEntity::Key => report.keys += updated,
                BudgetEntity::Team => report.teams += updated,
                BudgetEntity::User => report.users += updated,
            }
            if count < 500 {
                break;
            }
        }
    }
    Ok(report)
}

pub async fn budget_reset_loop(db: Db, cache: Arc<AuthCache>, shutdown: CancellationToken) {
    let mut first = true;
    loop {
        let wait = if first {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(597 + u64::from(rand::random::<u8>() % 9))
        };
        first = false;
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        match reset_due_budgets(&db, &cache, now_ms()).await {
            Ok(report) if report.keys + report.teams + report.users > 0 => {
                tracing::info!(
                    keys = report.keys,
                    teams = report.teams,
                    users = report.users,
                    "budgets reset"
                );
            }
            Ok(_) => {}
            Err(error) => tracing::error!(error = %error, "budget reset failed"),
        }
    }
}

pub async fn retention_loop(
    db: Db,
    keep: Duration,
    interval: Duration,
    shutdown: CancellationToken,
) {
    let mut first = true;
    loop {
        let wait = if first {
            Duration::from_secs(300)
        } else {
            interval
        };
        first = false;
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        let cutoff = now_ms() - keep.as_millis() as i64;
        let started = std::time::Instant::now();
        let mut deleted = 0u64;
        loop {
            match db.delete_spend_logs_before(cutoff, 1000).await {
                Ok(0) => break,
                Ok(n) => {
                    deleted += n;
                    if started.elapsed() > Duration::from_secs(300) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    tracing::error!(error = %error, "spend log retention failed");
                    break;
                }
            }
        }
        if deleted > 0 {
            tracing::info!(deleted, "old spend logs removed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::hour("1h", 1_700_000_000_000, 1_700_002_800_000)]
    #[case::day("1d", 1_700_000_000_000, 1_700_006_400_000)]
    #[case::week("1w", 1_700_000_000_000, 1_700_092_800_000)]
    #[case::month("1mo", 1_700_000_000_000, 1_701_388_800_000)]
    fn resets_align_to_period_boundaries(
        #[case] duration: &str,
        #[case] now: i64,
        #[case] expected: i64,
    ) {
        assert_eq!(next_reset_at(now, duration), Some(expected));
    }

    #[rstest]
    fn invalid_durations_are_rejected_and_expiry_is_unaligned() {
        assert_eq!(parse_duration("0d"), None);
        assert_eq!(parse_duration("1y"), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(expires_at(1_000, "30d"), Some(1_000 + 30 * 86_400_000));
        assert_eq!(
            expires_at(1_700_000_000_000, "1mo"),
            Some(1_702_592_000_000)
        );
        assert_eq!(to_duration("90d"), Some(Duration::from_secs(90 * 86_400)));
    }
}
