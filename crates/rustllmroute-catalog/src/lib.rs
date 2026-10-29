mod catalog;
mod cost;
mod error;
mod pricing;
mod resolve;

pub use catalog::{Catalog, ModelEntry, ModelMatch};
pub use cost::{Cost, cost_usd};
pub use error::Error;
pub use pricing::{Caps, Mode, ModelPricing};

/// The pricing table shipped inside the binary (LiteLLM's `model_prices_and_context_window.json`, MIT).
pub fn bundled_bytes() -> &'static [u8] {
    include_bytes!("../../../model_prices_and_context_window.json")
}

pub fn bundled() -> Result<Catalog, Error> {
    Catalog::parse(bundled_bytes())
}
