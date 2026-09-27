//! Public profiles, and the addresses behind them.
//!
//! `/v1/users/…` is a new top-level family, and the thing to keep
//! straight while reading it is that the routes in this one file have
//! three different audiences:
//!
//! * **Anybody** reads a profile, its links, its pins and its public
//!   repository count. That is the point of the surface — a logged-out
//!   visitor has to see it, which is the whole "public and
//!   unauthenticated by default" promise.
//! * **Only its owner** reads or writes the address list. It is the
//!   authorship-linkage surface: whoever knows a person's `git config
//!   user.email` values knows exactly what to put in a commit to look
//!   like them, and knows a mailbox to spam.
//! * **An org administrator** writes an org's profile, which anybody
//!   reads.
//!
//! Every self-only handler routes through [`require_self`], so "who is
//! allowed" is one function rather than a habit repeated eight times.
//! Nothing here re-derives repository visibility: pins come back through
//! [`stratum_control::profiles::pins`] with a
//! [`Viewer`](stratum_control::registry::Viewer), which is the same rule
//! `api/search.rs` uses, and the count published is a count of public
//! repositories for every caller alike, so it cannot be watched to learn
//! that a private one exists.

use crate::api::{internal, json_error};
use crate::app::SharedState;
use crate::authx;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use stratum_control::profiles::{self, AddEmail, Link, OrgProfileUpdate, Profile, ProfileUpdate};
use stratum_control::registry::Viewer;

/// `None` for an absent key, `Some(None)` for an explicit `null`.
///
/// `#[serde(default)]` on an `Option<Option<T>>` collapses both to
/// `None`, which would make clearing a bio impossible to express. This
/// distinguishes them, which is what the control plane's patch shape
/// needs.
#[derive(Debug, Default, Clone)]
pub struct Patch<T>(pub Option<Option<T>>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(d).map(|v| Patch(Some(v)))
    }
}

#[derive(Deserialize, Default)]
pub struct PatchProfileBody {
    #[serde(default)]
    pub display_name: Patch<String>,
    #[serde(default)]
    pub bio: Patch<String>,
    #[serde(default)]
    pub location: Patch<String>,
    #[serde(default)]
    pub company: Patch<String>,
    #[serde(default)]
    pub pronouns: Patch<String>,
    #[serde(default)]
    pub contrib_private_optin: Option<bool>,
    #[serde(default)]
    pub profile_repo: Patch<String>,
    /// Replaces the list outright when present.
    #[serde(default)]
    pub links: Option<Vec<LinkBody>>,
}

#[derive(Deserialize)]
pub struct LinkBody {
    #[serde(default)]
    pub label: Option<String>,
    pub url: String,
}

#[derive(Deserialize)]
pub struct PinsBody {
    pub pins: Vec<PinBody>,
}

/// A pin names a repository the way a URL does — namespace and name —
/// rather than by id. An id is not something anyone has in hand, and a
/// pin list is built by clicking repositories in a picker.
#[derive(Deserialize)]
pub struct PinBody {
    pub org: String,
    pub repo: String,
    /// Only `repo` in this slice. Present so the refusal for anything
    /// else is explicit rather than a silently ignored field.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Deserialize)]
pub struct EmailBody {
    pub email: String,
}

#[derive(Deserialize)]
pub struct VerifyBody {
    pub token: String,
}

#[derive(Deserialize, Default)]
pub struct PatchOrgProfileBody {
    #[serde(default)]
    pub display_name: Patch<String>,
    #[serde(default)]
    pub description: Patch<String>,
    #[serde(default)]
    pub location: Patch<String>,
    #[serde(default)]
    pub website: Patch<String>,
    #[serde(default)]
    pub contact_email: Patch<String>,
}

/// The viewer for a visibility-filtered read.
fn viewer_of(caller: &Option<String>) -> Viewer<'_> {
    match caller {
        Some(user_id) => Viewer::User(user_id),
        None => Viewer::Anonymous,
    }
}

/// Resolve a handle to a profile, or 404.
fn profile_or_404(state: &SharedState, handle: &str) -> Result<(Profile, String), Response> {
    match profiles::by_handle(&state.db, handle) {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err(authx::not_found()),
        Err(e) => Err(internal(e)),
    }
}

/// The gate on every self-only route.
///
/// A handle is a public URL, so refusing with 403 rather than 404 leaks
/// nothing that `GET /v1/users/:handle` does not already answer to
/// anyone — and it is the honest answer, which is what lets somebody
/// signed in as the wrong account work out why their request failed
/// instead of concluding the page is broken.
///
/// Anonymous is 401, because a browser can act on that: sign in.
fn require_self(
    state: &SharedState,
    headers: &HeaderMap,
    handle: &str,
) -> Result<(Profile, String), Response> {
    let (profile, org_id) = profile_or_404(state, handle)?;
    match crate::api::caller_person(state, headers)? {
        Some(uid) if uid == profile.user_id => Ok((profile, org_id)),
        Some(_) => Err(authx::forbidden("this is somebody else's account")),
        None => Err(authx::unauthorized(authx::Challenge::None)),
    }
}

fn profile_json(p: &Profile, links: &[Link], public_repos: i64) -> serde_json::Value {
    serde_json::json!({
        "handle": p.handle,
        "name": p.name,
        "display_name": p.display_name,
        "bio": p.bio,
        "location": p.location,
        "company": p.company,
        "pronouns": p.pronouns,
        "kind": p.kind.as_str(),
        "contrib_private_optin": p.contrib_private_optin,
        "profile_repo": p.profile_repo,
        "created_at": p.created_at,
        "links": links,
        "public_repos": public_repos,
    })
}

/// `GET /v1/users/:handle` — public.
pub async fn get(State(state): State<SharedState>, Path(handle): Path<String>) -> Response {
    let (profile, org_id) = match profile_or_404(&state, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let links = match profiles::links(&state.db, &profile.user_id) {
        Ok(l) => l,
        Err(e) => return internal(e),
    };
    let count = match profiles::public_repo_count(&state.db, &org_id) {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    Json(profile_json(&profile, &links, count)).into_response()
}

/// `PATCH /v1/users/:handle` — the account itself, and nobody else.
pub async fn patch(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PatchProfileBody>,
) -> Response {
    let (profile, org_id) = match require_self(&state, &headers, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let update = ProfileUpdate {
        display_name: body.display_name.0,
        bio: body.bio.0,
        location: body.location.0,
        company: body.company.0,
        pronouns: body.pronouns.0,
        contrib_private_optin: body.contrib_private_optin,
        profile_repo: body.profile_repo.0,
        links: body.links.map(|links| {
            links
                .into_iter()
                .map(|l| Link {
                    label: l.label,
                    url: l.url,
                })
                .collect()
        }),
    };
    if let Err(e) = profiles::update(&state.db, &profile.user_id, &update) {
        // Everything the control plane refuses here is something the
        // caller typed and can retype — a cap, a control character, a
        // scheme we will not render as a link.
        return json_error(StatusCode::BAD_REQUEST, e);
    }
    let (profile, _) = match profile_or_404(&state, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let links = match profiles::links(&state.db, &profile.user_id) {
        Ok(l) => l,
        Err(e) => return internal(e),
    };
    let count = match profiles::public_repo_count(&state.db, &org_id) {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    Json(profile_json(&profile, &links, count)).into_response()
}

/// `GET /v1/users/:handle/pins` — public, filtered to what the caller
/// may see.
pub async fn get_pins(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (_, org_id) = match profile_or_404(&state, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let caller = match crate::api::caller_person(&state, &headers) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match profiles::pins(&state.db, &org_id, &viewer_of(&caller)) {
        Ok(pins) => Json(serde_json::json!({ "pins": pins })).into_response(),
        Err(e) => internal(e),
    }
}

/// `PUT /v1/users/:handle/pins` — the account itself.
///
/// Repositories are resolved **through the caller's own visibility**
/// before the control plane is asked to pin them. That is what keeps the
/// two refusals apart honestly: a private repository the caller can see
/// answers "a private repository cannot be pinned", which is actionable;
/// one they cannot see answers "no such repository", which is the same
/// thing a name that does not exist answers. Resolving first and
/// refusing after would have made the pin form an existence oracle for
/// every private repository on the platform.
pub async fn put_pins(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PinsBody>,
) -> Response {
    let (profile, org_id) = match require_self(&state, &headers, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if body.pins.len() > profiles::MAX_PINS {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("at most {} pinned items", profiles::MAX_PINS),
        );
    }
    let viewer = Viewer::User(&profile.user_id);
    let mut ids = Vec::with_capacity(body.pins.len());
    for pin in &body.pins {
        match pin.kind.as_deref() {
            None | Some("repo") => {}
            Some(other) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    format!("cannot pin {other:?}; only repositories can be pinned"),
                )
            }
        }
        match visible_repo_id(&state, &profile.user_id, &pin.org, &pin.repo) {
            Ok(Some(id)) => ids.push(id),
            Ok(None) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    format!("no such repository {}/{}", pin.org, pin.repo),
                )
            }
            Err(r) => return r,
        }
    }
    match profiles::set_pins(&state.db, &org_id, &ids) {
        Ok(()) => match profiles::pins(&state.db, &org_id, &viewer) {
            Ok(pins) => Json(serde_json::json!({ "pins": pins })).into_response(),
            Err(e) => internal(e),
        },
        Err(e) => json_error(StatusCode::BAD_REQUEST, e),
    }
}

/// A repository id, but only if this **person** may know the repository
/// exists. `Ok(None)` covers "no such namespace", "no such repository"
/// and "not yours to see" alike, which is the masking `repo_or_masked`
/// applies on the repo routes, expressed for a caller who is naming
/// somebody else's namespace.
///
/// A person, and not a [`Viewer`]: [`put_pins`] is the only caller and
/// it always asks on behalf of the account whose pins are being written,
/// which `require_self` has already established is a person. Taking the
/// wider type meant carrying an anonymous arm and an org-token arm that
/// nothing could ever reach — arms no test could pin, on the one seam
/// here that decides whether a private repository's existence leaks.
fn visible_repo_id(
    state: &SharedState,
    user_id: &str,
    org_name: &str,
    repo_name: &str,
) -> Result<Option<String>, Response> {
    use stratum_control::registry;
    let org = match registry::org_by_name(&state.db, org_name) {
        Ok(Some(o)) => o,
        Ok(None) => return Ok(None),
        Err(e) => return Err(internal(e)),
    };
    let repo = match registry::repo_by_name(&state.db, &org.id, repo_name) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(None),
        Err(e) => return Err(internal(e)),
    };
    if repo.public {
        return Ok(Some(repo.id));
    }
    // Private: only a member of the holding namespace may learn it is
    // there. Same predicate as search's `r.org_id = ANY(orgs_of(you))`.
    let member = stratum_control::members::orgs_of(&state.db, user_id)
        .map_err(internal)?
        .contains(&org.id);
    Ok(member.then_some(repo.id))
}

/// `GET /v1/users/:handle/emails` — the account itself, never public.
pub async fn list_emails(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (profile, _) = match require_self(&state, &headers, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match profiles::list_emails(&state.db, &profile.user_id) {
        Ok(rows) => Json(serde_json::json!({ "emails": rows })).into_response(),
        Err(e) => internal(e),
    }
}

/// `POST /v1/users/:handle/emails` — claim an address and mail its
/// holder a link.
///
/// Unlike signup, the answers here are *not* uniform, and that is
/// deliberate: the caller has already authenticated as this account, so
/// "that address is spoken for" tells them about their own request
/// rather than about somebody else's existence. What the 409 must never
/// grow is a name — whose address it is, is exactly what a caller must
/// not be able to ask, and [`AddEmail::Taken`] carries nowhere to put it.
pub async fn add_email(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
    Json(body): Json<EmailBody>,
) -> Response {
    let (profile, _) = match require_self(&state, &headers, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let added = match profiles::add_email(&state.db, &profile.user_id, &body.email) {
        Ok(a) => a,
        Err(e) => return internal(e),
    };
    match added {
        AddEmail::Added(token) => {
            let address = stratum_control::users::normalize_email(&body.email);
            // The link is the only proof, so it goes to the mailbox and
            // never into the response — a claim confirmed over the API
            // would prove nothing at all.
            if let Err(e) = state
                .mailer
                .send(&crate::mail::templates::address_verification(
                    &address,
                    &profile.handle,
                    &state.public_url,
                    &token,
                ))
            {
                eprintln!("weft: mail to {address}: {e}");
            }
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "address": address,
                    "status": "check your email",
                    "detail": "Confirm the address from the link we sent it. \
                               Until then it counts for nothing.",
                })),
            )
                .into_response()
        }
        AddEmail::AlreadyYours => json_error(
            StatusCode::CONFLICT,
            "that address is already on this account",
        ),
        AddEmail::Taken => json_error(StatusCode::CONFLICT, "that address is already in use"),
        AddEmail::Invalid(e) => json_error(StatusCode::BAD_REQUEST, e),
    }
}

/// `POST /v1/users/:handle/emails/verify` — spend a link.
pub async fn verify_email(
    State(state): State<SharedState>,
    Path(handle): Path<String>,
    headers: HeaderMap,
    Json(body): Json<VerifyBody>,
) -> Response {
    let (profile, _) = match require_self(&state, &headers, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match profiles::verify_email(&state.db, &profile.user_id, &body.token) {
        Ok(Some(address)) => Json(serde_json::json!({
            "address": address,
            "verified": true,
        }))
        .into_response(),
        // Every wrong shape is one answer, so a link cannot be used to
        // ask which addresses are registered here.
        Ok(None) => json_error(
            StatusCode::NOT_FOUND,
            "this confirmation link is not valid any more",
        ),
        Err(e) => internal(e),
    }
}

/// `DELETE /v1/users/:handle/emails/:email`
pub async fn delete_email(
    State(state): State<SharedState>,
    Path((handle, email)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (profile, _) = match require_self(&state, &headers, &handle) {
        Ok(x) => x,
        Err(r) => return r,
    };
    match profiles::remove_email(&state.db, &profile.user_id, &email) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        // The sign-in address and an address that is not theirs are the
        // same answer. Removing the credential would leave an account
        // nobody can authenticate or reach, and it is refused with the
        // reason named because it is the caller's own account.
        Ok(false) => json_error(
            StatusCode::NOT_FOUND,
            "no such address on this account, or it is the address you sign in with",
        ),
        Err(e) => internal(e),
    }
}

/// `GET /v1/orgs/:org/profile` — public.
pub async fn get_org_profile(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let profile = match profiles::org_profile(&state.db, &org.id) {
        Ok(p) => p,
        Err(e) => return internal(e),
    };
    let count = match profiles::public_repo_count(&state.db, &org.id) {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    Json(serde_json::json!({
        "org": org.name,
        "display_name": profile.display_name,
        "description": profile.description,
        "location": profile.location,
        "website": profile.website,
        "contact_email": profile.contact_email,
        "public_repos": count,
    }))
    .into_response()
}

/// `PATCH /v1/orgs/:org/profile` — an administrator of that org.
pub async fn patch_org_profile(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PatchOrgProfileBody>,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    // `require` is the seam every other org-admin route uses: it refuses
    // a foreign caller by masking, a member without the scope by
    // forbidding, and an anonymous one by challenging.
    if let Err(r) = authx::require(
        &state.db,
        &headers,
        &org.id,
        None,
        stratum_control::auth::Scope::OrgAdmin,
    ) {
        return r;
    }
    let patch = OrgProfileUpdate {
        display_name: body.display_name.0,
        description: body.description.0,
        location: body.location.0,
        website: body.website.0,
        contact_email: body.contact_email.0,
    };
    if let Err(e) = profiles::set_org_profile(&state.db, &org.id, &patch) {
        return json_error(StatusCode::BAD_REQUEST, e);
    }
    get_org_profile(State(state), Path(org_name)).await
}
