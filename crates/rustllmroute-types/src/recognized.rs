use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A value that is either a variant this crate understands or an opaque JSON value that is
/// preserved untouched, so unknown wire shapes round-trip through the gateway.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Recognized<T> {
    Known(T),
    Unknown(Value),
}

impl<T> Recognized<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown(_) => None,
        }
    }

    pub fn known_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown(_) => None,
        }
    }
}

impl<T> From<T> for Recognized<T> {
    fn from(value: T) -> Self {
        Self::Known(value)
    }
}
