#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid catalog JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("catalog is empty")]
    Empty,
    #[error("catalog has {actual} models, below {minimum} required")]
    TooSmall { actual: usize, minimum: usize },
}
