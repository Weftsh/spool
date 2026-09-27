//! OWNERS endpoints: who owns a path, and whether a set of approvals
//! would suffice for a diff. Read-only — the verdicts here are previews;
//! landing authority arrives with the change/land endpoints and re-runs
//! the same engine.

use crate::api::{json_error, reads};
use crate::app::SharedState;
use crate::review::owners::{RuleLevel, RuleOutcome};
use crate::review::sufficiency::{self, ApproverSet, PathRequirement, Verdict};
use crate::review::{load, owners, resolve};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use stratum_control::auth::Scope;
use stratum_engine::treediff;

/// Bounded hostile input (I13): a preview asks about people, not a crowd.
const MAX_PREVIEW_APPROVERS: usize = 100;

fn rule_levels_json(chain: &[RuleLevel]) -> Vec<serde_json::Value> {
    chain
        .iter()
        .map(|l| {
            serde_json::json!({
                "dir": l.dir,
                "entries": l.entries.iter().map(|e| e.display()).collect::<Vec<_>>(),
                "noparent": l.noparent,
            })
        })
        .collect()
}

pub(crate) fn verdict_json(v: &Verdict) -> serde_json::Value {
    serde_json::json!({
        "landable": v.landable,
        "explanation": v.explanation,
        "per_path": v.per_path.iter().map(|p| serde_json::json!({
            "path": p.path,
            "satisfied": p.satisfied,
            "owners": p.owners,
            "explanation": p.explanation,
        })).collect::<Vec<_>>(),
    })
}

/// GET /owners?path=<p>&at=<rev> — the effective OWNERS rule for one path.
pub async fn owners(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let Some(path) = params.get("path").cloned() else {
        return json_error(StatusCode::BAD_REQUEST, "path is required");
    };
    if !owners::valid_repo_path(&path) {
        return json_error(StatusCode::BAD_REQUEST, format!("invalid path {path:?}"));
    }
    let at = params.get("at").cloned().unwrap_or_else(|| "HEAD".into());
    let prefix = repo_row.prefix().as_str().to_string();
    let path2 = path.clone();
    let out = reads::with_reader(&state, prefix, move |reader| {
        let commit = reader
            .resolve_rev(&at)?
            .ok_or_else(|| format!("unknown rev {at:?}"))?;
        let files = load::owners_files_for_paths(reader, &commit, std::slice::from_ref(&path2))?;
        Ok((commit, owners::effective_owners(&path2, &files)))
    })
    .await;
    let (commit, outcome) = match out {
        Ok(x) => x,
        Err(e) => return reads::err_to_response(e),
    };
    let (error, rules, resolved) = match &outcome {
        RuleOutcome::Error { dir, line, message } => (
            Some(serde_json::json!({ "dir": dir, "line": line, "message": message })),
            Vec::new(),
            resolve::ResolvedOwners::default(),
        ),
        RuleOutcome::Rules { chain, entries } => {
            let resolved = match resolve::resolve_entries(&state.db, &org_row.id, entries) {
                Ok(r) => r,
                Err(e) => return crate::api::internal(e),
            };
            (None, rule_levels_json(chain), resolved)
        }
    };
    Json(serde_json::json!({
        "path": path,
        "at": commit,
        "rules": rules,
        "error": error,
        "resolved": {
            "users": resolved.users.iter().map(|(email, name)| {
                serde_json::json!({ "email": email, "name": name })
            }).collect::<Vec<_>>(),
            "teams": resolved.teams.iter().map(|(name, n)| {
                serde_json::json!({ "name": name, "member_count": n })
            }).collect::<Vec<_>>(),
            "anyone_with_write": resolved.anyone_with_write,
            "unknown": resolved.unknown,
        },
    }))
    .into_response()
}

/// GET /owners/check?from=<rev>&to=<rev>[&approvers=a@b,c@d] — the changed
/// paths between two revs, each path's requirement, and a preview verdict
/// for the (optional) approver list: "would these approvals suffice?"
pub async fn check(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let (Some(from), Some(to)) = (params.get("from").cloned(), params.get("to").cloned()) else {
        return json_error(StatusCode::BAD_REQUEST, "from and to are required");
    };
    let approver_emails: Vec<String> = params
        .get("approvers")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if approver_emails.len() > MAX_PREVIEW_APPROVERS {
        return json_error(StatusCode::BAD_REQUEST, "too many approvers");
    }
    let prefix = repo_row.prefix().as_str().to_string();
    let out = reads::with_reader(&state, prefix, move |reader| {
        let a = reader
            .resolve_rev(&from)?
            .ok_or_else(|| format!("unknown rev {from:?}"))?;
        let b = reader
            .resolve_rev(&to)?
            .ok_or_else(|| format!("unknown rev {to:?}"))?;
        let paths: Vec<String> = treediff::diff_commits(reader, Some(&a), &b)?
            .into_iter()
            .map(|c| c.path)
            .collect();
        // Rules are read at the `to` commit: the state being landed is
        // the state whose OWNERS govern it.
        let files = load::owners_files_for_paths(reader, &b, &paths)?;
        let outcomes: Vec<(String, RuleOutcome)> = paths
            .iter()
            .map(|p| (p.clone(), owners::effective_owners(p, &files)))
            .collect();
        Ok((a, b, outcomes))
    })
    .await;
    let (from_oid, to_oid, outcomes) = match out {
        Ok(x) => x,
        Err(e) => return reads::err_to_response(e),
    };
    match check_verdict(
        &state,
        &org_row.id,
        &repo_row.id,
        &approver_emails,
        &outcomes,
    ) {
        Ok((verdict, unknown)) => Json(serde_json::json!({
            "from": from_oid,
            "to": to_oid,
            "changed_paths": outcomes.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
            "unknown_approvers": unknown,
            "verdict": verdict_json(&verdict),
        }))
        .into_response(),
        Err(e) => crate::api::internal(e),
    }
}

/// Resolve rule outcomes into the requirements sufficiency judges.
/// Shared with the change endpoints, which feed recorded approvals
/// instead of a preview list.
pub(crate) fn requirements_for_outcomes(
    state: &SharedState,
    org_id: &str,
    outcomes: &[(String, RuleOutcome)],
) -> Result<Vec<PathRequirement>, String> {
    let mut requirements = Vec::new();
    for (path, outcome) in outcomes {
        let (requirement, _) = resolve::requirement_for(&state.db, org_id, outcome)?;
        requirements.push(PathRequirement {
            path: path.clone(),
            requirement,
        });
    }
    Ok(requirements)
}

/// Resolve outcomes + approver emails into a preview verdict.
fn check_verdict(
    state: &SharedState,
    org_id: &str,
    repo_id: &str,
    approver_emails: &[String],
    outcomes: &[(String, RuleOutcome)],
) -> Result<(Verdict, Vec<String>), String> {
    let requirements = requirements_for_outcomes(state, org_id, outcomes)?;
    let (approvers, unknown) = resolve::approvers_from_emails(&state.db, org_id, approver_emails)?;
    let writers = resolve::writer_ids(&state.db, org_id, repo_id)?;
    let set = ApproverSet { approvers, writers };
    Ok((sufficiency::evaluate(&requirements, &set), unknown))
}
