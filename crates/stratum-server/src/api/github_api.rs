//! Connecting GitHub once, and picking a repository from a list.
//!
//! The mirror flow used to ask a person for an `installation_id` — an
//! opaque number they had to find in a GitHub settings URL — and for a
//! `owner/name` that had to match it, with no way to check either until
//! the first sync failed minutes later. These routes replace that with
//! the round trip GitHub is designed for: send them to install the App,
//! take the callback, and then *ask the installation* what it can read.
//!
//! # The callback is the security boundary
//!
//! `GET /v1/github/setup` is a URL GitHub redirects a browser to, so it
//! is a URL anybody can construct, and it answers two questions that
//! must be answered by two different proofs:
//!
//! * **Which org?** The `state`: minted per flow by an org admin,
//!   random, stored only as a hash, single-use and expiring. Without one
//!   — GitHub's redirect after an installation is *edited* carries none,
//!   and an install begun on GitHub's side never had one — the person's
//!   own session stands in, if they began exactly one connect recently.
//! * **Whose installation?** The `code` GitHub appends when the App
//!   requests user authorization during installation. It is exchanged
//!   for that person's token and `GET /user/installations` says what
//!   they control. Without this, the state proved the org and nothing
//!   proved the installation: any org admin could mint a state for their
//!   own org and arrive with *another customer's* installation id —
//!   they are sequential integers — and mirror that customer's private
//!   repositories. Found reviewing the flow after the first real install
//!   on weft.sh; `connect_e2e::a_state_for_your_org_does_not_bind_somebody_elses_installation`
//!   fails without it. A deployment whose App has no OAuth client falls
//!   back to trusting the id, which is fine for a private single-tenant
//!   App and is what the terraform precondition refuses on a fleet.
//!
//! See [`stratum_control::installations`].
//!
//! Everything downstream then asks [`installations::org_may_use`] before
//! touching an installation id, because an id the caller supplies is
//! otherwise a bearer token to somebody else's source: the App will mint
//! a token for any installation it is asked about.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use crate::mirror::origin::UserAuthError;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::installations;

const PROVIDER: &str = "github";

/// Which App a request is about: its handle, its provider name, and
/// where to send a person to install it.
struct Target {
    app: Option<Arc<crate::mirror::origin::GithubApp>>,
    provider: &'static str,
    install_url: Option<String>,
}

fn target(state: &SharedState) -> Target {
    Target {
        app: state.sync.github_app(),
        provider: PROVIDER,
        install_url: state.github_install_url.clone(),
    }
}

/// Begin the install round trip.
///
/// Answers the URL to send the person to, with the state already in it.
/// The state is returned separately as well, so a client that would
/// rather build its own link cannot get the two out of step.
pub async fn start_install(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let target = target(&state);
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    // Connecting an account to this org is an org-level act, not a
    // per-repo one: whoever does it decides what every future mirror
    // here may read.
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(r) = authx::require_verified(&state.db, &principal) {
        return r;
    }
    let Some(install_url) = target.install_url.clone() else {
        return json_error(
            StatusCode::NOT_IMPLEMENTED,
            "this server has no GitHub App configured — set STRATUM_GITHUB_INSTALL_URL",
        );
    };
    let token = match installations::start(
        &state.db,
        &org.id,
        target.provider,
        principal.user_id.as_deref(),
    ) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let joiner = if install_url.contains('?') { '&' } else { '?' };
    Json(serde_json::json!({
        "url": format!("{install_url}{joiner}state={token}"),
        "state": token,
        "expires_in": installations::STATE_TTL_SECS,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct SetupParams {
    #[serde(default)]
    pub installation_id: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// GitHub's user-authorization code, present when the App requests
    /// it during installation. Spent on the first exchange.
    #[serde(default)]
    pub code: Option<String>,
}

/// GitHub sends the browser here after an install.
///
/// This is a browser landing, not an API call: it answers with a
/// redirect back into the dashboard either way, because the person on
/// the other end is looking at a page and a JSON error body is not an
/// answer for them. What went wrong rides in the query string so the
/// dashboard can say it in words:
///
/// * `ok` — bound; `org` names which.
/// * `notyours` — GitHub did not confirm the person controls this
///   installation (no `code`, a spent one, or an installation that is
///   not in their list). Nothing is bound.
/// * `expired` — the `state` is not live: malformed, unknown, spent,
///   expired, another provider's. One answer for all of them.
/// * `claim` — no `state`, and no single pending connect to stand in
///   for it: the installation is proved the person's and parked, and
///   the dashboard asks which org after sign-in. This is the path an
///   install that begins on GitHub takes.
/// * `missing` — no installation id at all.
/// * `taken` — another org here already holds this installation.
/// * `error` — the control plane failed.
pub async fn setup_callback(
    State(state): State<SharedState>,
    Query(params): Query<SetupParams>,
    headers: HeaderMap,
) -> Response {
    callback(state, params, headers).await
}

async fn callback(state: SharedState, params: SetupParams, headers: HeaderMap) -> Response {
    let dash = format!("{}/dashboard/", state.public_url);
    let back = |outcome: &str| Redirect::to(&format!("{dash}?connect={outcome}")).into_response();
    let Some(installation_id) = params.installation_id else {
        return back("missing");
    };
    if !installations::plausible_id(&installation_id) {
        return back("missing");
    }

    let target = target(&state);
    let provider = target.provider;

    // Whose installation. Asked first, before any state is spent, so a
    // refusal here costs the person nothing but the trip.
    if let Some(app) = target.app.as_ref() {
        if let Some(auth) = app.user_auth.as_ref() {
            let Some(code) = params.code.as_deref().filter(|c| !c.is_empty()) else {
                return back("notyours");
            };
            match app.installations_of_user(auth, code) {
                Ok(theirs) if theirs.iter().any(|id| id == &installation_id) => {}
                Ok(_) => return back("notyours"),
                Err(UserAuthError::Refused(e)) => {
                    eprintln!("weft: install callback: user authorization: {e}");
                    return back("notyours");
                }
                // GitHub did not answer. Nothing was learned about the
                // person, so nothing is bound — and the state is not
                // spent, so the same redirect works once GitHub does.
                Err(UserAuthError::Unanswered(e)) => {
                    eprintln!("weft: install callback: user authorization: {e}");
                    return back("error");
                }
            }
        }
    }

    // Which org. The state when there is one; the person's own pending
    // connect when there is not.
    let org_id = match params.state.as_deref().filter(|s| !s.is_empty()) {
        Some(presented) => match installations::spend(&state.db, presented, provider) {
            Ok(Some(id)) => id,
            // Malformed, unknown, replayed, expired, or somebody else's:
            // one answer for all of them.
            Ok(None) => return back("expired"),
            Err(e) => {
                eprintln!("weft: install callback: {e}");
                return back("error");
            }
        },
        None => {
            // The person's own pending connect, when they have exactly
            // one. Anything else — nobody signed in, no connect begun,
            // connects begun from several orgs — is parked: the
            // installation has been proved theirs above, and what is
            // missing is only the org, which the dashboard asks for
            // after sign-in. This is how an install that starts on
            // GitHub (the Marketplace listing, the App's public page)
            // reaches an org at all; it used to dead-end on `missing`.
            let pending = match signed_in_user(&state, &headers) {
                Some(user_id) => match installations::pending_for(&state.db, &user_id, provider) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("weft: install callback: {e}");
                        return back("error");
                    }
                },
                None => Vec::new(),
            };
            match pending.as_slice() {
                [(state_id, _)] => match installations::spend_id(&state.db, state_id, provider) {
                    Ok(Some(id)) => id,
                    Ok(None) => return back("expired"),
                    Err(e) => {
                        eprintln!("weft: install callback: {e}");
                        return back("error");
                    }
                },
                _ => {
                    let account = account_of(target.app.as_deref(), &installation_id);
                    let claim = match installations::park(
                        &state.db,
                        provider,
                        &installation_id,
                        account.as_deref(),
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("weft: install callback: {e}");
                            return back("error");
                        }
                    };
                    return (
                        [(
                            header::SET_COOKIE,
                            claim_cookie(&state, &claim, installations::CLAIM_TTL_SECS),
                        )],
                        Redirect::to(&format!("{dash}?connect=claim")),
                    )
                        .into_response();
                }
            }
        }
    };

    // The account name is a convenience — it makes two installations
    // distinguishable to a person — so failing to learn it must not fail
    // the connection.
    let account = account_of(target.app.as_deref(), &installation_id);
    if let Err(e) = installations::bind(
        &state.db,
        &org_id,
        provider,
        &installation_id,
        account.as_deref(),
        None,
    ) {
        // The unique index refusing means another org already holds this
        // installation. That is the defence working, and the person
        // needs to be told something they can act on.
        eprintln!("weft: install callback could not bind: {e}");
        return back("taken");
    }
    let ctx = AuditCtx::of(&org_id, None);
    crate::api::record_or_warn(
        &state.db,
        &ctx,
        None,
        "github.connect",
        Some(&serde_json::json!({
            "installation_id": installation_id,
            "account": account,
        })),
    );
    let org_name = crate::app::org_name_of(&state, &org_id).unwrap_or_default();
    Redirect::to(&format!("{dash}?connect=ok&org={org_name}")).into_response()
}

/// The cookie carrying a parked installation's claim secret.
///
/// Its own cookie rather than a query parameter: the secret must reach
/// the dashboard and survive a sign-in or sign-up round trip, and a URL
/// is copied, logged and shared in ways a cookie is not. Same flags as
/// the session cookie, for the same reasons.
const CLAIM_COOKIE: &str = "weft_install";

fn claim_cookie(state: &SharedState, value: &str, ttl_secs: i64) -> String {
    let secure = if state.public_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    format!("{CLAIM_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={ttl_secs}{secure}")
}

fn claim_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|kv| kv.strip_prefix(&format!("{CLAIM_COOKIE}=")))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// What the browser's parked installation is, if it has one that is
/// still live. The dashboard asks this on `?connect=claim` to say whose
/// installation it is about to connect.
pub async fn pending_install(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    let Some(claim) = claim_from_headers(&headers) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "no installation is waiting to be connected",
        );
    };
    match installations::peek(&state.db, &claim) {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => json_error(
            StatusCode::NOT_FOUND,
            "no installation is waiting to be connected",
        ),
        Err(e) => internal(e),
    }
}

/// Connect the browser's parked installation to this org.
///
/// The claim cookie is the proof the installation is the person's: it
/// was minted by the callback only after GitHub confirmed that. What is
/// proved here is the org half — org:admin, verified — and the bind is
/// the same one the callback makes.
pub async fn claim_install(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(r) = authx::require_verified(&state.db, &principal) {
        return r;
    }
    let Some(claim) = claim_from_headers(&headers) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "no installation is waiting to be connected",
        );
    };
    let pending = match installations::claim(&state.db, &claim) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return json_error(
                StatusCode::NOT_FOUND,
                "no installation is waiting to be connected",
            )
        }
        Err(e) => return internal(e),
    };
    if let Err(e) = installations::bind(
        &state.db,
        &org.id,
        &pending.provider,
        &pending.installation_id,
        pending.account.as_deref(),
        principal.user_id.as_deref(),
    ) {
        eprintln!("weft: install claim could not bind: {e}");
        return json_error(
            StatusCode::CONFLICT,
            "that GitHub installation is already connected to another organization here",
        );
    }
    let ctx = AuditCtx::of(&org.id, Some(&principal));
    crate::api::record_or_warn(
        &state.db,
        &ctx,
        None,
        "github.connect",
        Some(&serde_json::json!({
            "installation_id": pending.installation_id,
            "account": pending.account,
            "provider": pending.provider,
        })),
    );
    let app = "github";
    (
        [(header::SET_COOKIE, claim_cookie(&state, "", 0))],
        Json(serde_json::json!({
            "org": org_name,
            "installation_id": pending.installation_id,
            "account": pending.account,
            "app": app,
        })),
    )
        .into_response()
}

/// The person behind a browser landing, if any — the session cookie,
/// verified. A token has no browser and never arrives here.
fn signed_in_user(state: &SharedState, headers: &HeaderMap) -> Option<String> {
    let cookie = authx::session_from_headers(headers)?;
    stratum_control::sessions::verify(&state.db, cookie)
        .ok()
        .flatten()
        .map(|s| s.user_id)
}

/// Every installation this org has connected.
pub async fn list_installations(
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
    let target = target(&state);
    let list = match installations::list(&state.db, &org.id, target.provider) {
        Ok(list) => list,
        Err(e) => return internal(e),
    };
    // What each installation may do, asked of GitHub live: forwarding a
    // push needs a permission the App did not always request, and the
    // page has to be able to say "approve this on GitHub" before the
    // first push is refused for want of it. An answer GitHub will not
    // give right now is `null`, not an error — the list is still the
    // list.
    let app = target.app.clone();
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|i| {
            let mut v = serde_json::to_value(i).unwrap_or_default();
            let detail = app
                .as_ref()
                .and_then(|app| match app.installation(&i.installation_id) {
                    Ok(Some(d)) => Some(serde_json::json!({
                        "account": d.account,
                        "target_type": d.target_type,
                        "contents_write": d.contents_write,
                        // Forwarding a push to the origin needs one more,
                        // which every installation made before write-through
                        // mirrors lacks until its owner approves it.
                        "push_ready": d.contents_write,
                        "approve_url": app.approve_url(&d),
                        "suspended": d.suspended,
                    })),
                    Ok(None) => Some(serde_json::json!({ "gone": true })),
                    Err(e) => {
                        eprintln!("weft: installation {} detail: {e}", i.installation_id);
                        None
                    }
                });
            v["detail"] = detail.unwrap_or(serde_json::Value::Null);
            v
        })
        .collect();
    Json(serde_json::json!({ "installations": items })).into_response()
}

#[derive(Deserialize)]
pub struct RepoPage {
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub per_page: Option<u32>,
}

/// What this installation can actually read.
///
/// The whole point of the flow: the person picks from a list instead of
/// typing an id and a name that have to agree.
pub async fn list_installation_repos(
    State(state): State<SharedState>,
    Path((org_name, installation_id)): Path<(String, String)>,
    Query(page): Query<RepoPage>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        return r;
    }
    // An installation id from a caller is a bearer token to somebody's
    // source until this says otherwise. A stranger's installation
    // answers 404, not 403: which installations exist is not this org's
    // business either — and so does one that could never be an id,
    // which is the same answer and never reaches the database.
    if !installations::plausible_id(&installation_id) {
        return json_error(StatusCode::NOT_FOUND, "no such installation");
    }
    match installations::org_may_use(&state.db, &org.id, PROVIDER, &installation_id) {
        Ok(true) => {}
        Ok(false) => return json_error(StatusCode::NOT_FOUND, "no such installation"),
        Err(e) => return internal(e),
    }
    let Some(app) = state.sync.github_app() else {
        return json_error(
            StatusCode::NOT_IMPLEMENTED,
            "this server has no GitHub App configured",
        );
    };
    let (p, pp) = (page.page.unwrap_or(1), page.per_page.unwrap_or(50));
    let out = tokio::task::spawn_blocking(move || app.installation_repos(&installation_id, p, pp))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    match out {
        Ok(repos) => Json(serde_json::json!({ "repositories": repos })).into_response(),
        // Upstream said no, or said nothing. This is not a 500: nothing
        // here is broken, GitHub is not answering, and the person needs
        // to know which of the two it was.
        Err(e) => json_error(StatusCode::BAD_GATEWAY, format!("asking GitHub: {e}")),
    }
}

/// Stop offering an installation here. GitHub keeps it installed — only
/// GitHub can uninstall — so this is a local disconnect.
pub async fn forget_installation(
    State(state): State<SharedState>,
    Path((org_name, installation_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let provider = target(&state).provider;
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let principal = match authx::require(&state.db, &headers, &org.id, None, Scope::OrgAdmin) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !installations::plausible_id(&installation_id) {
        return json_error(StatusCode::NOT_FOUND, "no such installation");
    }
    match installations::unbind(&state.db, &org.id, provider, &installation_id) {
        Ok(true) => {}
        Ok(false) => return json_error(StatusCode::NOT_FOUND, "no such installation"),
        Err(e) => return internal(e),
    }
    let ctx = AuditCtx::of(&org.id, Some(&principal));
    crate::api::record_or_warn(
        &state.db,
        &ctx,
        None,
        "github.disconnect",
        Some(&serde_json::json!({ "installation_id": installation_id })),
    );
    StatusCode::NO_CONTENT.into_response()
}

/// Best-effort account name for an installation.
///
/// Separate so the callback reads as one thing: `None` here costs a
/// nicer label, never the connection.
fn account_of(
    app: Option<&crate::mirror::origin::GithubApp>,
    installation_id: &str,
) -> Option<String> {
    let app = app?;
    let found = app.installations().ok()?;
    found
        .into_iter()
        .find(|(id, _)| id == installation_id)
        .map(|(_, account)| account)
        .filter(|a| !a.is_empty())
}
