pub mod auth;
pub mod chat;
pub mod deployment;
pub mod embeddings;
pub mod gateway;
pub mod guard;
pub mod health;
pub mod hooks;
pub mod http;
pub mod messages;
pub mod observe;
pub mod responses;
pub mod stream;

pub use chat::{ChatOutcome, execute_chat};
pub use deployment::{Deployment, RouterMeta};
pub use embeddings::execute_embeddings;
pub use gateway::{ChatReply, Gateway, GatewaySettings, JsonReply, MessagesReply, ResponsesReply};
pub use hooks::*;
pub use http::{ChatStream, HttpClients, RawStream};
pub use rustllmroute_guardrails as guardrails;
pub use rustllmroute_router as router;

pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub fn new_request_id() -> String {
    uuid::Uuid::now_v7().to_string()
}
