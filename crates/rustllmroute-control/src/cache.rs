use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;

use crate::principal::Principal;

/// Resolved principals by token hash, plus a short negative cache that absorbs bad-key floods.
pub struct AuthCache {
    principals: Cache<Arc<str>, Arc<Principal>>,
    negative: Cache<Arc<str>, ()>,
}

impl AuthCache {
    pub fn new(ttl: Duration, capacity: u64, negative_ttl: Duration) -> Self {
        Self {
            principals: Cache::builder()
                .time_to_live(ttl)
                .max_capacity(capacity)
                .support_invalidation_closures()
                .build(),
            negative: Cache::builder()
                .time_to_live(negative_ttl)
                .max_capacity(capacity.saturating_mul(2).max(1_000))
                .build(),
        }
    }

    pub async fn get(&self, hash: &str) -> Option<Arc<Principal>> {
        self.principals.get(hash).await
    }

    pub async fn insert(&self, principal: Arc<Principal>) {
        self.negative.invalidate(&principal.token_hash).await;
        self.principals
            .insert(principal.token_hash.clone(), principal)
            .await;
    }

    pub async fn is_negative(&self, hash: &str) -> bool {
        self.negative.get(hash).await.is_some()
    }

    pub async fn mark_negative(&self, hash: &str) {
        self.negative.insert(Arc::from(hash), ()).await;
    }

    pub async fn invalidate_key(&self, hash: &str) {
        self.principals.invalidate(hash).await;
        self.negative.invalidate(hash).await;
    }

    pub fn invalidate_all(&self) {
        self.principals.invalidate_all();
        self.negative.invalidate_all();
    }

    /// Drops every cached principal matching the predicate, so management writes and budget
    /// resets take effect on the next request.
    pub fn invalidate_where(&self, predicate: impl Fn(&Principal) -> bool + Send + Sync + 'static) {
        let _ = self
            .principals
            .invalidate_entries_if(move |_, principal| predicate(principal));
    }

    /// Members re-read their team row (with the freshly flushed spend) on their next request.
    pub fn invalidate_team(&self, team_id: &str) {
        let team_id = team_id.to_owned();
        self.invalidate_where(move |p| p.key.team_id.as_deref() == Some(team_id.as_str()));
    }

    pub fn invalidate_user(&self, user_id: &str) {
        let user_id = user_id.to_owned();
        self.invalidate_where(move |p| p.key.user_id.as_deref() == Some(user_id.as_str()));
    }

    /// Keeps cached key spend in step with what the writer just persisted, so the cached row and
    /// the pending map never double count.
    pub async fn bump_spend(&self, hash: &str, delta: f64) {
        if let Some(current) = self.principals.get(hash).await {
            let mut updated = (*current).clone();
            updated.key.spend += delta;
            self.principals
                .insert(updated.token_hash.clone(), Arc::new(updated))
                .await;
        }
    }

    pub fn entry_count(&self) -> u64 {
        self.principals.entry_count()
    }
}
