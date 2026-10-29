pub mod chat;
pub mod embeddings;
pub mod error;
pub mod messages;
pub mod metrics;
pub mod models;
pub mod recognized;
pub mod responses;

pub use chat::*;
pub use embeddings::*;
pub use error::*;
pub use messages::*;
pub use models::*;
pub use recognized::Recognized;
pub use responses::*;

pub type Extra = serde_json::Map<String, serde_json::Value>;

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}
