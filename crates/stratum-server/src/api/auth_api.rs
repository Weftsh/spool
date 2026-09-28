//! Sign-in, sign-out, and who-am-I for the dashboard.
//!
//! Sessions live in an HttpOnly cookie. The dashboard previously asked
//! the user to paste an API token into a form and kept it in
//! localStorage, which is readable by any script that ever runs on the
//! page and hands a long-lived org credential to a browser. A session
//! cookie is neither.
//!
//! Bearer tokens are untouched: CI, git and every API client keep working
//! exactly as before. This is an additional way in, not a replacement.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use stratum_control::members::{self, Role};
use stratum_control::sessions;
use stratum_control::users::{self, User};

#[derive(Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct PreviewInviteBody {
    pub invite: String,
}

#[derive(Deserialize)]
pub struct AcceptInviteBody {
    /// The `stinv_…` link from the invitation.
    pub invite: String,
    #[serde(default)]
    pub name: String,
    /// Required only when the invitation is for a new account.
    #[serde(default)]
    pub password: Option<String>,
    /// The new account's handle — the `you` in `/you/repo`. Optional:
    /// left out, it is made from the invited address, and it is ignored
    /// when the invitation is for an account that already has one.
    #[serde(default)]
    pub handle: Option<String>,
}

#[derive(Deserialize)]
pub struct ChangePasswordBody {
    pub current_password: String,
    pub new_password: String,
}

fn user_json(user: &User, memberships: &[(String, String, Role)]) -> serde_json::Value {
    serde_json::json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "created_at": user.created_at,
        // Who this account *is*, publicly.
        //
        // The person-shaped routes are addressed by handle —
        // `/v1/users/:handle/emails` and its siblings — so a client that
        // does not know its own handle cannot ask about itself. It was
        // derivable from `orgs` only by guessing which membership is the
        // personal namespace, and a guess about identity is the wrong
        // shape of answer.
        //
        // `null` for an account that has none, which is a state
        // `admin repair-identities` exists to end but which a
        // deployment predating that fix can still be in.
        "handle": user.handle,
        "orgs": memberships
            .iter()
            .map(|(id, name, role)| serde_json::json!({
                "id": id, "name": name, "role": role.as_str(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// The orgs this person belongs to, with names, for the org switcher.
fn memberships(state: &SharedState, user_id: &str) -> Result<Vec<(String, String, Role)>, String> {
    let org_ids = members::orgs_of(&state.db, user_id)?;
    let mut out = Vec::new();
    for org_id in org_ids {
        let Some(org) = stratum_control::registry::org_by_id(&state.db, &org_id)? else {
            continue;
        };
        if let Some(role) = members::role_of(&state.db, &org_id, user_id)? {
            out.push((org.id, org.name, role));
        }
    }
    Ok(out)
}

/// The cookie carrying a session.
///
/// `HttpOnly` so no script can read it, `SameSite=Lax` so it does not
/// ride cross-site form posts, `Path=/` because the API and the SPA share
/// an origin. `Secure` is set only when the deployment is HTTPS — the
/// local stack is plain HTTP on localhost, and a Secure cookie there
/// would simply never be stored.
pub(crate) fn set_cookie(state: &SharedState, value: &str, ttl_secs: i64) -> String {
    cookie_line(state, sessions::COOKIE, value, ttl_secs)
}

/// One cookie line, with this deployment's `Secure` decision made once.
///
/// Generalized out of [`set_cookie`] when the GitHub sign-in grew an
/// anti-CSRF cookie of its own. Two copies of these flags would be two
/// places for one of them to drift — and the drift that matters is
/// silent: a `Secure` a second copy forgot is a credential that rides
/// plaintext, and one it adds on a plain-HTTP deployment is a cookie the
/// browser simply never stores, which reads as "sign-in is broken" with
/// nothing in any log.
pub(crate) fn cookie_line(state: &SharedState, name: &str, value: &str, ttl_secs: i64) -> String {
    let secure = if state.public_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    format!("{name}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={ttl_secs}{secure}")
}

fn clear_cookie(state: &SharedState) -> String {
    set_cookie(state, "", 0)
}

pub async fn login(State(state): State<SharedState>, Json(body): Json<LoginBody>) -> Response {
    let user = match users::authenticate(&state.db, &body.email, &body.password) {
        Ok(Some(u)) => u,
        // One answer for every failure — unknown address, wrong password,
        // disabled account — so this cannot be used to discover who has
        // an account here.
        Ok(None) => return json_error(StatusCode::UNAUTHORIZED, "invalid email or password"),
        Err(e) => return internal(e),
    };
    let (_, token) = match sessions::create(&state.db, &user.id, sessions::DEFAULT_TTL_SECS) {
        Ok(x) => x,
        Err(e) => return internal(e),
    };
    let orgs = match memberships(&state, &user.id) {
        Ok(o) => o,
        Err(e) => return internal(e),
    };
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            set_cookie(&state, &token, sessions::DEFAULT_TTL_SECS),
        )],
        Json(user_json(&user, &orgs)),
    )
        .into_response()
}

pub async fn logout(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Some(cookie) = authx::session_from_headers(&headers) {
        if let Ok(Some(s)) = sessions::verify(&state.db, cookie) {
            let _ = sessions::revoke(&state.db, &s.id);
        }
    }
    // Clearing the cookie is unconditional: a caller with no valid
    // session still wants the stale one gone from their browser.
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, clear_cookie(&state))],
    )
        .into_response()
}

/// The signed-in person and the orgs they can reach.
pub async fn me(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    let Some(cookie) = authx::session_from_headers(&headers) else {
        return json_error(StatusCode::UNAUTHORIZED, "not signed in");
    };
    let session = match sessions::verify(&state.db, cookie) {
        Ok(Some(s)) => s,
        Ok(None) => return json_error(StatusCode::UNAUTHORIZED, "not signed in"),
        Err(e) => return internal(e),
    };
    let user = match users::by_id(&state.db, &session.user_id) {
        Ok(Some(u)) => u,
        // The session verified against a live user, so this is a torn
        // read rather than a client error.
        Ok(None) => return internal("session user vanished".into()),
        Err(e) => return internal(e),
    };
    let orgs = match memberships(&state, &user.id) {
        Ok(o) => o,
        Err(e) => return internal(e),
    };
    Json(user_json(&user, &orgs)).into_response()
}

/// What an invitation is *for*, before somebody commits to it.
///
/// A person arriving from an email should see which organization they
/// are joining and at what role before they choose a password. Without
/// this the screen can only say "accept your invitation", which asks for
/// a credential in exchange for nothing they can check.
///
/// The token is the credential, so this tells its holder nothing they
/// were not already given — and every failure shape (malformed, unknown,
/// expired, already accepted, wrong secret) is the same 404, exactly as
/// `invites::verify` produces it, so a link cannot be used to probe
/// which invitations exist.
///
/// POST rather than GET, with the token in the body: a token in a path
/// or query lands in every access log and proxy trace between here and
/// the browser.
pub async fn preview_invite(
    State(state): State<SharedState>,
    Json(body): Json<PreviewInviteBody>,
) -> Response {
    let invite = match stratum_control::invites::verify(&state.db, &body.invite) {
        Ok(Some(i)) => i,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "this invitation is not valid"),
        Err(e) => return internal(e),
    };
    let org = match stratum_control::registry::org_by_id(&state.db, &invite.org_id) {
        Ok(Some(o)) => o,
        // An invitation whose org has been deleted is not usable, and
        // says the same thing as any other dead link.
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "this invitation is not valid"),
        Err(e) => return internal(e),
    };
    Json(serde_json::json!({
        "org": org.name,
        "role": invite.role.as_str(),
        "email": invite.email,
        "expires_at": invite.expires_at,
    }))
    .into_response()
}

/// Accept an invitation, creating the account if this is a new person,
/// and sign them straight in — an invite that leaves you at a login form
/// is a worse experience for no security gain.
pub async fn accept_invite(
    State(state): State<SharedState>,
    Json(body): Json<AcceptInviteBody>,
) -> Response {
    use stratum_control::invites::AcceptError;
    let accepted = match stratum_control::invites::accept(
        &state.db,
        &body.invite,
        &body.name,
        body.password.as_deref(),
        body.handle.as_deref(),
    ) {
        Ok(a) => a,
        Err(AcceptError::Refused(e)) => return json_error(StatusCode::BAD_REQUEST, e),
        // The invitation is untouched: the same link accepts with
        // another handle.
        Err(AcceptError::HandleTaken(e)) => return json_error(StatusCode::CONFLICT, e),
        Err(AcceptError::Failed(e)) => return internal(e),
    };
    let user = match users::by_id(&state.db, &accepted.user_id) {
        Ok(Some(u)) => u,
        Ok(None) => return internal("accepted invite has no user".into()),
        Err(e) => return internal(e),
    };
    let (_, token) = match sessions::create(&state.db, &user.id, sessions::DEFAULT_TTL_SECS) {
        Ok(x) => x,
        Err(e) => return internal(e),
    };
    let orgs = match memberships(&state, &user.id) {
        Ok(o) => o,
        Err(e) => return internal(e),
    };
    (
        StatusCode::CREATED,
        [(
            header::SET_COOKIE,
            set_cookie(&state, &token, sessions::DEFAULT_TTL_SECS),
        )],
        Json(user_json(&user, &orgs)),
    )
        .into_response()
}

/// Change your own password. Requires the current one — a hijacked
/// session should not be able to lock the owner out of their account.
pub async fn change_password(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordBody>,
) -> Response {
    let Some(cookie) = authx::session_from_headers(&headers) else {
        return json_error(StatusCode::UNAUTHORIZED, "not signed in");
    };
    let session = match sessions::verify(&state.db, cookie) {
        Ok(Some(s)) => s,
        Ok(None) => return json_error(StatusCode::UNAUTHORIZED, "not signed in"),
        Err(e) => return internal(e),
    };
    let user = match users::by_id(&state.db, &session.user_id) {
        Ok(Some(u)) => u,
        Ok(None) => return internal("session user vanished".into()),
        Err(e) => return internal(e),
    };
    match users::authenticate(&state.db, &user.email, &body.current_password) {
        Ok(Some(_)) => {}
        Ok(None) => return json_error(StatusCode::UNAUTHORIZED, "current password is incorrect"),
        Err(e) => return internal(e),
    }
    if let Err(e) = users::set_password(&state.db, &user.id, &body.new_password) {
        return json_error(StatusCode::BAD_REQUEST, e);
    }
    // Every other session is now suspect: a password change is what
    // someone does after fearing a compromise, so it must end the
    // sessions an attacker might hold. This one survives so the user is
    // not signed out of the tab they just used.
    if let Err(e) = sessions::revoke_all_for_user(&state.db, &user.id) {
        return internal(e);
    }
    let (_, token) = match sessions::create(&state.db, &user.id, sessions::DEFAULT_TTL_SECS) {
        Ok(x) => x,
        Err(e) => return internal(e),
    };
    (
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            set_cookie(&state, &token, sessions::DEFAULT_TTL_SECS),
        )],
    )
        .into_response()
}

// ---------------------------------------------------------------------
// Getting back in.
//
// There is no signing yourself up. A person arrives by invitation from
// somebody already inside, or from an operator's `admin user-create`,
// and either way their address is proved on arrival: the invitation was
// mailed to it, and the operator vouched for it.
// ---------------------------------------------------------------------

#[derive(Deserialize)]
pub struct EmailBody {
    pub email: String,
}

#[derive(Deserialize)]
pub struct ResetBody {
    pub token: String,
    pub new_password: String,
}

/// Attempts allowed per address, and in total, per window.
///
/// Two limits because they stop different things. The per-address one
/// stops one mailbox being flooded by somebody who typed it in a form;
/// the global one stops a script walking an address list and using this
/// server as a spam relay.
///
/// There is deliberately no per-source limit. This server does not see a
/// peer address it can trust — behind any proxy it is the proxy's, and
/// trusting `X-Forwarded-For` because it is usually right is how a
/// per-source limit becomes a header an attacker sets. A limit that can
/// be bypassed by typing is worse than an honest global one.
const MAX_PER_ADDRESS: usize = 3;
const MAX_GLOBAL: usize = 60;
const MAIL_WINDOW: Duration = Duration::from_secs(600);

/// Recent address-mailing attempts. Process-local, like the origin
/// probe's, and honest about it: a fleet of N tasks allows N times this.
/// It limits mail, not access — a reset link is worth nothing to anybody
/// but the holder of the mailbox it went to.
static RECENT_MAIL: Mutex<Option<Vec<(String, Instant)>>> = Mutex::new(None);

fn allow_mail_to(address: &str) -> bool {
    let mut guard = RECENT_MAIL.lock().unwrap_or_else(|e| e.into_inner());
    let seen = guard.get_or_insert_with(Vec::new);
    let now = Instant::now();
    seen.retain(|(_, t)| now.duration_since(*t) < MAIL_WINDOW);
    if seen.len() >= MAX_GLOBAL {
        return false;
    }
    if seen.iter().filter(|(a, _)| a == address).count() >= MAX_PER_ADDRESS {
        return false;
    }
    seen.push((address.to_string(), now));
    true
}

/// The one answer every address-taking endpoint gives.
///
/// Identical for "we just mailed you", "that address has no account",
/// "that account is disabled" and "you have asked too many times".
/// Sign-in already answers this way for the same reason: any difference
/// here is a way to ask whether somebody has an account.
fn mailed_if_it_applies() -> Response {
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "check your email",
            "detail": "If that address can receive mail here, a message is on its way.",
        })),
    )
        .into_response()
}

/// Send, and say so in the log if it fails. The caller's answer never
/// depends on the outcome — it must not, or the response becomes the
/// oracle the uniform body exists to close.
fn send_quietly(state: &SharedState, msg: &crate::mail::Message) {
    if let Err(e) = state.mailer.send(msg) {
        eprintln!("weft: mail to {}: {e}", msg.to);
    }
}

/// Ask for a reset link.
pub async fn forgot_password(
    State(state): State<SharedState>,
    Json(body): Json<EmailBody>,
) -> Response {
    let email = users::normalize_email(&body.email);
    if !users::valid_email(&email) || !allow_mail_to(&email) {
        return mailed_if_it_applies();
    }
    match users::by_email(&state.db, &email) {
        // A disabled account gets no reset link: letting somebody
        // recover an account an operator switched off would undo the
        // switching off.
        Ok(Some(u)) if u.disabled_at.is_none() => {
            match stratum_control::usertokens::issue(
                &state.db,
                &u.id,
                stratum_control::usertokens::Kind::Reset,
            ) {
                Ok(token) => send_quietly(
                    &state,
                    &crate::mail::templates::password_reset(&email, &state.public_url, &token),
                ),
                Err(e) => return internal(e),
            }
        }
        Ok(_) => {}
        Err(e) => return internal(e),
    }
    mailed_if_it_applies()
}

/// Redeem a reset link and set a new password.
///
/// Every other session on the account ends. Whoever asked for this may
/// have done so because somebody else was in the account, and leaving
/// that somebody signed in would make the reset theatre.
pub async fn reset_password(
    State(state): State<SharedState>,
    Json(body): Json<ResetBody>,
) -> Response {
    // Strength first: a refused password must not spend the link, or a
    // typo costs somebody another round through their inbox.
    if let Err(e) = users::check_password_strength(&body.new_password) {
        return json_error(StatusCode::BAD_REQUEST, e);
    }
    let user_id = match stratum_control::usertokens::redeem(
        &state.db,
        &body.token,
        stratum_control::usertokens::Kind::Reset,
    ) {
        Ok(Some(id)) => id,
        Ok(None) => {
            return json_error(
                StatusCode::NOT_FOUND,
                "this reset link is not valid any more",
            )
        }
        Err(e) => return internal(e),
    };
    if let Err(e) = users::set_password(&state.db, &user_id, &body.new_password) {
        return internal(e);
    }
    // Holding the mailed link proves the address. A no-op for an account
    // made since sign-up closed — every one is proved on the way in — and
    // for one left unproved by open sign-up, whose credentials were all
    // taken away, it is how the owner of the mailbox gets it back.
    if let Err(e) = stratum_control::usertokens::mark_verified(&state.db, &user_id) {
        return internal(e);
    }
    if let Err(e) = sessions::revoke_all_for_user(&state.db, &user_id) {
        return internal(e);
    }
    let user = match users::by_id(&state.db, &user_id) {
        Ok(Some(u)) => u,
        Ok(None) => return internal("reset user vanished".into()),
        Err(e) => return internal(e),
    };
    if user.disabled_at.is_some() {
        return json_error(StatusCode::UNAUTHORIZED, "this account is disabled");
    }
    let (_, token) = match sessions::create(&state.db, &user.id, sessions::DEFAULT_TTL_SECS) {
        Ok(x) => x,
        Err(e) => return internal(e),
    };
    let orgs = match memberships(&state, &user.id) {
        Ok(o) => o,
        Err(e) => return internal(e),
    };
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            set_cookie(&state, &token, sessions::DEFAULT_TTL_SECS),
        )],
        Json(user_json(&user, &orgs)),
    )
        .into_response()
}
