//! `GET …/site` — what this repository publishes, and where.
//!
//! The route exists because without it the feature is undiscoverable.
//! Publishing is driven entirely by a committed file, so nothing in the
//! product would otherwise tell an author the address their site is now
//! at, and a hosting feature whose URL you have to guess is not one.
//!
//! It reports the config **as it parses right now**, not as it parsed
//! when the last deploy went out. Those differ exactly when somebody has
//! just broken the file, which is the moment the answer matters: the
//! site keeps serving the last good deploy, and this says why nothing
//! new has appeared.

use crate::api::internal;
use crate::app::SharedState;
use crate::workflow::read::{self, SiteFile};
use crate::workflow::siteconfig;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use stratum_control::auth::Scope;
use stratum_control::sites;

/// The most deploys reported. History is for orientation, not an
/// archive; a site that has published ten thousand times does not owe
/// the dashboard all of them.
const MAX_DEPLOYS: i64 = 20;

fn deploy_json(d: &sites::Deploy) -> serde_json::Value {
    serde_json::json!({
        "id": d.id,
        "commit": d.commit_oid,
        "tree": d.tree_oid,
        "publish": d.publish,
        "spa": d.spa,
        "not_found": d.not_found,
        "created_at": d.created_at,
    })
}

pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };

    // The config, read fresh. A refusal is reported rather than hidden:
    // it is the answer to "why has my site not updated".
    let prefix = repo_row.prefix().as_str().to_string();
    let head = format!("refs/heads/{}", repo_row.default_branch);
    let cfg = crate::api::reads::with_reader(&state, prefix, move |r| {
        Ok(match read::read_config(r, &head)? {
            SiteFile::NoRev | SiteFile::Absent => ConfigState::Absent,
            SiteFile::Present(name, src) => match siteconfig::parse(&src) {
                Ok(c) => ConfigState::Ok(Box::new(c)),
                Err(refusal) => ConfigState::Refused(
                    refusal.render(&format!("{}/{name}", crate::workflow::read::DIR)),
                ),
            },
        })
    })
    .await;
    // A store hiccup must not read as "you have no site config".
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => return internal(e),
    };

    let site = match sites::get(&state.db, &repo_row.id) {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let deploys = match &site {
        Some(_) => match sites::deploys(&state.db, &repo_row.id, MAX_DEPLOYS) {
            Ok(d) => d,
            Err(e) => return internal(e),
        },
        None => Vec::new(),
    };
    let current = site.as_ref().and_then(|s| s.current.clone());

    // The URL is only real if this deployment actually hosts sites.
    // Reporting one that resolves nowhere would be worse than reporting
    // none: somebody would send it to a colleague.
    let url = site.as_ref().and_then(|s| {
        state
            .sites_domain
            .as_ref()
            .map(|d| format!("https://{}.{d}", s.host))
    });

    let (config_state, config_error, config) = match &cfg {
        ConfigState::Absent => ("absent", None, None),
        ConfigState::Refused(msg) => ("refused", Some(msg.clone()), None),
        ConfigState::Ok(c) => (
            "ok",
            None,
            Some(serde_json::json!({
                "publish": c.publish,
                "branch": c.branch,
                "spa": c.spa,
                "not_found": c.not_found,
            })),
        ),
    };

    Json(serde_json::json!({
        "enabled": site.is_some(),
        "host": site.as_ref().map(|s| s.host.clone()),
        "url": url,
        "branch": site.as_ref().and_then(|s| s.branch.clone()),
        "config_state": config_state,
        "config_error": config_error,
        "config": config,
        "current": current,
        "deploys": deploys.iter().map(deploy_json).collect::<Vec<_>>(),
    }))
    .into_response()
}

enum ConfigState {
    Absent,
    Refused(String),
    Ok(Box<siteconfig::SiteConfig>),
}
