//! Teams, their membership, and the repo access they carry.
//!
//! Every mutation here requires `OrgAdmin`; listing is a read, because a
//! member needs to see which teams exist before asking to be in one.
//!
//! `POST …/repos/:repo/grants` is deliberately one endpoint for three
//! shapes — one person, several people, or a team — because they are one
//! act ("give these people this role here") and splitting them would put
//! the same authorization and audit code in three places. The older
//! single-`user_id` body keeps working.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::members::{self, Role};
use stratum_control::teams::{self, Team};

#[derive(Deserialize)]
pub struct TeamBody {
    pub name: Option<String>,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct GrantBody {
    /// One person. The original shape, still accepted.
    pub user_id: Option<String>,
    /// Several people at once.
    #[serde(default)]
    pub user_ids: Vec<String>,
    /// A whole team.
    pub team_id: Option<String>,
    pub role: String,
}

fn parse_role(s: &str) -> Result<Role, Response> {
    Role::parse(s).ok_or_else(|| {
        json_error(
            StatusCode::BAD_REQUEST,
            format!("unknown role {s:?} (owner | admin | member | viewer)"),
        )
    })
}

fn team_json(t: &Team) -> serde_json::Value {
    serde_json::json!({
        "id": t.id,
        "name": t.name,
        "description": t.description,
        "created_at": t.created_at,
        "member_count": t.member_count,
    })
}

pub async fn list(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        return r;
    }
    match teams::list(&state.db, &org.id) {
        Ok(ts) => Json(serde_json::json!({
            "teams": ts.iter().map(team_json).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

pub async fn create(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<TeamBody>,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(name) = body.name.as_deref() else {
        return json_error(StatusCode::BAD_REQUEST, "name is required");
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match teams::create(
        &state.db,
        &org.id,
        name,
        body.description.as_deref(),
        Some(&actx),
    ) {
        Ok(t) => (StatusCode::CREATED, Json(team_json(&t))).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, e),
    }
}

pub async fn update(
    State(state): State<SharedState>,
    Path((org_name, team_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<TeamBody>,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match teams::update(
        &state.db,
        &org.id,
        &team_id,
        body.name.as_deref(),
        body.description.as_deref(),
        Some(&actx),
    ) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authx::not_found(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, e),
    }
}

pub async fn delete(
    State(state): State<SharedState>,
    Path((org_name, team_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match teams::delete(&state.db, &org.id, &team_id, Some(&actx)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}

pub async fn list_members(
    State(state): State<SharedState>,
    Path((org_name, team_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        return r;
    }
    // Resolve the team inside the org first: a team id from elsewhere
    // must read as absent, not list somebody else's roster.
    match teams::by_id(&state.db, &org.id, &team_id) {
        Ok(None) => return authx::not_found(),
        Err(e) => return internal(e),
        Ok(Some(_)) => {}
    }
    match teams::members(&state.db, &team_id) {
        Ok(ms) => Json(serde_json::json!({
            "members": ms.iter().map(|m| serde_json::json!({
                "user_id": m.user_id,
                "email": m.email,
                "name": m.name,
                "created_at": m.created_at,
            })).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

pub async fn add_member(
    State(state): State<SharedState>,
    Path((org_name, team_id, user_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match teams::add_member(&state.db, &org.id, &team_id, &user_id, Some(&actx)) {
        // Idempotent: already in the team is the state that was asked for.
        Ok(Ok(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => json_error(StatusCode::BAD_REQUEST, e),
        Err(e) => internal(e),
    }
}

pub async fn remove_member(
    State(state): State<SharedState>,
    Path((org_name, team_id, user_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match teams::remove_member(&state.db, &org.id, &team_id, &user_id, Some(&actx)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}

/// Grant a role on one repo to people, or to a team.
///
/// Replaces the single-user handler that was here; `{user_id, role}`
/// still works, and is just the one-element case of `user_ids`.
pub async fn grant_repo(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<GrantBody>,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let role = match parse_role(&body.role) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));

    if let Some(team_id) = body.team_id.as_deref() {
        if body.user_id.is_some() || !body.user_ids.is_empty() {
            return json_error(
                StatusCode::BAD_REQUEST,
                "grant either people or a team, not both — they are different rules",
            );
        }
        return match teams::grant_repo(&state.db, &org.id, &repo.id, team_id, role, Some(&actx)) {
            Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
            Ok(Err(e)) => json_error(StatusCode::BAD_REQUEST, e),
            Err(e) => internal(e),
        };
    }

    let mut people: Vec<String> = body.user_ids.clone();
    if let Some(u) = body.user_id.as_deref() {
        people.push(u.to_string());
    }
    if people.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "no user_ids or team_id given");
    }
    if people.len() > 200 {
        return json_error(StatusCode::BAD_REQUEST, "too many users in one grant");
    }
    // Everyone is checked before anyone is granted. A partial application
    // would leave the caller unable to tell which half took effect, and
    // "the third id was a typo" is not a reason to half-change access.
    for u in &people {
        match members::role_of(&state.db, &org.id, u) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    format!("user {u} is not a member of this org"),
                )
            }
            Err(e) => return internal(e),
        }
    }
    for u in &people {
        if let Err(e) = members::grant_repo(&state.db, &repo.id, u, role, Some(&actx)) {
            return json_error(StatusCode::BAD_REQUEST, e);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

pub async fn revoke_team_grant(
    State(state): State<SharedState>,
    Path((org_name, repo_name, team_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match teams::revoke_repo_grant(&state.db, &org.id, &repo.id, &team_id, Some(&actx)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}

/// Who can reach this repo, and where each person's access came from.
///
/// The endpoint that makes teams usable: with three rules interacting,
/// "why can Alice write here?" has to be answerable on one screen rather
/// than by reading the schema.
pub async fn access(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo) = match crate::app::repo_or_masked(&state, &headers, &org_name, &repo_name) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Who else can reach a repo is administrative: it is the org's
    // access map, and a member holding a repo token should not be able
    // to enumerate the roster through it.
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        return r;
    }
    match teams::repo_access(&state.db, &org.id, &repo.id) {
        Ok((people, granted)) => Json(serde_json::json!({
            "people": people.iter().map(|a| serde_json::json!({
                "user_id": a.user_id,
                "email": a.email,
                "name": a.name,
                "role": a.role.as_str(),
                "source": a.source.as_str(),
                "team_id": a.team_id,
                "team_name": a.team_name,
            })).collect::<Vec<_>>(),
            "teams": granted.iter().map(|t| serde_json::json!({
                "team_id": t.team_id,
                "team_name": t.team_name,
                "role": t.role.as_str(),
                "member_count": t.member_count,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}
