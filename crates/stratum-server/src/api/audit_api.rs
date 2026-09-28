//! Audit query API (R7): filterable, paginated, org-scoped.
//!
//! Org-scoped is the whole contract, and it used to be broken. This
//! endpoint asks for `org:read`, and a *repo-bound* token deliberately
//! satisfies org-level `org:read` — that is what lets a per-repo CI token
//! read its repo's metadata. The two together handed a token scoped to
//! one repo the entire org's trail, `token.mint` records included. A
//! repo-bound principal now only ever sees its own repo's entries.

use crate::api::internal;
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use stratum_control::audit::{self, AuditQuery};
use stratum_control::auth::Scope;
use stratum_control::registry;

pub async fn query(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_masked(&state, &headers, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgRead) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let csv = params.get("format").map(String::as_str) == Some("csv");
    // Repo filter accepts a repo *name* and resolves it org-scoped.
    let asked = match params.get("repo") {
        None => None,
        Some(name) => match registry::repo_by_name(&state.db, &org.id, name) {
            Ok(Some(r)) => Some(r.id),
            // No such repo, so nothing to show — answered in whichever
            // shape was asked for. A CSV export that quietly downloads a
            // JSON body named `.csv` is a worse answer than an empty one.
            Ok(None) if csv => return csv_response(&[]),
            Ok(None) => {
                return Json(serde_json::json!({
                    "entries": [], "next_after": null, "next_before": null,
                }))
                .into_response()
            }
            Err(e) => return internal(e),
        },
    };
    let repo_id = match (&principal.repo_id, asked) {
        // A repo-bound credential sees its own repo and nothing else,
        // whether or not it asked for a filter.
        (Some(bound), None) => Some(bound.clone()),
        (Some(bound), Some(asked)) if *bound == asked => Some(asked),
        (Some(_), Some(_)) => return authx::not_found(),
        (None, asked) => asked,
    };
    // Oldest-first by default, because that is what the shipper and every
    // existing caller expect. `order=desc` is what a person reading an
    // activity feed asks for, and it pages with `before` rather than
    // `after` — same rows, read from the other end.
    let newest_first = params.get("order").map(String::as_str) == Some("desc");
    let q = AuditQuery {
        repo_id: repo_id.as_deref(),
        principal: params.get("principal").map(String::as_str),
        user_id: params.get("user").map(String::as_str),
        action: params.get("action").map(String::as_str),
        since_ms: params.get("since").and_then(|s| s.parse().ok()),
        until_ms: params.get("until").and_then(|s| s.parse().ok()),
        after_seq: params.get("after").and_then(|s| s.parse().ok()),
        before_seq: params.get("before").and_then(|s| s.parse().ok()),
        newest_first,
        limit: params
            .get("limit")
            .and_then(|l| l.parse().ok())
            .unwrap_or(100),
    };
    match audit::query(&state.db, &org.id, &q) {
        Ok(entries) => {
            if csv {
                return csv_response(&entries);
            }
            // The cursor names the end the next page continues from, so
            // it follows the order: forwards is `after`, backwards is
            // `before`. The unused one stays null rather than absent, so
            // the response shape does not change with the order.
            let edge = entries.last().map(|e| e.seq);
            let (next_after, next_before) = if newest_first {
                (None, edge)
            } else {
                (edge, None)
            };
            Json(serde_json::json!({
                "entries": entries,
                "next_after": next_after,
                "next_before": next_before,
            }))
            .into_response()
        }
        Err(e) => internal(e),
    }
}

/// The same rows, for a spreadsheet or a ticket. Every field is quoted
/// and inner quotes are doubled: an action's context blob is JSON, which
/// is full of commas and quotes, and a CSV that splits on them is worse
/// than no CSV.
fn csv_response(entries: &[audit::AuditEntry]) -> Response {
    let mut csv = String::from("seq,at,action,principal,user_email,repo_id,context\n");
    for e in entries {
        let cell = |v: &str| format!("\"{}\"", v.replace('"', "\"\""));
        csv.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            e.seq,
            e.at,
            cell(&e.action),
            cell(&e.principal),
            cell(e.user_email.as_deref().unwrap_or("")),
            cell(e.repo_id.as_deref().unwrap_or("")),
            cell(
                &e.context
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            ),
        ));
    }
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/csv")], csv).into_response()
}
