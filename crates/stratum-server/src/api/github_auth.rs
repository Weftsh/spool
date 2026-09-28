//! Signing in with GitHub, to an account this server already has.
//!
//! Two routes and no credentials of their own, exactly like the install
//! callback next door in [`crate::api::github_api`] — and for the same
//! reason: the person on the other end is a browser mid-redirect, so
//! every answer here is a redirect back into the dashboard with the
//! outcome in the query string, never a JSON error body.
//!
//! # It never makes an account
//!
//! Accounts on this server are made by invitation or by an operator —
//! there is no signing yourself up, and GitHub is not a way around
//! that. So GitHub can sign somebody in to an account they already
//! have, and nothing more: a GitHub identity that matches nobody here is
//! refused with `noaccount`, and the dashboard tells the person to ask
//! for an invitation.
//!
//! A match is one of two things. A link made on an earlier sign-in,
//! keyed on GitHub's numeric id; or, the first time, the account whose
//! address is the one GitHub reports as the person's **primary** and
//! **verified** address — GitHub has sent a link to it and seen it
//! clicked, and every account here proved its own address when it was
//! made (or, left unproved by a build with open sign-up, has had every
//! credential its maker held taken away). Nothing else GitHub says is
//! trusted.
//!
//! # Why never the login
//!
//! A GitHub login is renameable, and a released one becomes claimable
//! by somebody else. An identity keyed on the name would hand whoever
//! takes it next the account it used to mean, so
//! [`stratum_control::identities`] keys on the numeric id and this
//! module never looks an account up by login at all.

use crate::app::SharedState;
use crate::mail::templates::urlencode;
use crate::mirror::origin::{GithubApp, GithubIdentity, UserAuth, UserAuthError};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use serde::Deserialize;
use stratum_control::identities::{self, GITHUB as PROVIDER};
use stratum_control::ids::token_secret;
use stratum_control::sessions;
use stratum_control::users;
use stratum_control::usertokens;

/// The anti-CSRF state, parked in the browser for the round trip.
///
/// A cookie rather than a row, unlike the install flow's
/// [`stratum_control::installations::start`]. That one has an org to
/// bind the state to and an admin to attribute it to; this one has
/// neither — the person is by definition not signed in yet — so there
/// is nothing to write a row *about*. What the state has to prove is
/// only that the browser finishing the flow is the browser that started
/// it, and a cookie proves exactly that, natively, with no table to
/// expire.
const STATE_COOKIE: &str = "weft_ghauth";

/// Ten minutes, matching the install flow. Long enough to read GitHub's
/// authorization screen and sign in there; short enough that a state
/// left in a browser is not a live credential tomorrow.
const STATE_TTL_SECS: i64 = 600;

/// The anti-CSRF cookie, through `auth_api`'s one cookie builder.
///
/// `SameSite=Lax` comes from there and it has to be: GitHub returns the
/// browser here by top-level navigation, which is the one cross-site
/// case `Lax` still sends cookies on. `Strict` would withhold it and
/// every sign-in would land on `expired`; `None` would attach it to
/// cross-site requests that have nothing to do with this flow.
fn state_cookie(state: &SharedState, value: &str, ttl_secs: i64) -> String {
    crate::api::auth_api::cookie_line(state, STATE_COOKIE, value, ttl_secs)
}

fn state_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == STATE_COOKIE).then(|| v.trim().to_string())
    })
}

/// Where GitHub sends the browser back to. Derived from the deployment's
/// own public URL rather than configured separately, so it cannot drift
/// from the host the person is actually on — but it must also be listed
/// on the App, which is the one step of this that lives outside the
/// repository.
fn callback_url(state: &SharedState) -> String {
    format!("{}/v1/auth/github/callback", state.public_url)
}

fn dashboard(state: &SharedState, outcome: &str) -> String {
    format!("{}/dashboard/?github={outcome}", state.public_url)
}

/// The App and its OAuth client, or neither.
///
/// Handed back together rather than looked up twice. The callback used
/// to read the App again after checking the client, which is an arm
/// that cannot be reached — the client came *from* the App — and an
/// unreachable arm on a security path is worse than no arm: nothing can
/// ever show it still does what it says.
fn user_auth(state: &SharedState) -> Option<(std::sync::Arc<GithubApp>, UserAuth)> {
    let app = state.sync.github_app()?;
    let auth = app.user_auth.clone()?;
    Some((app, auth))
}

/// Send the browser to GitHub's authorization screen.
///
/// Answers a redirect, not a URL in a body: the dashboard's button is an
/// ordinary link, so a browser with no script running can still sign in,
/// and there is no second round trip between the click and the leave.
pub async fn start(State(state): State<SharedState>) -> Response {
    // SSO is the only way in: a linked GitHub account would otherwise
    // let somebody switched off at the company's provider keep signing
    // in. The App itself stays, for mirrors.
    if state.sso_only {
        return Redirect::to(&dashboard(&state, "unavailable")).into_response();
    }
    let Some((_, auth)) = user_auth(&state) else {
        // A deployment with no OAuth client on its App. An operator
        // state rather than a person's, but the person is the one
        // looking at it, so it is said in the dashboard's words.
        return Redirect::to(&dashboard(&state, "unavailable")).into_response();
    };
    let secret = token_secret();
    let url = format!(
        "{}/login/oauth/authorize?client_id={}&state={}&redirect_uri={}",
        auth.oauth_base,
        urlencode(&auth.client_id),
        urlencode(&secret),
        urlencode(&callback_url(&state)),
    );
    (
        StatusCode::SEE_OTHER,
        [
            (
                header::SET_COOKIE,
                state_cookie(&state, &secret, STATE_TTL_SECS),
            ),
            (header::LOCATION, url),
        ],
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct CallbackParams {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// GitHub's own refusal — `access_denied` when somebody presses
    /// Cancel on the authorization screen, which is a choice and not a
    /// failure.
    #[serde(default)]
    pub error: Option<String>,
}

/// Where the sign-in ended, in the one word the dashboard says it in.
///
/// * `ok` — signed in.
/// * `noaccount` — GitHub said who this is, and nobody here is them.
///   Accounts are made by invitation; this is never a way to make one.
/// * `denied` — the person declined at GitHub's screen.
/// * `expired` — the state did not match the browser's cookie, or the
///   code was refused. One answer for both: neither tells the person
///   anything they can act on beyond "start again".
/// * `noemail` — GitHub has no proved primary address for them, or the
///   App may not read addresses. The password path still works.
/// * `disabled` — the account exists and is switched off.
/// * `unavailable` — no OAuth client configured on this server.
/// * `error` — GitHub did not answer, or the control plane failed.
enum Landing {
    SignedIn { user_id: String },
    Refused(&'static str),
}

/// GitHub sends the browser here after the authorization screen.
pub async fn callback(
    State(state): State<SharedState>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    // The state cookie is spent by arriving, whatever the outcome: it
    // authorizes exactly one trip, and leaving it live would let a
    // second one reuse it.
    let clear = state_cookie(&state, "", 0);
    let back = |outcome: &str| {
        (
            StatusCode::SEE_OTHER,
            [
                (header::SET_COOKIE, clear.clone()),
                (header::LOCATION, dashboard(&state, outcome)),
            ],
        )
            .into_response()
    };

    if params.error.is_some() {
        return back("denied");
    }
    if state.sso_only {
        return back("unavailable");
    }
    let Some((app, auth)) = user_auth(&state) else {
        return back("unavailable");
    };

    // Whose browser. Asked before the code is spent, so a mismatch costs
    // the person nothing but the trip.
    //
    // Both halves are required to be **non-empty**, and that is not
    // belt-and-braces. `constant_time_eq(b"", b"")` is true, so without
    // the filters an empty `?state=` against an empty `weft_ghauth=`
    // cookie compares equal — and an attacker who can park an empty
    // cookie could then hand somebody a callback URL that signs them
    // in as whoever the attached code names. A state that proves
    // nothing must never compare equal to another state that proves
    // nothing.
    let presented = params.state.as_deref().filter(|s| !s.is_empty());
    let held = state_from_headers(&headers).filter(|s| !s.is_empty());
    let (Some(presented), Some(held)) = (presented, held) else {
        return back("expired");
    };
    if !sessions::constant_time_eq(presented.as_bytes(), held.as_bytes()) {
        return back("expired");
    }
    let Some(code) = params.code.as_deref().filter(|c| !c.is_empty()) else {
        return back("expired");
    };

    let token = match app.exchange_code(&auth, code) {
        Ok(t) => t,
        Err(UserAuthError::Refused(e)) => {
            eprintln!("weft: github sign-in: token exchange: {e}");
            return back("expired");
        }
        Err(UserAuthError::Unanswered(e)) => {
            eprintln!("weft: github sign-in: token exchange: {e}");
            return back("error");
        }
    };
    let ident = match app.user_identity(&token) {
        Ok(i) => i,
        Err(UserAuthError::Refused(e) | UserAuthError::Unanswered(e)) => {
            // Both are "GitHub did not tell us who this is", and neither
            // is the person's doing.
            eprintln!("weft: github sign-in: identity: {e}");
            return back("error");
        }
    };

    let landing = match resolve(&state, &ident) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("weft: github sign-in: {e}");
            return back("error");
        }
    };
    let user_id = match landing {
        Landing::SignedIn { user_id } => user_id,
        Landing::Refused(outcome) => return back(outcome),
    };

    // A disabled account is refused at the last moment rather than the
    // first, because every path above can reach one and there is exactly
    // one place they all pass through.
    match users::by_id(&state.db, &user_id) {
        Ok(Some(u)) if u.disabled_at.is_none() => {}
        Ok(Some(_)) => return back("disabled"),
        Ok(None) => {
            eprintln!("weft: github sign-in: resolved user vanished");
            return back("error");
        }
        Err(e) => {
            eprintln!("weft: github sign-in: {e}");
            return back("error");
        }
    }

    let (_, session) = match sessions::create(&state.db, &user_id, sessions::DEFAULT_TTL_SECS) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("weft: github sign-in: {e}");
            return back("error");
        }
    };
    (
        StatusCode::SEE_OTHER,
        // Appended, not an array of pairs: axum *inserts* each pair of an
        // array, so a second `Set-Cookie` replaced the first and the state
        // cookie this sign-in spent was never cleared.
        AppendHeaders([
            (header::SET_COOKIE, clear.clone()),
            (
                header::SET_COOKIE,
                crate::api::auth_api::set_cookie(&state, &session, sessions::DEFAULT_TTL_SECS),
            ),
        ]),
        [(header::LOCATION, dashboard(&state, "ok"))],
    )
        .into_response()
}

/// Which account this GitHub identity is, if any.
///
/// The link we already hold beats the address, and the address is only
/// consulted when GitHub has proved it.
fn resolve(state: &SharedState, ident: &GithubIdentity) -> Result<Landing, String> {
    // 1. A link we already hold. Keyed on the numeric id, so a person
    //    who renamed themselves on GitHub since last time still lands on
    //    their own account.
    if let Some(user_id) = identities::user_for(&state.db, PROVIDER, &ident.id)? {
        return Ok(Landing::SignedIn { user_id });
    }

    // 2. An address GitHub has proved, matching an account here.
    let Some(email) = ident.email.clone() else {
        return Ok(Landing::Refused("noemail"));
    };
    if let Some(user) = users::by_email(&state.db, &email)? {
        identities::link(&state.db, &user.id, PROVIDER, &ident.id)?;
        // GitHub has proved the address. A no-op for every account made
        // since sign-up closed; for one left unproved by open sign-up —
        // its maker's credentials already taken away — this is the owner
        // of the mailbox arriving.
        usertokens::mark_verified(&state.db, &user.id)?;
        return Ok(Landing::SignedIn { user_id: user.id });
    }

    // 3. Nobody here. Not an account: those are made by invitation.
    Ok(Landing::Refused("noaccount"))
}
