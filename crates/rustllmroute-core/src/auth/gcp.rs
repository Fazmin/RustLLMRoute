use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rustllmroute_providers::{TokenFuture, TokenSource};
use rustllmroute_types::{ErrorKind, GatewayError};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::{home_dir, http_client};

pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const METADATA_TOKEN_PATH: &str = "/computeMetadata/v1/instance/service-accounts/default/token";
const REFRESH_MARGIN: Duration = Duration::from_secs(120);
const JWT_LIFETIME_SECS: u64 = 3600;

/// The two credential file shapes Google tooling produces.
#[derive(Clone, Deserialize)]
#[serde(tag = "type")]
pub enum CredentialFile {
    #[serde(rename = "service_account")]
    ServiceAccount {
        client_email: String,
        private_key: SecretString,
        #[serde(default)]
        token_uri: Option<String>,
    },
    #[serde(rename = "authorized_user")]
    AuthorizedUser {
        client_id: String,
        client_secret: SecretString,
        refresh_token: SecretString,
    },
}

enum Source {
    File(CredentialFile),
    Metadata,
}

/// Mints Google access tokens and caches them until shortly before expiry.
pub struct GcpAuth {
    source: Source,
    cache: Mutex<Option<(String, Instant)>>,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expiry")]
    expires_in: u64,
}

fn default_expiry() -> u64 {
    3600
}

fn parse_credentials(text: &str) -> Result<CredentialFile, GatewayError> {
    serde_json::from_str(text).map_err(|error| {
        GatewayError::internal(format!("google credentials JSON is not valid: {error}"))
    })
}

fn adc_path() -> Option<std::path::PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let path = Path::new(&appdata)
            .join("gcloud")
            .join("application_default_credentials.json");
        if path.exists() {
            return Some(path);
        }
    }
    let path = home_dir()?
        .join(".config")
        .join("gcloud")
        .join("application_default_credentials.json");
    path.exists().then_some(path)
}

impl GcpAuth {
    /// `spec` is `vertex_credentials`: inline JSON, base64-encoded JSON or a file path. Without it
    /// the application-default chain applies: `GOOGLE_APPLICATION_CREDENTIALS`, the gcloud ADC
    /// file, then the GCE metadata server.
    pub fn new(spec: Option<&str>) -> Result<Self, GatewayError> {
        let source = match spec.map(str::trim).filter(|s| !s.is_empty()) {
            Some(spec) if spec.starts_with('{') => Source::File(parse_credentials(spec)?),
            Some(spec) if Path::new(spec).is_file() => {
                let text = std::fs::read_to_string(spec).map_err(|error| {
                    GatewayError::internal(format!("reading vertex_credentials {spec}: {error}"))
                })?;
                Source::File(parse_credentials(&text)?)
            }
            Some(spec) => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(spec)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .ok_or_else(|| {
                        GatewayError::internal(
                            "vertex_credentials is neither JSON, base64 JSON nor an existing file",
                        )
                    })?;
                Source::File(parse_credentials(&decoded)?)
            }
            None => match std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
                .ok()
                .filter(|p| !p.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(adc_path)
            {
                Some(path) => {
                    let text = std::fs::read_to_string(&path).map_err(|error| {
                        GatewayError::internal(format!(
                            "reading google credentials {}: {error}",
                            path.display()
                        ))
                    })?;
                    Source::File(parse_credentials(&text)?)
                }
                None => Source::Metadata,
            },
        };
        Ok(Self {
            source,
            cache: Mutex::new(None),
        })
    }

    pub fn from_credentials(file: CredentialFile) -> Self {
        Self {
            source: Source::File(file),
            cache: Mutex::new(None),
        }
    }

    fn token_error(message: String, status: Option<u16>) -> GatewayError {
        match status {
            Some(status) => GatewayError::Upstream {
                provider: "vertex_ai",
                status,
                kind: ErrorKind::Auth,
                code: Some("token_exchange_failed".to_owned()),
                message,
                retry_after: None,
                body: None,
            },
            None => GatewayError::Transport(message),
        }
    }

    async fn exchange(uri: &str, form: String) -> Result<(String, Duration), GatewayError> {
        let response = http_client()
            .post(uri)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await
            .map_err(|error| Self::token_error(format!("google token endpoint: {error}"), None))?;
        let status = response.status();
        let body = response.bytes().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Self::token_error(
                format!(
                    "google token endpoint returned {status}: {}",
                    String::from_utf8_lossy(&body)
                        .chars()
                        .take(512)
                        .collect::<String>()
                ),
                Some(status.as_u16()),
            ));
        }
        let token: TokenResponse = serde_json::from_slice(&body).map_err(|error| {
            Self::token_error(format!("google token response: {error}"), Some(502))
        })?;
        Ok((token.access_token, Duration::from_secs(token.expires_in)))
    }

    async fn fetch(&self) -> Result<(String, Duration), GatewayError> {
        match &self.source {
            Source::File(CredentialFile::ServiceAccount {
                client_email,
                private_key,
                token_uri,
            }) => {
                let token_uri = token_uri.as_deref().unwrap_or(DEFAULT_TOKEN_URI);
                let now = crate::now_epoch_secs();
                let claims = Claims {
                    iss: client_email,
                    scope: SCOPE,
                    aud: token_uri,
                    iat: now,
                    exp: now + JWT_LIFETIME_SECS,
                };
                let key = EncodingKey::from_rsa_pem(private_key.expose_secret().as_bytes())
                    .map_err(|error| {
                        GatewayError::internal(format!(
                            "google service account private_key is not a valid RSA PEM: {error}"
                        ))
                    })?;
                let assertion = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
                    .map_err(|error| {
                        GatewayError::internal(format!("signing google JWT: {error}"))
                    })?;
                let form = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
                    .append_pair("assertion", &assertion)
                    .finish();
                Self::exchange(token_uri, form).await
            }
            Source::File(CredentialFile::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
            }) => {
                let form = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "refresh_token")
                    .append_pair("client_id", client_id)
                    .append_pair("client_secret", client_secret.expose_secret())
                    .append_pair("refresh_token", refresh_token.expose_secret())
                    .finish();
                Self::exchange(DEFAULT_TOKEN_URI, form).await
            }
            Source::Metadata => {
                let host = std::env::var("GCE_METADATA_HOST")
                    .ok()
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| "metadata.google.internal".to_owned());
                let url = format!("http://{host}{METADATA_TOKEN_PATH}");
                let response = http_client()
                    .get(&url)
                    .header("Metadata-Flavor", "Google")
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                    .map_err(|error| {
                        Self::token_error(
                            format!("no google credentials configured and the metadata server is unreachable: {error}"),
                            None,
                        )
                    })?;
                let status = response.status();
                let body = response.bytes().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(Self::token_error(
                        format!("metadata server returned {status}"),
                        Some(status.as_u16()),
                    ));
                }
                let token: TokenResponse = serde_json::from_slice(&body).map_err(|error| {
                    Self::token_error(format!("metadata token response: {error}"), Some(502))
                })?;
                Ok((token.access_token, Duration::from_secs(token.expires_in)))
            }
        }
    }
}

impl TokenSource for GcpAuth {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            if let Some((token, expires_at)) = cache.as_ref()
                && Instant::now() + REFRESH_MARGIN < *expires_at
            {
                return Ok(token.clone());
            }
            let (token, ttl) = self.fetch().await?;
            *cache = Some((token.clone(), Instant::now() + ttl));
            Ok(token)
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDxhNncj06QTLuW
Tahk/Wdu5Oc6ENMxCa/fPRi30yGpcXgbM5A9gGZkhmM1NvASNddGcJj+Kaklfb9f
+UNNkaJfZGI4uPG2qGaiF83UXRaOaOZ+q7MMJksEgROBO42/n7HBkghMi14s/Lp7
WUXv6HflQTCrBR8e+73qkG5HLV7BgolFLbys92RUOWk9WHN0SvaHBeoKc/hkcjAo
r6tEC0ELoz7u1ru8euCns8asWwa0hr+jXHOJ0EhJE1MQ8rvRZJnNISKUB16h1un6
gG7pbzqUidiL9VlUjWzf/1UsSI1dKi3n13lPQZJDv+XGEu/5j/wAiujDltEDgIdq
q1tKSyXhAgMBAAECggEAAflRCzBtoSf7xGzSbw2LSglV8uhLnaau19saA0HE501k
XuJt1KT190+HmdoUCVlPTRryQO40ReKyfv6vSWCEsERBsDGE38Jttrkz/J5zINHZ
5/RoMYfK923HPOsAXTLBJx9lqaov+A/RKy9AILzYSrBadiciEoJM9YwT9m1887Xy
LCDdUZ1eMB0lq5933EUvj2/aU22GhX80SfiKCdJ5MDa/ocDyE3z9vHsQ1SSvEMXw
iqvUYybRseNi0gB5LxTedGtZ04pKRgV3wYGuOwd8YKhu1vn6Hj2gWh8b+nGC5t8S
kt4q2GLZlebItdl/CEzliNj3Mitv0kfOgTjM4jfokQKBgQD8Gf7VeSNaEGX0YkKJ
lJYMY8lbAvR0vgjf5Cg187a96NcK5/iSjoiMt70fTvHV34r0C0zysDknrIJOvCuu
t017N0dlIRI+huzhtgBaVXR2Ygn/6WJdJXuIBW5iP2OOwC1MXwZSu+fGYPKE5gl3
P/1TdapBaMPnCELD4l/lnLn80wKBgQD1QPY6dgi6DhKPABeH/gxAs0xhmSdvA/Mn
vR4NfUiBIVAVID0cRmFGpRelEIn8/G81iGiLi4zNmPKc0+gfBpq7WZPg0v8duEYa
bs+iuLWvK8NulAsxibITN9DyQWI0kuHJMlLpOUypcGxJs/LwKjjmp16YSxnRIw/L
Q9/coFjR+wKBgQCI4pCVVwZUA+1oKeZZD939cj1F9q/Hp9vnvdn7mY6TRZdmK5iz
80K/rZC6LEJxz8SCSJSKfhc6rJfAqrnJT2x5yAUzKybd3qNG120Pn7DyCr/FPfMU
AqM1s0VBgYvMWhr+ieZEDBWIiOGGOeXP42Xo6E3+4CTnEHw4InXRli0C0wKBgQCA
bOoDJvfe5auWMkggMEoeP0WJ/fcGTR0w+/kWuzoSVG1pAp69haU+pwjAqO55CX76
m1mTaRnsN2Ej+lsclArGfFvRaOuqlmovQUN9RBfviI0oBGHwKNiU8694jABu41NU
VgPQmO6p83LhDVfzp0hwj8gbG4iDWUbV+87n6xdXrQKBgASoYCSII/1YrHX7aU6p
PTTTxBHT4vc5/1/q+LxuKuZQpDeeL1PqsucGhtQYWMLas7Zu5lJQXOLZ4zbwwuqK
6nse0FjM2YAdb5Q35K8uAK1yuP3JUJYnXmip63NmA8sA96qILvzWQ+VtcKJ2QaLm
lz3F/hJiI3EoP7H2vrZm2Zj7
-----END PRIVATE KEY-----
";

    #[tokio::test]
    async fn service_accounts_exchange_a_signed_jwt_and_cache_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer",
            ))
            .and(body_string_contains("assertion=eyJ"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.test", "expires_in": 3600, "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let auth = GcpAuth::new(Some(
            &serde_json::json!({
                "type": "service_account",
                "client_email": "sa@proj.iam.gserviceaccount.com",
                "private_key": TEST_KEY,
                "token_uri": format!("{}/token", server.uri()),
            })
            .to_string(),
        ))
        .unwrap();
        assert_eq!(auth.token().await.unwrap(), "ya29.test");
        assert_eq!(
            auth.token().await.unwrap(),
            "ya29.test",
            "second call is served from cache"
        );
        let request = &server.received_requests().await.unwrap()[0];
        let body = String::from_utf8_lossy(&request.body);
        let assertion = body.split("assertion=").nth(1).unwrap();
        assert_eq!(
            assertion.split('.').count(),
            3,
            "assertion is a compact JWT"
        );
    }

    #[tokio::test]
    async fn token_endpoint_failures_are_auth_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string("{\"error\":\"invalid_grant\"}"),
            )
            .mount(&server)
            .await;
        let auth = GcpAuth::from_credentials(CredentialFile::ServiceAccount {
            client_email: "sa@x".into(),
            private_key: SecretString::from(TEST_KEY),
            token_uri: Some(format!("{}/token", server.uri())),
        });
        let error = auth.token().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Auth);
        assert!(GcpAuth::new(Some("not json, not a file, not base64 {")).is_err());
    }
}
