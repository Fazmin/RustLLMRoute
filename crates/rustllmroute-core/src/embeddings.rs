use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use rustllmroute_providers::CallContext;
use rustllmroute_types::{EmbeddingInput, EmbeddingRequest, EmbeddingResponse, GatewayError};

use crate::deployment::Deployment;
use crate::http::HttpClients;

const CONCURRENT_BATCHES: usize = 4;

pub struct EmbeddingOutcome {
    pub response: EmbeddingResponse,
    /// Upstream bytes, forwardable verbatim when the provider is OpenAI-shaped and one call served
    /// the whole request.
    pub raw: Option<Bytes>,
}

async fn one_call(
    http: &HttpClients,
    deployment: &Deployment,
    request: &EmbeddingRequest,
    provider_model: &str,
    request_id: &str,
) -> Result<(EmbeddingResponse, Bytes), GatewayError> {
    let provider = deployment
        .embed
        .as_ref()
        .ok_or_else(|| unsupported(deployment))?;
    let mut cx = CallContext::new(&deployment.provider, provider_model, false, request_id);
    let call = provider.build(request, &mut cx)?;
    let completed = http
        .execute_json(call, &cx, &|status, headers, body| {
            provider.parse_error(status, headers, body)
        })
        .await?;
    let response = provider.parse_response(&completed.body, request, &cx)?;
    Ok((response, completed.body))
}

fn unsupported(deployment: &Deployment) -> GatewayError {
    GatewayError::BadRequest {
        message: format!(
            "deployment {} ({}) does not serve embeddings",
            deployment.id, deployment.provider.provider_name
        ),
        param: Some("model".to_owned()),
        code: None,
    }
}

/// Rough token count for providers that report none: four characters per text token.
pub fn estimate_tokens(input: &EmbeddingInput) -> u64 {
    match input {
        EmbeddingInput::Text(text) => (text.len() as u64).div_ceil(4).max(1),
        EmbeddingInput::Texts(texts) => texts
            .iter()
            .map(|t| (t.len() as u64).div_ceil(4).max(1))
            .sum(),
        EmbeddingInput::Tokens(tokens) => tokens.len() as u64,
        EmbeddingInput::TokenLists(lists) => lists.iter().map(|l| l.len() as u64).sum(),
    }
}

/// One attempt against one deployment. Inputs beyond the provider's batch size are split into
/// concurrent calls and stitched back together in order.
pub async fn execute_embeddings(
    http: &HttpClients,
    deployment: &Deployment,
    request: &EmbeddingRequest,
    provider_model: &str,
    request_id: &str,
) -> Result<EmbeddingOutcome, GatewayError> {
    let provider = deployment
        .embed
        .as_ref()
        .ok_or_else(|| unsupported(deployment))?;
    let total = request.input.len();
    if total == 0 {
        return Err(GatewayError::BadRequest {
            message: "input must not be empty".to_owned(),
            param: Some("input".to_owned()),
            code: None,
        });
    }
    let probe = CallContext::new(&deployment.provider, provider_model, false, request_id);
    let max_batch = provider.max_batch(&probe).max(1);
    if total <= max_batch {
        let (response, raw) =
            one_call(http, deployment, request, provider_model, request_id).await?;
        return Ok(EmbeddingOutcome {
            response,
            raw: provider.passthrough_response().then_some(raw),
        });
    }
    let texts = request
        .input
        .texts()
        .ok_or_else(|| GatewayError::BadRequest {
            message: format!(
                "this deployment accepts at most {max_batch} token inputs per request"
            ),
            param: Some("input".to_owned()),
            code: None,
        })?;
    let batches: Vec<EmbeddingRequest> = texts
        .chunks(max_batch)
        .map(|chunk| EmbeddingRequest {
            input: EmbeddingInput::Texts(chunk.iter().map(|s| (*s).to_owned()).collect()),
            ..request.clone()
        })
        .collect();
    let results: Vec<EmbeddingResponse> = futures_util::stream::iter(batches)
        .map(|batch| async move {
            one_call(http, deployment, &batch, provider_model, request_id)
                .await
                .map(|(response, _)| response)
        })
        .buffered(CONCURRENT_BATCHES)
        .try_collect()
        .await?;
    let mut merged = EmbeddingResponse {
        object: "list".to_owned(),
        data: Vec::with_capacity(total),
        model: results
            .first()
            .map(|r| r.model.clone())
            .unwrap_or_else(|| provider_model.to_owned()),
        usage: Default::default(),
        extra: Default::default(),
    };
    for result in results {
        for mut item in result.data {
            item.index = merged.data.len() as u32;
            merged.data.push(item);
        }
        merged.usage.prompt_tokens += result.usage.prompt_tokens;
        merged.usage.total_tokens += result.usage.total_tokens;
    }
    Ok(EmbeddingOutcome {
        response: merged,
        raw: None,
    })
}
