use serde::{Deserialize, Serialize};

use crate::Extra;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: EmbeddingInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Text(String),
    Texts(Vec<String>),
    Tokens(Vec<u64>),
    TokenLists(Vec<Vec<u64>>),
}

impl EmbeddingInput {
    /// Number of embeddings the request asks for.
    pub fn len(&self) -> usize {
        match self {
            Self::Text(_) | Self::Tokens(_) => 1,
            Self::Texts(items) => items.len(),
            Self::TokenLists(items) => items.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn texts(&self) -> Option<Vec<&str>> {
        match self {
            Self::Text(text) => Some(vec![text.as_str()]),
            Self::Texts(items) => Some(items.iter().map(String::as_str).collect()),
            Self::Tokens(_) | Self::TokenLists(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    #[serde(default = "list_object")]
    pub object: String,
    pub data: Vec<EmbeddingData>,
    pub model: String,
    #[serde(default)]
    pub usage: EmbeddingUsage,
    #[serde(flatten)]
    pub extra: Extra,
}

fn list_object() -> String {
    "list".to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingData {
    #[serde(default = "embedding_object")]
    pub object: String,
    pub index: u32,
    pub embedding: EmbeddingVector,
    #[serde(flatten)]
    pub extra: Extra,
}

fn embedding_object() -> String {
    "embedding".to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingVector {
    Floats(Vec<f32>),
    Base64(String),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(flatten)]
    pub extra: Extra,
}
