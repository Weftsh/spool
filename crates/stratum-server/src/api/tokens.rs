//! Token management (M5): mint (plaintext shown once), list, revoke
//! (instant).
//!
//! Two kinds of token live here and the caller decides which by *who they
//! are*, not by a flag:
//!
//! * A request that names a person mints a **personal access token**. It
//!   is owned by them, capped by their role, and dies with their
//!   membership — the credential a developer puts in a `.netrc`.
//! * A request that names no one (an org service token, which is every
//!   token minted before people existed) mints another service token and
//!   still requires `org:admin`. CI keeps working exactly as before.
//!
//! Listing and revocation follow the same split: an administrator sees
//! and ends anything in the org, a member only their own.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::{self, Principal, Scope, TokenView};
use stratum_control::ids::now_ms;
use stratum_control::members;
use stratum_control::registry;

#[derive(Deserialize)]
pub struct MintBody {
    /// e.g. ["repo:read"], ["repo:write"], ["org:admin"]
    pub scopes: Vec<String>,
    /// Restrict to a single repo by name.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    /// Seconds until this credential stops working. Absent means never,
    /// which is what a personal access token wants and what every token
    /// minted before this field existed already is.
    ///
    /// Seconds-from-now rather than an absolute instant, deliberately: a
    /// caller sending a deadline has to agree with us about the clock,
    /// and the ones that get this wrong send local time, or seconds
    /// where we read millis, and mint a credential that is either
    /// already dead or good for fifty thousand years. A duration has no
    /// such failure.
    #[serde(default)]
    pub expires_in_secs: Option<i64>,
}

/// The longest life a minted credential may be given.
///
/// A cap rather than a policy: a year is far longer than any machine
/// credential should want and short enough that "expires_in_secs" cannot
/// be used to express "never" by accident. `None` is how you say never,
/// and saying it explicitly is the point.
const MAX_TTL_SECS: i64 = 365 * 24 * 60 * 60;

/// Every scope a token can carry, in the order the dashboard shows them.
const SCOPES: [Scope; 4] = [
    Scope::OrgAdmin,
    Scope::OrgRead,
    Scope::RepoRead,
    Scope::RepoWrite,
];

fn parse_scopes(raw: &[String]) -> Result<Vec<Scope>, String> {
    let mut out = Vec::new();
    for s in raw {
        out.push(match s.as_str() {
            "org:admin" => Scope::OrgAdmin,
            "org:read" => Scope::OrgRead,
            "repo:read" => Scope::RepoRead,
            "repo:write" => Scope::RepoWrite,
            other => return Err(format!("unknown scope {other:?}")),
        });
    }
    Ok(out)
}

fn token_json(t: &TokenView) -> serde_json::Value {
    serde_json::json!({
        "id": t.id,
        "label": t.label,
        "scopes": t.scopes,
        "repo_id": t.repo_id,
        "user_id": t.user_id,
        "created_at": t.created_at,
        "revoked_at": t.revoked_at,
        "expires_at": t.expires_at,
    })
}

/// Is this principal acting as an administrator of the org, rather than
/// as a member managing their own credentials?
fn is_admin(p: &Principal) -> bool {
    p.allows(Scope::OrgAdmin, None)
}

pub async fn mint(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MintBody>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    // A member may mint for themselves; only an admin may mint a token
    // that belongs to nobody.
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if caller.user_id.is_none() && !is_admin(&caller) {
        return authx::forbidden("minting an org service token needs org:admin");
    }
    let scopes = match parse_scopes(&body.scopes) {
        Ok(s) => s,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, e),
    };
    // A personal token can never exceed what its holder can actually do
    // somewhere in this org — their org role, or a per-repo grant if one
    // gives them more. Refusing loudly beats silently minting a token
    // that turns out to grant less than it claims.
    if let Some(user_id) = caller.user_id.as_deref() {
        let role = match members::max_role(&state.db, &org.id, user_id) {
            Ok(Some(r)) => r,
            Ok(None) => return authx::not_found(),
            Err(e) => return internal(e),
        };
        if let Some(too_much) = scopes.iter().find(|s| !role.carries(**s)) {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "scope {:?} is more than you can do anywhere in this org ({})",
                    too_much.as_str(),
                    role.as_str()
                ),
            );
        }
    }
    let repo_id = match &body.repo {
        None => None,
        Some(name) => match registry::repo_by_name(&state.db, &org.id, name) {
            Ok(Some(r)) => Some(r.id),
            Ok(None) => return json_error(StatusCode::NOT_FOUND, format!("repo {name:?}")),
            Err(e) => return internal(e),
        },
    };
    // A bounded, positive lifetime or none at all. Zero and negatives are
    // refused rather than clamped: both mean somebody computed a duration
    // and got it wrong, and minting a credential that is dead on arrival
    // would show up as an authentication failure somewhere else entirely.
    let expires_at = match body.expires_in_secs {
        None => None,
        Some(secs) if secs <= 0 => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "expires_in_secs must be positive; omit it for a token that never expires",
            );
        }
        Some(secs) if secs > MAX_TTL_SECS => {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!("expires_in_secs may not exceed {MAX_TTL_SECS} (one year)"),
            );
        }
        Some(secs) => Some(now_ms() + secs * 1000),
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    match auth::mint_for(
        &state.db,
        &org.id,
        &scopes,
        auth::Mint {
            repo_id: repo_id.as_deref(),
            label: body.label.as_deref(),
            user_id: caller.user_id.as_deref(),
            expires_at,
        },
        Some(&actx),
    ) {
        Ok(t) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": t.id,
                // Echoed so a caller can hold it beside the secret
                // rather than recomputing our arithmetic from its own
                // clock — which is the mistake `expires_in_secs` exists
                // to prevent on the way in.
                "expires_at": expires_at,
                // Shown exactly once; only the hash is stored.
                "token": t.plaintext,
            })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
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
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let listed = match (is_admin(&caller), caller.user_id.as_deref()) {
        (true, _) => auth::list(&state.db, &org.id),
        (false, Some(user_id)) => auth::list_for_user(&state.db, &org.id, user_id),
        // A non-admin service token owns nothing, so it sees nothing —
        // rather than the whole org's credential inventory.
        (false, None) => Ok(Vec::new()),
    };
    // What this caller may put on a new token. The dashboard offers
    // exactly these, so it never presents a choice the server will
    // refuse — and a grant that raises what they can do shows up here
    // without the UI needing its own copy of the rule.
    let mintable = match caller.user_id.as_deref() {
        None => SCOPES.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        Some(user_id) => match members::max_role(&state.db, &org.id, user_id) {
            Ok(Some(role)) => SCOPES
                .iter()
                .filter(|s| role.carries(**s))
                .map(|s| s.as_str())
                .collect(),
            Ok(None) => Vec::new(),
            Err(e) => return internal(e),
        },
    };
    match listed {
        Ok(ts) => Json(serde_json::json!({
            "tokens": ts.iter().map(token_json).collect::<Vec<_>>(),
            "mintable_scopes": mintable,
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

pub async fn revoke(
    State(state): State<SharedState>,
    Path((org_name, token_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let caller = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let actx = AuditCtx::of(&org.id, Some(&caller));
    let revoked = match (is_admin(&caller), caller.user_id.as_deref()) {
        (true, _) => auth::revoke(&state.db, &org.id, &token_id, Some(&actx)),
        (false, Some(user_id)) => {
            auth::revoke_own(&state.db, &org.id, &token_id, user_id, Some(&actx))
        }
        (false, None) => Ok(false),
    };
    match revoked {
        // The trail was written in the same transaction, so a 204 here
        // means both happened.
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        // Somebody else's token is masked, not refused: a member must not
        // be able to probe which token ids exist in the org.
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}
