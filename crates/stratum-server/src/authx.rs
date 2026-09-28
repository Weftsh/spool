//! Request authentication: extract a token (Bearer for REST, HTTP Basic
//! for the git wire — either Basic field may carry it), verify against the
//! control database on every request (instant revocation, M5), and answer
//! scope questions.
//!
//! Existence masking: without credentials a private resource answers 401
//! (so git supplies credentials on retry); with valid credentials that
//! lack access it answers 404 — org A can never distinguish "org B's repo
//! exists" from "no such repo" (R8).

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use stratum_control::auth::{Principal, Scope};
use stratum_control::ControlDb;

pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(t) = v.strip_prefix("Bearer ") {
        return Some(t.trim().to_string());
    }
    if let Some(b64) = v.strip_prefix("Basic ") {
        let decoded = base64_decode(b64.trim())?;
        let s = String::from_utf8(decoded).ok()?;
        let (user, pass) = s.split_once(':')?;
        if pass.starts_with("weft_") {
            return Some(pass.to_string());
        }
        if user.starts_with("weft_") {
            return Some(user.to_string());
        }
    }
    None
}

/// Whether a 401 carries an HTTP Basic challenge.
///
/// This is not a style choice, and both answers are load-bearing:
///
/// * the **git wire** needs `WWW-Authenticate: Basic` — it is what makes
///   `git clone` retry with credentials instead of failing outright;
/// * the **REST API must not send it**. A browser that sees a Basic
///   challenge on a same-origin `fetch()` handles the 401 itself and
///   opens its native credential dialog; the promise never settles, so
///   the dashboard's sign-in button sticks on "Checking…" forever with
///   no error. Measured against a real browser — a mocked 401 cannot
///   reproduce it, which is why this survived the e2e suite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Challenge {
    /// Git wire: ask the client to resend with credentials.
    Basic,
    /// REST / browser callers: answer plainly, no dialog.
    None,
}

pub fn unauthorized(challenge: Challenge) -> Response {
    let body = "authentication required\n";
    match challenge {
        Challenge::Basic => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"stratum\"")],
            body,
        )
            .into_response(),
        Challenge::None => (StatusCode::UNAUTHORIZED, body).into_response(),
    }
}

pub fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

/// "You may not have this", said so that it cannot be read as "this does
/// not exist".
///
/// The two answers are chosen by what the caller **proved**, not by what
/// they asked for, and that is what closes the oracle:
///
/// * **No credential that authenticates** → 401. A missing repository
///   and a private one answer identically, so a caller who cannot prove
///   who they are is unable to enumerate a namespace's private work by
///   reading status codes. This is what the git wire has always done.
/// * **A credential that authenticates** → 404, the masking rule (R8).
///   Having authenticated tells you nothing about repositories you
///   cannot reach; a foreign token and a real absence look the same.
///
/// # This took a `db` for a reason, and the reason is a vulnerability
///
/// It used to key off whether a credential was **present**, which is not
/// the same question and is trivially forged. `token_from_headers` only
/// pulls the string out of the header; it does not check that any such
/// token exists. So `Bearer weft_not_a_real_token` counted as "some
/// credential", and the two branches split on nothing:
///
/// * a **private** repository resolved, and `rest_repo_auth` then went on
///   to `principal_opt`, which refuses an unverifiable token with 401;
/// * a repository that **did not exist** never resolved, and this
///   function answered 404 because a token-shaped string was present.
///
/// 401 therefore meant "this exists and you may not have it" and 404
/// meant "this does not exist" — an existence oracle over every private
/// repository in every namespace, on every repo-scoped REST route in the
/// API, available to anybody who can type a header. Not authenticating
/// was strictly *better* than authenticating for an attacker, because a
/// real foreign token gets an honest 404 for both.
///
/// So the question has to be whether the credential authenticates, and
/// answering it needs the database. A caller whose token is nonsense is
/// told their token is nonsense — and told exactly that, for a
/// repository that exists and one that does not alike.
pub fn masked(db: &ControlDb, headers: &HeaderMap) -> Response {
    // A token that resolves to a principal is a credential. One that
    // does not resolve — revoked, typo'd, or invented — is not, and must
    // not buy the 404. A session cookie is the other way in and is
    // checked the same way rather than by presence: an expired or forged
    // cookie is not a credential either.
    //
    // These are the *same* `verify` functions the enforcing path calls —
    // `principal_opt` for the token, `session_principal` for the cookie
    // — and not a reimplementation. That is what makes the two branches
    // agree; a second opinion about what counts as authenticated is a
    // second place for them to diverge, and divergence here is the
    // oracle coming back.
    let authenticated = match token_from_headers(headers) {
        Some(tok) => stratum_control::auth::verify(db, &tok).map(|p| p.is_some()),
        None => match session_from_headers(headers) {
            Some(sid) => stratum_control::sessions::verify(db, sid).map(|s| s.is_some()),
            None => Ok(false),
        },
    };
    match authenticated {
        Ok(true) => not_found(),
        Ok(false) => unauthorized(Challenge::None),
        // A database error is a 500 here **because the enforcing path
        // answers 500**, and matching it is the whole requirement.
        //
        // This swallowed the error and answered 401, on the reasoning
        // that we do not know the caller authenticated so the safe
        // direction is the one that reveals nothing. That is right in
        // isolation and wrong in context. Consider a database that can
        // still answer `repo_by_name` but fails reads of `tokens` — a
        // corrupted index, a `statement_timeout` only the tokens join
        // trips. The repository that does not exist resolves to
        // `Ok(None)`, reaches here, and answers 401. The private
        // repository that *does* exist never reaches here at all: it
        // resolves, goes on to `principal_opt`, and answers 500. 401
        // versus 500 on the same input, and the oracle is back for as
        // long as the condition holds.
        //
        // The reveal-nothing direction is not 401 and not 404. It is
        // whichever answer the sibling path gives, and "we could not
        // tell" is a more honest thing to say than "you are not
        // authenticated" in any case.
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Refuse a caller whose credential does not authenticate, before
/// anything is looked up.
///
/// Every repository is private to its organisation, so there is nothing
/// an anonymous caller may read. A route that masks what a caller may
/// not see as "not found" has to ask this *first*: masking an anonymous
/// caller's 401 into the 404 an absent record gets would tell them to
/// look harder rather than to sign in, and answering 401 only for a
/// record that exists would be the existence oracle [`masked`] is there
/// to close. One answer — the same one `masked` gives — for every name.
pub fn require_authenticated(db: &ControlDb, headers: &HeaderMap) -> Result<(), Response> {
    let answer = masked(db, headers);
    if answer.status() == StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(answer)
    }
}

pub fn forbidden(msg: &str) -> Response {
    (StatusCode::FORBIDDEN, format!("{msg}\n")).into_response()
}

/// The session cookie on this request, if any.
pub fn session_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(stratum_control::sessions::from_cookie_header)
}

/// Resolve a browser session to a principal scoped to one org.
///
/// A session identifies a *person*, not an org — someone may belong to
/// several — so the org comes from the route and the role is looked up
/// per request. That also means removing a member takes effect on their
/// very next request, with no cached authority to outlive it.
pub fn session_principal(
    db: &ControlDb,
    headers: &HeaderMap,
    org_id: &str,
    repo_id: Option<&str>,
) -> Result<SessionAuth, Response> {
    let Some(cookie) = session_from_headers(headers) else {
        return Ok(SessionAuth::None);
    };
    let session = stratum_control::sessions::verify(db, cookie)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e).into_response())?;
    let Some(session) = session else {
        // An expired or revoked cookie is not an error — the browser
        // simply is not signed in, and should be told so once rather
        // than handed a 500.
        return Ok(SessionAuth::None);
    };
    stratum_control::sessions::touch(db, &session.id);
    let role = stratum_control::members::effective_role(db, org_id, repo_id, &session.user_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e).into_response())?;
    Ok(match role {
        Some(r) => {
            SessionAuth::Principal(Principal::for_user(org_id, &session.user_id, r.scopes()))
        }
        None => SessionAuth::NoAccess(session.user_id),
    })
}

/// What a session cookie amounted to on this request.
///
/// The three cases answer differently on purpose. No cookie is a 401 —
/// "sign in". A signed-in person with no role in *this* org is a 404, the
/// same masked answer a foreign API token gets: whether an org exists
/// must not depend on which kind of credential asked.
///
/// `NoAccess` still names the person. On a public repository they are
/// not nobody: they read it as themselves, and what they do there — open
/// a change from their fork, comment on it, tick the files they have
/// read — is attributed to them. Without the id, every seam that wanted
/// to say who had to resolve the cookie a second time, and the ones that
/// did not left the outside contributor anonymous.
pub enum SessionAuth {
    None,
    NoAccess(String),
    Principal(Principal),
}

/// Verify the presented token, if any. Err = a response to return as-is.
pub fn principal_opt(
    db: &ControlDb,
    headers: &HeaderMap,
    challenge: Challenge,
) -> Result<Option<Principal>, Response> {
    match token_from_headers(headers) {
        None => Ok(None),
        Some(tok) => match stratum_control::auth::verify(db, &tok) {
            Ok(Some(p)) => Ok(Some(p)),
            // A presented-but-invalid token is a 401: the caller may have a
            // typo'd or revoked token and must know to fix it.
            Ok(None) => Err(unauthorized(challenge)),
            Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e).into_response()),
        },
    }
}

/// Refuse a person who has not proved their email address.
///
/// The line is "cheap things stay open, things that cost us money do
/// not": an unverified account may sign in, look around and read
/// whatever its role allows, but may not create a repository or a
/// mirror. That is where signup abuse actually lands — an address nobody
/// holds, turned into storage and outbound fetches.
///
/// **Service tokens are exempt.** A token with no `user_id` was minted
/// by somebody who is verified, and every token minted before addresses
/// were proved at all is one of these. CI does not stop working because
/// the person who set it up has not read their email.
pub fn require_verified(
    db: &stratum_control::ControlDb,
    principal: &Principal,
) -> Result<(), axum::response::Response> {
    let Some(user_id) = &principal.user_id else {
        return Ok(());
    };
    match stratum_control::usertokens::is_verified(db, user_id) {
        Ok(true) => Ok(()),
        Ok(false) => Err(crate::api::json_error(
            axum::http::StatusCode::FORBIDDEN,
            "confirm your email address before creating anything — check your \
             inbox for the link we sent, or ask for another one",
        )),
        Err(e) => Err(crate::api::internal(e)),
    }
}

/// Require `need` on (org, optional repo). `org_id` comes from the path
/// lookup; the principal must belong to that org or the resource does not
/// exist for them.
pub fn require(
    db: &ControlDb,
    headers: &HeaderMap,
    org_id: &str,
    repo_id: Option<&str>,
    need: Scope,
) -> Result<Principal, Response> {
    // One seam for every org-level check in the API: a token if one was
    // presented, otherwise the browser session. Doing this here rather
    // than at each of the ~16 call sites is what keeps sessions from
    // becoming a second, divergent way to say "may".
    let p = match principal_opt(db, headers, Challenge::None)? {
        // A session is resolved against the repo already; a token has
        // not met the per-repo grants yet.
        Some(p) => match repo_id {
            Some(repo) => refine(db, p, repo)?,
            None => p,
        },
        None => match session_principal(db, headers, org_id, repo_id)? {
            SessionAuth::Principal(p) => p,
            SessionAuth::NoAccess(_) => return Err(not_found()),
            SessionAuth::None => return Err(unauthorized(Challenge::None)),
        },
    };
    if p.org_id != org_id || !p.allows(need, repo_id) {
        return Err(not_found());
    }
    Ok(p)
}

/// Resolve what a principal may do on one specific repo.
///
/// A per-repo grant replaces the org role on the repo it names, and which
/// repo is being reached is only known after the credential has been
/// verified — so every seam that has resolved a repo passes the principal
/// through here before asking `allows`. A service token belongs to no
/// person, has no grants, and comes back unchanged.
pub fn refine(db: &ControlDb, p: Principal, repo_id: &str) -> Result<Principal, Response> {
    match stratum_control::members::refine_for_repo(db, &p, repo_id) {
        Ok(Some(refined)) => Ok(refined),
        // Their membership ended between authenticating and reaching this
        // repo — an SSH connection authenticates once and can serve much
        // later. Fail closed and masked.
        Ok(None) => Err(not_found()),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e).into_response()),
    }
}

/// Standard base64, no padding required.
///
/// `pub(crate)` because the registry needs it too: an npm publish
/// carries its tarball base64 in `_attachments`. One decoder, so the
/// two doors cannot disagree about what is valid.
pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for &c in s {
        acc = (acc << 6) | val(c)? as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        assert_eq!(base64_decode("dXNlcjpwYXNz").unwrap(), b"user:pass");
        assert_eq!(base64_decode("eDp3ZWZ0X2FfYg==").unwrap(), b"x:weft_a_b");
        assert!(base64_decode("!!!").is_none());
    }

    #[test]
    fn token_extraction_prefers_strat_field() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer weft_a_b".parse().unwrap());
        assert_eq!(token_from_headers(&h).unwrap(), "weft_a_b");

        // Basic x:weft_a_b (git's usual shape: token as password)
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            "Basic eDp3ZWZ0X2FfYg==".parse().unwrap(),
        );
        assert_eq!(token_from_headers(&h).unwrap(), "weft_a_b");
    }

    /// Which 401 carries a Basic challenge is a correctness property of
    /// each transport, not a style choice: git needs it to retry with
    /// credentials, and a browser that sees it on a same-origin fetch()
    /// opens its own credential dialog and never settles the promise.
    /// The e2e test proves the wiring; this pins the enum itself.
    #[test]
    fn the_basic_challenge_is_carried_only_when_asked_for() {
        let git = unauthorized(Challenge::Basic);
        assert_eq!(git.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            git.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Basic realm=\"stratum\"",
        );

        let rest = unauthorized(Challenge::None);
        assert_eq!(rest.status(), StatusCode::UNAUTHORIZED);
        assert!(
            rest.headers().get(header::WWW_AUTHENTICATE).is_none(),
            "a REST 401 must not make the browser open a credential dialog",
        );
    }
}
