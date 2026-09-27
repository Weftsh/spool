//! `GET …/workflows` — what `.weft/` asks to have run, at a rev.
//!
//! Read-only, and nothing here starts anything. It exists before the
//! runner does because the two questions a reader has are already worth
//! answering: *what would run when I push this*, and *why is my file
//! wrong*.
//!
//! The second is the one that earns its keep. The workflow subset
//! refuses every key it does not implement, so somebody pasting an
//! Actions workflow will be refused — and being refused by a push, after
//! the fact, with the reason in a log they have to go and find, is a bad
//! way to learn it. This route is where they find out instead, against
//! any ref, before anything runs.

use crate::api::internal;
use crate::api::reads::with_reader;
use crate::app::SharedState;
use crate::workflow::read::{read_dir, stem, DIR};
use crate::workflow::{parse, plan};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::collections::HashMap;
use stratum_control::auth::Scope;

pub async fn list(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let prefix = repo_row.prefix().as_str().to_string();
    let at = params.get("at").cloned().unwrap_or_else(|| "HEAD".into());

    let files = with_reader(&state, prefix, move |reader| read_dir(reader, &at)).await;

    let files = match files {
        Ok(Some(f)) => f,
        Ok(None) => {
            return crate::api::json_error(
                axum::http::StatusCode::NOT_FOUND,
                format!(
                    "unknown rev {:?}",
                    params.get("at").map(String::as_str).unwrap_or("HEAD")
                ),
            )
        }
        Err(e) => return internal(e),
    };

    let mut out: Vec<serde_json::Value> = Vec::new();
    for (name, src) in files {
        let path = format!("{DIR}/{name}");
        // Parse, then plan. A file that parses can still be refused by
        // the planner — an unknown `needs`, a cycle, a matrix too big —
        // and both kinds of refusal read the same way to whoever has to
        // fix them.
        match parse::parse(stem(&name), &src) {
            Err(r) => out.push(serde_json::json!({
                "file": path,
                "ok": false,
                "problems": [refusal_json(&r, &path)],
            })),
            Ok(wf) => match plan::plan(&wf) {
                Err(rs) => out.push(serde_json::json!({
                    "file": path,
                    "ok": false,
                    "name": wf.name,
                    "problems": rs.iter().map(|r| refusal_json(r, &path)).collect::<Vec<_>>(),
                })),
                Ok(p) => out.push(serde_json::json!({
                    "file": path,
                    "ok": true,
                    "name": wf.name,
                    "on": wf.on.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
                    // The expanded jobs, in the order they would be
                    // allowed to start. This is the "what would run"
                    // half, and a matrix is already expanded here so a
                    // reader sees the cells rather than the recipe.
                    "jobs": p.order.iter().map(|&i| {
                        let j = &p.jobs[i];
                        serde_json::json!({
                            "key": j.key,
                            "job": j.job_id,
                            "matrix": j.matrix,
                            "needs": j.needs.iter().map(|&n| p.jobs[n].key.clone()).collect::<Vec<_>>(),
                        })
                    }).collect::<Vec<_>>(),
                })),
            },
        }
    }
    Json(serde_json::json!({ "workflows": out })).into_response()
}

fn refusal_json(r: &crate::workflow::model::Refusal, file: &str) -> serde_json::Value {
    serde_json::json!({
        "line": r.line,
        "key": r.key,
        "message": r.message,
        "hint": r.hint,
        // Pre-rendered as well as structured: every caller wants the
        // sentence, and only some want to lay it out themselves.
        "text": r.render(file),
    })
}
