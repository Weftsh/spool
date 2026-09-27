//! Signing in with GitHub, and trusting the address GitHub proved.
//!
//! Two routes and no credentials of their own, exactly like the install
//! callback next door in [`crate::api::github_api`] — and for the same
//! reason: the person on the other end is a browser mid-redirect, so
//! every answer here is a redirect back into the dashboard with the
//! outcome in the query string, never a JSON error body.
//!
//! # Why this may skip the confirmation mail
//!
//! Our own sign-up mails a link because an address typed into a form is
//! a claim, not a fact, and everything that costs money is gated on
//! proving it ([`crate::authx::require_verified`]). GitHub has already
//! done that work: `GET /user/emails` reports which addresses it has
//! itself sent a link to and seen clicked. So an account created here
//! is created **already proved** — `users.verified_at` stamped on
//! arrival — and the gate then passes with no change to any of its call
//! sites. That is the entire mechanism, and it is worth being precise
//! about what is being trusted: the `verified` flag, on the `primary`
//! address, and nothing else. An unproved primary lands on the password
//! path with its confirmation mail, unchanged.
//!
//! # The two things that would be an account takeover
//!
//! **Keying on the login.** A GitHub login is renameable, and a
//! released one becomes claimable by somebody else. An identity keyed
//! on the name would hand whoever takes it next the account it used to
//! mean, so [`stratum_control::identities`] keys on the numeric id and
//! this module never looks an account up by login at all.
//!
//! **Adopting a waiting account.** Anyone can sign up with an address
//! they do not own and never confirm it. That account can do nothing —
//! which is the confirmation gate working — but it holds the address
//! with a password its maker knows. If the real owner of the mailbox
//! then arrives here, handing them that account as-is would hand them
//! one the first person can still open. So an account whose address was
//! never proved has its password cleared and its sessions revoked at
//! the moment somebody else proves the address. See
//! [`stratum_control::users::clear_password`].

use crate::app::SharedState;
use crate::mail::templates::urlencode;
use crate::mirror::origin::{GithubApp, GithubIdentity, UserAuth, UserAuthError};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use stratum_control::identities::{self, GITHUB as PROVIDER};
use stratum_control::ids::token_secret;
use stratum_control::profiles;
use stratum_control::registry;
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
/// * `ok` — signed in to an account that already existed.
/// * `new` — signed in to an account created just now, which is the
///   only case with anything left to do: pick repositories to mirror.
/// * `denied` — the person declined at GitHub's screen.
/// * `expired` — the state did not match the browser's cookie, or the
///   code was refused. One answer for both: neither tells the person
///   anything they can act on beyond "start again".
/// * `noemail` — GitHub has no proved primary address for them, or the
///   App may not read addresses. The password path still works.
/// * `emailtaken` — that address already belongs to another account
///   here. Said plainly, unlike sign-up's uniform answer, because GitHub
///   has just proved this person owns the mailbox: they are not probing
///   for somebody else's account, they are locked out of their own.
/// * `disabled` — the account exists and is switched off.
/// * `unavailable` — no OAuth client configured on this server.
/// * `error` — GitHub did not answer, or the control plane failed.
enum Landing {
    SignedIn { user_id: String, fresh: bool },
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
    let (user_id, fresh) = match landing {
        Landing::SignedIn { user_id, fresh } => (user_id, fresh),
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
        [
            (header::SET_COOKIE, clear.clone()),
            (
                header::SET_COOKIE,
                crate::api::auth_api::set_cookie(&state, &session, sessions::DEFAULT_TTL_SECS),
            ),
            (
                header::LOCATION,
                dashboard(&state, if fresh { "new" } else { "ok" }),
            ),
        ],
    )
        .into_response()
}

/// Which account this GitHub identity is, creating one if it is nobody.
///
/// Three steps, in this order, and the order is the security argument:
/// the link we already hold beats the address, and the address is only
/// consulted when GitHub has proved it.
fn resolve(state: &SharedState, ident: &GithubIdentity) -> Result<Landing, String> {
    // 1. A link we already hold. Keyed on the numeric id, so a person
    //    who renamed themselves on GitHub since last time still lands on
    //    their own account.
    if let Some(user_id) = identities::user_for(&state.db, PROVIDER, &ident.id)? {
        return Ok(Landing::SignedIn {
            user_id,
            fresh: false,
        });
    }

    // 2. An address GitHub has proved, matching an account here.
    let Some(email) = ident.email.clone() else {
        return Ok(Landing::Refused("noemail"));
    };
    if let Some(user) = users::by_email(&state.db, &email)? {
        // The pre-hijacking defence. An account that never proved this
        // address does not get to keep a credential once somebody else
        // proves it — see this module's header.
        //
        // Before the link, not after, and the order is the point: these
        // are three statements and any of them can fail. Clearing first
        // fails *closed* — the waiting password is gone and the sign-in
        // is not finished, which the same trip repairs on its next
        // attempt. Linking first would fail *open*: signed in, linked,
        // and the password the defence exists to kill still working,
        // with nothing to say it did not happen.
        if user.verified_at.is_none() {
            users::clear_password(&state.db, &user.id)?;
            sessions::revoke_all_for_user(&state.db, &user.id)?;
        }
        identities::link(&state.db, &user.id, PROVIDER, &ident.id)?;
        // Proved on both sides now. A no-op for an account that had
        // already confirmed.
        usertokens::mark_verified(&state.db, &user.id)?;
        return Ok(Landing::SignedIn {
            user_id: user.id,
            fresh: false,
        });
    }

    // 3. Nobody here. A new account, with no password at all and its
    //    address proved on arrival.
    //
    //    `users.email` said nobody signs in with this address, which is
    //    not the same as nobody holding it: `user_emails` is keyed on
    //    the address platform-wide, so it may be a *secondary* on
    //    somebody's account. Asked rather than discovered from a failed
    //    INSERT, so the person hears the one thing they can act on
    //    instead of a 500. An unproved claim does not lose to GitHub's
    //    proof here — taking an address off another account is a bigger
    //    decision than a sign-in should make on its own.
    if profiles::address_is_held(&state.db, &email)? {
        return Ok(Landing::Refused("emailtaken"));
    }
    let name = ident.name.clone().unwrap_or_else(|| ident.login.clone());
    let user = users::create(&state.db, &email, &name, None)?;
    claim_namespace(state, &user.id, &ident.login)?;
    identities::link(&state.db, &user.id, PROVIDER, &ident.id)?;
    usertokens::mark_verified(&state.db, &user.id)?;
    Ok(Landing::SignedIn {
        user_id: user.id,
        fresh: true,
    })
}

/// A personal namespace for somebody who never typed one.
///
/// Their GitHub login: the name they already answer to, and the one
/// their URLs elsewhere already use. It can be unavailable for two
/// reasons — reserved here (`settings`, `dashboard`) or already
/// somebody else's — and neither is worth stopping a sign-up over, so
/// the fallback is the same name with a random suffix. Somebody who
/// dislikes the result can rename; somebody who never gets an account
/// cannot.
///
/// This must not be skipped on failure. An account with no handle is a
/// *half-made* account and both halves it is missing are silent: it is
/// attributed to nobody, and it has nowhere to put a fork. See
/// [`stratum_control::users::without_handle`].
fn claim_namespace(state: &SharedState, user_id: &str, login: &str) -> Result<(), String> {
    let base = handle_base(login);
    if registry::create_personal_namespace(&state.db, user_id, &base, None).is_ok() {
        return Ok(());
    }
    let alt = format!("{base}-{}", &token_secret()[..6]);
    registry::create_personal_namespace(&state.db, user_id, &alt, None).map(|_| ())
}

/// A GitHub login, as a namespace name this server will accept.
///
/// Pure, and separated from the database call above so it can be tested
/// exhaustively without one. The output is always a legal
/// [`registry::valid_name`]: non-empty, within the length bound, and
/// built from the allowed alphabet — which is what lets the caller treat
/// a refusal from the database as "taken or reserved" rather than
/// having to tell three failures apart.
fn handle_base(login: &str) -> String {
    let cleaned: String = login
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(60)
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == '-' || c == '_');
    if trimmed.is_empty() {
        // GitHub logins cannot actually be empty or all-punctuation, but
        // a name in every URL forever is not the place to find out we
        // were right about somebody else's validation rules.
        "user".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_becomes_a_legal_namespace_name() {
        assert_eq!(handle_base("ada"), "ada");
        // GitHub allows mixed case; namespaces here are lowercase.
        assert_eq!(handle_base("AdaLovelace"), "adalovelace");
        assert_eq!(handle_base("ada-lovelace"), "ada-lovelace");
        // A leading dash or underscore would be a name that reads as a
        // flag in every command line it appears in.
        assert_eq!(handle_base("-ada-"), "ada");
        assert_eq!(handle_base("_ada_"), "ada");
        // Anything outside the alphabet is dropped, never substituted:
        // a dot would make `.` -prefixed names reachable, and `valid_name`
        // refuses those.
        assert_eq!(handle_base("ada.lovelace"), "adalovelace");
        assert_eq!(handle_base("ada/../root"), "adaroot");
        // Length is bounded well inside the 100-byte limit, leaving room
        // for the suffix the caller may add.
        assert_eq!(handle_base(&"a".repeat(200)).len(), 60);
        // Nothing usable left is still a legal name.
        assert_eq!(handle_base("---"), "user");
        assert_eq!(handle_base(""), "user");
        assert_eq!(handle_base("!!!"), "user");
    }

    /// Every output above is one `create_personal_namespace` will accept
    /// on its own terms, which is the property the caller relies on.
    #[test]
    fn every_derived_name_passes_the_registry_rules() {
        for login in [
            "ada",
            "AdaLovelace",
            "-ada-",
            "ada.lovelace",
            "ada/../root",
            "---",
            "",
            &"a".repeat(200),
        ] {
            let base = handle_base(login);
            assert!(
                registry::valid_name(&base),
                "{login:?} derived {base:?}, which the registry refuses"
            );
        }
    }
}
