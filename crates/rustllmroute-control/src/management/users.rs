use axum::Json;
use axum::extract::{Query, State};
use rustllmroute_db::{KeyFilter, KeyRow, Page, UserPatch, UserRow, now_ms};
use rustllmroute_types::GatewayError;
use serde_json::{Value, json};

use super::api::{ApiError, ApiResult, Caller, PlaneState, RequireAdmin};
use super::dto::{
    ListQuery, NewUserRequest, UpdateUserRequest, UserIds, UserQuery, json_text, key_json,
    object_text, user_json,
};
use crate::budget::{next_reset_at, parse_duration};
use crate::credentials::{generate_key, hash_token, key_name};

fn not_found() -> ApiError {
    ApiError(GatewayError::NotFound("user not found".to_owned()))
}

fn invalidate(plane: &PlaneState, user_id: &str) {
    let id = user_id.to_owned();
    plane
        .cache()
        .invalidate_where(move |p| p.key.user_id.as_deref() == Some(id.as_str()));
}

pub async fn create(
    State(plane): State<PlaneState>,
    RequireAdmin(auth): RequireAdmin,
    Json(body): Json<NewUserRequest>,
) -> ApiResult<Json<Value>> {
    if let Some(duration) = &body.budget_duration
        && parse_duration(duration).is_none()
    {
        return Err(ApiError(GatewayError::bad_request(format!(
            "budget_duration {duration:?} is not a valid duration"
        ))));
    }
    let now = now_ms();
    let user_id = body
        .user_id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let mut row = UserRow::new(user_id.clone(), now);
    row.user_email = body.user_email.filter(|e| !e.is_empty());
    row.user_alias = body.user_alias;
    row.user_role = body.user_role.unwrap_or_else(|| "internal_user".to_owned());
    row.max_budget = body.max_budget;
    row.budget_reset_at = body
        .budget_duration
        .as_deref()
        .and_then(|d| next_reset_at(now, d));
    row.budget_duration = body.budget_duration;
    row.models = json_text(&body.models.clone().unwrap_or_default());
    row.metadata = object_text(&body.metadata).unwrap_or_else(|| "{}".to_owned());
    row.max_parallel_requests = body.max_parallel_requests;
    row.tpm_limit = body.tpm_limit;
    row.rpm_limit = body.rpm_limit;
    plane.db().insert_user(&row).await?;
    let mut response = user_json(&row);
    if body.auto_create_key {
        let raw = generate_key();
        let mut key = KeyRow::new(hash_token(&raw), now);
        key.key_name = Some(key_name(&raw));
        key.key_alias = body.key_alias;
        key.user_id = Some(user_id.clone());
        key.team_id = body.team_id;
        key.models = json_text(&body.models.unwrap_or_default());
        key.created_by = auth.key_alias.clone();
        plane.db().insert_key(&key).await?;
        if let Value::Object(map) = &mut response
            && let Value::Object(key_fields) = key_json(&key, Some(&raw))
        {
            for (name, value) in key_fields {
                map.entry(name).or_insert(value);
            }
            map.insert("key".to_owned(), Value::String(raw));
        }
    }
    Ok(Json(response))
}

pub async fn update(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<UpdateUserRequest>,
) -> ApiResult<Json<Value>> {
    let now = now_ms();
    let patch = UserPatch {
        user_email: body.user_email,
        user_alias: body.user_alias,
        user_role: body.user_role,
        spend: None,
        max_budget: body.max_budget,
        budget_reset_at: body
            .budget_duration
            .as_ref()
            .map(|d| d.as_deref().and_then(|d| next_reset_at(now, d))),
        budget_duration: body.budget_duration,
        models: body.models.map(|m| json_text(&m)),
        metadata: object_text(&body.metadata),
        max_parallel_requests: body.max_parallel_requests,
        tpm_limit: body.tpm_limit,
        rpm_limit: body.rpm_limit,
    };
    if !plane.db().update_user(&body.user_id, &patch).await? {
        return Err(not_found());
    }
    invalidate(&plane, &body.user_id);
    let row = plane
        .db()
        .get_user(&body.user_id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(user_json(&row)))
}

pub async fn delete(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<UserIds>,
) -> ApiResult<Json<Value>> {
    let deleted = plane.db().delete_users(&body.user_ids).await?;
    for id in &body.user_ids {
        invalidate(&plane, id);
    }
    Ok(Json(
        json!({ "deleted_users": body.user_ids, "count": deleted }),
    ))
}

pub async fn info(
    State(plane): State<PlaneState>,
    Caller(auth): Caller,
    Query(query): Query<UserQuery>,
) -> ApiResult<Json<Value>> {
    let user_id = query
        .user_id
        .or_else(|| auth.user_id.clone())
        .ok_or_else(|| ApiError(GatewayError::bad_request("user_id is required")))?;
    if !auth.is_admin && auth.user_id.as_deref() != Some(user_id.as_str()) {
        return Err(ApiError(GatewayError::Forbidden(
            "a key may only read its own user".to_owned(),
        )));
    }
    let row = plane.db().get_user(&user_id).await?.ok_or_else(not_found)?;
    let keys = plane
        .db()
        .list_keys(
            &KeyFilter {
                user_id: Some(user_id.clone()),
                include_blocked: true,
                ..KeyFilter::default()
            },
            Page::new(1, 1000),
        )
        .await?;
    Ok(Json(json!({
        "user_id": user_id,
        "user_info": user_json(&row),
        "keys": keys.items.iter().map(|k| key_json(k, None)).collect::<Vec<_>>(),
        "teams": [],
    })))
}

pub async fn list(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let page = Page::new(
        query.page.unwrap_or(1),
        query.size.or(query.page_size).unwrap_or(50),
    );
    let paged = plane.db().list_users(page).await?;
    Ok(Json(json!({
        "users": paged.items.iter().map(user_json).collect::<Vec<_>>(),
        "total": paged.total,
        "page": paged.page,
        "total_pages": paged.total_pages(),
    })))
}
