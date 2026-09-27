//! Vendor CI intake: the on-ramp for a build system that already keeps
//! somebody's trunk green.
//!
//! `POST /changes/:change/checks` has always existed, but it wants a
//! Stratum token with `repo:write` — the same credential that can push,
//! land and delete. Handing that to a third-party CI runner so it can
//! say "the tests passed" is a bad trade, and it is the reason a
//! migrating project gets nothing from us on day one: nobody pastes a
//! write token into GitHub Actions to report a build.
//!
//! So this is the same verdict through a narrower door. The credential
//! is a **per-repo intake secret** that can do exactly one thing — write
//! a check row — and the request proves possession the way every other
//! webhook in this codebase does: `X-Weft-Signature-256:
//! sha256=<hmac-sha256 of the raw body>`. That is deliberately the same
//! scheme `mirror::origin` verifies on the way in and `workers::notify`
//! produces on the way out; a second signing scheme would be a second
//! thing to get wrong.
//!
//! **Deliberately not here: running anybody's code.** We accept a
//! verdict; we do not produce one. A runner is a sandboxing project.
//!
//! ## What the secret can and cannot say
//!
//! It reports a check on **the latest patchset of a change**, and it must
//! name the commit it built. Anything else is refused. It cannot approve,
//! cannot land, cannot read the repository, and cannot report on an
//! abandoned change. It *may* report on a landed one — see the handler
//! for why that is the case a README badge exists for.
//!
//! ## Masking
//!
//! Three different failures answer identically — no such repository, no
//! intake secret configured, and a signature that does not match. They
//! have to: a 401-for-bad-signature against a 404-for-no-such-repo is an
//! existence oracle for private repositories, exactly the one
//! `app::repo_or_masked` exists to close, and this route has no
//! Authorization header for `authx::masked` to key off. The refusal body
//! names all three possibilities so an operator with the secret in hand
//! still has a checklist.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hmac::Mac;
use serde::Deserialize;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::changes;
use stratum_control::ids::{now_ms, token_secret};
use stratum_control::webhooks::{meta_get, meta_set};

/// The header a delivery signs with — the same one `workers::notify`
/// sets on the way out, so a reader only ever learns one name.
pub const SIG_HEADER: &str = "X-Weft-Signature-256";

/// I13: a check report is four short strings. Sixteen kibibytes is
/// already two orders of magnitude more than one needs, and the point
/// of the bound is that there *is* one.
const MAX_INTAKE_BODY: usize = 16 * 1024;

/// A build summary is prose for a human reading the change. Bounded,
/// and **refused rather than truncated** — a summary silently cut in
/// half is a summary that lies about what happened.
const MAX_SUMMARY: usize = 2000;

/// How far a delivery's own timestamp may sit from ours. Five minutes
/// absorbs ordinary clock skew between somebody's CI runner and us, and
/// bounds how long a captured body is worth anything.
const FRESHNESS_MS: i64 = 5 * 60 * 1000;

/// How many recent delivery digests a repository remembers, so that a
/// captured body cannot simply be sent twice.
///
/// One row per repository rather than one per delivery: the row is
/// bounded (64 digests, ~4 KiB) where per-delivery rows would grow
/// without limit and there is no way to prune them from here — the
/// control database's raw handle is `pub(crate)` and correctly so.
/// Beyond 64 deliveries inside the freshness window the clock is the
/// remaining bound, which is a real limit and is documented as one.
const REPLAY_RING: usize = 64;

/// The principal recorded on a check that arrived this way. Honest about
/// what it is: not a person, and not a token either.
const INTAKE_PRINCIPAL: &str = "ci-intake";

/// What `check_runs.provider` says about a row that came in through this
/// door. One value for every vendor, deliberately: the reading side must
/// never grow a branch per CI system, and a project on Buildkite and a
/// project on Jenkins are the same product to a Checks tab.
const INTAKE_PROVIDER: &str = "intake";

// ---------------------------------------------------------------------
// The secret
// ---------------------------------------------------------------------

fn secret_key(repo_id: &str) -> String {
    format!("ci-intake-secret:{repo_id}")
}

fn ring_key(repo_id: &str) -> String {
    format!("ci-intake-seen:{repo_id}")
}

/// What is stored under the secret key. `rotated_at` is reportable;
/// the secret itself is shown once, at rotation, and never again.
#[derive(serde::Serialize, Deserialize)]
struct StoredSecret {
    secret: String,
    rotated_at: i64,
}

/// The repository's current intake secret, or None when there is not one.
///
/// Revocation writes an empty secret rather than deleting the row —
/// `webhooks::meta_set` is an upsert and there is no delete on that
/// surface — so an empty string means "revoked", and an empty secret
/// must never be treated as a valid one that happens to match nothing.
fn load_secret(
    db: &stratum_control::ControlDb,
    repo_id: &str,
) -> Result<Option<StoredSecret>, String> {
    let Some(raw) = meta_get(db, &secret_key(repo_id))? else {
        return Ok(None);
    };
    let stored: StoredSecret = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    Ok((!stored.secret.is_empty()).then_some(stored))
}

// ---------------------------------------------------------------------
// Pure pieces — everything decidable without a database lives here so
// it can be tested without one.
// ---------------------------------------------------------------------

/// `sha256=<hex>` over the raw body, with the repo's intake secret.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(body);
    format!(
        "sha256={}",
        stratum_store::pack::hex(&mac.finalize().into_bytes())
    )
}

/// Does the presented header sign this body with this secret?
///
/// Every failure is the same failure to the caller; the reason is for
/// this module's own tests.
fn signature_ok(secret: &str, presented: Option<&str>, body: &[u8]) -> bool {
    let Some(presented) = presented else {
        return false;
    };
    let want = sign(secret, body);
    constant_time_eq(want.as_bytes(), presented.as_bytes())
}

/// FINDING (not fixed here, it reaches five files this track does not
/// own): `constant_time_eq` now exists six times in this workspace —
/// `cdn.rs`, `billing/stripe.rs`, `mirror/origin.rs`, `control/auth.rs`,
/// `control/invites.rs` and here — byte-identical each time. A
/// comparison that decides whether a credential matches is not a thing
/// to hold six opinions about. Proposed patch: one
/// `stratum_store::sig::constant_time_eq` (the crate both sides already
/// depend on for `hex`), with the five copies deleted and their tests
/// pointed at it.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A delivery's own timestamp, against ours.
fn is_fresh(sent_at: i64, now: i64) -> bool {
    (now - sent_at).abs() <= FRESHNESS_MS
}

/// Push a digest onto the repository's replay ring.
///
/// `None` means the digest is already there — this exact body has been
/// delivered before and must not be applied again. Otherwise the new
/// ring, ready to store.
///
/// The read-modify-write is not atomic, and the limit that leaves is
/// worth stating rather than glossing. Two deliveries to one repository
/// that overlap can lose one digest from the ring, which leaves *that*
/// body replayable — but only until its `sent_at` ages out of the
/// freshness window, so the exposure is bounded at five minutes and
/// needs the attacker to have captured a body inside the same instant
/// two deliveries collided. What replay protection is actually for is a
/// body captured now and sent back tomorrow, and tomorrow always reads a
/// ring that has already been written. Closing the race properly wants a
/// single atomic statement, which wants the control database's raw
/// handle, which is `pub(crate)` and correctly so.
fn ring_push(existing: Option<&str>, digest: &str) -> Option<String> {
    let mut seen: Vec<String> = existing
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    if seen.iter().any(|d| d == digest) {
        return None;
    }
    seen.insert(0, digest.to_string());
    seen.truncate(REPLAY_RING);
    Some(serde_json::to_string(&seen).expect("a list of strings serializes"))
}

/// I13 on the summary: bounded, and printable enough to render. Newline
/// and tab are prose; a NUL or an escape sequence is not.
fn summary_ok(summary: &str) -> bool {
    summary.len() <= MAX_SUMMARY
        && !summary
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
}

/// A git object id, **normalised to the one spelling everything else in
/// this codebase uses**, or `None` if it is not one.
///
/// Forty hex digits, case-insensitively — `is_ascii_hexdigit` accepts
/// `A-F` and that is correct, because hex conventionally is
/// case-insensitive and uppercase shas are not exotic: a `tr` in a
/// pipeline, a Windows toolchain, anyone who read "40-hex" the way the
/// rest of the world does.
///
/// **Validating without normalising was the bug.** `check_runs` stores
/// `commit_sha` verbatim and every read matches it exactly, so an
/// uppercase sha was accepted, given a 200 and a run id, written — and
/// then never found again, because the commit page fetches with the
/// lowercase sha git handed it and got `{"runs": []}`. Nothing errored
/// on either side. It also split the id-less upsert identity
/// `(repo_id, provider, commit_sha, name)`: one run reported once in
/// each case became two rows. The patchset path failed differently and
/// no better — `latest.commit_oid != parsed.commit` is a byte
/// comparison, so it answered 409 with "this reports on ABC123…, but
/// the latest patchset is abc123…", two shas that read as identical.
///
/// So this returns the value to use rather than a verdict about it. A
/// checker that hands back a `bool` is a checker whose caller can go on
/// using the unchecked string, which is precisely what happened.
fn normalise_commit(commit: &str) -> Option<String> {
    (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| commit.to_ascii_lowercase())
}

/// The most one of a commit-scoped report's short labels may be — a
/// branch name, an event, an actor, a provider's run id.
///
/// I13 again: every one of these is rendered in a table cell beside a
/// verdict, and a caller with the secret is still a caller whose input
/// has to have a stated bound. Two hundred bytes is longer than any
/// branch name anybody has defended in review.
const MAX_LABEL: usize = 200;

/// One short label, bounded and renderable.
///
/// Control characters are refused rather than stripped, for the same
/// reason [`summary_ok`] refuses them: an escape sequence in a table
/// cell is not a name, and a value silently altered on the way in is a
/// value the caller cannot reconcile with what they sent.
fn label_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_LABEL && !s.chars().any(char::is_control)
}

/// FINDING (not fixed here: it is a one-word change in a crate this
/// track does not own): `changes::valid_check_url` is private, so this
/// is a second copy of a rule that already exists — http(s), at most
/// `changes::MAX_CHECK_URL` bytes, graphic ASCII only. Two copies of a
/// validator is how the two surfaces come to disagree about what a link
/// is, and the disagreement will be found by somebody whose CI posts the
/// same URL to both routes and is refused by one. Proposed patch: make
/// `changes::valid_check_url` `pub` and delete this, exactly as
/// `valid_check_name` beside it already is.
fn detail_url_ok(url: &str) -> bool {
    url.len() <= stratum_control::changes::MAX_CHECK_URL
        && (url.starts_with("http://") || url.starts_with("https://"))
        && url.bytes().all(|b| b.is_ascii_graphic())
}

// ---------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------

/// The one answer three different failures share. See the module note:
/// telling them apart is an existence oracle for private repositories.
fn opaque() -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        "no CI intake here: the repository does not exist, has no intake \
         secret configured, or the signature did not match",
    )
}

/// POST /v1/orgs/:org/repos/:repo/ci/secret — mint or rotate the
/// repository's intake secret.
///
/// Rotation is the only way to read one: the value is returned exactly
/// once, here, and rotating invalidates whatever the previous holder
/// had. That is the same contract `webhooks_api::create` gives the
/// outbound secret, and for the same reason — a secret a server can
/// re-read is a secret an admin session can be tricked into re-reading.
pub async fn rotate_secret(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    let stored = StoredSecret {
        secret: token_secret(),
        rotated_at: now_ms(),
    };
    let raw = serde_json::to_string(&stored).expect("a struct of two fields serializes");
    if let Err(e) = meta_set(&state.db, &secret_key(&repo.id), &raw) {
        return internal(e);
    }
    // Who can report a build is an authorization fact about the repo,
    // so it belongs in the trail exactly like a webhook subscription.
    crate::api::record_or_warn(
        &state.db,
        &AuditCtx::of(&org.id, principal.as_ref()),
        Some(&repo.id),
        "ci.secret.rotate",
        None,
    );
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "secret": stored.secret,
            "rotated_at": stored.rotated_at,
        })),
    )
        .into_response()
}

/// GET /v1/orgs/:org/repos/:repo/ci/secret — whether one is configured,
/// and when it last moved. Never the secret.
pub async fn secret_status(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo, _) = match crate::app::rest_repo_auth(
        &state,
        &headers,
        &org_name,
        &repo_name,
        Scope::RepoRead,
    ) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match load_secret(&state.db, &repo.id) {
        Ok(Some(s)) => Json(serde_json::json!({
            "configured": true,
            "rotated_at": s.rotated_at,
        }))
        .into_response(),
        Ok(None) => Json(serde_json::json!({ "configured": false })).into_response(),
        Err(e) => internal(e),
    }
}

/// DELETE /v1/orgs/:org/repos/:repo/ci/secret — revoke it. The next
/// delivery from whoever held it is answered like any stranger's.
pub async fn revoke_secret(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    let raw = serde_json::to_string(&StoredSecret {
        secret: String::new(),
        rotated_at: now_ms(),
    })
    .expect("a struct of two fields serializes");
    if let Err(e) = meta_set(&state.db, &secret_key(&repo.id), &raw) {
        return internal(e);
    }
    crate::api::record_or_warn(
        &state.db,
        &AuditCtx::of(&org.id, principal.as_ref()),
        Some(&repo.id),
        "ci.secret.revoke",
        None,
    );
    StatusCode::NO_CONTENT.into_response()
}

/// A report about a run, in either of the two shapes this route takes.
///
/// **`change` is the discriminator, and nothing else is.** A body that
/// names a change is patchset-scoped and behaves exactly as it always
/// has: it writes `change_checks`, its `state` vocabulary is the three
/// that gate a merge, and `commit` is not a target but an assertion that
/// the run built the change's current tip. A body with no `change` is
/// commit-scoped: it writes `check_runs` with `provider = "intake"`,
/// its vocabulary is [`RunState`]'s six, and `commit` is the whole
/// subject of the report.
///
/// There is deliberately no way to ask for both at once. The two are not
/// two renderings of one fact: one is a merge gate on a patchset that a
/// force-push invalidates, the other is a historical verdict about a
/// commit that outlives every change it was ever part of. They have
/// different vocabularies, different identities, and different
/// lifetimes, and one HTTP response cannot honestly describe what
/// happened to both — a 201 naming a patchset says nothing about whether
/// the run row was inserted or updated, and a caller who got a 400 from
/// the second write would have no way to tell that the first had already
/// been applied. A reporter that wants both sends two requests, gets two
/// answers, and can retry the one that failed.
#[derive(Deserialize)]
pub struct IntakeBody {
    /// The change key the run built, e.g. `I1a2d0001`. Absent for a
    /// commit-scoped report.
    pub change: Option<String>,
    /// The commit the run built. Required either way: for a
    /// patchset-scoped report it must be the tip of the change's latest
    /// patchset (see the handler for why), and for a commit-scoped one
    /// it is what the row is about.
    #[serde(default)]
    pub commit: String,
    /// CI-suite label, e.g. `ci/tests`.
    pub name: String,
    /// `pending | passing | failing` for a patchset-scoped report;
    /// `queued | running | passing | failing | cancelled | skipped` for
    /// a commit-scoped one.
    pub state: String,
    /// Where the run's detail lives (http/https).
    pub url: Option<String>,
    /// One human sentence about the run. Recorded in the audit trail.
    pub summary: Option<String>,
    /// The sender's own clock, unix milliseconds.
    pub sent_at: i64,

    // --- commit-scoped only ------------------------------------------
    /// The provider's own id for the run, if it has one. Supplying it is
    /// what makes a re-report an update rather than a second row keyed
    /// on `(commit, name)` — see `checks::upsert`, which argues both.
    #[serde(default)]
    pub external_id: Option<String>,
    /// The branch or tag it ran for. `ref` on the wire, because that is
    /// what git calls it and what every CI system's environment names.
    #[serde(default, rename = "ref")]
    pub ref_name: Option<String>,
    /// The per-repository counter a person recognises in their CI's own
    /// UI. Not an identity here; `external_id` is.
    #[serde(default)]
    pub run_number: Option<i64>,
    /// What triggered it — `push`, `pull_request`, `schedule`.
    #[serde(default)]
    pub event: Option<String>,
    /// Who it ran for.
    #[serde(default)]
    pub actor: Option<String>,
    /// Epoch milliseconds. Absent rather than zero when unknown: a zero
    /// renders as January 1970, which is a confident wrong answer.
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
}

/// Whether `state` is one this scope's table can hold, and if not, a
/// refusal that says which scope it was judged under.
///
/// Neither list is written out here. The commit-scoped one is whatever
/// [`RunState::parse`] accepts and the patchset-scoped one is
/// [`changes::PATCHSET_CHECK_STATES`]; restating either would put a
/// fourth copy of a vocabulary in the codebase, and the copy that goes
/// stale is the one in the error message, which is the only copy a
/// caller ever reads.
fn state_ok_for_scope(
    state: &str,
    commit_scoped: bool,
) -> Result<Option<stratum_control::checks::RunState>, String> {
    let patchset = changes::PATCHSET_CHECK_STATES.join(", ");
    let commit = stratum_control::checks::RunState::names();
    if commit_scoped {
        // Parsed once, here, and handed on. `record_commit_run` used to
        // parse it again and carry its own `Err` arm for a word this
        // guard had already refused — dead the moment the guard landed,
        // and the coverage gate said so. Two parses of one field is also
        // two places for the vocabulary to be applied differently.
        if let Ok(s) = stratum_control::checks::RunState::parse(state) {
            return Ok(Some(s));
        }
        return Err(format!(
            "check state {state:?} is not one of {commit} — the states a report on \
             a commit may use. Name a `change` to report against the change's latest \
             patchset instead, which uses {patchset}."
        ));
    }
    if changes::PATCHSET_CHECK_STATES.contains(&state) {
        return Ok(None);
    }
    Err(format!(
        "check state {state:?} is not one of {patchset} — the states a report on a \
         change's patchset may use. Drop `change` to report against the commit alone \
         instead, which uses {commit}."
    ))
}

/// POST /v1/orgs/:org/repos/:repo/ci/checks — a signed verdict from a
/// build system that is not us.
pub async fn intake(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Resolve without headers: the signature *is* the credential here,
    // exactly like the signed CDN pack URL, so `repo_or_masked` has
    // nothing to key off and would answer 401 to every CI runner alive.
    let (org, repo) = match crate::app::repo_or_404(&state, &org_name, &repo_name) {
        Ok(x) => x,
        // A database that is down is not "no such repository". Masking
        // hides *whether the repo exists*; folding an outage into the
        // same answer hides an incident instead, and sends whoever is
        // debugging it to look at their repo name.
        Err(r) if r.status() == StatusCode::INTERNAL_SERVER_ERROR => return r,
        Err(_) => return opaque(),
    };
    let stored = match load_secret(&state.db, &repo.id) {
        Ok(Some(s)) => s,
        Ok(None) => return opaque(),
        // Only we ever write this row, so a malformed one is our fault
        // and not something a caller can provoke to use as an oracle.
        Err(e) => return internal(e),
    };
    let presented = headers.get(SIG_HEADER).and_then(|v| v.to_str().ok());
    if !signature_ok(&stored.secret, presented, &body) {
        return opaque();
    }

    // Past this line the caller has proven possession of the secret, so
    // refusals may be specific: they are talking to their own repo and
    // a vague answer only costs them an afternoon.
    if body.len() > MAX_INTAKE_BODY {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "body is {} bytes; the limit is {MAX_INTAKE_BODY}",
                body.len()
            ),
        );
    }
    let mut parsed: IntakeBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("body: {e}")),
    };
    if let Some(s) = parsed.summary.as_deref() {
        if !summary_ok(s) {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!("summary must be at most {MAX_SUMMARY} printable bytes"),
            );
        }
    }
    // Which of the two reports this is, decided by `change` alone. See
    // [`IntakeBody`] for why there is no third answer.
    let commit_scoped = parsed.change.is_none();
    // Normalised in place, once, before anything downstream can see the
    // caller's spelling: the patchset comparison, the row, the audit
    // entry and the response body all read `parsed.commit` and they must
    // all read the same string. Doing it at each use is how one of them
    // gets missed — and the one that gets missed is a row that is
    // written and never read.
    match normalise_commit(&parsed.commit) {
        Some(c) => parsed.commit = c,
        None => {
            return json_error(
                StatusCode::BAD_REQUEST,
                if commit_scoped {
                    // A body that named neither. Saying "commit must be
                    // 40-hex" here would send a reader looking for a typo in
                    // a field they never wrote, so this names both doors.
                    "this reports on nothing: send `change` for a report on a change's \
                 latest patchset, or a 40-hex `commit` for a report on a commit"
                } else {
                    "commit must be a 40-hex object id"
                },
            );
        }
    }
    // The state, checked here because **here is where the two
    // vocabularies meet** and nowhere downstream knows that.
    //
    // `change_checks` takes three words and `check_runs` takes six, and
    // this one door leads to both depending on whether the body named a
    // change. Each downstream validator refuses correctly for its own
    // table and names its own list — and that is exactly the unhelpful
    // answer, because the list a caller is shown depends on a field they
    // may not have realised they were choosing with. A push reporting
    // `pending` (the state the docs led with, and the natural word for
    // "started") is told it must be one of queued, running, passing,
    // failing, cancelled, skipped: six words, none of them `pending`,
    // with no hint that `pending` is valid one field away.
    //
    // So the refusal names the scope it is refusing under and what the
    // other scope would accept. Both lists come from the tables' own
    // authorities rather than being restated, so they cannot drift.
    let run_state = match state_ok_for_scope(&parsed.state, commit_scoped) {
        Ok(s) => s,
        Err(why) => return json_error(StatusCode::BAD_REQUEST, why),
    };
    let now = now_ms();
    if !is_fresh(parsed.sent_at, now) {
        return json_error(
            StatusCode::CONFLICT,
            format!(
                "sent_at is {} ms from this server's clock; the window is {FRESHNESS_MS} ms \
                 (a replayed delivery looks exactly like this)",
                (now - parsed.sent_at).abs()
            ),
        );
    }
    let digest = stratum_store::sig::sha256_hex(&body[..]);
    let ring = match meta_get(&state.db, &ring_key(&repo.id)) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let Some(next_ring) = ring_push(ring.as_deref(), &digest) else {
        return json_error(
            StatusCode::CONFLICT,
            "this exact delivery has already been applied",
        );
    };

    // Past every check the two shapes share — the signature, the size,
    // the freshness window and the replay ring, none of which is
    // weakened or skipped for the new shape. What differs from here is
    // only *which table the verdict is about*.
    let Some(change_key) = parsed.change.clone() else {
        // `run_state` is `Some` on exactly this branch: the guard above
        // parses when `commit_scoped`, and `commit_scoped` is
        // `parsed.change.is_none()` — the same condition this `else`
        // stands on. Passing the value rather than re-deriving it is
        // what keeps that a fact rather than an assumption.
        let run_state = run_state.expect("commit-scoped bodies carry a parsed state");
        return record_commit_run(&state, &org, &repo, &parsed, run_state, &next_ring);
    };

    let change = match changes::by_key(&state.db, &repo.id, &change_key) {
        Ok(Some(c)) => c,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "no such change"),
        Err(e) => return internal(e),
    };
    // `landed` is accepted here where `changes_api::post_check` refuses
    // it, and the difference is deliberate rather than an oversight.
    //
    // Landing is already gated on no check failing, so a landed change
    // could only ever have been green or amber — which made the badge's
    // red unreachable by construction, a colour nobody could ever see.
    // The case it was missing is the ordinary one: a nightly, or a
    // post-merge suite, finding trunk broken an hour after it landed.
    // That is precisely what a README badge is for.
    //
    // It is safe because the commit check below still applies: a report
    // on a landed change must name that change's tip, which the lander
    // fast-forwarded the branch to. And the land queue cannot be
    // affected — that change is done, and `failing_check` is only ever
    // asked about a change on its way in.
    //
    // `abandoned` stays refused. It is a report about nothing.
    if !matches!(change.state.as_str(), "open" | "landing" | "landed") {
        return json_error(StatusCode::CONFLICT, format!("change is {}", change.state));
    }
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    // Naming the commit is not bookkeeping, it is the second half of
    // replay protection and the whole of "yesterday's green run says
    // nothing about code it never built". A verdict that arrives after
    // the author pushed a revision would otherwise mark the *new*
    // patchset green on the strength of a build of the old one.
    if latest.commit_oid != parsed.commit {
        return json_error(
            StatusCode::CONFLICT,
            format!(
                "this reports on {}, but the latest patchset is {}",
                parsed.commit, latest.commit_oid
            ),
        );
    }

    match changes::set_check(
        &state.db,
        &change.id,
        &latest.id,
        &parsed.name,
        &parsed.state,
        parsed.url.as_deref(),
        INTAKE_PRINCIPAL,
    ) {
        Ok(Ok(created)) => {
            if let Err(e) = meta_set(&state.db, &ring_key(&repo.id), &next_ring) {
                // The check is written. Failing the request now would
                // tell CI to retry something that already happened; the
                // cost of the miss is that one body stays replayable
                // until its freshness window closes.
                eprintln!("weft: ci intake replay ring not updated: {e}");
            }
            // `change_checks` has nowhere to put prose, and inventing a
            // column is not this track's to do. The trail is where a
            // sentence about a run belongs anyway — it is dated, it
            // names who said it, and it is never overwritten by the next
            // report the way the check row is.
            crate::api::record_or_warn(
                &state.db,
                &AuditCtx::system(&org.id, INTAKE_PRINCIPAL),
                Some(&repo.id),
                "ci.check",
                Some(&serde_json::json!({
                    "change": change.change_key,
                    "commit": parsed.commit,
                    "name": parsed.name,
                    "state": parsed.state,
                    "url": parsed.url,
                    "summary": parsed.summary,
                })),
            );
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(serde_json::json!({
                    "change": change.change_key,
                    "patchset": latest.number,
                    "name": parsed.name,
                    "state": parsed.state,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => json_error(StatusCode::BAD_REQUEST, e),
        Err(e) => internal(e),
    }
}

/// The commit-scoped half of [`intake`]: a verdict about a commit,
/// written to `check_runs` under `provider = "intake"`.
///
/// Everything a patchset-scoped report is checked for has already
/// happened by the time this is called. What is left is the part where
/// the two shapes genuinely differ, and all of it is validation the
/// other path gets from `changes::set_check` and this one would
/// otherwise not get at all: `checks::upsert` writes what it is handed,
/// so a commit-scoped report that skipped these would be the one door in
/// the API through which an unbounded name or a `javascript:` link could
/// walk.
fn record_commit_run(
    state: &SharedState,
    org: &stratum_control::registry::Org,
    repo: &stratum_control::registry::Repo,
    parsed: &IntakeBody,
    // Already parsed by `state_ok_for_scope`, which is the only place
    // that decides *which* vocabulary applies. The six, not the three:
    // a provider reporting `queued` on a commit is saying something
    // true that the merge-gate vocabulary cannot express.
    run_state: stratum_control::checks::RunState,
    next_ring: &str,
) -> Response {
    if !changes::valid_check_name(&parsed.name) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid check name {:?}", parsed.name),
        );
    }
    if let Some(u) = parsed.url.as_deref() {
        if !detail_url_ok(u) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "url must be http(s) and at most 1000 bytes",
            );
        }
    }
    for (field, value) in [
        ("ref", parsed.ref_name.as_deref()),
        ("event", parsed.event.as_deref()),
        ("actor", parsed.actor.as_deref()),
        ("external_id", parsed.external_id.as_deref()),
    ] {
        if let Some(v) = value {
            if !label_ok(v) {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    format!("{field} must be 1..={MAX_LABEL} printable bytes"),
                );
            }
        }
    }

    let run = stratum_control::checks::NewCheckRun {
        commit_sha: &parsed.commit,
        ref_name: parsed.ref_name.as_deref(),
        provider: INTAKE_PROVIDER,
        external_id: parsed.external_id.as_deref(),
        name: &parsed.name,
        run_number: parsed.run_number,
        event: parsed.event.as_deref(),
        state: run_state,
        detail_url: parsed.url.as_deref(),
        actor: parsed.actor.as_deref(),
        started_at: parsed.started_at,
        completed_at: parsed.completed_at,
    };
    let written = match stratum_control::checks::upsert(&state.db, &repo.id, &run) {
        Ok(w) => w,
        Err(e) => return internal(e),
    };
    if let Err(e) = meta_set(&state.db, &ring_key(&repo.id), next_ring) {
        // Same trade as the patchset path: the row is written, and
        // failing now would tell CI to retry something that happened.
        eprintln!("weft: ci intake replay ring not updated: {e}");
    }
    // `summary` is validated above and recorded here, and it is
    // deliberately not on the row: `check_runs` has nowhere to put prose
    // and inventing a column is not this track's to do. The trail is
    // where a sentence about a run belongs anyway — it is dated, it
    // names who said it, and it is never overwritten by the next report
    // the way the run row is. Exactly the same trade the patchset path
    // makes with `change_checks`, and stated in both places so that
    // neither reads as an oversight.
    crate::api::record_or_warn(
        &state.db,
        &AuditCtx::system(&org.id, INTAKE_PRINCIPAL),
        Some(&repo.id),
        "ci.check",
        Some(&serde_json::json!({
            "commit": parsed.commit,
            "name": parsed.name,
            "state": parsed.state,
            "url": parsed.url,
            "summary": parsed.summary,
        })),
    );
    // **200, never 201.** The write is an upsert and this route does not
    // know which half it did: a provider re-reporting a run it already
    // reported is the ordinary case, not an error, and claiming
    // `Created` for it would be a guess. The row's id is returned so a
    // caller has something to point at either way.
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "id": written.id,
            "commit": written.commit_sha,
            "name": written.name,
            "state": written.state,
            "provider": written.provider,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------
// The GitHub Actions poller's door
// ---------------------------------------------------------------------
//
// Here rather than in `checks_api` because `/ci/*` is this module's
// namespace: the intake, the secret that opens it, and the poller are
// the three ways a verdict gets into `check_runs`, and a reader looking
// for "how does CI reach us" should find them together. The read API
// over the rows themselves is a different question and lives with the
// other read routes.

/// `POST /v1/orgs/:org/repos/:repo/ci/poll` — ask for a poll of this
/// repository's GitHub Actions runs.
///
/// Write, not admin: unlike an import this writes nothing a person has
/// to undo by hand — it restates verdicts a provider already reached,
/// idempotently, into rows nobody edits.
pub async fn poll_now(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org, repo, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
        {
            Ok(x) => x,
            Err(r) => return r,
        };
    // Refused before anything long-running starts, and in a sentence
    // that names the way through. A poll that is accepted and then does
    // nothing for a minute, silently, is a much worse answer.
    if repo.origin_provider.as_deref() != Some("github") || repo.origin_installation.is_none() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "this repository has no GitHub origin to poll. Connect it as a mirror \
             first, so the checks come through the same installation the commits do \
             — or report your builds to /ci/checks with an intake secret instead.",
        );
    }
    if let Err(e) = crate::workers::checks_poll::enqueue(&state, &org.id, &repo.id) {
        return internal(e);
    }
    // 202: a job exists, and no run has been recorded yet.
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "queued" })),
    )
        .into_response()
}

/// `GET /v1/orgs/:org/repos/:repo/ci/poll` — what the poller has to say.
///
/// The field that earns this route is `denied`. An installation without
/// `actions: read` is the one failure that renders identically to
/// success — an empty Checks tab — and a page that cannot tell them
/// apart tells a maintainer their CI is not configured when the truth is
/// that we may not look at it. With this, the tab can say so and offer
/// the re-approve link.
///
/// **Two audiences, and they get different answers.** `RepoRead` on a
/// public mirror is *anybody at all*, so what a stranger may have is the
/// shape of the problem — `connected`, `polled`, `denied` — and nothing
/// else. That is the whole point of keeping those three states apart,
/// and none of them says anything a visitor could not infer from the
/// empty tab in front of them.
///
/// `error` and `resuming_from` need `RepoWrite`. They are operator text:
/// `error` is GitHub's raw response body or a `GET {url}: {e}` carrying
/// the full `api.github.com` URL, which names the origin `owner/repo`
/// this mirror pulls from, and `resuming_from` is a raw upstream page
/// URL. A private repository mirrored from a private upstream would have
/// been publishing that upstream's name to every anonymous reader of its
/// public fork. No credential is in either — the installation token is a
/// header and never appears in a URL — but the origin's identity is not
/// ours to give away.
///
/// Gated **here and not in the client**: a page that merely declines to
/// render the field still shipped it, and anybody who opens devtools
/// reads it out of the JSON. The only place a field can be withheld is
/// the place that decides to serialise it.
pub async fn poll_status(
    State(state): State<SharedState>,
    Path((org_name, repo_name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo, _) = match crate::app::rest_repo_auth(
        &state,
        &headers,
        &org_name,
        &repo_name,
        Scope::RepoRead,
    ) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Asked a second time rather than inferred from the first: the
    // authorization rules for write are `rest_repo_auth`'s to know, and
    // a second opinion reconstructed here from a principal would be a
    // copy of them that can drift. The call is pure — it resolves and
    // checks, and records nothing — so asking twice costs two lookups
    // and buys the guarantee that this route and every other route mean
    // the same thing by "may write".
    let may_write =
        crate::app::rest_repo_auth(&state, &headers, &org_name, &repo_name, Scope::RepoWrite)
            .is_ok();
    let connected =
        repo.origin_provider.as_deref() == Some("github") && repo.origin_installation.is_some();
    match crate::workers::checks_poll::state_of(&state, &repo.id) {
        Ok(s) => Json(serde_json::json!({
            "provider": "github",
            "connected": connected,
            "polled": s.polled,
            "denied": s.denied,
            "high_water": s.high_water,
            "retry_in_ms": s.retry_in_ms,
            // Null for a reader, not omitted: a field that disappears
            // makes a client write `"error" in poll`, which then reads
            // "we could not tell you" as "there is no error". Present
            // and null says the same thing to both.
            "error": may_write.then_some(s.error).flatten(),
            "resuming_from": may_write.then_some(s.resuming_from).flatten(),
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_is_over_the_exact_bytes_with_the_exact_secret() {
        let body = br#"{"name":"ci/tests","state":"passing"}"#;
        let sig = sign("s3cret", body);
        assert!(sig.starts_with("sha256="), "{sig}");
        assert_eq!(sig.len(), "sha256=".len() + 64, "{sig}");
        assert!(signature_ok("s3cret", Some(&sig), body));

        // Every axis that must matter, does.
        assert!(!signature_ok("s3cret", None, body), "unsigned");
        assert!(!signature_ok("other", Some(&sig), body), "wrong secret");
        assert!(
            !signature_ok("s3cret", Some(&sig), b"{\"name\":\"ci/tests\"}"),
            "different body"
        );
        assert!(
            !signature_ok("s3cret", Some(&sig[..sig.len() - 1]), body),
            "truncated signature"
        );
        assert!(
            !signature_ok("s3cret", Some(&sig.replace("sha256=", "sha1=")), body),
            "a signature of another algorithm is not this one"
        );

        // A single flipped hex digit fails — the comparison is over the
        // whole digest, not a prefix.
        let mut flipped: Vec<char> = sig.chars().collect();
        let last = flipped.len() - 1;
        flipped[last] = if flipped[last] == 'a' { 'b' } else { 'a' };
        let flipped: String = flipped.into_iter().collect();
        assert!(!signature_ok("s3cret", Some(&flipped), body));
    }

    #[test]
    fn constant_time_eq_is_an_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn freshness_is_a_window_in_both_directions() {
        let now = 1_700_000_000_000;
        assert!(is_fresh(now, now));
        assert!(is_fresh(now - FRESHNESS_MS, now), "at the past edge");
        assert!(is_fresh(now + FRESHNESS_MS, now), "at the future edge");
        assert!(!is_fresh(now - FRESHNESS_MS - 1, now), "one ms too old");
        // A clock far ahead is refused too: it would otherwise mint a
        // body that stays valid for as long as the sender chose.
        assert!(!is_fresh(now + FRESHNESS_MS + 1, now), "one ms too new");
    }

    #[test]
    fn the_replay_ring_refuses_a_digest_it_has_seen_and_forgets_the_oldest() {
        let first = ring_push(None, "aa").expect("a fresh ring accepts");
        assert!(
            ring_push(Some(&first), "aa").is_none(),
            "the same body twice is a replay"
        );
        let second = ring_push(Some(&first), "bb").expect("a different body is accepted");
        assert!(ring_push(Some(&second), "aa").is_none(), "still remembered");

        // Exactly REPLAY_RING digests are remembered, and the oldest
        // falls off rather than the ring growing without bound.
        let mut ring = String::from("[]");
        for i in 0..REPLAY_RING {
            ring = ring_push(Some(&ring), &format!("d{i}")).expect("all distinct");
        }
        assert!(ring_push(Some(&ring), "d0").is_none(), "d0 is still in");
        let ring = ring_push(Some(&ring), "overflow").expect("distinct");
        let held: Vec<String> = serde_json::from_str(&ring).unwrap();
        assert_eq!(held.len(), REPLAY_RING);
        assert!(
            ring_push(Some(&ring), "d0").is_some(),
            "the oldest digest has aged out, which is what the freshness \
             window is the remaining bound for"
        );

        // Garbage in the row must not wedge the door open *or* shut: it
        // is treated as an empty ring, so the delivery is accepted and
        // the row is repaired by the write that follows.
        assert!(ring_push(Some("not json"), "aa").is_some());
    }

    #[test]
    fn a_summary_is_bounded_and_printable() {
        assert!(summary_ok(""));
        assert!(summary_ok("42 passed, 0 failed\n\tin 31s"));
        assert!(summary_ok(&"x".repeat(MAX_SUMMARY)));
        assert!(!summary_ok(&"x".repeat(MAX_SUMMARY + 1)), "one over");
        assert!(!summary_ok("green\u{0}"), "NUL");
        assert!(!summary_ok("green\u{1b}[31m"), "an escape sequence");
    }

    /// Forty hex digits either way, and **one** spelling out.
    ///
    /// The lowercasing is the assertion that matters. Accepting
    /// uppercase and storing it verbatim gave a CI script a 200 and a
    /// run id for a row the commit page could never find, and split one
    /// run into two under the id-less upsert identity.
    #[test]
    fn a_commit_is_forty_hex_digits_and_comes_back_lowercase() {
        let lower = "3f2a1b4c5d6e7f8091a2b3c4d5e6f708192a3b4c";
        assert_eq!(normalise_commit(lower).as_deref(), Some(lower));
        assert_eq!(
            normalise_commit("3F2A1B4C5D6E7F8091A2B3C4D5E6F708192A3B4C").as_deref(),
            Some(lower),
            "an uppercase sha must normalise, not merely pass"
        );
        assert_eq!(
            normalise_commit("3f2A1b4C5d6E7f8091a2B3c4D5e6F708192a3B4c").as_deref(),
            Some(lower),
            "mixed case is what a hand-edited pipeline actually sends"
        );
        assert_eq!(normalise_commit(""), None);
        assert_eq!(normalise_commit("3f2a1b4c"), None, "abbreviated");
        assert_eq!(
            normalise_commit("3f2a1b4c5d6e7f8091a2b3c4d5e6f708192a3b4cd"),
            None,
            "one too long"
        );
        assert_eq!(
            normalise_commit("3f2a1b4c5d6e7f8091a2b3c4d5e6f708192a3b4z"),
            None,
            "not hex"
        );
    }

    /// The short labels a commit-scoped report carries. Nothing here is
    /// reachable through the patchset-scoped path, so without these the
    /// new shape would be the one door in the API with no bound on what
    /// it stores and renders.
    #[test]
    fn a_label_is_bounded_and_renderable() {
        assert!(label_ok("main"));
        assert!(label_ok("release/2024-05"));
        assert!(label_ok(&"x".repeat(MAX_LABEL)));
        assert!(!label_ok(&"x".repeat(MAX_LABEL + 1)), "one over");
        assert!(!label_ok(""), "an empty label is absence, spelled wrong");
        assert!(!label_ok("main\u{0}"), "NUL");
        assert!(!label_ok("main\u{1b}[31m"), "an escape sequence");
        assert!(!label_ok("two\nlines"), "a newline is not a branch name");
    }

    /// The same rule `changes::valid_check_url` applies to the other
    /// path — asserted here because it is a second copy of it, and two
    /// copies that disagree is the failure the FINDING beside it names.
    #[test]
    fn a_detail_url_is_an_http_link_and_nothing_else() {
        assert!(detail_url_ok("https://ci.example/runs/1"));
        assert!(detail_url_ok("http://ci.example/runs/1"));
        assert!(!detail_url_ok("javascript:alert(1)"));
        assert!(!detail_url_ok("data:text/html,<script>"));
        assert!(!detail_url_ok("//ci.example/runs/1"), "protocol-relative");
        assert!(!detail_url_ok("ci.example/runs/1"), "no scheme");
        assert!(
            !detail_url_ok(&format!("https://x/{}", "y".repeat(1000))),
            "over the length bound"
        );
        assert!(
            !detail_url_ok("https://ci.example/a b"),
            "a space is not graphic ascii"
        );
        // Byte-for-byte the same verdicts the other path reaches, for
        // every case both are asked about.
        for u in [
            "https://ci.example/runs/1",
            "javascript:alert(1)",
            "ci.example/runs/1",
        ] {
            let mine = detail_url_ok(u);
            let theirs = stratum_control::changes::set_check(
                &stratum_control::ControlDb::open(&stratum_testkit::pg::test_db_url(
                    "ci-intake-url",
                ))
                .unwrap(),
                "not-an-id",
                "not-an-id",
                "ci/tests",
                "passing",
                Some(u),
                INTAKE_PRINCIPAL,
            );
            // `set_check` refuses the ids after the url, so a url it
            // accepts reaches "no such patchset" and one it refuses does
            // not. That is the observable that tells the two apart.
            let theirs_ok = matches!(&theirs, Ok(Err(e)) if e == "no such patchset");
            assert_eq!(mine, theirs_ok, "the two url rules disagree about {u:?}");
        }
    }

    #[test]
    fn a_revoked_secret_reads_as_absent_rather_than_as_the_empty_string() {
        let db =
            stratum_control::ControlDb::open(&stratum_testkit::pg::test_db_url("ci-intake-secret"))
                .unwrap();
        let org = stratum_control::registry::create_org(&db, "acme").unwrap();
        let repo = stratum_control::registry::create_repo(
            &db,
            &org.id,
            &stratum_control::registry::NewRepo {
                name: "app",
                description: None,
                kind: stratum_control::RepoKind::Native,
                public: false,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();

        assert!(
            load_secret(&db, &repo.id).unwrap().is_none(),
            "unconfigured"
        );

        let raw = serde_json::to_string(&StoredSecret {
            secret: "sekrit".into(),
            rotated_at: 7,
        })
        .unwrap();
        meta_set(&db, &secret_key(&repo.id), &raw).unwrap();
        let got = load_secret(&db, &repo.id).unwrap().expect("configured");
        assert_eq!(got.secret, "sekrit");
        assert_eq!(got.rotated_at, 7);

        // Revocation is an empty secret, and an empty secret is not a
        // secret. Without this, `sign("", body)` is a perfectly valid
        // HMAC that anybody can compute.
        let raw = serde_json::to_string(&StoredSecret {
            secret: String::new(),
            rotated_at: 8,
        })
        .unwrap();
        meta_set(&db, &secret_key(&repo.id), &raw).unwrap();
        assert!(load_secret(&db, &repo.id).unwrap().is_none(), "revoked");

        // A row that is not the shape we wrote is an error, not a
        // silently absent secret and certainly not a silently valid one.
        meta_set(&db, &secret_key(&repo.id), "{{{").unwrap();
        assert!(load_secret(&db, &repo.id).is_err());
    }

    #[test]
    fn the_two_meta_keys_are_per_repo_and_do_not_collide() {
        assert_eq!(secret_key("01abc"), "ci-intake-secret:01abc");
        assert_eq!(ring_key("01abc"), "ci-intake-seen:01abc");
        assert_ne!(secret_key("01abc"), ring_key("01abc"));
        assert_ne!(secret_key("01abc"), secret_key("01abd"));
    }
}
