use rustllmroute_types::Usage;

use crate::ModelPricing;

const LONG_CONTEXT_THRESHOLD: u64 = 200_000;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    pub input: f64,
    pub cache_read: f64,
    pub cache_creation: f64,
    pub output: f64,
    pub reasoning: f64,
    pub total: f64,
    /// False when the table had no input or output rate; `total` is then a lower bound (usually 0).
    pub known: bool,
    /// True when the provider reported the cost itself (OpenRouter) and the table was not used.
    pub provider_reported: bool,
}

fn pick(above: bool, long: Option<f64>, base: Option<f64>) -> Option<f64> {
    if above { long.or(base) } else { base }
}

/// Mirrors LiteLLM's `generic_cost_per_token`: `prompt_tokens` includes cached and cache-creation
/// tokens, reasoning tokens are part of `completion_tokens`, and a prompt above 200k tokens switches the
/// whole request to the long-context rates.
pub fn cost_usd(pricing: &ModelPricing, usage: &Usage) -> Cost {
    if let Some(cost) = usage.cost.filter(|cost| cost.is_finite() && *cost >= 0.0) {
        return Cost {
            total: cost,
            known: true,
            provider_reported: true,
            ..Cost::default()
        };
    }
    let cached = usage.cached_tokens();
    let creation = usage.cache_creation_tokens();
    let text_in = usage.prompt_tokens.saturating_sub(cached + creation);
    let reasoning = usage.reasoning_tokens();
    let text_out = usage.completion_tokens.saturating_sub(reasoning);
    let above = usage.prompt_tokens > LONG_CONTEXT_THRESHOLD;
    let rate_in = pick(above, pricing.input_above_200k, pricing.input);
    let rate_out = pick(above, pricing.output_above_200k, pricing.output);
    let rate_cache_read =
        pick(above, pricing.cache_read_above_200k, pricing.cache_read).or(rate_in);
    let rate_cache_creation = pick(
        above,
        pricing.cache_creation_above_200k,
        pricing.cache_creation,
    )
    .or(rate_in);
    let rate_reasoning = pricing.output_reasoning.or(rate_out);
    let mul = |tokens: u64, rate: Option<f64>| tokens as f64 * rate.unwrap_or(0.0);
    let mut cost = Cost {
        input: mul(text_in, rate_in),
        cache_read: mul(cached, rate_cache_read),
        cache_creation: mul(creation, rate_cache_creation),
        output: mul(text_out, rate_out),
        reasoning: mul(reasoning, rate_reasoning),
        total: 0.0,
        known: rate_in.is_some() && rate_out.is_some(),
        provider_reported: false,
    };
    cost.total = cost.input + cost.cache_read + cost.cache_creation + cost.output + cost.reasoning;
    cost
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustllmroute_types::PromptTokensDetails;

    use super::*;

    fn usage(prompt: u64, completion: u64, cached: u64, creation: u64) -> Usage {
        let mut usage = Usage::new(prompt, completion);
        if cached > 0 {
            usage.prompt_tokens_details = Some(PromptTokensDetails {
                cached_tokens: Some(cached),
                ..PromptTokensDetails::default()
            });
        }
        if creation > 0 {
            usage.cache_creation_input_tokens = Some(creation);
        }
        usage
    }

    fn pricing(
        input: f64,
        output: f64,
        cache_read: Option<f64>,
        cache_creation: Option<f64>,
    ) -> ModelPricing {
        ModelPricing {
            input: Some(input),
            output: Some(output),
            cache_read,
            cache_creation,
            ..ModelPricing::default()
        }
    }

    // Rows from LiteLLM's python_reference.tsv (cost crate test fixture): expected input and output cost.
    #[rstest]
    #[case::ordinary(usage(100, 20, 0, 0), pricing(2.0, 4.0, None, None), 200.0, 80.0)]
    #[case::cache_fallback(usage(100, 20, 25, 10), pricing(2.0, 4.0, None, None), 200.0, 80.0)]
    #[case::cache_specific(
        usage(100, 20, 25, 10),
        pricing(2.0, 4.0, Some(0.5), Some(3.0)),
        172.5,
        80.0
    )]
    #[case::free_cache(
        usage(100, 20, 25, 10),
        pricing(2.0, 4.0, Some(0.0), Some(0.0)),
        130.0,
        80.0
    )]
    fn matches_litellm_reference(
        #[case] usage: Usage,
        #[case] pricing: ModelPricing,
        #[case] input: f64,
        #[case] output: f64,
    ) {
        let cost = cost_usd(&pricing, &usage);
        assert!(cost.known);
        assert_eq!(cost.input + cost.cache_read + cost.cache_creation, input);
        assert_eq!(cost.output + cost.reasoning, output);
        assert_eq!(cost.total, input + output);
    }

    #[rstest]
    fn provider_reported_cost_wins() {
        let mut usage = usage(100, 20, 0, 0);
        usage.cost = Some(0.42);
        let cost = cost_usd(&pricing(2.0, 4.0, None, None), &usage);
        assert!(cost.provider_reported);
        assert_eq!(cost.total, 0.42);
    }

    #[rstest]
    fn long_context_switches_every_rate() {
        let pricing = ModelPricing {
            input: Some(1.0),
            output: Some(2.0),
            input_above_200k: Some(10.0),
            output_above_200k: Some(20.0),
            ..ModelPricing::default()
        };
        let cost = cost_usd(&pricing, &usage(300_000, 10, 0, 0));
        assert_eq!(cost.input, 3_000_000.0);
        assert_eq!(cost.output, 200.0);
    }

    #[rstest]
    fn missing_rates_are_reported_not_guessed() {
        let cost = cost_usd(&ModelPricing::default(), &usage(10, 10, 0, 0));
        assert!(!cost.known);
        assert_eq!(cost.total, 0.0);
    }
}
