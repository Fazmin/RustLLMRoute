//! Dashboard sessions: `POST /login` checks `UI_USERNAME`/`UI_PASSWORD` (defaults: `admin` and
//! the master key) and issues an HS256 JWT signed with the master key, carried as the
//! `rustllmroute_ui` cookie or as a bearer token.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rustllmroute_core::AuthContext;
use rustllmroute_types::GatewayError;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub const COOKIE: &str = "rustllmroute_ui";
const ISSUER: &str = "rustllmroute";
const SESSION_TTL: Duration = Duration::from_secs(24 * 3600);
const MAX_FAILURES_PER_MINUTE: u32 = 10;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub user_role: String,
    pub iss: String,
    pub iat: u64,
    pub exp: u64,
}

/// Failed-login throttle: one window for the whole gateway (no per-IP state without a proxy
/// header convention), which still stops online guessing.
#[derive(Debug)]
pub struct LoginThrottle {
    window_start: Instant,
    failures: u32,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self {
            window_start: Instant::now(),
            failures: 0,
        }
    }
}

impl LoginThrottle {
    fn allow(&mut self) -> bool {
        if self.window_start.elapsed() >= Duration::from_secs(60) {
            self.window_start = Instant::now();
            self.failures = 0;
        }
        self.failures < MAX_FAILURES_PER_MINUTE
    }

    fn record_failure(&mut self) {
        self.failures += 1;
    }
}

pub fn issue_token(master_key: &str) -> Result<(String, u64), GatewayError> {
    let now = rustllmroute_core::now_epoch_secs();
    let claims = Claims {
        sub: "admin".to_owned(),
        user_role: "proxy_admin".to_owned(),
        iss: ISSUER.to_owned(),
        iat: now,
        exp: now + SESSION_TTL.as_secs(),
    };
    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(master_key.as_bytes()),
    )
    .map_err(|error| GatewayError::internal(format!("signing session token: {error}")))?;
    Ok((token, claims.exp))
}

pub fn validate_token(master_key: &str, token: &str) -> Option<Claims> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_required_spec_claims(&["exp", "iss", "sub"]);
    jsonwebtoken::decode::<Claims>(
        token,
        &DecodingKey::from_secret(master_key.as_bytes()),
        &validation,
    )
    .ok()
    .map(|data| data.claims)
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .find_map(|pair| pair.strip_prefix(&format!("{COOKIE}=")))
        .map(str::to_owned)
}

fn bearer_jwt(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let token = value
        .split_once(char::is_whitespace)
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())?;
    (token.matches('.').count() == 2 && !token.starts_with("sk-")).then(|| token.to_owned())
}

/// An admin context when the request carries a valid session cookie or bearer JWT.
pub fn session_from_headers(headers: &HeaderMap, master_key: &str) -> Option<AuthContext> {
    let token = cookie_value(headers).or_else(|| bearer_jwt(headers))?;
    let claims = validate_token(master_key, &token)?;
    Some(AuthContext {
        is_admin: claims.user_role == "proxy_admin",
        key_alias: Some("ui-session".to_owned()),
        user_id: Some(claims.sub),
        ..AuthContext::default()
    })
}

fn set_cookie(token: &str, max_age: u64, secure: bool) -> HeaderValue {
    let mut cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    if secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    if !state
        .login_throttle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow()
    {
        return Err(ApiError(GatewayError::RateLimited {
            message: "too many failed logins; try again in a minute".to_owned(),
            retry_after: Some(Duration::from_secs(60)),
            scope: rustllmroute_types::RateLimitScope::Key,
        }));
    }
    let master = state.master_key.expose_secret();
    let expected_user = std::env::var("UI_USERNAME").unwrap_or_else(|_| "admin".to_owned());
    let expected_password = std::env::var("UI_PASSWORD").unwrap_or_else(|_| master.to_owned());
    let user_ok = rustllmroute_control::constant_time_eq(&request.username, &expected_user);
    let password_ok = rustllmroute_control::constant_time_eq(&request.password, &expected_password);
    if !(user_ok && password_ok) {
        state
            .login_throttle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record_failure();
        return Err(ApiError(GatewayError::Unauthorized(
            "invalid username or password".to_owned(),
        )));
    }
    let (token, expires_at) = issue_token(master)?;
    let mut response = Json(json!({
        "token": token,
        "expires_at": expires_at,
        "user_id": "admin",
        "user_role": "proxy_admin",
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        set_cookie(&token, SESSION_TTL.as_secs(), state.ui_secure_cookie),
    );
    Ok(response)
}

pub async fn logout(State(state): State<Arc<AppState>>) -> Response {
    let mut response = (StatusCode::OK, Json(json!({"ok": true}))).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        set_cookie("", 0, state.ui_secure_cookie),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_reject_other_keys() {
        let (token, exp) = issue_token("master").unwrap();
        let claims = validate_token("master", &token).unwrap();
        assert_eq!(claims.user_role, "proxy_admin");
        assert_eq!(claims.exp, exp);
        assert!(validate_token("other", &token).is_none());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("a=b; {COOKIE}={token}").parse().unwrap(),
        );
        assert!(session_from_headers(&headers, "master").unwrap().is_admin);
        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(session_from_headers(&bearer, "master").is_some());
        let mut key = HeaderMap::new();
        key.insert(
            header::AUTHORIZATION,
            "Bearer sk-abc.def.ghi".parse().unwrap(),
        );
        assert!(session_from_headers(&key, "master").is_none());
        let mut throttle = LoginThrottle::default();
        for _ in 0..MAX_FAILURES_PER_MINUTE {
            assert!(throttle.allow());
            throttle.record_failure();
        }
        assert!(!throttle.allow());
    }
}
