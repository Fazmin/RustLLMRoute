use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelObject {
    pub id: String,
    #[serde(default = "model_object")]
    pub object: String,
    #[serde(default)]
    pub created: u64,
    #[serde(default = "owned_by")]
    pub owned_by: String,
}

fn model_object() -> String {
    "model".to_owned()
}

fn owned_by() -> String {
    "rustllmroute".to_owned()
}

impl ModelObject {
    pub fn new(id: impl Into<String>, created: u64) -> Self {
        Self {
            id: id.into(),
            object: model_object(),
            created,
            owned_by: owned_by(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelList {
    #[serde(default = "list_object")]
    pub object: String,
    pub data: Vec<ModelObject>,
}

fn list_object() -> String {
    "list".to_owned()
}

impl ModelList {
    pub fn new(data: Vec<ModelObject>) -> Self {
        Self {
            object: list_object(),
            data,
        }
    }
}
