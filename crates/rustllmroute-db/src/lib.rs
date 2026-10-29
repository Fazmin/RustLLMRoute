mod budgets;
mod error;
mod keys;
mod kv;
mod pool;
mod rows;
mod spend;
mod teams;
mod users;

pub use budgets::{BudgetDue, BudgetEntity};
pub use error::DbError;
pub use keys::{KeyFilter, KeyPatch};
pub use pool::{Db, DbOptions, Page, Paged};
pub use rows::*;
pub use spend::{
    DailyDelta, DailyKey, DailySpendRow, SpendBatch, SpendBatchResult, SpendGroupBy, SpendGroupRow,
    SpendLogFilter, SpendRange,
};
pub use teams::TeamPatch;
pub use users::UserPatch;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// UTC day bucket for an epoch-millisecond timestamp, as `YYYY-MM-DD`.
pub fn day_of(epoch_ms: i64) -> String {
    let days = epoch_ms.div_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_buckets_are_utc_calendar_days() {
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(86_400_000 - 1), "1970-01-01");
        assert_eq!(day_of(86_400_000), "1970-01-02");
        assert_eq!(day_of(1_700_000_000_000), "2023-11-14");
    }
}
