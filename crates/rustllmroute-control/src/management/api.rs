use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::request::Parts;
use http::{HeaderValue, header};
use rustllmroute_core::AuthContext;
use rustllmroute_db::DbError;
use rustllmroute_types::GatewayError;

use super::{keys, spend, teams, users};
use crate::plane::ControlPlane;

/// Renders a [`GatewayError`] as an OpenAI-style error response.
#[derive(Debug)]
pub struct ApiError(pub GatewayError);

impl From<GatewayError> for ApiError {
    fn from(error: GatewayError) -> Self {
        Self(error)
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        Self(match error {
            DbError::Conflict { field } => GatewayError::BadRequest {
                message: format!("{field} already exists"),
                param: Some(field.to_owned()),
                code: Some("conflict"),
            },
            DbError::NotFound => GatewayError::NotFound("not found".to_owned()),
            other => {
                tracing::error!(error = %other, "management database error");
                GatewayError::Internal("database error".to_owned())
            }
        })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.0.status();
        let mut response = Response::new(Body::from(self.0.to_openai_body().to_string()));
        *response.status_mut() = status;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(retry_after) = self.0.retry_after()
            && let Ok(value) = HeaderValue::from_str(&retry_after.as_secs().max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// The caller as established by the auth middleware.
pub struct Caller(pub AuthContext);

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthContext>()
            .cloned()
            .map(Self)
            .ok_or_else(|| {
                ApiError(GatewayError::Unauthorized(
                    "authentication required".to_owned(),
                ))
            })
    }
}

/// Master key, UI session, or a key whose user is `proxy_admin`.
pub struct RequireAdmin(pub AuthContext);

impl<S: Send + Sync> FromRequestParts<S> for RequireAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Caller(auth) = Caller::from_request_parts(parts, state).await?;
        if !auth.is_admin {
            return Err(ApiError(GatewayError::Forbidden(
                "this route requires the master key or a proxy_admin key".to_owned(),
            )));
        }
        Ok(Self(auth))
    }
}

pub type PlaneState = Arc<ControlPlane>;

/// Management routes. Mount behind the auth middleware; each handler enforces its own role.
pub fn router(plane: Arc<ControlPlane>) -> Router {
    Router::new()
        .route("/key/generate", post(keys::generate))
        .route("/key/update", post(keys::update))
        .route("/key/delete", post(keys::delete))
        .route("/key/info", get(keys::info))
        .route("/key/list", get(keys::list))
        .route("/key/block", post(keys::block))
        .route("/key/unblock", post(keys::unblock))
        .route("/key/{key}/regenerate", post(keys::regenerate))
        .route("/team/new", post(teams::create))
        .route("/team/update", post(teams::update))
        .route("/team/delete", post(teams::delete))
        .route("/team/block", post(teams::block))
        .route("/team/unblock", post(teams::unblock))
        .route("/team/member_add", post(teams::member_add))
        .route("/team/member_delete", post(teams::member_delete))
        .route("/team/info", get(teams::info))
        .route("/team/list", get(teams::list))
        .route("/user/new", post(users::create))
        .route("/user/update", post(users::update))
        .route("/user/delete", post(users::delete))
        .route("/user/info", get(users::info))
        .route("/user/list", get(users::list))
        .route("/user/daily/activity", get(spend::daily_activity))
        .route("/spend/logs", get(spend::logs))
        .route("/spend/logs/ui", get(spend::logs_ui))
        .route("/spend/keys", get(spend::by_key))
        .route("/spend/users", get(spend::by_user))
        .route("/spend/teams", get(spend::by_team))
        .route("/global/spend", get(spend::global))
        .route("/global/spend/models", get(spend::by_model))
        .route("/global/spend/keys", get(spend::by_key))
        .route("/global/spend/teams", get(spend::by_team))
        .route("/global/spend/provider", get(spend::by_provider))
        .with_state(plane)
}
