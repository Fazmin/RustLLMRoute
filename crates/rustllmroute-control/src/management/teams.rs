use axum::Json;
use axum::extract::{Query, State};
use rustllmroute_db::{KeyFilter, Page, TeamPatch, TeamRow, now_ms};
use rustllmroute_types::GatewayError;
use serde_json::{Value, json};

use super::api::{ApiError, ApiResult, Caller, PlaneState, RequireAdmin};
use super::dto::{
    ListQuery, NewTeamRequest, TeamIds, TeamMemberAdd, TeamMemberDelete, TeamQuery, TeamRef,
    UpdateTeamRequest, json_text, key_json, object_text, team_json,
};
use crate::budget::{next_reset_at, parse_duration};

fn not_found() -> ApiError {
    ApiError(GatewayError::NotFound("team not found".to_owned()))
}

async fn load(plane: &PlaneState, team_id: &str) -> ApiResult<TeamRow> {
    plane.db().get_team(team_id).await?.ok_or_else(not_found)
}

fn invalidate(plane: &PlaneState, team_id: &str) {
    let id = team_id.to_owned();
    plane
        .cache()
        .invalidate_where(move |p| p.key.team_id.as_deref() == Some(id.as_str()));
}

pub async fn create(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<NewTeamRequest>,
) -> ApiResult<Json<Value>> {
    if let Some(duration) = &body.budget_duration
        && parse_duration(duration).is_none()
    {
        return Err(ApiError(GatewayError::bad_request(format!(
            "budget_duration {duration:?} is not a valid duration"
        ))));
    }
    let now = now_ms();
    let team_id = body
        .team_id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let mut row = TeamRow::new(team_id, now);
    row.team_alias = body.team_alias.filter(|a| !a.is_empty());
    row.members = json_text(&body.members_with_roles.unwrap_or_default());
    row.max_budget = body.max_budget;
    row.budget_reset_at = body
        .budget_duration
        .as_deref()
        .and_then(|d| next_reset_at(now, d));
    row.budget_duration = body.budget_duration;
    row.models = json_text(&body.models.unwrap_or_default());
    row.metadata = object_text(&body.metadata).unwrap_or_else(|| "{}".to_owned());
    row.max_parallel_requests = body.max_parallel_requests;
    row.tpm_limit = body.tpm_limit;
    row.rpm_limit = body.rpm_limit;
    row.blocked = body.blocked.unwrap_or(false);
    plane.db().insert_team(&row).await?;
    Ok(Json(team_json(&row)))
}

pub async fn update(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<UpdateTeamRequest>,
) -> ApiResult<Json<Value>> {
    let now = now_ms();
    let patch = TeamPatch {
        team_alias: body.team_alias,
        members: None,
        spend: None,
        max_budget: body.max_budget,
        budget_reset_at: body
            .budget_duration
            .as_ref()
            .map(|d| d.as_deref().and_then(|d| next_reset_at(now, d))),
        budget_duration: body.budget_duration,
        models: body.models.map(|m| json_text(&m)),
        model_max_budget: None,
        metadata: object_text(&body.metadata),
        max_parallel_requests: body.max_parallel_requests,
        tpm_limit: body.tpm_limit,
        rpm_limit: body.rpm_limit,
        blocked: body.blocked,
    };
    if !plane.db().update_team(&body.team_id, &patch).await? {
        return Err(not_found());
    }
    invalidate(&plane, &body.team_id);
    Ok(Json(team_json(&load(&plane, &body.team_id).await?)))
}

pub async fn delete(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<TeamIds>,
) -> ApiResult<Json<Value>> {
    let deleted = plane.db().delete_teams(&body.team_ids).await?;
    for id in &body.team_ids {
        invalidate(&plane, id);
    }
    Ok(Json(
        json!({ "deleted_teams": body.team_ids, "count": deleted }),
    ))
}

async fn set_blocked(plane: &PlaneState, team_id: &str, blocked: bool) -> ApiResult<Json<Value>> {
    let patch = TeamPatch {
        blocked: Some(blocked),
        ..TeamPatch::default()
    };
    if !plane.db().update_team(team_id, &patch).await? {
        return Err(not_found());
    }
    invalidate(plane, team_id);
    Ok(Json(team_json(&load(plane, team_id).await?)))
}

pub async fn block(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<TeamRef>,
) -> ApiResult<Json<Value>> {
    set_blocked(&plane, &body.team_id, true).await
}

pub async fn unblock(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<TeamRef>,
) -> ApiResult<Json<Value>> {
    set_blocked(&plane, &body.team_id, false).await
}

pub async fn member_add(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<TeamMemberAdd>,
) -> ApiResult<Json<Value>> {
    let team = load(&plane, &body.team_id).await?;
    let user_id = body
        .member
        .get("user_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError(GatewayError::bad_request("member.user_id is required")))?;
    let mut members: Vec<Value> = serde_json::from_str(&team.members).unwrap_or_default();
    members.retain(|m| m.get("user_id").and_then(Value::as_str) != Some(user_id));
    let role = body
        .member
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    members.push(json!({ "user_id": user_id, "role": role }));
    let patch = TeamPatch {
        members: Some(json_text(&members)),
        ..TeamPatch::default()
    };
    plane.db().update_team(&body.team_id, &patch).await?;
    invalidate(&plane, &body.team_id);
    Ok(Json(team_json(&load(&plane, &body.team_id).await?)))
}

pub async fn member_delete(
    State(plane): State<PlaneState>,
    RequireAdmin(_auth): RequireAdmin,
    Json(body): Json<TeamMemberDelete>,
) -> ApiResult<Json<Value>> {
    let team = load(&plane, &body.team_id).await?;
    let mut members: Vec<Value> = serde_json::from_str(&team.members).unwrap_or_default();
    members.retain(|m| m.get("user_id").and_then(Value::as_str) != Some(body.user_id.as_str()));
    let patch = TeamPatch {
        members: Some(json_text(&members)),
        ..TeamPatch::default()
    };
    plane.db().update_team(&body.team_id, &patch).await?;
    invalidate(&plane, &body.team_id);
    Ok(Json(team_json(&load(&plane, &body.team_id).await?)))
}

pub async fn info(
    State(plane): State<PlaneState>,
    Caller(auth): Caller,
    Query(query): Query<TeamQuery>,
) -> ApiResult<Json<Value>> {
    let team_id = query
        .team_id
        .or_else(|| auth.team_id.clone())
        .ok_or_else(|| ApiError(GatewayError::bad_request("team_id is required")))?;
    if !auth.is_admin && auth.team_id.as_deref() != Some(team_id.as_str()) {
        return Err(ApiError(GatewayError::Forbidden(
            "a key may only read its own team".to_owned(),
        )));
    }
    let team = load(&plane, &team_id).await?;
    let keys = plane
        .db()
        .list_keys(
            &KeyFilter {
                team_id: Some(team_id.clone()),
                include_blocked: true,
                ..KeyFilter::default()
            },
            Page::new(1, 1000),
        )
        .await?;
    Ok(Json(json!({
        "team_id": team_id,
        "team_info": team_json(&team),
        "keys": keys.items.iter().map(|k| key_json(k, None)).collect::<Vec<_>>(),
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
    let paged = plane.db().list_teams(page).await?;
    Ok(Json(json!({
        "teams": paged.items.iter().map(team_json).collect::<Vec<_>>(),
        "total_count": paged.total,
        "current_page": paged.page,
        "total_pages": paged.total_pages(),
    })))
}
