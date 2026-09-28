//! Single sign-on routes: what the sign-in screen may offer, and the
//! round trip through the company's identity provider.
//!
//! Like GitHub sign-in next door, every answer from the round trip is a
//! redirect back into the dashboard with the outcome in the query string
//! — the person on the other end is a browser mid-redirect, and a JSON
//! error body would be a blank page to them. The outcomes, in the
//! dashboard's words:
//!
//! * `ok` — signed in.
//! * `denied` — the person cancelled at the provider.
//! * `expired` — the round trip does not belong to this browser, took too
//!   long, or its code was refused. One answer for all three: none of them
//!   tells the person anything beyond "start again".
//! * `noemail` — the provider gave no address this server trusts, so there
//!   is nothing to link or make an account by.
//! * `domain` — the address is outside the domains this provider speaks
//!   for.
//! * `disabled` — the account exists and is switched off.
//! * `unavailable` — SSO is not configured here.
//! * `error` — the provider did not answer, answered something that does
//!   not verify, or the control plane failed. The server's log says which.

use crate::app::SharedState;
use crate::oidc::{Exchange, Reject, Trust};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::ids::token_secret;
use stratum_control::{sessions, sso, users};

/// The browser's half of the round trip: `state.nonce.verifier`, all
/// three random and single-use. A cookie for the reason GitHub's flow
/// gives — the person is not signed in yet, so there is nothing to write
/// a row about — and `SameSite=Lax` because the provider returns the
/// browser by a top-level navigation, the one cross-site case `Lax`
/// still sends cookies on.
const COOKIE: &str = "weft_sso";
const TTL_SECS: i64 = 600;

/// `GET /v1/auth/methods` — what the sign-in screen may offer. Public:
/// it describes this server's configuration and nobody's account.
pub async fn methods(State(state): State<SharedState>) -> Response {
    let github = !state.sso_only
        && state
            .sync
            .github_app()
            .is_some_and(|a| a.user_auth.is_some());
    Json(serde_json::json!({
        "password": !state.sso_only,
        "github": github,
        "sso": state.sso.as_ref().map(|s| serde_json::json!({
            "name": s.cfg.name,
            "start": "/v1/auth/sso/start",
        })),
    }))
    .into_response()
}

fn callback_url(state: &SharedState) -> String {
    format!("{}/v1/auth/sso/callback", state.public_url)
}

fn dashboard(state: &SharedState, outcome: &str) -> String {
    format!("{}/dashboard/?sso={outcome}", state.public_url)
}

fn cookie(state: &SharedState, value: &str, ttl: i64) -> String {
    crate::api::auth_api::cookie_line(state, COOKIE, value, ttl)
}

fn held(headers: &HeaderMap) -> Option<(String, String, String)> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    let v = raw.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == COOKIE).then(|| v.trim().to_string())
    })?;
    let mut it = v.split('.');
    let (Some(s), Some(n), Some(p), None) = (it.next(), it.next(), it.next(), it.next()) else {
        return None;
    };
    // Every half must be non-empty: an empty state compares equal to an
    // empty `?state=`, and a cookie that proves nothing must never match
    // a parameter that proves nothing.
    if s.is_empty() || n.is_empty() || p.is_empty() {
        return None;
    }
    Some((s.to_string(), n.to_string(), p.to_string()))
}

/// `GET /v1/auth/sso/start` — send the browser to the provider.
pub async fn start(State(state): State<SharedState>) -> Response {
    let Some(sso) = state.sso.clone() else {
        return Redirect::to(&dashboard(&state, "unavailable")).into_response();
    };
    let d = match sso.discovery() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("weft: sso: discovery: {e}");
            return Redirect::to(&dashboard(&state, "error")).into_response();
        }
    };
    let (st, nonce, verifier) = (token_secret(), token_secret(), token_secret());
    let url = sso.authorize_url(
        &d,
        &callback_url(&state),
        &st,
        &nonce,
        &crate::oidc::pkce_challenge(&verifier),
    );
    (
        StatusCode::SEE_OTHER,
        [
            (
                header::SET_COOKIE,
                cookie(&state, &format!("{st}.{nonce}.{verifier}"), TTL_SECS),
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
    #[serde(default)]
    pub error: Option<String>,
}

/// `GET /v1/auth/sso/callback` — the provider sends the browser here.
pub async fn callback(
    State(state): State<SharedState>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    // The cookie is spent by arriving, whatever the outcome.
    let clear = cookie(&state, "", 0);
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
    let fail = |why: String| {
        eprintln!("weft: sso: {why}");
        back("error")
    };

    if let Some(err) = params.error.as_deref() {
        return back(if err == "access_denied" {
            "denied"
        } else {
            "error"
        });
    }
    let Some(sso) = state.sso.clone() else {
        return back("unavailable");
    };
    let Some((held_state, nonce, verifier)) = held(&headers) else {
        return back("expired");
    };
    let Some(presented) = params.state.as_deref().filter(|s| !s.is_empty()) else {
        return back("expired");
    };
    if !sessions::constant_time_eq(presented.as_bytes(), held_state.as_bytes()) {
        return back("expired");
    }
    let Some(code) = params.code.as_deref().filter(|c| !c.is_empty()) else {
        return back("expired");
    };

    let d = match sso.discovery() {
        Ok(d) => d,
        Err(e) => return fail(format!("discovery: {e}")),
    };
    let tokens = match sso.exchange(&d, code, &callback_url(&state), &verifier) {
        Ok(t) => t,
        Err(Exchange::Refused(e)) => {
            eprintln!("weft: sso: the provider refused the code: {e}");
            return back("expired");
        }
        Err(Exchange::Client(e)) => {
            return fail(format!(
                "the provider refused this server's client credentials ({e}) — \
                 check STRATUM_OIDC_CLIENT_ID and STRATUM_OIDC_CLIENT_SECRET"
            ))
        }
        Err(Exchange::Unanswered(e)) => return fail(format!("token exchange: {e}")),
    };
    let mut claims = match sso.verify(&d, &tokens.id_token, &nonce) {
        Ok(c) => c,
        // A token for another round trip: start again.
        Err(Reject::Nonce) => return back("expired"),
        Err(r) => return fail(format!("the ID token was refused: {r:?}")),
    };
    // Logged now, acted on only if the address turns out to be needed.
    let userinfo = sso
        .complete(&d, tokens.access_token.as_deref(), &mut claims)
        .inspect_err(|e| eprintln!("weft: sso: {e}"))
        .err();

    let email = match crate::oidc::trusted_email(&sso.cfg, &claims) {
        Trust::Email(e) => Some(e),
        Trust::NoEmail => None,
        Trust::Domain => return back("domain"),
    };
    let arrival = sso::Arrival {
        issuer: sso.cfg.issuer.clone(),
        subject: claims.sub.clone(),
        email,
        name: claims.name.clone(),
        preferred_username: claims.preferred_username.clone(),
    };
    let org_id = match sso.org_id(&state.db) {
        Ok(o) => o,
        Err(e) => return fail(e),
    };
    let user_id = match sso::resolve(&state.db, &arrival, &org_id, sso.cfg.role) {
        Ok(Ok(landing)) => landing.user_id().to_string(),
        // Not the person's fault when userinfo was the reason.
        Ok(Err(sso::Refusal::NoEmail)) if userinfo.is_some() => return back("error"),
        Ok(Err(sso::Refusal::NoEmail)) => return back("noemail"),
        Ok(Err(sso::Refusal::NoHandle)) => return fail("no free handle for a newcomer".into()),
        Err(e) => return fail(e),
    };
    // Disabled is checked last, as GitHub's flow does: every path above
    // can reach a disabled account, and this is where they all meet.
    match users::by_id(&state.db, &user_id) {
        Ok(Some(u)) if u.disabled_at.is_none() => {}
        Ok(Some(_)) => return back("disabled"),
        Ok(None) => return fail("the resolved account vanished".into()),
        Err(e) => return fail(e),
    }
    let ttl = sso.cfg.session_ttl_secs;
    let (_, session) = match sessions::create(&state.db, &user_id, ttl) {
        Ok(x) => x,
        Err(e) => return fail(e),
    };
    (
        StatusCode::SEE_OTHER,
        // Appended: an array of pairs is inserted one by one, and the
        // session would replace the spent round-trip cookie's clearing.
        AppendHeaders([
            (header::SET_COOKIE, clear.clone()),
            (
                header::SET_COOKIE,
                crate::api::auth_api::set_cookie(&state, &session, ttl),
            ),
        ]),
        [(header::LOCATION, dashboard(&state, "ok"))],
    )
        .into_response()
}

/// The refusal every password door gives when SSO is the only way in.
pub fn sso_only_refusal(state: &SharedState) -> Option<Response> {
    if !state.sso_only {
        return None;
    }
    let name = state
        .sso
        .as_ref()
        .map(|s| s.cfg.name.clone())
        .unwrap_or_else(|| "SSO".into());
    Some(crate::api::json_error(
        StatusCode::FORBIDDEN,
        format!("this server signs in with {name}, not with a password"),
    ))
}
