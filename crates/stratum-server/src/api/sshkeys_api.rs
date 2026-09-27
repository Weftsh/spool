//! SSH key management. Two kinds of key, told apart by who registers it:
//!
//! * A member registering a key with no `token_id` gets a **personal
//!   key**. It authenticates as them, so its authority follows their role
//!   — including per-repo grants — and dies with their membership.
//! * An administrator supplying a `token_id` gets a **deploy key**, which
//!   inherits exactly that token's authority. This is what every key was
//!   before people existed, and it still works unchanged.
//!
//! Revoking the key, the token, the membership or the account cuts SSH
//! access on the next connection; nothing is cached.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::{Principal, Scope};
use stratum_control::sshkeys::{self, SshKey};

#[derive(Deserialize)]
pub struct AddKeyBody {
    /// authorized_keys-style line: `ssh-ed25519 AAAA… [comment]`.
    pub public_key: String,
    /// Deploy keys only: the token this key authenticates as. Omit it to
    /// register a key for yourself.
    #[serde(default)]
    pub token_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

fn key_json(k: &SshKey) -> serde_json::Value {
    serde_json::json!({
        "id": k.id,
        "token_id": k.token_id,
        "user_id": k.user_id,
        "algo": k.algo,
        "public_key": k.pubkey,
        "fingerprint_sha256": k.fingerprint_sha256,
        "label": k.label,
        "created_at": k.created_at,
        "revoked_at": k.revoked_at,
    })
}

fn is_admin(p: &Principal) -> bool {
    p.allows(Scope::OrgAdmin, None)
}

/// Map the control plane's refusals onto status codes. A duplicate key is
/// a conflict; anything the caller could have typed differently is a 400;
/// everything else is ours.
fn add_error(e: String) -> Response {
    if e.contains("already registered") {
        json_error(StatusCode::CONFLICT, e)
    } else if e.contains("no such token")
        || e.contains("token is revoked")
        || e.contains("no such member")
        || e.contains("key type")
        || e.contains("public key")
        || e.contains("base64")
    {
        json_error(StatusCode::BAD_REQUEST, e)
    } else {
        internal(e)
    }
}

pub async fn add(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<AddKeyBody>,
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
    let added = match (&body.token_id, caller.user_id.as_deref()) {
        // A deploy key hands its holder a token's authority outright, so
        // only an administrator may create one.
        (Some(token_id), _) => {
            if !is_admin(&caller) {
                return authx::forbidden("registering a deploy key needs org:admin");
            }
            sshkeys::add(
                &state.db,
                &org.id,
                token_id,
                &body.public_key,
                body.label.as_deref(),
                Some(&actx),
            )
        }
        (None, Some(user_id)) => sshkeys::add_for_user(
            &state.db,
            &org.id,
            user_id,
            &body.public_key,
            body.label.as_deref(),
            Some(&actx),
        ),
        // A service token is nobody, so there is no "yourself" to
        // register a key for.
        (None, None) => {
            return json_error(
                StatusCode::BAD_REQUEST,
                "token_id is required when registering a key without a signed-in user",
            )
        }
    };
    match added {
        // The trail was written in the same transaction, so a 201 here
        // means the key exists *and* is recorded.
        Ok(key) => (StatusCode::CREATED, Json(key_json(&key))).into_response(),
        Err(e) => add_error(e),
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
        (true, _) => sshkeys::list(&state.db, &org.id),
        (false, Some(user_id)) => sshkeys::list_for_user(&state.db, user_id),
        (false, None) => Ok(Vec::new()),
    };
    match listed {
        Ok(keys) => Json(serde_json::json!({
            "keys": keys.iter().map(key_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

pub async fn revoke(
    State(state): State<SharedState>,
    Path((org_name, key_id)): Path<(String, String)>,
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
        (true, _) => sshkeys::revoke(&state.db, &org.id, &key_id, Some(&actx)),
        (false, Some(user_id)) => sshkeys::revoke_own(&state.db, &key_id, user_id, Some(&actx)),
        (false, None) => Ok(false),
    };
    match revoked {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        // Somebody else's key is masked, not refused.
        Ok(false) => authx::not_found(),
        Err(e) => internal(e),
    }
}
