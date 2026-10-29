use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Query, State};
use rustllmroute_db::{
    DailySpendRow, Page, SpendGroupBy, SpendLogFilter, SpendLogRow, SpendRange, now_ms,
};
use rustllmroute_types::GatewayError;
use serde_json::{Map, Value, json};

use super::api::{ApiError, ApiResult, Caller, PlaneState, RequireAdmin};
use super::dto::{
    SpendLogsQuery, SpendRangeQuery, day_string, parse_date_end, parse_date_start, rfc3339,
};
use crate::credentials::{hash_token, is_virtual_key};

const DEFAULT_WINDOW_MS: i64 = 7 * 86_400_000;

fn bad_date(param: &str, value: &str) -> ApiError {
    ApiError(GatewayError::BadRequest {
        message: format!("{param} must be YYYY-MM-DD or RFC 3339 (got {value:?})"),
        param: Some(param.to_owned()),
        code: None,
    })
}

fn log_json(row: &SpendLogRow) -> Value {
    let mut value = serde_json::to_value(row).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.insert(
            "startTime".to_owned(),
            Value::String(rfc3339(row.start_time)),
        );
        map.insert("endTime".to_owned(), Value::String(rfc3339(row.end_time)));
        for key in ["metadata", "request_tags", "error"] {
            if let Some(Value::String(text)) = map.get(key)
                && let Ok(parsed) = serde_json::from_str::<Value>(text)
            {
                map.insert(key.to_owned(), parsed);
            }
        }
    }
    value
}

fn filter_from(
    query: &SpendLogsQuery,
    auth: &rustllmroute_core::AuthContext,
) -> ApiResult<SpendLogFilter> {
    let now = now_ms();
    let start_ms = match &query.start_date {
        Some(text) => Some(parse_date_start(text).ok_or_else(|| bad_date("start_date", text))?),
        None => Some(now - DEFAULT_WINDOW_MS),
    };
    let end_ms = match &query.end_date {
        Some(text) => Some(parse_date_end(text).ok_or_else(|| bad_date("end_date", text))?),
        None => None,
    };
    let mut filter = SpendLogFilter {
        api_key: query.api_key.as_deref().map(|k| {
            if is_virtual_key(k) {
                hash_token(k)
            } else {
                k.to_owned()
            }
        }),
        user_id: query.user_id.clone(),
        team_id: query.team_id.clone(),
        request_id: query.request_id.clone(),
        session_id: query.session_id.clone(),
        status: query.status_filter.clone(),
        model: query.model.clone(),
        start_ms,
        end_ms,
    };
    if filter.request_id.is_some() {
        filter.start_ms = None;
    }
    if !auth.is_admin {
        match &auth.key_hash {
            Some(own) => filter.api_key = Some(own.to_string()),
            None => {
                return Err(ApiError(GatewayError::Forbidden(
                    "admin access required".to_owned(),
                )));
            }
        }
    }
    Ok(filter)
}

fn page_of(page: Option<u32>, size: Option<u32>) -> Page {
    Page::new(page.unwrap_or(1), size.unwrap_or(50))
}

pub async fn logs(
    State(plane): State<PlaneState>,
    Caller(auth): Caller,
    Query(query): Query<SpendLogsQuery>,
) -> ApiResult<Json<Value>> {
    let filter = filter_from(&query, &auth)?;
    let paged = plane
        .db()
        .query_spend_logs(&filter, page_of(query.page, query.page_size))
        .await?;
    Ok(Json(Value::Array(
        paged.items.iter().map(log_json).collect(),
    )))
}

pub async fn logs_ui(
    State(plane): State<PlaneState>,
    Caller(auth): Caller,
    Query(query): Query<SpendLogsQuery>,
) -> ApiResult<Json<Value>> {
    let filter = filter_from(&query, &auth)?;
    let paged = plane
        .db()
        .query_spend_logs(&filter, page_of(query.page, query.page_size))
        .await?;
    Ok(Json(json!({
        "data": paged.items.iter().map(log_json).collect::<Vec<_>>(),
        "total": paged.total,
        "page": paged.page,
        "page_size": paged.size,
        "total_pages": paged.total_pages(),
    })))
}

fn range_from(query: &SpendRangeQuery) -> ApiResult<SpendRange> {
    let now = now_ms();
    let start = match &query.start_date {
        Some(text) => parse_date_start(text).ok_or_else(|| bad_date("start_date", text))?,
        None => now - 30 * 86_400_000,
    };
    let end = match &query.end_date {
        Some(text) => parse_date_end(text).ok_or_else(|| bad_date("end_date", text))?,
        None => now,
    };
    Ok(SpendRange {
        start_day: day_string(start),
        end_day: day_string(end),
        api_key: query.api_key.as_deref().map(|k| {
            if is_virtual_key(k) {
                hash_token(k)
            } else {
                k.to_owned()
            }
        }),
        team_id: query.team_id.clone(),
        user_id: query.user_id.clone(),
        model: query.model.clone(),
    })
}

async fn grouped(
    plane: &PlaneState,
    query: &SpendRangeQuery,
    group_by: SpendGroupBy,
    label: &str,
) -> ApiResult<Json<Value>> {
    let rows = plane
        .db()
        .spend_summary(&range_from(query)?, group_by)
        .await?;
    Ok(Json(Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    label: row.key,
                    "spend": row.spend,
                    "total_tokens": row.total_tokens,
                    "requests": row.requests,
                })
            })
            .collect(),
    )))
}

pub async fn by_model(
    State(plane): State<PlaneState>,
    RequireAdmin(_): RequireAdmin,
    Query(query): Query<SpendRangeQuery>,
) -> ApiResult<Json<Value>> {
    grouped(&plane, &query, SpendGroupBy::Model, "model").await
}

pub async fn by_key(
    State(plane): State<PlaneState>,
    RequireAdmin(_): RequireAdmin,
    Query(query): Query<SpendRangeQuery>,
) -> ApiResult<Json<Value>> {
    grouped(&plane, &query, SpendGroupBy::ApiKey, "api_key").await
}

pub async fn by_team(
    State(plane): State<PlaneState>,
    RequireAdmin(_): RequireAdmin,
    Query(query): Query<SpendRangeQuery>,
) -> ApiResult<Json<Value>> {
    grouped(&plane, &query, SpendGroupBy::Team, "team_id").await
}

pub async fn by_user(
    State(plane): State<PlaneState>,
    RequireAdmin(_): RequireAdmin,
    Query(query): Query<SpendRangeQuery>,
) -> ApiResult<Json<Value>> {
    grouped(&plane, &query, SpendGroupBy::User, "user_id").await
}

pub async fn by_provider(
    State(plane): State<PlaneState>,
    RequireAdmin(_): RequireAdmin,
    Query(query): Query<SpendRangeQuery>,
) -> ApiResult<Json<Value>> {
    grouped(&plane, &query, SpendGroupBy::Provider, "provider").await
}

pub async fn global(
    State(plane): State<PlaneState>,
    RequireAdmin(_): RequireAdmin,
) -> ApiResult<Json<Value>> {
    let spend = plane.db().global_spend().await?;
    Ok(Json(json!({ "spend": spend, "max_budget": Value::Null })))
}

type DayBreakdown = BTreeMap<&'static str, BTreeMap<String, Acc>>;

#[derive(Default)]
struct Acc {
    prompt: i64,
    completion: i64,
    cache_read: i64,
    cache_creation: i64,
    spend: f64,
    requests: i64,
    ok: i64,
    failed: i64,
}

impl Acc {
    fn add(&mut self, row: &DailySpendRow) {
        self.prompt += row.prompt_tokens;
        self.completion += row.completion_tokens;
        self.cache_read += row.cache_read_input_tokens;
        self.cache_creation += row.cache_creation_input_tokens;
        self.spend += row.spend;
        self.requests += row.api_requests;
        self.ok += row.successful_requests;
        self.failed += row.failed_requests;
    }

    fn json(&self) -> Value {
        json!({
            "spend": self.spend,
            "prompt_tokens": self.prompt,
            "completion_tokens": self.completion,
            "cache_read_input_tokens": self.cache_read,
            "cache_creation_input_tokens": self.cache_creation,
            "total_tokens": self.prompt + self.completion,
            "api_requests": self.requests,
            "successful_requests": self.ok,
            "failed_requests": self.failed,
        })
    }
}

/// LiteLLM's `/user/daily/activity` shape: one entry per day with totals and per-dimension breakdowns.
pub async fn daily_activity(
    State(plane): State<PlaneState>,
    Caller(auth): Caller,
    Query(mut query): Query<SpendRangeQuery>,
) -> ApiResult<Json<Value>> {
    if !auth.is_admin {
        match &auth.key_hash {
            Some(own) => query.api_key = Some(own.to_string()),
            None => {
                return Err(ApiError(GatewayError::Forbidden(
                    "admin access required".to_owned(),
                )));
            }
        }
    }
    let rows = plane.db().daily_activity(&range_from(&query)?).await?;
    let mut days: BTreeMap<String, (Acc, DayBreakdown)> = BTreeMap::new();
    let mut total = Acc::default();
    for row in &rows {
        total.add(row);
        let (day_total, breakdown) = days.entry(row.day.clone()).or_default();
        day_total.add(row);
        for (dimension, value) in [
            ("models", row.model.as_str()),
            ("providers", row.custom_llm_provider.as_str()),
            ("api_keys", row.api_key.as_str()),
            ("model_groups", row.model_group.as_str()),
            ("teams", row.team_id.as_str()),
        ] {
            if value.is_empty() {
                continue;
            }
            breakdown
                .entry(dimension)
                .or_default()
                .entry(value.to_owned())
                .or_default()
                .add(row);
        }
    }
    let results: Vec<Value> = days
        .iter()
        .map(|(day, (day_total, breakdown))| {
            let mut dims = Map::new();
            for (dimension, entries) in breakdown {
                let map: Map<String, Value> = entries
                    .iter()
                    .map(|(k, acc)| (k.clone(), json!({ "metrics": acc.json() })))
                    .collect();
                dims.insert((*dimension).to_owned(), Value::Object(map));
            }
            json!({ "date": day, "metrics": day_total.json(), "breakdown": dims })
        })
        .collect();
    let page = page_of(query.page, query.page_size);
    let total_pages = (results.len() as i64 + i64::from(page.size) - 1) / i64::from(page.size);
    let start = usize::try_from(page.offset())
        .unwrap_or(0)
        .min(results.len());
    let end = (start + page.size as usize).min(results.len());
    Ok(Json(json!({
        "results": results[start..end],
        "metadata": {
            "total_spend": total.spend,
            "total_prompt_tokens": total.prompt,
            "total_completion_tokens": total.completion,
            "total_tokens": total.prompt + total.completion,
            "total_api_requests": total.requests,
            "total_successful_requests": total.ok,
            "total_failed_requests": total.failed,
            "page": page.page,
            "total_pages": total_pages,
            "has_more": i64::from(page.page) < total_pages,
        }
    })))
}
