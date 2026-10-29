use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rustllmroute_core::{AuthContext, CallRecord, CallStatus, RequestKind};
use rustllmroute_db::{DailyDelta, DailyKey, Db, SpendBatch, SpendLogRow, day_of, now_ms};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::cache::AuthCache;

/// Spend recorded but not yet flushed; budget checks add it to the cached row so the very next
/// request already sees it.
#[derive(Default)]
pub struct PendingSpend {
    keys: DashMap<String, f64>,
    teams: DashMap<String, f64>,
    users: DashMap<String, f64>,
}

impl PendingSpend {
    pub fn key(&self, hash: &str) -> f64 {
        self.keys.get(hash).map(|v| *v).unwrap_or(0.0)
    }

    pub fn team(&self, id: &str) -> f64 {
        self.teams.get(id).map(|v| *v).unwrap_or(0.0)
    }

    pub fn user(&self, id: &str) -> f64 {
        self.users.get(id).map(|v| *v).unwrap_or(0.0)
    }

    pub fn add_key(&self, hash: &str, delta: f64) {
        *self.keys.entry(hash.to_owned()).or_default() += delta;
    }

    fn add(&self, batch: &SpendBatch) {
        for (k, v) in &batch.key_delta {
            *self.keys.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &batch.team_delta {
            *self.teams.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &batch.user_delta {
            *self.users.entry(k.clone()).or_default() += v;
        }
    }

    fn subtract(&self, batch: &SpendBatch) {
        fn take(map: &DashMap<String, f64>, k: &str, v: f64) {
            if let Some(mut entry) = map.get_mut(k) {
                *entry -= v;
                if entry.abs() < 1e-12 {
                    drop(entry);
                    map.remove_if(k, |_, value| value.abs() < 1e-12);
                }
            }
        }
        for (k, v) in &batch.key_delta {
            take(&self.keys, k, *v);
        }
        for (k, v) in &batch.team_delta {
            take(&self.teams, k, *v);
        }
        for (k, v) in &batch.user_delta {
            take(&self.users, k, *v);
        }
    }
}

#[derive(Clone, Debug)]
pub struct SpendSettings {
    pub flush_interval: Duration,
    pub queue_capacity: usize,
    pub batch_rows: usize,
    pub disable_spend_logs: bool,
    pub max_held_rows: usize,
}

impl Default for SpendSettings {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_secs(10),
            queue_capacity: 10_000,
            batch_rows: 1_000,
            disable_spend_logs: false,
            max_held_rows: 50_000,
        }
    }
}

fn ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn call_type(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::ChatCompletion => "chat_completion",
        RequestKind::Messages => "messages",
        RequestKind::Responses => "responses",
        RequestKind::Embedding => "embedding",
    }
}

/// Builds the rows for one finished call. Failures are logged with zero spend.
pub fn batch_for(record: &CallRecord, auth: &AuthContext, store_logs: bool) -> SpendBatch {
    let api_key = auth.key_hash.as_deref().unwrap_or("").to_owned();
    let start = ms(record.started_at);
    let end = ms(record.ended_at);
    let (status, error, partial) = match &record.status {
        CallStatus::Ok => ("success", None, false),
        CallStatus::ClientDisconnected => ("success", None, true),
        CallStatus::Error(error) => (
            "failure",
            Some(
                json!({
                    "status_code": error.status().as_u16(),
                    "type": error.openai_type(),
                    "message": error.to_string().chars().take(1024).collect::<String>(),
                })
                .to_string(),
            ),
            false,
        ),
    };
    let success = status == "success";
    let model = record.provider_model.clone().unwrap_or_default();
    let provider = record.provider.map(str::to_owned);
    let mut batch = SpendBatch::default();
    if store_logs {
        batch.logs.push(SpendLogRow {
            request_id: record.request_id.clone(),
            call_type: call_type(record.kind).to_owned(),
            api_key: api_key.clone(),
            key_alias: auth.key_alias.clone(),
            user_id: auth.user_id.clone(),
            team_id: auth.team_id.clone(),
            end_user: None,
            spend: record.cost_usd,
            total_tokens: record
                .usage
                .total_tokens
                .max(record.usage.prompt_tokens + record.usage.completion_tokens)
                as i64,
            prompt_tokens: record.usage.prompt_tokens as i64,
            completion_tokens: record.usage.completion_tokens as i64,
            cache_read_input_tokens: record.usage.cached_tokens() as i64,
            cache_creation_input_tokens: record.usage.cache_creation_tokens() as i64,
            start_time: start,
            end_time: end,
            completion_start_time: record.first_byte_at.map(|at| {
                start
                    + at.saturating_duration_since(record.started_instant)
                        .as_millis() as i64
            }),
            request_duration_ms: Some((end - start).max(0)),
            model: model.clone(),
            model_id: record.deployment_id.as_deref().map(str::to_owned),
            model_group: record.model_group.clone(),
            custom_llm_provider: provider.clone(),
            api_base: record.api_base.clone(),
            status: status.to_owned(),
            error,
            cache_hit: false,
            stream: record.stream,
            partial,
            request_tags: "[]".to_owned(),
            metadata: json!({
                "requested_model": record.requested_model,
                "attempted_retries": record.attempted_retries,
                "attempted_fallbacks": record.attempted_fallbacks,
                "pricing_known": record.pricing_known,
            })
            .to_string(),
            request_body: None,
            response_body: None,
            session_id: None,
            litellm_call_id: Some(record.request_id.clone()),
            created_at: now_ms(),
        });
    }
    if record.cost_usd > 0.0 {
        if !api_key.is_empty() {
            batch.key_delta.insert(api_key.clone(), record.cost_usd);
        }
        if let Some(team) = &auth.team_id {
            batch.team_delta.insert(team.clone(), record.cost_usd);
        }
        if let Some(user) = &auth.user_id {
            batch.user_delta.insert(user.clone(), record.cost_usd);
        }
    }
    batch.daily.insert(
        DailyKey {
            day: day_of(start),
            api_key: api_key.clone(),
            model,
            custom_llm_provider: provider.unwrap_or_default(),
            model_group: record.model_group.clone().unwrap_or_default(),
            team_id: auth.team_id.clone().unwrap_or_default(),
            user_id: auth.user_id.clone().unwrap_or_default(),
        },
        DailyDelta {
            prompt_tokens: record.usage.prompt_tokens as i64,
            completion_tokens: record.usage.completion_tokens as i64,
            cache_read_input_tokens: record.usage.cached_tokens() as i64,
            cache_creation_input_tokens: record.usage.cache_creation_tokens() as i64,
            spend: record.cost_usd,
            api_requests: 1,
            successful_requests: i64::from(success),
            failed_requests: i64::from(!success),
        },
    );
    if !api_key.is_empty() {
        batch.last_active.insert(api_key, end);
    }
    batch
}

/// Hands finished calls to the writer task. Never blocks the request path.
#[derive(Clone)]
pub struct SpendSink {
    tx: mpsc::Sender<SpendBatch>,
    pending: Arc<PendingSpend>,
    store_logs: bool,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl SpendSink {
    pub fn record(&self, record: &CallRecord, auth: &AuthContext) {
        let batch = batch_for(record, auth, self.store_logs);
        self.pending.add(&batch);
        metrics::counter!(rustllmroute_types::metrics::SPEND_EVENTS, "status" => "queued")
            .increment(1);
        if let Err(error) = self.tx.try_send(batch) {
            let batch = match error {
                mpsc::error::TrySendError::Full(batch)
                | mpsc::error::TrySendError::Closed(batch) => batch,
            };
            self.pending.subtract(&batch);
            metrics::counter!(rustllmroute_types::metrics::SPEND_EVENTS_DROPPED).increment(1);
            let dropped = self
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if dropped.is_power_of_two() {
                tracing::error!(dropped, "spend queue full; dropping spend events");
            }
        }
    }

    pub fn pending(&self) -> &Arc<PendingSpend> {
        &self.pending
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub struct SpendWriter {
    pub sink: SpendSink,
    pub task: tokio::task::JoinHandle<()>,
}

/// Spawns the single writer task: accumulate, flush every interval or at `batch_rows`, retry with
/// backoff, settle the pending map and cached rows after each successful flush.
pub fn spawn_writer(
    db: Db,
    cache: Arc<AuthCache>,
    settings: SpendSettings,
    shutdown: CancellationToken,
) -> SpendWriter {
    let (tx, mut rx) = mpsc::channel::<SpendBatch>(settings.queue_capacity.max(1));
    let pending = Arc::new(PendingSpend::default());
    let sink = SpendSink {
        tx,
        pending: pending.clone(),
        store_logs: !settings.disable_spend_logs,
        dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };
    let task = tokio::spawn(async move {
        let mut batch = SpendBatch::default();
        let mut interval =
            tokio::time::interval(settings.flush_interval.max(Duration::from_millis(100)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut backoff = Duration::from_secs(1);
        let mut draining = false;
        loop {
            let holding_too_much = batch.logs.len() >= settings.max_held_rows;
            tokio::select! {
                biased;
                _ = shutdown.cancelled(), if !draining => {
                    draining = true;
                    rx.close();
                }
                item = rx.recv(), if !holding_too_much => match item {
                    Some(item) => {
                        batch.merge(item);
                        if batch.logs.len() < settings.batch_rows {
                            continue;
                        }
                    }
                    None => {
                        flush(&db, &cache, &pending, &mut batch, &mut backoff, true).await;
                        return;
                    }
                },
                _ = interval.tick() => {}
            }
            flush(&db, &cache, &pending, &mut batch, &mut backoff, draining).await;
            if draining && batch.is_empty() {
                return;
            }
        }
    });
    SpendWriter { sink, task }
}

async fn flush(
    db: &Db,
    cache: &AuthCache,
    pending: &PendingSpend,
    batch: &mut SpendBatch,
    backoff: &mut Duration,
    final_attempt: bool,
) {
    if batch.is_empty() {
        return;
    }
    let started = std::time::Instant::now();
    match db.apply_spend_batch(batch).await {
        Ok(result) => {
            pending.subtract(batch);
            for (hash, delta) in &batch.key_delta {
                cache.bump_spend(hash, *delta).await;
            }
            for team in batch.team_delta.keys() {
                cache.invalidate_team(team);
            }
            for user in batch.user_delta.keys() {
                cache.invalidate_user(user);
            }
            tracing::debug!(
                logs = result.logs_written,
                entities = result.entities_updated,
                daily = result.daily_rows,
                took_ms = started.elapsed().as_millis() as u64,
                "spend flushed"
            );
            metrics::histogram!(rustllmroute_types::metrics::SPEND_FLUSH_DURATION)
                .record(started.elapsed().as_secs_f64());
            *batch = SpendBatch::default();
            *backoff = Duration::from_secs(1);
        }
        Err(error) => {
            tracing::error!(error = %error, held_rows = batch.logs.len(), "spend flush failed; will retry");
            metrics::counter!(rustllmroute_types::metrics::SPEND_FLUSH_FAILURES).increment(1);
            if final_attempt {
                tracing::error!(held_rows = batch.logs.len(), "spend lost at shutdown");
                *batch = SpendBatch::default();
                return;
            }
            let jitter = Duration::from_millis(u64::from(rand::random::<u16>()) % 500);
            tokio::time::sleep(*backoff + jitter).await;
            *backoff = (*backoff * 2).min(Duration::from_secs(30));
        }
    }
}
