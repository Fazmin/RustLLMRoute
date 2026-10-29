use bytes::Bytes;
use rustllmroute_providers::CallContext;
use rustllmroute_types::{ChatRequest, ChatResponse, GatewayError};

use crate::deployment::Deployment;
use crate::http::{ChatStream, HttpClients, Upstream};

pub enum ChatOutcome {
    Complete {
        response: Box<ChatResponse>,
        /// The upstream bytes, forwardable verbatim when the provider is OpenAI-shaped.
        raw: Bytes,
        passthrough: bool,
    },
    Stream(ChatStream),
}

/// One attempt against one deployment. Returns `Stream` only after the upstream committed with
/// 2xx headers and a decodable first event, so callers may retry anything that fails here.
pub async fn execute_chat(
    http: &HttpClients,
    deployment: &Deployment,
    request: &ChatRequest,
    provider_model: &str,
    request_id: &str,
) -> Result<ChatOutcome, GatewayError> {
    let mut cx = CallContext::new(
        &deployment.provider,
        provider_model,
        request.stream,
        request_id,
    );
    let call = deployment.chat.build(request, &mut cx)?;
    let wanted_stream = call.stream;
    match http
        .execute_chat(deployment.chat.as_ref(), call, &cx)
        .await?
    {
        Upstream::Completed(completed) => {
            let response = deployment.chat.parse_response(&completed.body, &cx)?;
            if wanted_stream {
                tracing::debug!(
                    provider = deployment.provider.provider_name,
                    "upstream answered a streaming request with a complete body; synthesising stream"
                );
            }
            Ok(ChatOutcome::Complete {
                response: Box::new(response),
                raw: completed.body,
                passthrough: deployment.chat.passthrough_response(),
            })
        }
        Upstream::Stream(stream) => Ok(ChatOutcome::Stream(stream)),
    }
}
