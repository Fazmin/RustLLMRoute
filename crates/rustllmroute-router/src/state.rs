use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use rustllmroute_types::ErrorKind;
use tokio::time::Instant;

use crate::stats::DeploymentStats;

#[derive(Clone, Debug)]
pub struct CooldownEntry {
    pub until: Instant,
    pub status: Option<u16>,
    pub kind: ErrorKind,
    pub reason: String,
}

impl CooldownEntry {
    pub fn remaining(&self) -> Option<Duration> {
        let now = Instant::now();
        (self.until > now).then(|| self.until - now)
    }
}

/// One batch read of the shared state for a group's members, index-aligned with the input ids.
#[derive(Clone, Debug, Default)]
pub struct StateSnapshot {
    pub cooled: Vec<Option<Duration>>,
    pub rpm: Vec<u32>,
    pub tpm: Vec<u64>,
    pub fails: Vec<u32>,
    pub successes: Vec<u32>,
}

/// State worth sharing between gateway instances (cooldowns and per-minute counters).
/// In-flight counts, latency samples and health stay local. A Redis implementation can map
/// `snapshot` to one pipeline and the writes to fire-and-forget commands.
#[async_trait::async_trait]
pub trait RouterStateStore: Send + Sync + 'static {
    async fn snapshot(&self, ids: &[&Arc<str>], minute: u64) -> StateSnapshot;
    fn set_cooldown(&self, id: &Arc<str>, entry: CooldownEntry);
    fn record_request(&self, id: &Arc<str>, minute: u64);
    fn record_result(&self, id: &Arc<str>, minute: u64, ok: bool, tokens: u64);
    fn active_cooldowns(&self) -> Vec<(Arc<str>, CooldownEntry)>;
    fn clear_cooldown(&self, id: &str);
}

pub type StatsMap = Arc<DashMap<Arc<str>, Arc<DeploymentStats>>>;

pub struct MemoryStateStore {
    cooldowns: DashMap<Arc<str>, CooldownEntry>,
    stats: StatsMap,
}

impl MemoryStateStore {
    pub fn new(stats: StatsMap) -> Self {
        Self {
            cooldowns: DashMap::new(),
            stats,
        }
    }

    fn stats_for(&self, id: &Arc<str>) -> Arc<DeploymentStats> {
        self.stats
            .entry(id.clone())
            .or_insert_with(|| Arc::new(DeploymentStats::new(10)))
            .clone()
    }
}

#[async_trait::async_trait]
impl RouterStateStore for MemoryStateStore {
    async fn snapshot(&self, ids: &[&Arc<str>], minute: u64) -> StateSnapshot {
        let mut snapshot = StateSnapshot::default();
        for id in ids {
            let cooled = self.cooldowns.get(*id).and_then(|entry| entry.remaining());
            if cooled.is_none() {
                self.cooldowns
                    .remove_if(*id, |_, entry| entry.remaining().is_none());
            }
            snapshot.cooled.push(cooled);
            let window = self.stats_for(id).minute(minute);
            snapshot.rpm.push(window.rpm);
            snapshot.tpm.push(window.tpm);
            snapshot.fails.push(window.fails);
            snapshot.successes.push(window.successes);
        }
        snapshot
    }

    fn set_cooldown(&self, id: &Arc<str>, entry: CooldownEntry) {
        self.cooldowns.insert(id.clone(), entry);
    }

    fn record_request(&self, id: &Arc<str>, minute: u64) {
        self.stats_for(id).record_request(minute);
    }

    fn record_result(&self, id: &Arc<str>, minute: u64, ok: bool, tokens: u64) {
        self.stats_for(id).record_result(minute, ok, tokens);
    }

    fn active_cooldowns(&self) -> Vec<(Arc<str>, CooldownEntry)> {
        self.cooldowns
            .iter()
            .filter(|entry| entry.remaining().is_some())
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    fn clear_cooldown(&self, id: &str) {
        self.cooldowns.remove(id);
    }
}
