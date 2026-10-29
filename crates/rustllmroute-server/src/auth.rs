use http::HeaderMap;
pub use rustllmroute_control::{extract_credentials, generate_master_key};
use rustllmroute_core::{AuthContext, CallRecord, CallTicket, Hooks, NoTicket, PreCallRequest};
use rustllmroute_types::GatewayError;
use secrecy::{ExposeSecret, SecretString};

/// Master-key-only authentication: the mode the gateway runs in without a database.
pub struct MasterKeyHooks {
    master_key: SecretString,
}

impl MasterKeyHooks {
    pub fn new(master_key: SecretString) -> Self {
        Self { master_key }
    }
}

#[async_trait::async_trait]
impl Hooks for MasterKeyHooks {
    async fn authenticate(
        &self,
        headers: &HeaderMap,
        _path: &str,
    ) -> Result<AuthContext, GatewayError> {
        let Some(credential) = extract_credentials(headers) else {
            return Err(GatewayError::Unauthorized(
                "missing API key: send `Authorization: Bearer <key>` (or x-api-key / api-key)"
                    .to_owned(),
            ));
        };
        if rustllmroute_control::constant_time_eq(&credential, self.master_key.expose_secret()) {
            return Ok(AuthContext {
                is_admin: true,
                key_alias: Some("master-key".to_owned()),
                ..AuthContext::default()
            });
        }
        Err(GatewayError::Unauthorized(
            "invalid API key: this gateway runs without a database, only the master key is accepted"
                .to_owned(),
        ))
    }

    async fn pre_call(
        &self,
        _auth: &AuthContext,
        _req: &PreCallRequest<'_>,
    ) -> Result<Box<dyn CallTicket>, GatewayError> {
        Ok(Box::new(NoTicket))
    }

    fn on_complete(&self, _ticket: Box<dyn CallTicket>, _auth: &AuthContext, _record: CallRecord) {}
}
