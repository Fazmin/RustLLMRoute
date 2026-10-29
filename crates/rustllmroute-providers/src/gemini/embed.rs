//! Embeddings: AI Studio `batchEmbedContents`, Vertex `predict`.

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use rustllmroute_types::{
    EmbeddingData, EmbeddingRequest, EmbeddingResponse, EmbeddingUsage, EmbeddingVector, Extra,
    GatewayError,
};
use serde_json::{Value, json};
use url::Url;

use super::{Flavor, GeminiProvider};
use crate::errors::upstream_error;
use crate::traits::{CallContext, EmbeddingProvider, ProviderCall, StreamWire};

const AI_STUDIO_BATCH: usize = 100;
const VERTEX_BATCH: usize = 250;

fn texts(req: &EmbeddingRequest) -> Result<Vec<&str>, GatewayError> {
    req.input.texts().ok_or_else(|| GatewayError::BadRequest {
        message: "this provider embeds text only; token arrays are not supported".to_owned(),
        param: Some("input".to_owned()),
        code: None,
    })
}

fn task_type(req: &EmbeddingRequest) -> Option<Value> {
    req.extra.get("task_type").cloned().or_else(|| {
        match req.extra.get("input_type").and_then(Value::as_str)? {
            "query" | "search_query" => Some(json!("RETRIEVAL_QUERY")),
            "document" | "search_document" => Some(json!("RETRIEVAL_DOCUMENT")),
            "classification" => Some(json!("CLASSIFICATION")),
            "clustering" => Some(json!("CLUSTERING")),
            _ => None,
        }
    })
}

fn vectors(values: Vec<Vec<f32>>) -> Vec<EmbeddingData> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| EmbeddingData {
            object: "embedding".to_owned(),
            index: index as u32,
            embedding: EmbeddingVector::Floats(embedding),
            extra: Extra::new(),
        })
        .collect()
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

impl EmbeddingProvider for GeminiProvider {
    fn build(
        &self,
        req: &EmbeddingRequest,
        cx: &mut CallContext<'_>,
    ) -> Result<ProviderCall, GatewayError> {
        let inputs = texts(req)?;
        let model = GeminiProvider::model_path(cx.model);
        let base = cx.config.base_url.as_str().trim_end_matches('/');
        let task = task_type(req);
        let (url, body) = match self.flavor {
            Flavor::AiStudio => {
                let requests: Vec<Value> = inputs
                    .iter()
                    .map(|text| {
                        let mut request = json!({"model": format!("models/{model}"), "content": {"parts": [{"text": text}]}});
                        if let Some(dimensions) = req.dimensions {
                            request["outputDimensionality"] = json!(dimensions);
                        }
                        if let Some(task) = &task {
                            request["taskType"] = task.clone();
                        }
                        request
                    })
                    .collect();
                (
                    format!("{base}/v1beta/models/{model}:batchEmbedContents"),
                    json!({"requests": requests}),
                )
            }
            Flavor::Vertex => {
                let settings = cx.config.vertex.as_ref().ok_or_else(|| {
                    GatewayError::internal("vertex_ai deployment is missing project/location")
                })?;
                let instances: Vec<Value> = inputs
                    .iter()
                    .map(|text| {
                        let mut instance = json!({"content": text});
                        if let Some(task) = &task {
                            instance["task_type"] = task.clone();
                        }
                        instance
                    })
                    .collect();
                let mut body = json!({"instances": instances});
                if let Some(dimensions) = req.dimensions {
                    body["parameters"] = json!({"outputDimensionality": dimensions});
                }
                (
                    format!(
                        "{base}/v1/projects/{}/locations/{}/publishers/google/models/{model}:predict",
                        settings.project, settings.location
                    ),
                    body,
                )
            }
        };
        let mut body = body;
        if let Some(object) = body.as_object_mut() {
            for (key, value) in &cx.config.extra_body {
                object.insert(key.clone(), value.clone());
            }
        }
        let (headers, auth) = GeminiProvider::headers_and_auth(self.flavor, cx)?;
        Ok(ProviderCall {
            method: Method::POST,
            url: Url::parse(&url)
                .map_err(|error| GatewayError::internal(format!("gemini url: {error}")))?,
            headers,
            body: Bytes::from(body.to_string()),
            auth,
            wire: StreamWire::GeminiSse,
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
            GatewayError::InvalidUpstreamResponse(format!("gemini: embedding response: {error}"))
        })?;
        let mut tokens = 0u64;
        let values: Vec<Vec<f32>> = match self.flavor {
            Flavor::AiStudio => value
                .get("embeddings")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| floats(item.get("values").unwrap_or(&Value::Null)))
                        .collect()
                })
                .unwrap_or_default(),
            Flavor::Vertex => value
                .get("predictions")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            tokens += item
                                .pointer("/embeddings/statistics/token_count")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            floats(item.pointer("/embeddings/values").unwrap_or(&Value::Null))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        if values.is_empty() {
            return Err(GatewayError::InvalidUpstreamResponse(
                "gemini: embedding response carries no vectors".to_owned(),
            ));
        }
        Ok(EmbeddingResponse {
            object: "list".to_owned(),
            data: vectors(values),
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
        upstream_error(
            match self.flavor {
                Flavor::AiStudio => super::PROVIDER,
                Flavor::Vertex => crate::vertex::PROVIDER,
            },
            status,
            headers,
            body,
        )
    }

    fn max_batch(&self, _cx: &CallContext<'_>) -> usize {
        match self.flavor {
            Flavor::AiStudio => AI_STUDIO_BATCH,
            Flavor::Vertex => VERTEX_BATCH,
        }
    }
}

#[cfg(test)]
mod tests {
    use rustllmroute_types::EmbeddingInput;
    use secrecy::SecretString;

    use super::*;
    use crate::registry::ProviderKind;
    use crate::traits::{ProviderConfig, VertexSettings};

    #[test]
    fn ai_studio_batches_and_parses_values() {
        let mut config = ProviderConfig::new(
            ProviderKind::Gemini,
            "gemini-embedding-001",
            Url::parse("https://generativelanguage.googleapis.com").unwrap(),
        );
        config.api_key = Some(SecretString::from("k"));
        let mut cx = CallContext::new(&config, "gemini-embedding-001", false, "r");
        let request = EmbeddingRequest {
            model: "x".into(),
            input: EmbeddingInput::Texts(vec!["a".into(), "b".into()]),
            dimensions: Some(256),
            encoding_format: None,
            user: None,
            extra: Extra::new(),
        };
        let call = GeminiProvider::ai_studio()
            .build(&request, &mut cx)
            .unwrap();
        assert!(
            call.url
                .as_str()
                .ends_with("/v1beta/models/gemini-embedding-001:batchEmbedContents")
        );
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert_eq!(body["requests"].as_array().unwrap().len(), 2);
        assert_eq!(body["requests"][0]["outputDimensionality"], 256);
        let response = EmbeddingProvider::parse_response(
            &GeminiProvider::ai_studio(),
            br#"{"embeddings":[{"values":[0.1,0.2]},{"values":[0.3]}]}"#,
            &request,
            &cx,
        )
        .unwrap();
        assert_eq!(response.data.len(), 2);
        assert_eq!(response.data[1].index, 1);
    }

    #[test]
    fn vertex_predict_counts_tokens() {
        let mut config = ProviderConfig::new(
            ProviderKind::Vertex,
            "text-embedding-005",
            Url::parse("https://us-central1-aiplatform.googleapis.com").unwrap(),
        );
        config.vertex = Some(VertexSettings {
            project: "p".into(),
            location: "us-central1".into(),
        });
        config.api_key = Some(SecretString::from("ya29"));
        let mut cx = CallContext::new(&config, "text-embedding-005", false, "r");
        let request = EmbeddingRequest {
            model: "x".into(),
            input: EmbeddingInput::Text("hello".into()),
            dimensions: None,
            encoding_format: None,
            user: None,
            extra: serde_json::from_str(r#"{"input_type":"query"}"#).unwrap(),
        };
        let call = EmbeddingProvider::build(&GeminiProvider::vertex(), &request, &mut cx).unwrap();
        assert!(
            call.url
                .as_str()
                .ends_with("/publishers/google/models/text-embedding-005:predict")
        );
        let body: Value = serde_json::from_slice(&call.body).unwrap();
        assert_eq!(body["instances"][0]["task_type"], "RETRIEVAL_QUERY");
        let response = EmbeddingProvider::parse_response(
            &GeminiProvider::vertex(),
            br#"{"predictions":[{"embeddings":{"values":[1.0],"statistics":{"token_count":7}}}]}"#,
            &request,
            &cx,
        )
        .unwrap();
        assert_eq!(response.usage.prompt_tokens, 7);
    }
}
