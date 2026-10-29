use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;
use rustllmroute_core::{AuthContext, CallRecord, CallTicket, Hooks, PreCallRequest};
use rustllmroute_db::Db;
use rustllmroute_types::GatewayError;
use secrecy::{ExposeSecret, SecretString};
use tokio_util::sync::CancellationToken;

use crate::cache::AuthCache;
use crate::checks;
use crate::credentials::{constant_time_eq, extract_credentials, hash_token, is_virtual_key};
use crate::principal::Principal;
use crate::ratelimit::{
    Descriptor, LimitStatus, MemoryRateLimiter, RateLimitStore, Reservation, Scope,
};
use crate::spend::{SpendSettings, SpendSink, SpendWriter, spawn_writer};

#[derive(Clone, Debug)]
pub struct ControlSettings {
    pub cache_ttl: Duration,
    pub cache_capacity: u64,
    pub negative_ttl: Duration,
    pub spend: SpendSettings,
    pub retention: Option<(Duration, Duration)>,
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            cache_ttl: Duration::from_secs(60),
            cache_capacity: 20_000,
            negative_ttl: Duration::from_secs(10),
            spend: SpendSettings::default(),
            retention: None,
        }
    }
}

/// Everything the data plane needs from the control plane, behind the `Hooks` seam.
pub struct ControlPlane {
    db: Db,
    master_key: SecretString,
    cache: Arc<AuthCache>,
    limiter: Arc<dyn RateLimitStore>,
    sink: SpendSink,
    writer: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown: CancellationToken,
    background: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

pub struct ControlTicket {
    reservation: Option<Reservation>,
    status: Option<LimitStatus>,
}

impl CallTicket for ControlTicket {
    fn response_headers(&self, out: &mut HeaderMap) {
        if let Some(status) = &self.status {
            status.headers(out);
        }
    }
}

impl ControlPlane {
    pub fn start(db: Db, master_key: SecretString, settings: ControlSettings) -> Arc<Self> {
        let cache = Arc::new(AuthCache::new(
            settings.cache_ttl,
            settings.cache_capacity,
            settings.negative_ttl,
        ));
        let shutdown = CancellationToken::new();
        let SpendWriter { sink, task } = spawn_writer(
            db.clone(),
            cache.clone(),
            settings.spend.clone(),
            shutdown.clone(),
        );
        let mut background = Vec::new();
        background.push(tokio::spawn(crate::budget::budget_reset_loop(
            db.clone(),
            cache.clone(),
            shutdown.clone(),
        )));
        if let Some((keep, interval)) = settings.retention {
            background.push(tokio::spawn(crate::budget::retention_loop(
                db.clone(),
                keep,
                interval,
                shutdown.clone(),
            )));
        }
        Arc::new(Self {
            db,
            master_key,
            cache,
            limiter: Arc::new(MemoryRateLimiter::new()),
            sink,
            writer: parking_lot::Mutex::new(Some(task)),
            shutdown,
            background: parking_lot::Mutex::new(background),
        })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn cache(&self) -> &Arc<AuthCache> {
        &self.cache
    }

    pub fn sink(&self) -> &SpendSink {
        &self.sink
    }

    pub fn is_master_key(&self, raw: &str) -> bool {
        constant_time_eq(raw, self.master_key.expose_secret())
    }

    /// Flushes pending spend and stops background jobs. Call once, at process shutdown.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let tasks: Vec<_> = self.background.lock().drain(..).collect();
        for task in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
        let writer = self.writer.lock().take();
        if let Some(writer) = writer {
            match tokio::time::timeout(Duration::from_secs(10), writer).await {
                Ok(_) => tracing::info!("spend writer drained"),
                Err(_) => tracing::error!("spend writer did not drain within 10s"),
            }
        }
        if let Err(error) = self.db.checkpoint().await {
            tracing::warn!(error = %error, "final checkpoint failed");
        }
    }

    /// Cache, then database (key + team + user), then cache again. Unknown hashes are remembered
    /// briefly so a flood of bad keys does not hammer SQLite.
    pub async fn resolve_key(&self, raw: &str) -> Result<Arc<Principal>, GatewayError> {
        let hash = hash_token(raw);
        if let Some(principal) = self.cache.get(&hash).await {
            return Ok(principal);
        }
        if self.cache.is_negative(&hash).await {
            return Err(GatewayError::Unauthorized("invalid API key".to_owned()));
        }
        let row = match self.db.get_key(&hash).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                self.cache.mark_negative(&hash).await;
                return Err(GatewayError::Unauthorized("invalid API key".to_owned()));
            }
            Err(error) => {
                tracing::error!(error = %error, "key lookup failed");
                return Err(GatewayError::Internal("database unavailable".to_owned()));
            }
        };
        let team = match &row.team_id {
            Some(team_id) => self.db.get_team(team_id).await.map_err(db_unavailable)?,
            None => None,
        };
        let user = match &row.user_id {
            Some(user_id) => self.db.get_user(user_id).await.map_err(db_unavailable)?,
            None => None,
        };
        let principal = Arc::new(Principal::from_rows(row, team, user));
        self.cache.insert(principal.clone()).await;
        Ok(principal)
    }

    fn descriptors(principal: &Principal) -> Vec<Descriptor> {
        let mut out = Vec::with_capacity(3);
        let key = principal.key_limits();
        out.push(Descriptor {
            scope: Scope::Key,
            id: principal.token_hash.clone(),
            rpm: key.rpm,
            tpm: key.tpm,
            max_parallel: key.max_parallel,
        });
        if let (Some(team), Some(limits)) = (&principal.team, principal.team_limits()) {
            out.push(Descriptor {
                scope: Scope::Team,
                id: Arc::from(team.team_id.as_str()),
                rpm: limits.rpm,
                tpm: limits.tpm,
                max_parallel: limits.max_parallel,
            });
        }
        if let (Some(user), Some(limits)) = (&principal.user, principal.user_limits()) {
            out.push(Descriptor {
                scope: Scope::User,
                id: Arc::from(user.user_id.as_str()),
                rpm: limits.rpm,
                tpm: limits.tpm,
                max_parallel: limits.max_parallel,
            });
        }
        out
    }
}

fn db_unavailable(error: rustllmroute_db::DbError) -> GatewayError {
    tracing::error!(error = %error, "database lookup failed");
    GatewayError::Internal("database unavailable".to_owned())
}

#[async_trait::async_trait]
impl Hooks for ControlPlane {
    async fn authenticate(
        &self,
        headers: &HeaderMap,
        _path: &str,
    ) -> Result<AuthContext, GatewayError> {
        let Some(credential) = extract_credentials(headers) else {
            return Err(GatewayError::Unauthorized(
                "missing API key: send `Authorization: Bearer <key>` (or x-api-key / api-key)"
                    .to_owned(),
            ));
        };
        if self.is_master_key(&credential) {
            return Ok(AuthContext {
                is_admin: true,
                key_alias: Some("master-key".to_owned()),
                ..AuthContext::default()
            });
        }
        if !is_virtual_key(&credential) {
            return Err(GatewayError::Unauthorized("invalid API key".to_owned()));
        }
        let principal = self.resolve_key(&credential).await?;
        Ok(principal.auth_context())
    }

    async fn pre_call(
        &self,
        auth: &AuthContext,
        req: &PreCallRequest<'_>,
    ) -> Result<Box<dyn CallTicket>, GatewayError> {
        let Some(hash) = &auth.key_hash else {
            return Ok(Box::new(ControlTicket {
                reservation: None,
                status: None,
            }));
        };
        let Some(principal) = self.cache.get(hash).await else {
            return Err(GatewayError::Unauthorized(
                "API key is no longer valid".to_owned(),
            ));
        };
        checks::pre_call(&principal, req, self.sink.pending())?;
        budget_gauges(&principal);
        let descriptors = Self::descriptors(&principal);
        if !descriptors.iter().any(Descriptor::has_limits) {
            return Ok(Box::new(ControlTicket {
                reservation: None,
                status: None,
            }));
        }
        let estimated = req.estimated_input_tokens.unwrap_or(0)
            + req.max_output_tokens.unwrap_or_else(|| {
                principal
                    .key_limits()
                    .tpm
                    .map_or(4096, |tpm| (tpm / 10).clamp(1, 4096))
            });
        let reservation = self.limiter.try_acquire(&descriptors, estimated).await?;
        Ok(Box::new(ControlTicket {
            status: Some(reservation.status),
            reservation: Some(reservation),
        }))
    }

    fn on_complete(&self, ticket: Box<dyn CallTicket>, auth: &AuthContext, record: CallRecord) {
        let any: Box<dyn std::any::Any + Send + Sync> = ticket;
        if let Ok(ticket) = any.downcast::<ControlTicket>()
            && let Some(reservation) = ticket.reservation
        {
            let actual =
                u32::try_from(record.usage.prompt_tokens + record.usage.completion_tokens).ok();
            self.limiter.settle(reservation, actual);
        }
        self.sink.record(&record, auth);
    }
}

/// Remaining-budget gauges for dashboards, refreshed on every authenticated call.
fn budget_gauges(principal: &crate::principal::Principal) {
    use rustllmroute_types::metrics as m;
    let key = principal.token_hash.to_string();
    let alias = principal.key.key_alias.clone().unwrap_or_default();
    if let Some(max) = principal.key.max_budget {
        metrics::gauge!(m::API_KEY_MAX_BUDGET, "hashed_api_key" => key.clone(), "api_key_alias" => alias.clone()).set(max);
        metrics::gauge!(m::REMAINING_API_KEY_BUDGET, "hashed_api_key" => key, "api_key_alias" => alias)
            .set((max - principal.key.spend).max(0.0));
    }
    if let Some(team) = &principal.team
        && let Some(max) = team.max_budget
    {
        metrics::gauge!(
            m::REMAINING_TEAM_BUDGET,
            "team" => team.team_id.clone(),
            "team_alias" => team.team_alias.clone().unwrap_or_default()
        )
        .set((max - team.spend).max(0.0));
    }
}
