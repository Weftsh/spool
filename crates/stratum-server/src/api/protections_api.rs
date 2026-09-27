//! Branch protection API: declare which branches move only through the
//! land queue. Reading the list takes repo read; changing it takes an
//! admin on this repo — protection is the fence around review, and a
//! fence anyone can move is decoration.

use crate::api::{internal, json_error, reads};
use crate::app::SharedState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::protections;

fn protection_json(p: &protections::Protection) -> serde_json::Value {
    serde_json::json!({
        "branch": p.branch,
        "created_at": p.created_at,
    })
}

/// GET /protections — every protected branch, alphabetical.
pub async fn list(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    match protections::list(&state.db, &repo_row.id) {
        Ok(rows) => Json(serde_json::json!({
            "protections": rows.iter().map(protection_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct ProtectBody {
    pub branch: String,
}

/// POST /protections {branch} — protect a branch. Admin only; the branch
/// must exist (protecting a typo would fence off nothing and read as
/// safety). 201 on a new protection, 200 when it was already protected.
pub async fn protect(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ProtectBody>,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::OrgAdmin) {
            Ok(x) => x,
            Err(r) => return r,
        };
    // A mirror's branches belong to its origin; there is nothing here
    // for a queue to defend.
    if repo_row.kind == stratum_control::RepoKind::Mirror {
        return json_error(StatusCode::FORBIDDEN, "mirrors are read-only");
    }
    if !protections::valid_branch(&body.branch) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid branch {:?}", body.branch),
        );
    }
    let prefix = repo_row.prefix().as_str().to_string();
    let branch_ref = format!("refs/heads/{}", body.branch);
    let exists =
        reads::with_reader(&state, prefix, move |reader| reader.ref_oid(&branch_ref)).await;
    match exists {
        Ok(Some(_)) => {}
        Ok(None) => {
            return json_error(
                StatusCode::NOT_FOUND,
                format!("unknown branch {:?}", body.branch),
            )
        }
        Err(e) => return reads::err_to_response(e),
    }
    let actx = AuditCtx::of(&org_row.id, principal.as_ref());
    match protections::protect(&state.db, &repo_row.id, &body.branch, &actx) {
        Ok(new) => {
            let status = if new {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(serde_json::json!({ "branch": body.branch, "protected": true })),
            )
                .into_response()
        }
        // The shape was validated at the door above, so the only error
        // left out of protect() is the database itself.
        Err(e) => internal(e),
    }
}

/// DELETE /protections/*branch — remove a protection. Admin only.
pub async fn unprotect(
    State(state): State<SharedState>,
    Path((org, repo, branch)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::OrgAdmin) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let branch = branch.trim_start_matches('/');
    let actx = AuditCtx::of(&org_row.id, principal.as_ref());
    match protections::unprotect(&state.db, &repo_row.id, branch, &actx) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_error(
            StatusCode::NOT_FOUND,
            format!("branch {branch:?} is not protected"),
        ),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------
// Required checks
//
// Three decisions are baked into what follows, and each one had a
// plausible alternative:
//
// **The path is `/required-checks/*branch`, a sibling of
// `/protections/*branch` rather than a child of it.** The requirement
// is conceptually part of the branch's protection, so `.../protections/
// <branch>/checks` reads better — but it cannot be routed. A branch name
// contains slashes (`release/2.0`), which is why `unprotect` already
// spends the route's one trailing wildcard on it; a wildcard can only be
// the last segment, so `checks` cannot follow the branch, and putting
// `checks` *before* it (`/protections/checks/*branch`) both overlaps the
// existing catch-all and renames the resource to something a reader has
// to decode. So: the same wildcard trick the protections routes already
// use, applied to a resource named for what it holds. A check name may
// itself contain slashes (`ci/tests`), so it cannot be the second half
// of the path either; it travels in the JSON body on POST and as the
// `name` query parameter on DELETE, which is the same information in the
// only place a single-wildcard route leaves for it.
//
// **Requiring a check on an unprotected branch is an error (409), not an
// implicit protection.** A required check only has force through the
// land gate, and the land queue is only the *sole* writer of a branch
// that is protected — on an unprotected branch anyone can push straight
// past the requirement, so the row would be safety theatre that reads,
// in a settings list, exactly like safety. The alternative — protecting
// implicitly — makes one request perform a larger authority move than it
// names, and "I required a check" is not consent to "nobody may push
// here any more". Both calls are admin-only, so the explicit two-step
// costs one request and says what it did.
//
// **Both writes take `Scope::OrgAdmin`, exactly like `protect`.**
// Deciding which checks must pass before trunk moves is the same class
// of decision as deciding that trunk moves only through review; a weaker
// gate here would let anyone with write access delete the requirement
// that constrains them. Reading takes `Scope::RepoRead`, like `list`.
// Mirrors need no separate refusal: `protect` already refuses them, so a
// mirror has no protected branch for a requirement to hang off, and the
// protection check below turns that into the same 409.

fn required_check_json(c: &protections::RequiredCheck) -> serde_json::Value {
    serde_json::json!({
        "name": c.name,
        "created_at": c.created_at,
    })
}

/// The wildcard capture, minus the leading slash some routers keep —
/// `unprotect` trims defensively and these routes match it rather than
/// depending on the router's choice.
fn wildcard_branch(branch: &str) -> &str {
    branch.trim_start_matches('/')
}

/// GET /required-checks/*branch — the checks required on one branch,
/// alphabetical. Repo read: knowing what must pass before your change
/// lands is not privileged information, it is the rules of the road.
///
/// Deliberately *not* gated on the branch being protected. A requirement
/// can outlive the protection it was created under (`unprotect` removes
/// the fence, not the rows), and a list that hid those would hide the
/// reason re-protecting the branch immediately re-gates it.
pub async fn list_required(
    State(state): State<SharedState>,
    Path((org, repo, branch)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let branch = wildcard_branch(&branch);
    if !protections::valid_branch(branch) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid branch {branch:?}"),
        );
    }
    match protections::required_checks(&state.db, &repo_row.id, branch) {
        Ok(rows) => Json(serde_json::json!({
            "branch": branch,
            "required_checks": rows.iter().map(required_check_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct RequireCheckBody {
    pub name: String,
}

/// POST /required-checks/*branch {name} — require a named check on a
/// protected branch. Admin only. 201 on a new requirement, 200 when it
/// was already required, so a settings screen retries safely.
pub async fn require_check(
    State(state): State<SharedState>,
    Path((org, repo, branch)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<RequireCheckBody>,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::OrgAdmin) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let branch = wildcard_branch(&branch).to_string();
    if !protections::valid_branch(&branch) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid branch {branch:?}"),
        );
    }
    // The name rule is the admin-facing one, which is wider than the
    // intake's: mirrored Actions workflows are prose ("Build and test
    // (ubuntu-latest)") and have to be requirable.
    if !protections::valid_required_check_name(&body.name) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid check name {:?}", body.name),
        );
    }
    match protections::is_protected(&state.db, &repo_row.id, &branch) {
        Ok(true) => {}
        Ok(false) => {
            return json_error(
                StatusCode::CONFLICT,
                format!(
                    "branch {branch:?} is not protected: protect it first, \
                     or a required check is one anyone can push past"
                ),
            )
        }
        Err(e) => return internal(e),
    }
    let actx = AuditCtx::of(&org_row.id, principal.as_ref());
    match protections::require_check(&state.db, &repo_row.id, &branch, &body.name, &actx) {
        Ok(new) => {
            let status = if new {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(serde_json::json!({
                    "branch": branch,
                    "name": body.name,
                    "required": true,
                })),
            )
                .into_response()
        }
        // Both shapes were validated at the door, so the only error
        // left out of require_check() is the database itself.
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct UnrequireQuery {
    pub name: String,
}

/// DELETE /required-checks/*branch?name=... — stop requiring a check.
/// Admin only. 204 when one was required, 404 when none was.
///
/// No protection check here, unlike the POST: removal must work on a
/// branch whose fence has since come down, or a stale requirement would
/// be unremovable and would silently come back into force the moment the
/// branch was protected again.
pub async fn unrequire_check(
    State(state): State<SharedState>,
    Path((org, repo, branch)): Path<(String, String, String)>,
    Query(q): Query<UnrequireQuery>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::OrgAdmin) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let branch = wildcard_branch(&branch);
    let actx = AuditCtx::of(&org_row.id, principal.as_ref());
    match protections::unrequire_check(&state.db, &repo_row.id, branch, &q.name, &actx) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        // An unstorable shape can never be present, so it lands here as
        // "not required" rather than as a 400 — the honest answer to
        // "remove this" is the same either way.
        Ok(false) => json_error(
            StatusCode::NOT_FOUND,
            format!("check {:?} is not required on branch {branch:?}", q.name),
        ),
        Err(e) => internal(e),
    }
}
