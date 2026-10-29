use axum::Json;
use axum::extract::{Path, Query, State};
use rustllmroute_db::{KeyFilter, KeyPatch, KeyRow, Page, now_ms};
use rustllmroute_types::GatewayError;
use serde_json::{Value, json};

use super::api::{ApiError, ApiResult, Caller, PlaneState, RequireAdmin};
use super::dto::{
    DeleteKeysRequest, GenerateKeyRequest, KeyQuery, KeyRef, ListQuery, RegenerateRequest,
    UpdateKeyRequest, json_text, key_json, object_text,
};
use crate::budget::{expires_at, next_reset_at, parse_duration};
use crate::credentials::{generate_key, hash_token, is_virtual_key, key_name};

fn bad_request(message: impl Into<String>, param: &str) -> ApiError {
    ApiError(GatewayError::BadRequest {
        message: message.into(),
        param: Some(param.to_owned()),
        code: None,
    })
}

/// Management routes accept the raw `sk-` key or its hash; only the hash is ever stored or shown.
fn token_of(key: &str) -> String {
    if is_virtual_key(key) {
        hash_token(key)
    } else {
        key.to_owned()
    }
}

fn check_duration(value: &Option<String>, param: &str) -> ApiResult<()> {
    if let Some(text) = value
        && parse_duration(text).is_none()
    {
        return Err(bad_request(
            format!("{param} must look like 30s, 15m, 2h, 7d, 1w or 1mo (got {text:?})"),
            param,
        ));
    }
    Ok(())
}

pub async fn generate(
    State(plane): State<PlaneState>,
    RequireAdmin(auth): RequireAdmin,
    Json(body): Json<GenerateKeyRequest>,
) -> ApiResult<Json<Value>> {
    check_duration(&body.duration, "duration")?;
    check_duration(&body.budget_duration, "budget_duration")?;
    let db = plane.db();
    if let Some(team_id) = &body.team_id
        && db.get_team(team_id).await?.is_none()
    {
        return Err(bad_request(
            format!("team {team_id:?} does not exist"),
            "team_id",
        ));
    }
    if let Some(user_id) = &body.user_id
        && db.get_user(user_id).await?.is_none()
    {
        return Err(bad_request(
            format!("user {user_id:?} does not exist"),
            "user_id",
        ));
    }
    let now = now_ms();
    let raw = generate_key();
    let mut row = KeyRow::new(hash_token(&raw), now);
    row.key_name = Some(key_name(&raw));
    row.key_alias = body.key_alias.filter(|alias| !alias.is_empty());
    row.user_id = body.user_id;
    row.team_id = body.team_id;
    row.max_budget = body.max_budget;
    row.soft_budget = body.soft_budget;
    row.budget_reset_at = body
        .budget_duration
        .as_deref()
        .and_then(|d| next_reset_at(now, d));
    row.budget_duration = body.budget_duration;
    row.expires_at = body.duration.as_deref().and_then(|d| expires_at(now, d));
    row.models = json_text(&body.models.unwrap_or_default());
    row.aliases = json_text(&body.aliases.unwrap_or_default());
    row.model_max_budget = json_text(&body.model_max_budget.unwrap_or_default());
    row.allowed_routes = json_text(&body.allowed_routes.unwrap_or_default());
    row.metadata = object_text(&body.metadata).unwrap_or_else(|| "{}".to_owned());
    row.max_parallel_requests = body.max_parallel_requests;
    row.tpm_limit = body.tpm_limit;
    row.rpm_limit = body.rpm_limit;
    row.blocked = body.blocked.unwrap_or(false);
    row.created_by = auth.key_alias.clone().or(auth.user_id.clone());
    db.insert_key(&row).await?;
    tracing::info!(key_name = ?row.key_name, alias = ?row.key_alias, "virtual key created");
    Ok(Json(key_json(&row, Some(&raw))))
}

pub async fn update(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<UpdateKeyRequest>,
) -> ApiResult<Json<Value>> {
    let token = token_of(&body.key);
    if let Some(Some(duration)) = &body.budget_duration {
        check_duration(&Some(duration.clone()), "budget_duration")?;
    }
    if let Some(Some(duration)) = &body.duration {
        check_duration(&Some(duration.clone()), "duration")?;
    }
    let now = now_ms();
    let patch = KeyPatch {
        key_alias: body.key_alias,
        user_id: body.user_id,
        team_id: body.team_id,
        spend: body.spend,
        max_budget: body.max_budget,
        soft_budget: body.soft_budget,
        budget_reset_at: body
            .budget_duration
            .as_ref()
            .map(|duration| duration.as_deref().and_then(|d| next_reset_at(now, d))),
        budget_duration: body.budget_duration,
        expires_at: body
            .duration
            .map(|duration| duration.as_deref().and_then(|d| expires_at(now, d))),
        models: body.models.map(|m| json_text(&m)),
        aliases: body.aliases.map(|a| json_text(&a)),
        model_max_budget: body.model_max_budget.map(|m| json_text(&m)),
        allowed_routes: body.allowed_routes.map(|r| json_text(&r)),
        metadata: object_text(&body.metadata),
        max_parallel_requests: body.max_parallel_requests,
        tpm_limit: body.tpm_limit,
        rpm_limit: body.rpm_limit,
        blocked: body.blocked,
    };
    if !plane.db().update_key(&token, &patch).await? {
        return Err(ApiError(GatewayError::NotFound("key not found".to_owned())));
    }
    plane.cache().invalidate_key(&token).await;
    let row = plane
        .db()
        .get_key(&token)
        .await?
        .ok_or_else(|| ApiError(GatewayError::NotFound("key not found".to_owned())))?;
    Ok(Json(key_json(&row, None)))
}

pub async fn delete(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<DeleteKeysRequest>,
) -> ApiResult<Json<Value>> {
    let mut tokens: Vec<String> = body.keys.iter().map(|k| token_of(k)).collect();
    for alias in &body.key_aliases {
        if let Some(row) = plane.db().get_key_by_alias(alias).await? {
            tokens.push(row.token);
        }
    }
    tokens.sort();
    tokens.dedup();
    if tokens.is_empty() {
        return Err(bad_request("provide keys or key_aliases to delete", "keys"));
    }
    let deleted = plane.db().delete_keys(&tokens).await?;
    for token in &tokens {
        plane.cache().invalidate_key(token).await;
    }
    tracing::info!(deleted, "virtual keys deleted");
    Ok(Json(json!({ "deleted_keys": tokens })))
}

pub async fn info(
    State(plane): State<PlaneState>,
    Caller(auth): Caller,
    Query(query): Query<KeyQuery>,
) -> ApiResult<Json<Value>> {
    let token = match (query.key, &auth.key_hash) {
        (Some(key), _) => token_of(&key),
        (None, Some(own)) => own.to_string(),
        (None, None) => return Err(bad_request("key is required", "key")),
    };
    if !auth.is_admin && auth.key_hash.as_deref() != Some(token.as_str()) {
        return Err(ApiError(GatewayError::Forbidden(
            "a key may only read its own info".to_owned(),
        )));
    }
    let row = plane
        .db()
        .get_key(&token)
        .await?
        .ok_or_else(|| ApiError(GatewayError::NotFound("key not found".to_owned())))?;
    Ok(Json(json!({ "key": token, "info": key_json(&row, None) })))
}

pub async fn list(
    State(plane): State<PlaneState>,
    Caller(auth): Caller,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let mut filter = KeyFilter {
        user_id: query.user_id,
        team_id: query.team_id,
        key_alias: query.key_alias,
        include_blocked: query.include_blocked,
    };
    if !auth.is_admin {
        match &auth.user_id {
            Some(user_id) => filter.user_id = Some(user_id.clone()),
            None => {
                return Err(ApiError(GatewayError::Forbidden(
                    "listing keys requires an admin key or a key with a user".to_owned(),
                )));
            }
        }
    }
    let page = Page::new(
        query.page.unwrap_or(1),
        query.size.or(query.page_size).unwrap_or(50),
    );
    let paged = plane.db().list_keys(&filter, page).await?;
    Ok(Json(json!({
        "keys": paged.items.iter().map(|row| key_json(row, None)).collect::<Vec<_>>(),
        "total_count": paged.total,
        "current_page": paged.page,
        "total_pages": paged.total_pages(),
    })))
}

async fn set_blocked(plane: &PlaneState, key: &str, blocked: bool) -> ApiResult<Json<Value>> {
    let token = token_of(key);
    if plane
        .db()
        .set_keys_blocked(std::slice::from_ref(&token), blocked)
        .await?
        == 0
    {
        return Err(ApiError(GatewayError::NotFound("key not found".to_owned())));
    }
    plane.cache().invalidate_key(&token).await;
    let row = plane
        .db()
        .get_key(&token)
        .await?
        .ok_or_else(|| ApiError(GatewayError::NotFound("key not found".to_owned())))?;
    Ok(Json(key_json(&row, None)))
}

pub async fn block(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<KeyRef>,
) -> ApiResult<Json<Value>> {
    set_blocked(&plane, &body.key, true).await
}

pub async fn unblock(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<KeyRef>,
) -> ApiResult<Json<Value>> {
    set_blocked(&plane, &body.key, false).await
}

pub async fn regenerate(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Path(key): Path<String>,
    body: Option<Json<RegenerateRequest>>,
) -> ApiResult<Json<Value>> {
    let old = token_of(&key);
    let raw = generate_key();
    let new_token = hash_token(&raw);
    plane
        .db()
        .replace_key_token(&old, &new_token, &key_name(&raw))
        .await?;
    plane.cache().invalidate_key(&old).await;
    if let Some(Json(body)) = body
        && let Some(duration) = body.duration
    {
        check_duration(&Some(duration.clone()), "duration")?;
        let patch = KeyPatch {
            expires_at: Some(expires_at(now_ms(), &duration)),
            ..KeyPatch::default()
        };
        plane.db().update_key(&new_token, &patch).await?;
    }
    let row = plane
        .db()
        .get_key(&new_token)
        .await?
        .ok_or_else(|| ApiError(GatewayError::NotFound("key not found".to_owned())))?;
    tracing::info!(key_name = ?row.key_name, "virtual key regenerated");
    Ok(Json(key_json(&row, Some(&raw))))
}
