//! REST API (v1). JSON in/out; errors as `{ "error": "…" }` with the
//! matching status code.

use std::collections::HashMap;

pub mod audit_api;
pub mod auth_api;
pub mod cdn_api;
pub mod change_views_api;
pub mod changes_api;
pub mod changeset_revert;
pub mod changesets_api;
pub mod checks_api;
pub mod checks_intake;
pub mod commits;
pub mod contribs_api;
pub mod exports;
pub mod github_api;
pub mod github_auth;
pub mod imports_api;
pub mod issues_api;
pub mod members_api;
pub mod metrics_api;
pub mod mirrors;
pub mod orgs_api;
pub mod origins_api;
pub mod owners_api;
pub mod profiles_api;
pub mod protections_api;
pub mod reads;
pub mod refops_api;
pub mod repo_meta;
pub mod repos;
pub mod runner_api;
pub mod runners_api;
pub mod search;
pub mod sshkeys_api;
pub mod teams_api;
pub mod tokens;
pub mod watch_api;
pub mod webhooks_api;
pub mod workflow_runs_api;
pub mod workflows_api;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Which **person** is asking, with no namespace in the question.
///
/// Deliberately *not* the principal `rest_repo_auth` returns. That one
/// answers "what authority does this caller have in this org", and for
/// somebody signed in who is not a member it is `None` even though they
/// are plainly a person — correct for authorization, wrong for
/// identity. Starring a public project, filing an issue on one: the
/// most ordinary things a non-member does. Resolving the caller from
/// the org's membership would have meant only insiders could do them.
///
/// Getting this wrong does not announce itself, which is why it is
/// worth a paragraph rather than a line. The failure is not a refusal
/// anybody notices — it is a count, or a tracker, that quietly measures
/// only the people who were already inside.
///
/// A token wins if one is presented, then a browser session — the same
/// order the rest of the API uses. A **repo-bound** token is nobody
/// here: it was minted to reach one repository, and an opinion about a
/// project is not a thing a deploy key holds.
///
/// This lived three times in three modules, byte-identical, twice as
/// `caller_user` and once as `caller_person`. Issues would have been
/// the fourth. Three copies of one rule is three places for it to drift
/// and no way to tell which one is current — and this particular rule
/// decides who counts as a person, which is not a thing to hold three
/// opinions about.
pub fn caller_person(
    state: &crate::app::SharedState,
    headers: &axum::http::HeaderMap,
) -> Result<Option<String>, Response> {
    match crate::authx::principal_opt(&state.db, headers, crate::authx::Challenge::None)? {
        Some(p) => Ok(p.repo_id.is_none().then_some(p.user_id).flatten()),
        None => crate::app::session_user(state, headers),
    }
}

/// Record an action whose failure must not fail the request.
///
/// A push that succeeded is not un-pushed because its trail entry could
/// not be written, and answering 500 would tell the client to retry a
/// thing that already happened. But a dropped entry is a hole in the
/// record, so it is never dropped *silently* — it goes to stderr, where
/// an operator watching for exactly this will find it. Actions that move
/// authority do not come through here: they write their entry inside the
/// same transaction as the change, and fail together with it.
pub fn record_or_warn(
    db: &stratum_control::ControlDb,
    ctx: &stratum_control::audit::AuditCtx,
    repo_id: Option<&str>,
    action: &str,
    context: Option<&serde_json::Value>,
) {
    if let Err(e) = stratum_control::audit::record(db, ctx, repo_id, action, context) {
        eprintln!("weft: audit {action} not recorded: {e}");
    }
}

/// A query-string value, with blank read as absent.
///
/// `?branch=` is what a `<select>` set back to "All branches" submits,
/// and it means the reader cleared the filter. Taking it literally would
/// match `ref_name = ''`, which matches nothing, so the page would empty
/// itself the moment somebody stopped filtering — the exact opposite of
/// what they asked for, with no error to explain it.
///
/// **Here rather than in one handler**, because it was written in
/// `checks_api`, kept private, and then needed by `contribs_api` for the
/// same rail — which got a second copy, and briefly a second rule: a
/// blank `?limit=` was a 400 on one endpoint and a default on the other,
/// so a client that sets every key got different answers from two
/// routes written in the same week. A rule about what a query string
/// means belongs where every route can see it.
pub fn present<'a>(params: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}

/// What a request that asks for a public repository is told.
///
/// Every repository on this server belongs to its organization and is
/// read only by the people it grants. A body that still says
/// `"public": true` — a script written for the hosted edition, an old
/// SDK — is refused in words rather than quietly given a private
/// repository it did not ask for; `"public": false` is what already
/// happens, and is accepted.
pub const NO_PUBLIC_REPOS: &str =
    "this server has no public repositories: every repository is private to its \
     organization — omit \"public\" or set it to false";

/// [`NO_PUBLIC_REPOS`] as a 400, for a body field that asked for one.
pub fn refuse_public(public: Option<bool>) -> Result<(), Response> {
    if public == Some(true) {
        return Err(json_error(StatusCode::BAD_REQUEST, NO_PUBLIC_REPOS));
    }
    Ok(())
}

pub fn json_error(status: StatusCode, msg: impl Into<String>) -> Response {
    (
        status,
        axum::Json(serde_json::json!({ "error": msg.into() })),
    )
        .into_response()
}

/// Map a control/engine error string to a response, defaulting to 500.
pub fn internal(e: String) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, e)
}

#[cfg(test)]
mod tests {
    use stratum_control::audit::AuditCtx;
    use stratum_control::ControlDb;

    /// A dropped audit entry is never dropped silently.
    ///
    /// The warn is the whole point of this function, so it needs a
    /// failure that is real rather than mocked: `audit_log.user_id`
    /// references `users(id)`, so a context naming a person who does not
    /// exist is refused by Postgres itself. The call must return
    /// normally — the caller's write already happened and telling it to
    /// retry would be worse than the missing row.
    #[test]
    fn a_failed_audit_write_warns_instead_of_unwinding() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("api-recordwarn")).unwrap();
        let org = stratum_control::registry::create_org(&db, "acme").unwrap();
        let ctx = AuditCtx {
            principal: "user:01nobody".to_string(),
            user_id: Some("01nobodynobodynobodynobody".to_string()),
            org_id: org.id.clone(),
        };
        super::record_or_warn(&db, &ctx, None, "repo.create", None);

        // Nothing was written, and nothing panicked.
        let entries = stratum_control::audit::query(
            &db,
            &org.id,
            &stratum_control::audit::AuditQuery {
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(entries.is_empty(), "{entries:?}");

        // The same call with a real person does write — otherwise this
        // test would pass against a function that never records at all.
        let user = stratum_control::users::create(
            &db,
            "someone@acme.test",
            "Someone",
            Some("a long enough password"),
        )
        .unwrap();
        let ok = AuditCtx {
            principal: format!("user:{}", user.id),
            user_id: Some(user.id.clone()),
            org_id: org.id.clone(),
        };
        super::record_or_warn(&db, &ok, None, "repo.create", None);
        let entries = stratum_control::audit::query(
            &db,
            &org.id,
            &stratum_control::audit::AuditQuery {
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
    }
}
