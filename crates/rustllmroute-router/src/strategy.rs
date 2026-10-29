use std::sync::Arc;

use rand::distr::weighted::WeightedIndex;
use rand::prelude::*;

use crate::config::RoutingStrategy;
use crate::deployment::RoutedDeployment;
use crate::execute::Router;
use crate::state::StateSnapshot;

const UNKNOWN_COST_PER_TOKEN: f64 = 5.0;

pub(crate) struct Candidate<'a, D> {
    pub index: usize,
    pub deployment: &'a Arc<D>,
}

impl<D: RoutedDeployment> Router<D> {
    /// Picks one candidate per the configured strategy. `None` means every candidate is over its
    /// per-minute limit (usage, latency and cost strategies only).
    pub(crate) fn pick<'a>(
        &self,
        candidates: &[Candidate<'a, D>],
        snapshot: &StateSnapshot,
        input_tokens: u64,
        stream: bool,
    ) -> Option<&'a Arc<D>> {
        if candidates.is_empty() {
            return None;
        }
        let mut rng = rand::rng();
        match self.cfg.strategy {
            RoutingStrategy::SimpleShuffle => {
                for metric in [
                    |c: &Candidate<'_, D>| c.deployment.meta().weight,
                    |c: &Candidate<'_, D>| c.deployment.meta().rpm,
                    |c: &Candidate<'_, D>| c.deployment.meta().tpm,
                ] {
                    let weights: Vec<u32> =
                        candidates.iter().map(|c| metric(c).unwrap_or(0)).collect();
                    if weights.iter().any(|w| *w > 0)
                        && let Ok(index) = WeightedIndex::new(&weights)
                    {
                        return Some(candidates[index.sample(&mut rng)].deployment);
                    }
                }
                candidates.choose(&mut rng).map(|c| c.deployment)
            }
            RoutingStrategy::LeastBusy => {
                let busiest = candidates
                    .iter()
                    .map(|c| self.stats_for(c.deployment.id()).in_flight())
                    .min()?;
                let least: Vec<&Candidate<'a, D>> = candidates
                    .iter()
                    .filter(|c| self.stats_for(c.deployment.id()).in_flight() == busiest)
                    .collect();
                least.choose(&mut rng).map(|c| c.deployment)
            }
            RoutingStrategy::LatencyBased => {
                let within: Vec<(&Candidate<'a, D>, f32)> = candidates
                    .iter()
                    .filter(|c| !over_limit(c, snapshot, input_tokens))
                    .map(|c| (c, self.stats_for(c.deployment.id()).mean_latency(stream)))
                    .collect();
                let lowest = within
                    .iter()
                    .map(|(_, latency)| *latency)
                    .fold(f32::INFINITY, f32::min);
                if !lowest.is_finite() {
                    return None;
                }
                let ceiling = lowest * (1.0 + self.cfg.lowest_latency_buffer as f32);
                let valid: Vec<&Candidate<'a, D>> = within
                    .iter()
                    .filter(|(_, latency)| *latency <= ceiling)
                    .map(|(c, _)| *c)
                    .collect();
                valid.choose(&mut rng).map(|c| c.deployment)
            }
            RoutingStrategy::UsageBased => {
                let within: Vec<&Candidate<'a, D>> = candidates
                    .iter()
                    .filter(|c| !over_limit(c, snapshot, input_tokens))
                    .collect();
                let lowest = within.iter().map(|c| snapshot.tpm[c.index]).min()?;
                let least: Vec<&Candidate<'a, D>> = within
                    .into_iter()
                    .filter(|c| snapshot.tpm[c.index] == lowest)
                    .collect();
                least.choose(&mut rng).map(|c| c.deployment)
            }
            RoutingStrategy::CostBased => {
                let within: Vec<(&Candidate<'a, D>, f64)> = candidates
                    .iter()
                    .filter(|c| !over_limit(c, snapshot, input_tokens))
                    .map(|c| {
                        let pricing = c.deployment.pricing();
                        let cost = pricing.input.unwrap_or(UNKNOWN_COST_PER_TOKEN)
                            + pricing.output.unwrap_or(UNKNOWN_COST_PER_TOKEN);
                        (c, cost)
                    })
                    .collect();
                let cheapest = within
                    .iter()
                    .map(|(_, cost)| *cost)
                    .fold(f64::INFINITY, f64::min);
                if !cheapest.is_finite() {
                    return None;
                }
                let ties: Vec<&Candidate<'a, D>> = within
                    .iter()
                    .filter(|(_, cost)| *cost <= cheapest)
                    .map(|(c, _)| *c)
                    .collect();
                ties.choose(&mut rng).map(|c| c.deployment)
            }
        }
    }
}

fn over_limit<D: RoutedDeployment>(
    candidate: &Candidate<'_, D>,
    snapshot: &StateSnapshot,
    input_tokens: u64,
) -> bool {
    let meta = candidate.deployment.meta();
    if let Some(tpm) = meta.tpm
        && snapshot.tpm[candidate.index] + input_tokens > u64::from(tpm)
    {
        return true;
    }
    if let Some(rpm) = meta.rpm
        && snapshot.rpm[candidate.index] + 1 > rpm
    {
        return true;
    }
    false
}
