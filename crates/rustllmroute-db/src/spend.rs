use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::{QueryBuilder, Sqlite};

use crate::pool::{Page, Paged};
use crate::rows::SpendLogRow;
use crate::{Db, DbError, now_ms};

const LOG_COLUMNS: &str = "request_id, call_type, api_key, key_alias, user_id, team_id, end_user, spend, total_tokens, prompt_tokens, completion_tokens, cache_read_input_tokens, cache_creation_input_tokens, start_time, end_time, completion_start_time, request_duration_ms, model, model_id, model_group, custom_llm_provider, api_base, status, error, cache_hit, stream, partial, request_tags, metadata, request_body, response_body, session_id, litellm_call_id, created_at";
const DAILY_COLUMNS: &str = "day, api_key, model, custom_llm_provider, model_group, team_id, user_id, prompt_tokens, completion_tokens, cache_read_input_tokens, cache_creation_input_tokens, spend, api_requests, successful_requests, failed_requests";
const INSERT_CHUNK: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DailyKey {
    pub day: String,
    pub api_key: String,
    pub model: String,
    pub custom_llm_provider: String,
    pub model_group: String,
    pub team_id: String,
    pub user_id: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DailyDelta {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub spend: f64,
    pub api_requests: i64,
    pub successful_requests: i64,
    pub failed_requests: i64,
}

impl DailyDelta {
    pub fn add(&mut self, other: &DailyDelta) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.spend += other.spend;
        self.api_requests += other.api_requests;
        self.successful_requests += other.successful_requests;
        self.failed_requests += other.failed_requests;
    }
}

/// Everything one flush writes, pre-aggregated so the transaction is short.
#[derive(Clone, Debug, Default)]
pub struct SpendBatch {
    pub logs: Vec<SpendLogRow>,
    pub key_delta: BTreeMap<String, f64>,
    pub team_delta: BTreeMap<String, f64>,
    pub user_delta: BTreeMap<String, f64>,
    pub daily: BTreeMap<DailyKey, DailyDelta>,
    pub last_active: BTreeMap<String, i64>,
}

impl SpendBatch {
    pub fn is_empty(&self) -> bool {
        self.logs.is_empty()
            && self.key_delta.is_empty()
            && self.team_delta.is_empty()
            && self.user_delta.is_empty()
            && self.daily.is_empty()
    }

    pub fn merge(&mut self, other: SpendBatch) {
        self.logs.extend(other.logs);
        for (k, v) in other.key_delta {
            *self.key_delta.entry(k).or_default() += v;
        }
        for (k, v) in other.team_delta {
            *self.team_delta.entry(k).or_default() += v;
        }
        for (k, v) in other.user_delta {
            *self.user_delta.entry(k).or_default() += v;
        }
        for (k, v) in other.daily {
            self.daily.entry(k).or_default().add(&v);
        }
        for (k, v) in other.last_active {
            let slot = self.last_active.entry(k).or_insert(v);
            *slot = (*slot).max(v);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpendBatchResult {
    pub logs_written: u64,
    pub entities_updated: u64,
    pub daily_rows: u64,
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow, Serialize)]
pub struct DailySpendRow {
    pub day: String,
    pub api_key: String,
    pub model: String,
    pub custom_llm_provider: String,
    pub model_group: String,
    pub team_id: String,
    pub user_id: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub spend: f64,
    pub api_requests: i64,
    pub successful_requests: i64,
    pub failed_requests: i64,
}

#[derive(Clone, Debug, Default)]
pub struct SpendLogFilter {
    pub api_key: Option<String>,
    pub user_id: Option<String>,
    pub team_id: Option<String>,
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub status: Option<String>,
    pub model: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct SpendRange {
    pub start_day: String,
    pub end_day: String,
    pub api_key: Option<String>,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpendGroupBy {
    Model,
    ApiKey,
    Team,
    User,
    Provider,
    ModelGroup,
}

impl SpendGroupBy {
    fn column(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::ApiKey => "api_key",
            Self::Team => "team_id",
            Self::User => "user_id",
            Self::Provider => "custom_llm_provider",
            Self::ModelGroup => "model_group",
        }
    }
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow, Serialize)]
pub struct SpendGroupRow {
    pub key: String,
    pub spend: f64,
    pub total_tokens: i64,
    pub requests: i64,
}

fn push_log_filter(builder: &mut QueryBuilder<Sqlite>, filter: &SpendLogFilter) {
    builder.push(" WHERE 1 = 1");
    macro_rules! eq {
        ($field:ident, $column:literal) => {
            if let Some(value) = &filter.$field {
                builder
                    .push(concat!(" AND ", $column, " = "))
                    .push_bind(value.clone());
            }
        };
    }
    eq!(api_key, "api_key");
    eq!(user_id, "user_id");
    eq!(team_id, "team_id");
    eq!(request_id, "request_id");
    eq!(session_id, "session_id");
    eq!(status, "status");
    eq!(model, "model");
    if let Some(start) = filter.start_ms {
        builder.push(" AND start_time >= ").push_bind(start);
    }
    if let Some(end) = filter.end_ms {
        builder.push(" AND start_time <= ").push_bind(end);
    }
}

fn push_range(builder: &mut QueryBuilder<Sqlite>, range: &SpendRange) {
    builder
        .push(" WHERE day >= ")
        .push_bind(range.start_day.clone());
    builder
        .push(" AND day <= ")
        .push_bind(range.end_day.clone());
    if let Some(value) = &range.api_key {
        builder.push(" AND api_key = ").push_bind(value.clone());
    }
    if let Some(value) = &range.team_id {
        builder.push(" AND team_id = ").push_bind(value.clone());
    }
    if let Some(value) = &range.user_id {
        builder.push(" AND user_id = ").push_bind(value.clone());
    }
    if let Some(value) = &range.model {
        builder.push(" AND model = ").push_bind(value.clone());
    }
}

impl Db {
    /// One `BEGIN IMMEDIATE` transaction: spend log rows (duplicates ignored), entity spend
    /// counters, daily aggregates and key `last_active_at`.
    pub async fn apply_spend_batch(&self, batch: &SpendBatch) -> Result<SpendBatchResult, DbError> {
        let mut result = SpendBatchResult::default();
        if batch.is_empty() {
            return Ok(result);
        }
        let now = now_ms();
        let mut conn = self.writer().acquire().await?;
        let mut tx = sqlx::Connection::begin_with(&mut *conn, "BEGIN IMMEDIATE").await?;
        for chunk in batch.logs.chunks(INSERT_CHUNK) {
            let mut builder: QueryBuilder<Sqlite> =
                QueryBuilder::new(format!("INSERT INTO spend_logs ({LOG_COLUMNS}) "));
            builder.push_values(chunk, |mut b, row| {
                b.push_bind(&row.request_id)
                    .push_bind(&row.call_type)
                    .push_bind(&row.api_key)
                    .push_bind(&row.key_alias)
                    .push_bind(&row.user_id)
                    .push_bind(&row.team_id)
                    .push_bind(&row.end_user)
                    .push_bind(row.spend)
                    .push_bind(row.total_tokens)
                    .push_bind(row.prompt_tokens)
                    .push_bind(row.completion_tokens)
                    .push_bind(row.cache_read_input_tokens)
                    .push_bind(row.cache_creation_input_tokens)
                    .push_bind(row.start_time)
                    .push_bind(row.end_time)
                    .push_bind(row.completion_start_time)
                    .push_bind(row.request_duration_ms)
                    .push_bind(&row.model)
                    .push_bind(&row.model_id)
                    .push_bind(&row.model_group)
                    .push_bind(&row.custom_llm_provider)
                    .push_bind(&row.api_base)
                    .push_bind(&row.status)
                    .push_bind(&row.error)
                    .push_bind(row.cache_hit)
                    .push_bind(row.stream)
                    .push_bind(row.partial)
                    .push_bind(&row.request_tags)
                    .push_bind(&row.metadata)
                    .push_bind(&row.request_body)
                    .push_bind(&row.response_body)
                    .push_bind(&row.session_id)
                    .push_bind(&row.litellm_call_id)
                    .push_bind(row.created_at);
            });
            builder.push(" ON CONFLICT (request_id) DO NOTHING");
            result.logs_written += builder.build().execute(&mut *tx).await?.rows_affected();
        }
        for (table, column, deltas) in [
            ("keys", "token", &batch.key_delta),
            ("teams", "team_id", &batch.team_delta),
            ("users", "user_id", &batch.user_delta),
        ] {
            for (id, delta) in deltas {
                let sql = format!(
                    "UPDATE {table} SET spend = spend + $1, updated_at = $2 WHERE {column} = $3"
                );
                result.entities_updated += sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(delta)
                    .bind(now)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
        }
        for (key, delta) in &batch.daily {
            let sql = format!(
                "INSERT INTO daily_spend ({DAILY_COLUMNS}, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
                 ON CONFLICT (day, api_key, model, custom_llm_provider, model_group, team_id, user_id) DO UPDATE SET \
                 prompt_tokens = daily_spend.prompt_tokens + excluded.prompt_tokens, \
                 completion_tokens = daily_spend.completion_tokens + excluded.completion_tokens, \
                 cache_read_input_tokens = daily_spend.cache_read_input_tokens + excluded.cache_read_input_tokens, \
                 cache_creation_input_tokens = daily_spend.cache_creation_input_tokens + excluded.cache_creation_input_tokens, \
                 spend = daily_spend.spend + excluded.spend, \
                 api_requests = daily_spend.api_requests + excluded.api_requests, \
                 successful_requests = daily_spend.successful_requests + excluded.successful_requests, \
                 failed_requests = daily_spend.failed_requests + excluded.failed_requests, \
                 updated_at = excluded.updated_at"
            );
            result.daily_rows += sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&key.day)
                .bind(&key.api_key)
                .bind(&key.model)
                .bind(&key.custom_llm_provider)
                .bind(&key.model_group)
                .bind(&key.team_id)
                .bind(&key.user_id)
                .bind(delta.prompt_tokens)
                .bind(delta.completion_tokens)
                .bind(delta.cache_read_input_tokens)
                .bind(delta.cache_creation_input_tokens)
                .bind(delta.spend)
                .bind(delta.api_requests)
                .bind(delta.successful_requests)
                .bind(delta.failed_requests)
                .bind(now)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        for (token, at) in &batch.last_active {
            sqlx::query("UPDATE keys SET last_active_at = $1 WHERE token = $2 AND (last_active_at IS NULL OR last_active_at < $1)")
                .bind(at)
                .bind(token)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(result)
    }

    pub async fn query_spend_logs(
        &self,
        filter: &SpendLogFilter,
        page: Page,
    ) -> Result<Paged<SpendLogRow>, DbError> {
        let mut count: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT COUNT(*) FROM spend_logs");
        push_log_filter(&mut count, filter);
        let total: i64 = count.build_query_scalar().fetch_one(self.reader()).await?;
        let mut select: QueryBuilder<Sqlite> =
            QueryBuilder::new(format!("SELECT {LOG_COLUMNS} FROM spend_logs"));
        push_log_filter(&mut select, filter);
        select.push(" ORDER BY start_time DESC LIMIT ");
        select.push_bind(page.limit());
        select.push(" OFFSET ");
        select.push_bind(page.offset());
        let items = select
            .build_query_as::<SpendLogRow>()
            .fetch_all(self.reader())
            .await?;
        Ok(Paged {
            items,
            total,
            page: page.page,
            size: page.size,
        })
    }

    pub async fn spend_summary(
        &self,
        range: &SpendRange,
        group_by: SpendGroupBy,
    ) -> Result<Vec<SpendGroupRow>, DbError> {
        let column = group_by.column();
        let mut builder: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
            "SELECT {column} AS key, COALESCE(SUM(spend), 0) AS spend, COALESCE(SUM(prompt_tokens + completion_tokens), 0) AS total_tokens, COALESCE(SUM(api_requests), 0) AS requests FROM daily_spend"
        ));
        push_range(&mut builder, range);
        builder.push(format!(" GROUP BY {column} ORDER BY spend DESC LIMIT 1000"));
        Ok(builder
            .build_query_as::<SpendGroupRow>()
            .fetch_all(self.reader())
            .await?)
    }

    pub async fn daily_activity(&self, range: &SpendRange) -> Result<Vec<DailySpendRow>, DbError> {
        let mut builder: QueryBuilder<Sqlite> =
            QueryBuilder::new(format!("SELECT {DAILY_COLUMNS} FROM daily_spend"));
        push_range(&mut builder, range);
        builder.push(" ORDER BY day ASC LIMIT 100000");
        Ok(builder
            .build_query_as::<DailySpendRow>()
            .fetch_all(self.reader())
            .await?)
    }

    pub async fn global_spend(&self) -> Result<f64, DbError> {
        Ok(
            sqlx::query_scalar::<_, f64>("SELECT COALESCE(SUM(spend), 0) FROM daily_spend")
                .fetch_one(self.reader())
                .await?,
        )
    }

    pub async fn delete_spend_logs_before(
        &self,
        cutoff_ms: i64,
        limit: u32,
    ) -> Result<u64, DbError> {
        Ok(sqlx::query(
            "DELETE FROM spend_logs WHERE request_id IN (SELECT request_id FROM spend_logs WHERE start_time < $1 LIMIT $2)",
        )
        .bind(cutoff_ms)
        .bind(i64::from(limit))
        .execute(self.writer())
        .await?
        .rows_affected())
    }
}
