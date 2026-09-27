//! Managing who is in an org and what they may do.
//!
//! Every mutation here requires `OrgAdmin`, which owner and admin both
//! carry. The difference between those two roles is enforced further down
//! — in `members::remove` and `members::set_role`, which refuse to strip
//! the last owner — rather than by scope, because "may administer" really
//! is the same answer for both.

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
use stratum_control::invites;
use stratum_control::members::{self, MemberView, Role};

#[derive(Deserialize)]
pub struct InviteBody {
    pub email: String,
    pub role: String,
}

#[derive(Deserialize)]
pub struct RoleBody {
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

fn member_json(m: &MemberView) -> serde_json::Value {
    serde_json::json!({
        "user_id": m.user_id,
        "email": m.email,
        "name": m.name,
        "role": m.role.as_str(),
        "disabled": m.disabled,
        "created_at": m.created_at,
    })
}

pub async fn list(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    // Reading the roster is a read, not an administrative act: a member
    // needs to know who else is here.
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        return r;
    }
    match members::list(&state.db, &org.id) {
        Ok(ms) => Json(serde_json::json!({
            "members": ms.iter().map(member_json).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

pub async fn set_role(
    State(state): State<SharedState>,
    Path((org_name, user_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<RoleBody>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
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
    match members::set_role(&state.db, &org.id, &user_id, role, Some(&actx)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authx::not_found(),
        // The last-owner guard reports as a conflict: the request was
        // well-formed and authorized, the org's state forbids it.
        Err(e) => json_error(StatusCode::CONFLICT, e),
    }
}

pub async fn remove(
    State(state): State<SharedState>,
    Path((org_name, user_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match members::remove(&state.db, &org.id, &user_id, Some(&actx)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authx::not_found(),
        Err(e) => json_error(StatusCode::CONFLICT, e),
    }
}

// ------------------------------------------------------------- invites

pub async fn invite(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<InviteBody>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let role = match parse_role(&body.role) {
        Ok(r) => r,
        Err(r) => return r,
    };
    match invites::create(
        &state.db,
        &org.id,
        &body.email,
        role,
        &principal.audit_id(),
        invites::DEFAULT_TTL_SECS,
        Some(&AuditCtx::of(&org.id, Some(&principal))),
    ) {
        Ok((inv, link)) => {
            // Sent, and also returned. A relay that is down, or a
            // deployment that has not configured one, must not stop an
            // admin onboarding somebody — they can still carry the link
            // by hand, which is what they did before mail existed. The
            // response says which of the two happened rather than
            // leaving the admin to guess whether to paste it.
            let mail = crate::mail::templates::invitation(
                &inv.email,
                &org.name,
                inv.role.as_str(),
                &state.public_url,
                &link,
            );
            let delivery = match state.mailer.send(&mail) {
                Ok(()) => serde_json::json!({ "sent": state.mailer.kind() != "null" }),
                Err(e) => {
                    eprintln!("weft: invite mail to {}: {e}", inv.email);
                    serde_json::json!({ "sent": false, "error": e })
                }
            };
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": inv.id,
                    "email": inv.email,
                    "role": inv.role.as_str(),
                    "expires_at": inv.expires_at,
                    "invite_link": link,
                    "mail": delivery,
                })),
            )
                .into_response()
        }
        Err(e) => json_error(StatusCode::BAD_REQUEST, e),
    }
}

pub async fn list_invites(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        return r;
    }
    match invites::list(&state.db, &org.id) {
        Ok(list) => Json(serde_json::json!({
            "invites": list.iter().map(|i| serde_json::json!({
                "id": i.id,
                "email": i.email,
                "role": i.role.as_str(),
                "created_at": i.created_at,
                "expires_at": i.expires_at,
                "accepted_at": i.accepted_at,
            })).collect::<Vec<_>>()
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

pub async fn revoke_invite(
    State(state): State<SharedState>,
    Path((org_name, invite_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match invites::revoke(&state.db, &org.id, &invite_id, Some(&actx)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}

// --------------------------------------------------------- repo grants

pub async fn revoke_repo_grant(
    State(state): State<SharedState>,
    Path((org_name, repo_name, user_id)): Path<(String, String, String)>,
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
    match members::revoke_repo_grant(&state.db, &repo.id, &user_id, Some(&actx)) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}
