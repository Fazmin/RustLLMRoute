use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rustllmroute_providers::{AwsCredentialSource, AwsCredentials, CredentialsFuture};
use rustllmroute_types::GatewayError;
use secrecy::SecretString;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::{home_dir, http_client};

const REFRESH_MARGIN: Duration = Duration::from_secs(300);
const IMDS_DEFAULT: &str = "http://169.254.169.254";
const CONTAINER_DEFAULT: &str = "http://169.254.170.2";

/// The AWS default credential chain, minus SSO and `credential_process`: explicit deployment
/// parameters, environment, shared credentials file, web identity (EKS), container (ECS), IMDSv2.
pub struct AwsAuth {
    explicit: Option<AwsCredentials>,
    profile: Option<String>,
    region: Arc<str>,
    cache: Mutex<Option<AwsCredentials>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct JsonCredentials {
    access_key_id: String,
    secret_access_key: String,
    #[serde(default, alias = "SessionToken")]
    token: Option<String>,
    #[serde(default)]
    expiration: Option<String>,
}

fn parse_expiration(text: Option<&str>) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(text?)
        .ok()
        .map(SystemTime::from)
}

impl From<JsonCredentials> for AwsCredentials {
    fn from(json: JsonCredentials) -> Self {
        Self {
            access_key_id: json.access_key_id,
            secret_access_key: SecretString::from(json.secret_access_key),
            session_token: json.token.map(SecretString::from),
            expires_at: parse_expiration(json.expiration.as_deref()),
        }
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn xml_tag<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].trim())
}

/// Parses an `AssumeRoleWithWebIdentity` XML response.
pub fn parse_sts_response(body: &str) -> Option<AwsCredentials> {
    Some(AwsCredentials {
        access_key_id: xml_tag(body, "AccessKeyId")?.to_owned(),
        secret_access_key: SecretString::from(xml_tag(body, "SecretAccessKey")?),
        session_token: xml_tag(body, "SessionToken").map(SecretString::from),
        expires_at: parse_expiration(xml_tag(body, "Expiration")),
    })
}

/// Parses one profile out of an INI-style shared credentials file.
pub fn parse_profile(text: &str, profile: &str) -> Option<AwsCredentials> {
    let mut current = String::new();
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = name
                .trim()
                .strip_prefix("profile ")
                .unwrap_or(name.trim())
                .to_owned();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            sections
                .entry(current.clone())
                .or_default()
                .insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let section = sections.get(profile)?;
    Some(AwsCredentials {
        access_key_id: section.get("aws_access_key_id")?.clone(),
        secret_access_key: SecretString::from(section.get("aws_secret_access_key")?.clone()),
        session_token: section
            .get("aws_session_token")
            .cloned()
            .map(SecretString::from),
        expires_at: None,
    })
}

impl AwsAuth {
    pub fn new(
        explicit: Option<AwsCredentials>,
        profile: Option<String>,
        region: Arc<str>,
    ) -> Self {
        Self {
            explicit,
            profile,
            region,
            cache: Mutex::new(None),
        }
    }

    fn from_env() -> Option<AwsCredentials> {
        Some(AwsCredentials {
            access_key_id: env_non_empty("AWS_ACCESS_KEY_ID")?,
            secret_access_key: SecretString::from(env_non_empty("AWS_SECRET_ACCESS_KEY")?),
            session_token: env_non_empty("AWS_SESSION_TOKEN").map(SecretString::from),
            expires_at: None,
        })
    }

    fn shared_file(&self) -> Option<AwsCredentials> {
        let path = env_non_empty("AWS_SHARED_CREDENTIALS_FILE")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|home| home.join(".aws").join("credentials")))?;
        let text = std::fs::read_to_string(path).ok()?;
        let profile = self
            .profile
            .clone()
            .or_else(|| env_non_empty("AWS_PROFILE"))
            .unwrap_or_else(|| "default".to_owned());
        parse_profile(&text, &profile)
    }

    async fn web_identity(&self) -> Result<Option<AwsCredentials>, GatewayError> {
        let (Some(token_file), Some(role_arn)) = (
            env_non_empty("AWS_WEB_IDENTITY_TOKEN_FILE"),
            env_non_empty("AWS_ROLE_ARN"),
        ) else {
            return Ok(None);
        };
        let token = std::fs::read_to_string(&token_file).map_err(|error| {
            GatewayError::internal(format!("reading AWS_WEB_IDENTITY_TOKEN_FILE: {error}"))
        })?;
        let session = env_non_empty("AWS_ROLE_SESSION_NAME")
            .unwrap_or_else(|| format!("rustllmroute-{}", crate::now_epoch_secs()));
        let endpoint = env_non_empty("AWS_ENDPOINT_URL_STS")
            .unwrap_or_else(|| format!("https://sts.{}.amazonaws.com", self.region));
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("Action", "AssumeRoleWithWebIdentity")
            .append_pair("Version", "2011-06-15")
            .append_pair("RoleArn", &role_arn)
            .append_pair("RoleSessionName", &session)
            .append_pair("WebIdentityToken", token.trim())
            .finish();
        let response = http_client()
            .post(&endpoint)
            .header(
                http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(http::header::ACCEPT, "application/xml")
            .body(form)
            .send()
            .await
            .map_err(|error| GatewayError::Transport(format!("sts: {error}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(GatewayError::internal(format!(
                "sts AssumeRoleWithWebIdentity returned {status}: {}",
                text.chars().take(512).collect::<String>()
            )));
        }
        parse_sts_response(&text)
            .map(Some)
            .ok_or_else(|| GatewayError::internal("sts response did not contain credentials"))
    }

    async fn container(&self) -> Result<Option<AwsCredentials>, GatewayError> {
        let uri = match (
            env_non_empty("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
            env_non_empty("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
        ) {
            (Some(full), _) => full,
            (None, Some(relative)) => format!("{CONTAINER_DEFAULT}{relative}"),
            (None, None) => return Ok(None),
        };
        let token = env_non_empty("AWS_CONTAINER_AUTHORIZATION_TOKEN").or_else(|| {
            env_non_empty("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE")
                .and_then(|file| std::fs::read_to_string(file).ok())
                .map(|t| t.trim().to_owned())
        });
        let mut request = http_client().get(&uri).timeout(Duration::from_secs(3));
        if let Some(token) = token {
            request = request.header(http::header::AUTHORIZATION, token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| GatewayError::Transport(format!("container credentials: {error}")))?;
        if !response.status().is_success() {
            return Err(GatewayError::internal(format!(
                "container credentials endpoint returned {}",
                response.status()
            )));
        }
        let json: JsonCredentials = response.json().await.map_err(|error| {
            GatewayError::internal(format!("container credentials response: {error}"))
        })?;
        Ok(Some(json.into()))
    }

    async fn imds(&self) -> Option<AwsCredentials> {
        if env_non_empty("AWS_EC2_METADATA_DISABLED")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            return None;
        }
        let base = env_non_empty("AWS_EC2_METADATA_SERVICE_ENDPOINT")
            .unwrap_or_else(|| IMDS_DEFAULT.to_owned());
        let base = base.trim_end_matches('/');
        let client = http_client();
        let token = client
            .put(format!("{base}/latest/api/token"))
            .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?;
        let role = client
            .get(format!("{base}/latest/meta-data/iam/security-credentials/"))
            .header("X-aws-ec2-metadata-token", &token)
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?;
        let role = role.lines().next()?.trim().to_owned();
        if role.is_empty() {
            return None;
        }
        let json: JsonCredentials = client
            .get(format!(
                "{base}/latest/meta-data/iam/security-credentials/{role}"
            ))
            .header("X-aws-ec2-metadata-token", &token)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        Some(json.into())
    }

    async fn resolve(&self) -> Result<AwsCredentials, GatewayError> {
        if let Some(explicit) = &self.explicit {
            return Ok(explicit.clone());
        }
        if let Some(found) = Self::from_env() {
            return Ok(found);
        }
        if let Some(found) = self.shared_file() {
            return Ok(found);
        }
        if let Some(found) = self.web_identity().await? {
            return Ok(found);
        }
        if let Some(found) = self.container().await? {
            return Ok(found);
        }
        if let Some(found) = self.imds().await {
            return Ok(found);
        }
        Err(GatewayError::internal(
            "no AWS credentials found: set aws_access_key_id/aws_secret_access_key on the deployment, AWS_ACCESS_KEY_ID in the environment, a shared credentials profile, or run with an instance/task role",
        ))
    }
}

impl AwsCredentialSource for AwsAuth {
    fn credentials(&self) -> CredentialsFuture<'_> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            if let Some(cached) = cache.as_ref() {
                let fresh = match cached.expires_at {
                    None => true,
                    Some(expires_at) => SystemTime::now() + REFRESH_MARGIN < expires_at,
                };
                if fresh {
                    return Ok(cached.clone());
                }
            }
            let resolved = self.resolve().await?;
            *cache = Some(resolved.clone());
            Ok(resolved)
        })
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn parses_profiles_and_sts_xml() {
        let file = "[default]\naws_access_key_id = AKIA1\naws_secret_access_key = s1\n\n[profile prod]\naws_access_key_id=AKIA2\naws_secret_access_key=s2\naws_session_token=tok\n";
        let prod = parse_profile(file, "prod").unwrap();
        assert_eq!(prod.access_key_id, "AKIA2");
        assert_eq!(prod.session_token.unwrap().expose_secret(), "tok");
        assert_eq!(
            parse_profile(file, "default")
                .unwrap()
                .secret_access_key
                .expose_secret(),
            "s1"
        );
        assert!(parse_profile(file, "missing").is_none());
        let xml = "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials><AccessKeyId>ASIA</AccessKeyId><SecretAccessKey>sec</SecretAccessKey><SessionToken>st</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleWithWebIdentityResult></AssumeRoleWithWebIdentityResponse>";
        let creds = parse_sts_response(xml).unwrap();
        assert_eq!(creds.access_key_id, "ASIA");
        assert!(creds.expires_at.unwrap() > SystemTime::now());
    }

    #[tokio::test]
    async fn explicit_credentials_win_and_are_cached() {
        let auth = AwsAuth::new(
            Some(AwsCredentials {
                access_key_id: "AKIAEXPLICIT".into(),
                secret_access_key: SecretString::from("s"),
                session_token: None,
                expires_at: None,
            }),
            None,
            Arc::from("us-east-1"),
        );
        assert_eq!(
            auth.credentials().await.unwrap().access_key_id,
            "AKIAEXPLICIT"
        );
        assert!(auth.cache.lock().await.is_some());
    }
}
