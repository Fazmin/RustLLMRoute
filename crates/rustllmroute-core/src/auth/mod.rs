//! Credential sources for OAuth (Google Cloud) and SigV4 (AWS) providers. Token refreshes are rare
//! and off the request hot path, so they share one small HTTP client.

pub mod aws;
pub mod gcp;

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

pub use aws::AwsAuth;
pub use gcp::GcpAuth;

pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("rustllmroute/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    });
    &CLIENT
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
