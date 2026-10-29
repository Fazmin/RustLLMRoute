//! Bedrock embeddings: Amazon Titan (one text per call) and Cohere (batched).

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use rustllmroute_types::{
    EmbeddingData, EmbeddingRequest, EmbeddingResponse, EmbeddingUsage, EmbeddingVector, Extra,
    GatewayError,
};
use serde_json::{Value, json};

use super::{BedrockProvider, encode_model_id, errors, model_id};
use crate::traits::{CallContext, EmbeddingProvider, ProviderCall, StreamWire};

const COHERE_BATCH: usize = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Titan,
    Cohere,
}

fn family(model: &str) -> Result<Family, GatewayError> {
    let id = model_id(model);
    if id.contains("titan-embed") || id.contains("titan-e") {
        Ok(Family::Titan)
    } else if id.contains("cohere.embed") {
        Ok(Family::Cohere)
    } else {
        Err(GatewayError::BadRequest {
            message: format!(
                "bedrock model {id:?} is not a supported embedding model (Titan or Cohere)"
            ),
            param: Some("model".to_owned()),
            code: None,
        })
    }
}

fn texts(req: &EmbeddingRequest) -> Result<Vec<&str>, GatewayError> {
    req.input.texts().ok_or_else(|| GatewayError::BadRequest {
        message: "bedrock embeds text only; token arrays are not supported".to_owned(),
        param: Some("input".to_owned()),
        code: None,
    })
}

fn floats(value: &Value) -> Vec<f32> {
    value
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_f64)
                .map(|v| v as f32)
                .collect()
        })
        .unwrap_or_default()
}

impl EmbeddingProvider for BedrockProvider {
    fn build(
        &self,
        req: &EmbeddingRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let inputs = texts(req)?;
        let body = match family(cx.model)? {
            Family::Titan => {
                let text = inputs.first().copied().unwrap_or_default();
                let mut body = json!({"inputText": text});
                if let Some(dimensions) = req.dimensions {
                    body["dimensions"] = json!(dimensions);
                }
                if let Some(normalize) = req.extra.get("normalize") {
                    body["normalize"] = normalize.clone();
                }
                body
            }
            Family::Cohere => {
                let input_type = req
                    .extra
                    .get("input_type")
                    .and_then(Value::as_str)
                    .map(|kind| match kind {
                        "query" => "search_query",
                        "document" => "search_document",
                        other => other,
                    })
                    .unwrap_or("search_document");
                let mut body =
                    json!({"texts": inputs, "input_type": input_type, "truncate": "END"});
                if let Some(types) = req.extra.get("embedding_types") {
                    body["embedding_types"] = types.clone();
                }
                body
            }
        };
        let mut body = body;
        if let Some(object) = body.as_object_mut() {
            for (key, value) in &cx.config.extra_body {
                object.insert(key.clone(), value.clone());
            }
        }
        let base = cx.config.base_url.as_str().trim_end_matches('/');
        let url = url::Url::parse(&format!(
            "{base}/model/{}/invoke",
            encode_model_id(model_id(cx.model))
        ))
        .map_err(|error| GatewayError::internal(format!("bedrock url: {error}")))?;
        Ok(ProviderCall {
            method: Method::POST,
            url,
            headers: BedrockProvider::headers(cx, false),
            body: Bytes::from(body.to_string()),
            auth: BedrockProvider::auth(cx)?,
            wire: StreamWire::AwsEventStream,
            stream: false,
        })
    }

    fn parse_response(
        &self,
        body: &[u8],
        _req: &EmbeddingRequest,
        cx: &CallContext<'_>,
    ) -> Result<EmbeddingResponse, GatewayError> {
        let value: Value = serde_json::from_slice(body).map_err(|error| {
            GatewayError::InvalidUpstreamResponse(format!("bedrock: embedding response: {error}"))
        })?;
        let (vectors, tokens): (Vec<Vec<f32>>, u64) = match family(cx.model)? {
            Family::Titan => (
                vec![floats(value.get("embedding").unwrap_or(&Value::Null))],
                value
                    .get("inputTextTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
            Family::Cohere => {
                let list = match value.get("embeddings") {
                    Some(Value::Array(list)) => list.clone(),
                    Some(Value::Object(by_type)) => by_type
                        .get("float")
                        .or_else(|| by_type.values().next())
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                (list.iter().map(floats).collect(), 0)
            }
        };
        if vectors.is_empty() || vectors.iter().any(Vec::is_empty) {
            return Err(GatewayError::InvalidUpstreamResponse(
                "bedrock: embedding response carries no vectors".to_owned(),
            ));
        }
        Ok(EmbeddingResponse {
            object: "list".to_owned(),
            data: vectors
                .into_iter()
                .enumerate()
                .map(|(index, embedding)| EmbeddingData {
                    object: "embedding".to_owned(),
                    index: index as u32,
                    embedding: EmbeddingVector::Floats(embedding),
                    extra: Extra::new(),
                })
                .collect(),
            model: cx.model.to_owned(),
            usage: EmbeddingUsage {
                prompt_tokens: tokens,
                total_tokens: tokens,
                extra: Extra::new(),
            },
            extra: Extra::new(),
        })
    }

    fn parse_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> GatewayError {
        errors::parse_error(status, headers, body)
    }

    fn max_batch(&self, cx: &CallContext<'_>) -> usize {
        match family(cx.model) {
            Ok(Family::Titan) => 1,
            Ok(Family::Cohere) => COHERE_BATCH,
            Err(_) => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustllmroute_types::EmbeddingInput;
    use secrecy::SecretString;
    use url::Url;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::{
        AwsCredentialSource, AwsCredentials, AwsSettings, CredentialsFuture, ProviderConfig,
    };

    struct StaticCreds;

    impl AwsCredentialSource for StaticCreds {
        fn credentials(&self) -> CredentialsFuture<'_> {
            Box::pin(async {
                Ok(AwsCredentials {
                    access_key_id: "AKIA".into(),
                    secret_access_key: SecretString::from("s"),
                    session_token: None,
                    expires_at: None,
                })
            })
        }
    }

    fn config(model: &str) -> ProviderConfig {
        let mut config = ProviderConfig::new(
            ProviderKind::Bedrock,
            model,
            Url::parse("https://bedrock-runtime.us-east-1.amazonaws.com").unwrap(),
        );
        config.aws = Some(AwsSettings {
            region: Arc::from("us-east-1"),
        });
        config.aws_credentials = Some(Arc::new(StaticCreds));
        config
    }

    #[test]
    fn titan_is_one_text_per_call() {
        let config = config("amazon.titan-embed-text-v2:0");
        let mut cx = CallContext::new(&config, "amazon.titan-embed-text-v2:0", false, "r");
        let request = EmbeddingRequest {
            model: "x".into(),
            input: EmbeddingInput::Text("hi".into()),
            dimensions: Some(512),
            encoding_format: None,
            user: None,
            extra: Extra::new(),
        };
        assert_eq!(BedrockProvider.max_batch(&cx), 1);
        let call = EmbeddingProvider::build(&BedrockProvider, &request, &mut cx).unwrap();
        assert!(
            call.url
                .as_str()
                .ends_with("/model/amazon.titan-embed-text-v2%3A0/invoke")
        );
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert_eq!(body["inputText"], "hi");
        assert_eq!(body["dimensions"], 512);
        let response = EmbeddingProvider::parse_response(
            &BedrockProvider,
            br#"{"embedding":[0.5,0.25],"inputTextTokenCount":3}"#,
            &request,
            &cx,
        )
        .unwrap();
        assert_eq!(response.usage.prompt_tokens, 3);
        assert_eq!(
            response.data[0].embedding,
            EmbeddingVector::Floats(vec![0.5, 0.25])
        );
    }

    #[test]
    fn cohere_batches_texts() {
        let config = config("cohere.embed-english-v3");
        let mut cx = CallContext::new(&config, "cohere.embed-english-v3", false, "r");
        let request = EmbeddingRequest {
            model: "x".into(),
            input: EmbeddingInput::Texts(vec!["a".into(), "b".into()]),
            dimensions: None,
            encoding_format: None,
            user: None,
            extra: serde_json::from_str(r#"{"input_type":"query"}"#).unwrap(),
        };
        assert_eq!(BedrockProvider.max_batch(&cx), COHERE_BATCH);
        let call = EmbeddingProvider::build(&BedrockProvider, &request, &mut cx).unwrap();
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert_eq!(body["texts"].as_array().unwrap().len(), 2);
        assert_eq!(body["input_type"], "search_query");
        let response = EmbeddingProvider::parse_response(
            &BedrockProvider,
            br#"{"embeddings":{"float":[[1.0],[2.0]]}}"#,
            &request,
            &cx,
        )
        .unwrap();
        assert_eq!(response.data.len(), 2);
        assert!(family("anthropic.claude-3").is_err());
    }
}
