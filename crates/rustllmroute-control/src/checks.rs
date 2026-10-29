use rustllmroute_core::{PreCallRequest, RequestKind};
use rustllmroute_db::now_ms;
use rustllmroute_types::GatewayError;

use crate::principal::Principal;
use crate::spend::PendingSpend;

fn route_for(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::ChatCompletion => "/v1/chat/completions",
        RequestKind::Messages => "/v1/messages",
        RequestKind::Responses => "/v1/responses",
        RequestKind::Embedding => "/v1/embeddings",
    }
}

fn budget_error(what: &str, spend: f64, max: f64) -> GatewayError {
    GatewayError::BudgetExceeded(format!(
        "ExceededBudget: current spend for {what}: {spend:.6}; max budget for {what}: {max:.6}"
    ))
}

/// The LiteLLM check order, minus the enterprise layers: blocked -> routes -> model access ->
/// expiry -> key budget -> team budget -> user budget.
pub fn pre_call(
    principal: &Principal,
    req: &PreCallRequest<'_>,
    pending: &PendingSpend,
) -> Result<(), GatewayError> {
    if principal.key.blocked {
        return Err(GatewayError::Forbidden(
            "this API key is blocked".to_owned(),
        ));
    }
    if principal.team.as_ref().is_some_and(|team| team.blocked) {
        return Err(GatewayError::Forbidden(
            "the team of this API key is blocked".to_owned(),
        ));
    }
    let allowed_routes = principal.key.allowed_routes_list();
    if !allowed_routes.is_empty() {
        let route = route_for(req.kind);
        let allowed = allowed_routes.iter().any(|entry| {
            entry == "*"
                || entry == "llm_api_routes"
                || entry == route
                || entry
                    .strip_suffix('*')
                    .is_some_and(|prefix| route.starts_with(prefix))
        });
        if !allowed {
            return Err(GatewayError::Forbidden(format!(
                "this API key may not call {route}"
            )));
        }
    }
    if !principal.allows_model(req.requested_model) {
        return Err(GatewayError::Forbidden(format!(
            "this API key is not allowed to access model {:?}",
            req.requested_model
        )));
    }
    let now = now_ms();
    if principal
        .key
        .expires_at
        .is_some_and(|expires| expires <= now)
    {
        return Err(GatewayError::Unauthorized(
            "this API key has expired".to_owned(),
        ));
    }
    if let Some(max) = principal.key.max_budget {
        let spend = principal.key.spend + pending.key(&principal.key.token);
        if spend >= max {
            return Err(budget_error("api_key", spend, max));
        }
    }
    if let Some(team) = &principal.team
        && let Some(max) = team.max_budget
    {
        let spend = team.spend + pending.team(&team.team_id);
        if spend >= max {
            return Err(budget_error("team", spend, max));
        }
    }
    if let Some(user) = &principal.user
        && let Some(max) = user.max_budget
    {
        let spend = user.spend + pending.user(&user.user_id);
        if spend >= max {
            return Err(budget_error("user", spend, max));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustllmroute_db::{KeyRow, TeamRow};

    use super::*;

    fn request() -> PreCallRequest<'static> {
        PreCallRequest {
            requested_model: "gpt-4o",
            kind: RequestKind::ChatCompletion,
            stream: false,
            estimated_input_tokens: None,
            max_output_tokens: None,
        }
    }

    fn key() -> KeyRow {
        let mut key = KeyRow::new("h".to_owned(), 0);
        key.models = "[\"gpt-4o\"]".to_owned();
        key
    }

    #[rstest]
    fn budgets_include_pending_spend() {
        let mut key = key();
        key.max_budget = Some(1.0);
        key.spend = 0.5;
        let principal = Principal::from_rows(key, None, None);
        let pending = PendingSpend::default();
        assert!(pre_call(&principal, &request(), &pending).is_ok());
        pending.add_key("h", 0.5);
        let error = pre_call(&principal, &request(), &pending).unwrap_err();
        assert!(matches!(error, GatewayError::BudgetExceeded(_)), "{error}");
    }

    #[rstest]
    fn blocked_expired_and_denied_models_are_rejected_in_order() {
        let pending = PendingSpend::default();
        let mut blocked = key();
        blocked.blocked = true;
        assert!(matches!(
            pre_call(
                &Principal::from_rows(blocked, None, None),
                &request(),
                &pending
            ),
            Err(GatewayError::Forbidden(_))
        ));
        let mut expired = key();
        expired.expires_at = Some(1);
        assert!(matches!(
            pre_call(
                &Principal::from_rows(expired, None, None),
                &request(),
                &pending
            ),
            Err(GatewayError::Unauthorized(_))
        ));
        let mut other_model = key();
        other_model.models = "[\"claude\"]".to_owned();
        assert!(matches!(
            pre_call(
                &Principal::from_rows(other_model, None, None),
                &request(),
                &pending
            ),
            Err(GatewayError::Forbidden(_))
        ));
        let mut routed = key();
        routed.allowed_routes = "[\"/v1/embeddings\"]".to_owned();
        assert!(matches!(
            pre_call(
                &Principal::from_rows(routed, None, None),
                &request(),
                &pending
            ),
            Err(GatewayError::Forbidden(_))
        ));
        let mut team = TeamRow::new("t".to_owned(), 0);
        team.blocked = true;
        assert!(matches!(
            pre_call(
                &Principal::from_rows(key(), Some(team), None),
                &request(),
                &pending
            ),
            Err(GatewayError::Forbidden(_))
        ));
    }
}
