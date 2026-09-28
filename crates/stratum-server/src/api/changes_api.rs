//! Changes API: register a branch tip as a change, read its patchsets
//! and approvals, approve/unapprove, and ask for the sufficiency
//! verdict. Landing lives beside this in the land endpoints; both feed
//! the same engine the owners preview uses, so a preview and a landing
//! can never disagree about the rules.

use crate::api::{internal, json_error, owners_api, reads};
use crate::app::SharedState;
use crate::review::owners::RuleOutcome;
use crate::review::sufficiency::{self, Approver, ApproverSet, Verdict};
use crate::review::{change_id, load, owners, resolve};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use stratum_control::audit::AuditCtx;
use stratum_control::auth::Scope;
use stratum_control::changes::{self, Change, Patchset};
use stratum_engine::objwrite::{self, OBJ_COMMIT};
use stratum_engine::treediff;

#[derive(Deserialize)]
pub struct CreateChange {
    /// The rev (usually a branch) whose tip becomes the patchset.
    pub from: String,
    /// Branch the change intends to land on; the repo default if absent.
    pub target: Option<String>,
    /// `owner/name` of a **fork** the commits live in, when they do not
    /// live in the repository being targeted.
    ///
    /// This is how somebody with no push credential contributes at all:
    /// they fork the project, push to the repository they own, and
    /// open a change here. Absent means what it always meant — the
    /// commits are already in the target — so nothing about the
    /// enterprise flow changes.
    #[serde(default)]
    pub source: Option<String>,
}

/// A target branch is stored, then resolved as `refs/heads/<target>` at
/// landing time. Same shape discipline as everywhere: hostile bytes are
/// refused at the door, not carried around.
fn valid_target(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("..")
}

/// Resolve a change's source fork to `owner/name`, for the wire.
///
/// An id is no use to a client: the sentence GitHub prints — "bob wants
/// to merge 1 commit into ada:main from bob:fix-empty-config" — is built
/// from names, and a client holding an id would have to look it up.
///
/// `None` when the change did not come from a fork, and also when the
/// fork has since been deleted: `changes.source_repo_id` is
/// `ON DELETE SET NULL`, because a landed change whose source was later
/// removed is still a real thing that happened.
pub(crate) fn source_name(state: &SharedState, c: &Change) -> Option<String> {
    let id = c.source_repo_id.as_deref()?;
    let repo = stratum_control::registry::repo_by_id_any(&state.db, id)
        .ok()
        .flatten()?;
    let org = stratum_control::registry::org_by_id(&state.db, &repo.org_id)
        .ok()
        .flatten()?;
    Some(format!("{}/{}", org.name, repo.name))
}

pub(crate) fn change_json(
    c: &Change,
    latest: Option<&Patchset>,
    source: Option<String>,
) -> serde_json::Value {
    serde_json::json!({
        "key": c.change_key,
        "title": c.title,
        "target_branch": c.target_branch,
        // `owner/name` of the fork these commits live in, or null when
        // they are already in the target repository.
        "source": source,
        "state": c.state,
        "land_verdict": c.land_verdict,
        "landed_commit": c.landed_commit,
        "created_at": c.created_at,
        "updated_at": c.updated_at,
        "patchset": latest.map(patchset_json),
    })
}

fn patchset_json(p: &Patchset) -> serde_json::Value {
    serde_json::json!({
        "number": p.number,
        "commit": p.commit_oid,
        "parent": p.parent_oid,
        "message": p.message,
        "created_at": p.created_at,
    })
}

/// Resolve a `source` fork name to the repository its objects live in.
///
/// Three refusals, and they are different questions on purpose:
///
/// * the name must parse as `owner/name` — a bare name is ambiguous
///   between "a repo in this org" and "a namespace", and guessing is how
///   somebody proposes from a repository they did not mean;
/// * the caller must be able to **read** it, checked through the same
///   `rest_repo_auth` every other surface uses, so a private fork is
///   masked here exactly as it is everywhere else. Without this a
///   stranger could name any private repository and learn from the
///   error whether it exists;
/// * it must actually be a fork of the repository being targeted.
///   Not merely tidiness: landing copies objects out of this prefix into
///   the target's, so an arbitrary repository named here is a request to
///   move somebody else's bytes into a project they do not own.
fn resolve_source(
    state: &SharedState,
    headers: &HeaderMap,
    source: Option<&str>,
    target: &stratum_control::registry::Repo,
) -> Result<Option<stratum_control::registry::Repo>, Response> {
    let Some(name) = source else {
        return Ok(None);
    };
    let Some((owner, repo)) = name.split_once('/') else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            format!("source {name:?} must be \"owner/name\""),
        ));
    };
    let (_, src, _) = crate::app::rest_repo_auth(state, headers, owner, repo, Scope::RepoRead)?;
    // The fork link is the authority, not the caller's word for it.
    let is_fork = stratum_control::forks::parent_of(&state.db, &src.id)
        .map_err(internal)?
        .map(|(_, parent_id)| parent_id == target.id)
        .unwrap_or(false);
    if !is_fork {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            format!("{name} is not a fork of this repository"),
        ));
    }
    Ok(Some(src))
}

/// POST /changes — register `from`'s tip commit as a change/patchset.
pub async fn create(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<CreateChange>,
) -> Response {
    // **`RepoRead`, not `RepoWrite`, and the difference is the feature.**
    //
    // A change proposes; it does not write. Requiring write access to
    // propose one meant somebody who may read a repository but not push
    // to it — a viewer, or a member held down to viewer there — could
    // never contribute to it at all. The write check moves
    // to where a write actually happens: opening a change against a
    // repository you cannot push to is allowed, and landing it is still
    // governed by OWNERS sufficiency and the land queue exactly as
    // before.
    //
    // A change with no `source` is refused below unless the caller can
    // write, because those commits are already in this repository and
    // could only have got there through a push that was authorised.
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let can_write =
        crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite).is_ok();
    // Review needs somewhere to land; a mirror's trunk belongs to its
    // origin. Refused here at the door, so no change row can ever exist
    // on a mirror and no later surface needs its own guard.
    if repo_row.kind == stratum_control::RepoKind::Mirror {
        return json_error(StatusCode::FORBIDDEN, "mirrors are read-only");
    }
    let target = body
        .target
        .clone()
        .unwrap_or_else(|| repo_row.default_branch.clone());
    if !valid_target(&target) {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid target {target:?}"),
        );
    }
    // Where the commits actually are. A fork's objects live in the
    // fork's own prefix, so `from` must be resolved against that plane
    // and not against the target's — resolving it against the target
    // would answer "unknown rev" for a branch that plainly exists.
    let source = match resolve_source(&state, &headers, body.source.as_deref(), &repo_row) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if source.is_none() && !can_write {
        return json_error(
            StatusCode::FORBIDDEN,
            "opening a change from this repository needs write access — to \
             contribute without it, fork the repository, push there, and open \
             the change with `source` naming your fork",
        );
    }
    let prefix = match &source {
        Some(src) => src.prefix().as_str().to_string(),
        None => repo_row.prefix().as_str().to_string(),
    };
    let source_repo_id = source.as_ref().map(|r| r.id.clone());
    let from = body.from.clone();
    let out = reads::with_reader(&state, prefix, move |reader| {
        let tip = reader
            .resolve_rev(&from)?
            .ok_or_else(|| format!("unknown rev {from:?}"))?;
        let (k, data) = reader.object(&tip)?;
        if k != OBJ_COMMIT {
            return Err(format!("{tip} is not a commit"));
        }
        let c = objwrite::parse_commit(&data)?;
        Ok((tip, c.parents.first().cloned(), c.message))
    })
    .await;
    let (tip, parent, message) = match out {
        Ok(x) => x,
        Err(e) => return reads::err_to_response(e),
    };
    // The commits arrive **now**, not at landing time.
    //
    // GitHub's shape, for the same reason: it writes `refs/pull/N/head`
    // in the upstream repository the moment a pull request is opened, so
    // everything downstream — the diff, OWNERS resolution, the land
    // verdict — reads one repository rather than two. Deferring this to
    // the lander was the first attempt and it failed exactly where you
    // would expect: `approve` computes a verdict, the verdict resolves
    // OWNERS at the patchset commit, and that commit was still in the
    // fork. The error was `not in this layout`, which is true and
    // useless to the maintainer reading it.
    //
    // Idempotent, so a second patchset on the same change moves only
    // what is new, and a re-post of the same one moves nothing.
    //
    // **This is O(repo) and inline**, which is the honest cost of doing
    // it correctly today: both planes are materialised to move a handful
    // of objects. It belongs in a job the way forking is, and the
    // machinery for that already exists — that is a follow-up, and the
    // reason it has not been done yet is that a change that is open but
    // whose objects have not arrived is a state every read surface would
    // have to understand.
    if let Some(src) = &source {
        if let Err(e) = crate::review::transplant::ensure_present(
            &state,
            Some(src.clone()),
            &repo_row,
            &tip,
            &change_id::key_for(&message, &tip),
        )
        .await
        {
            return internal(format!(
                "could not bring the fork's commits into this repository: {e}"
            ));
        }
    }
    match register_patchset(
        &state,
        &org_row,
        &repo_row,
        Some(&principal),
        &target,
        &tip,
        parent.as_deref(),
        &message,
        source_repo_id.as_deref(),
    )
    .await
    {
        Ok((change, patchset, new)) => {
            let status = if new {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(serde_json::json!({
                    "change": change_json(&change, Some(&patchset), source_name(&state, &change)),
                    "patchset": patchset_json(&patchset),
                })),
            )
                .into_response()
        }
        Err(r) => r,
    }
}

/// Record `tip` as a patchset of the change its message names, on
/// `target`: pin the commit, write the row, give it its CI and tell the
/// people OWNERS names. Answers the change, the patchset and whether the
/// change is new. Shared by `POST …/changes` and by the changeset revert,
/// which makes its commits and then registers them exactly as if a
/// person had pushed them.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn register_patchset(
    state: &SharedState,
    org_row: &stratum_control::registry::Org,
    repo_row: &stratum_control::registry::Repo,
    principal: Option<&stratum_control::auth::Principal>,
    target: &str,
    tip: &str,
    parent: Option<&str>,
    message: &str,
    source_repo_id: Option<&str>,
) -> Result<(Change, Patchset, bool), Response> {
    let key = change_id::key_for(message, tip);
    let title = message.lines().next().unwrap_or("").trim().to_string();
    let title = if title.is_empty() { key.clone() } else { title };

    // **Pin the commit before recording that we reviewed it.**
    //
    // A patchset row names a commit by oid, and nothing else in the
    // layout has to keep that commit alive: a review branch that is
    // rebased, reset or deleted leaves it on no ref at all. Compaction
    // then rebuilds the epoch by re-ingesting a seed materialized from
    // `manifest.refs`, so the commit is simply not in the new epoch, and
    // GC drops the old one once no pointer names it. Neither collector
    // consults the review tables. The change page goes on saying
    // "patchset 1 was <oid>" with nothing behind the oid — the reviewer
    // cannot see what they were shown, and the record of the review
    // becomes a set of dangling references. It happens minutes later, at
    // a WAL threshold nobody is watching.
    //
    // Keyed by **oid rather than by patchset number**, which is what
    // makes the ordering safe: the number is allocated inside the
    // database transaction below, so a name that needed it could only be
    // written afterwards, leaving exactly the window this exists to
    // close. An oid needs nothing allocated, so the pin lands first and
    // the row is only ever recorded against an object already held down.
    // It is also idempotent — re-registering the same commit rewrites
    // the same ref — and shared, when two changes name one commit.
    //
    // `Expect::Any` for the same reason: this is a pin, not a claim.
    let pin = format!("refs/patchsets/{tip}");
    let store_url = state.store_url.clone();
    let prefix = repo_row.prefix().as_str().to_string();
    let tip_for_pin = tip.to_string();
    let pinned = tokio::task::spawn_blocking(move || {
        let store = stratum_store::ObjectStore::new(&store_url, stratum_store::LatencyModel::None);
        stratum_engine::refops::transact(
            &store,
            &prefix,
            &[stratum_engine::refops::RefUpdate {
                name: pin,
                expect: stratum_engine::refops::Expect::Any,
                new: Some(tip_for_pin),
            }],
            None,
        )
        .map(|_| ())
        .map_err(|e| match e {
            stratum_engine::refops::TxnError::Conflict(n, _) => format!("pin {n}"),
            stratum_engine::refops::TxnError::Other(e) => e,
        })
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")));
    if let Err(e) = pinned {
        return Err(internal(format!(
            "could not pin the patchset's commit: {e}"
        )));
    }

    let actx = AuditCtx::of(&org_row.id, principal);
    match changes::create_or_update(
        &state.db,
        &org_row.id,
        &repo_row.id,
        &key,
        &title,
        target,
        tip,
        parent,
        message,
        principal.and_then(|p| p.user_id.as_deref()),
        source_repo_id,
        Some(&actx),
    ) {
        Ok(Ok((change, patchset, new))) => {
            // The patchset is recorded and pinned; now it gets its CI.
            // A re-post of the same tip finds the run that already
            // exists, so this is safe to call every time.
            crate::workflow::trigger::on_change(
                state,
                repo_row,
                &change.change_key,
                &change.target_branch,
                tip,
                change.source_repo_id.is_some(),
            )
            .await;
            // And the changeset's, if this change is in one. A new
            // patchset is a new combination, so every member's composed
            // run is stale — not only this one's — which is why the
            // whole changeset is retriggered rather than one member.
            retrigger_changeset(state, &change.id).await;
            // Told after the write, never instead of it: the enqueue is
            // one INSERT and its failure is logged, because a change
            // that was created has been created and a mail problem must
            // not be reported as a review problem.
            if let Some(actor) = principal.and_then(|p| p.user_id.as_deref()) {
                crate::workers::notifier::enqueue(
                    state,
                    &change,
                    crate::workers::notifier::Event::Opened,
                    actor,
                );
            }
            Ok((change, patchset, new))
        }
        Ok(Err(e)) => Err(json_error(StatusCode::CONFLICT, e)),
        Err(e) => Err(internal(e)),
    }
}

/// The `?q=` grammar, in the words a refusal quotes back.
///
/// One string, so the sentence a client is told and the terms this file
/// actually implements cannot drift: a refusal that lists a filter the
/// parser does not know is worse than no help at all.
const Q_GRAMMAR: &str = "understood terms are is:open, is:landing, is:landed, is:abandoned, \
     author:@me, author:<email>, needs:my-approval and repo:<name>";

/// How many rows one `needs:my-approval` request may examine.
///
/// See [`filters`]: each row costs a store read, so this page is capped
/// far below the ordinary 500 and the client walks with `next`.
const NEEDS_PAGE_MAX: i64 = 50;

/// One `?q=` query, as written.
///
/// Parsed, never guessed at: **an unrecognised term is refused in words
/// and names itself.** A filter that quietly does nothing is how somebody
/// reads an unfiltered list of forty changes, concludes none of them is
/// theirs, and closes the tab — and the same query in a shared URL then
/// means something different to whoever opens it next. The query bar is
/// the API (see `web/FORGE-UX.md` §2), and an API that silently ignores
/// half its input is not one.
#[derive(Debug, Default, PartialEq, Eq)]
struct Terms {
    state: Option<String>,
    /// `@me`, or an email, exactly as typed. Turning it into a person is
    /// a question about the database *and* about the caller's
    /// credential, and neither is knowable here — which is what keeps
    /// this function pure and every arm of the grammar unit-tested with
    /// no server behind it.
    author: Option<String>,
    needs_my_approval: bool,
    repo: Option<String>,
}

/// Parse `?q=`. `Err` is the sentence the 400 carries.
///
/// Whitespace-separated `name:value` terms, and nothing else — no
/// quoting, because none of these values can contain a space, and a
/// quoting rule nothing exercises is a rule that will be wrong the first
/// time something needs it.
fn parse_q(q: &str) -> Result<Terms, String> {
    let mut t = Terms::default();
    for term in q.split_whitespace() {
        let Some((name, value)) = term.split_once(':') else {
            return Err(format!(
                "{term:?} is not a filter — a term is name:value, and {Q_GRAMMAR}"
            ));
        };
        // Said once per term rather than at each arm: "given twice" is
        // the same mistake whichever filter it is, and two `is:` terms
        // cannot both hold, so answering with the second silently
        // discards the first.
        let twice = || Err(format!("{name}: is given twice in {q:?}"));
        match name {
            "is" => {
                if t.state.is_some() {
                    return twice();
                }
                if !matches!(value, "open" | "landing" | "landed" | "abandoned") {
                    return Err(format!("unknown state {value:?} in {term:?} — {Q_GRAMMAR}"));
                }
                t.state = Some(value.to_string());
            }
            "author" => {
                if t.author.is_some() {
                    return twice();
                }
                // `@me` or an address. A shape that is neither is a typo,
                // and a typo that filtered to nothing would look exactly
                // like "nobody has anything open".
                if value != "@me" && !stratum_control::users::valid_email(value) {
                    return Err(format!(
                        "author: takes @me or an email address, not {value:?}"
                    ));
                }
                t.author = Some(value.to_string());
            }
            "needs" => {
                // Refused even though a second one would mean the same
                // thing: a query somebody typed twice is a query they
                // did not read back, and the one grammar rule here is
                // that nothing is quietly tolerated.
                if t.needs_my_approval {
                    return twice();
                }
                if value != "my-approval" {
                    return Err(format!(
                        "needs: takes my-approval, not {value:?} — {Q_GRAMMAR}"
                    ));
                }
                t.needs_my_approval = true;
            }
            "repo" => {
                if t.repo.is_some() {
                    return twice();
                }
                if !stratum_control::registry::valid_name(value) {
                    return Err(format!("{value:?} is not a repository name"));
                }
                t.repo = Some(value.to_string());
            }
            _ => return Err(format!("unknown filter {name:?} in {term:?} — {Q_GRAMMAR}")),
        }
    }
    Ok(t)
}

/// Which people a listing is about, once `author:` has been resolved.
enum Who {
    /// No `author:` term.
    Anyone,
    Someone(String),
    /// An address that names no account here. An empty page, not an
    /// unfiltered one — and deliberately not a refusal: answering "no
    /// such user" would turn this list into an address oracle for
    /// anybody who can read one repository.
    Nobody,
}

/// `?q=`, `?state=`, `?after=` and `?limit=`, resolved against this
/// caller.
struct Filters {
    state: Option<String>,
    author: Who,
    /// The caller's own user id, when `needs:my-approval` was asked for.
    /// `None` is what keeps the OWNERS resolution off the page for every
    /// caller who did not ask — see [`only_awaiting`].
    needs: Option<String>,
    /// `repo:` as written; the org-wide list resolves it against what
    /// this caller may read, and the per-repo list refuses it.
    repo: Option<String>,
    after: Option<String>,
    limit: i64,
}

fn filters(
    params: &HashMap<String, String>,
    principal: &stratum_control::auth::Principal,
    db: &stratum_control::db::ControlDb,
    default_limit: i64,
) -> Result<Filters, Response> {
    let terms = match params.get("q") {
        Some(q) => parse_q(q).map_err(|e| json_error(StatusCode::BAD_REQUEST, e))?,
        None => Terms::default(),
    };
    // `?state=` predates the query bar and stays: it is what every
    // existing client sends. Both together must agree — answering one
    // and discarding the other is the silent-filter failure this whole
    // parser exists to refuse, and there is no reading of
    // `?state=open&q=is:landed` that is not a bug in the caller.
    let from_param = params.get("state").map(String::as_str);
    if let Some(s) = from_param {
        if !matches!(s, "open" | "landing" | "landed" | "abandoned") {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                format!("unknown state {s:?}"),
            ));
        }
    }
    let state = match (from_param, terms.state.as_deref()) {
        (Some(a), Some(b)) if a != b => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                format!("?state={a} and q=is:{b} ask for different things"),
            ))
        }
        (a, b) => b.or(a).map(str::to_string),
    };

    // A person, for the two terms that are about one. A service token is
    // refused rather than answered with an empty page: a token has no
    // reviews waiting on it, and "no results" would read as "nothing to
    // do" to whatever is scripting it.
    let me = |what: &str| -> Result<String, Response> {
        acting_user(
            principal,
            &format!("{what} is about a person, and a service token is nobody's reviewer"),
        )
    };
    let author = match terms.author.as_deref() {
        None => Who::Anyone,
        Some("@me") => Who::Someone(me("author:@me")?),
        Some(email) => match stratum_control::users::by_email(db, email) {
            Ok(Some(u)) => Who::Someone(u.id),
            Ok(None) => Who::Nobody,
            Err(e) => return Err(internal(e)),
        },
    };
    let needs = match terms.needs_my_approval {
        true => Some(me("needs:my-approval")?),
        false => None,
    };

    // A cursor this list did not mint is a client bug, and refusing it
    // says so where silently answering page one would look like the walk
    // restarting on its own.
    let after = match params.get("after") {
        Some(a) if !stratum_control::ids::valid_id(a) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                format!("{a:?} is not a cursor; pass the `next` this list answered with"),
            ))
        }
        other => other.cloned(),
    };
    let limit: i64 = params
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(default_limit)
        .clamp(1, 500);
    // `needs:my-approval` costs one resolution of the target branch's
    // OWNERS tree — a store read — per row *examined*, so the window it
    // is allowed to examine is a fraction of what an ordinary page may
    // ask for. The consequence is deliberate and documented: such a page
    // can come back shorter than `limit`, or empty, with `next` set, and
    // the client keeps walking. A short page is honest; a request that
    // reads five hundred trees to fill one is not.
    let limit = if needs.is_some() {
        limit.min(NEEDS_PAGE_MAX)
    } else {
        limit
    };
    Ok(Filters {
        state,
        author,
        needs,
        repo: terms.repo,
        after,
        limit,
    })
}

/// The rows of `page`, cut down to the ones this person is *required* to
/// approve and has not yet approved.
///
/// This is the term no other forge can answer, and the reason is
/// structural rather than clever: on GitHub the reviewer set is
/// nominated, so "waiting on me" can only mean "somebody typed my name".
/// Here it is *derived* from the OWNERS files governing the paths the
/// patchset touches, so the question has an answer even when nobody has
/// done anything.
///
/// Four conditions, and each drops a change that would otherwise be
/// noise in the one list somebody is supposed to be able to trust:
///
/// - it is still `open` — a landing, landed or abandoned change is not
///   waiting on anybody;
/// - it is **not yet landable**, so a change that already has everything
///   it needs is not still asking for it;
/// - OWNERS *requires* the caller for a path it touches. Not "the caller
///   has write access": where OWNERS says `*`, or governs nothing, any
///   writer may approve and **nobody is required**, so listing it there
///   would put every change to an ungoverned file in front of every
///   writer in the organisation. That is the same distinction
///   [`required_from`] draws for the notification recipient rule, and it
///   is drawn once, there, rather than a second time here;
/// - the caller has not already approved this patchset.
///
/// **The cost is why this is a separate pass.** It resolves the target
/// branch's OWNERS tree out of object storage once per row, which is far
/// too much to spend on a page nobody asked this question of — so it runs
/// only when the query actually contained `needs:`, and the caller's page
/// is capped at the rows it can afford to examine (see [`list`]).
async fn only_awaiting(
    state: &SharedState,
    rows: Vec<Change>,
    repo_of: &HashMap<String, stratum_control::Repo>,
    me: &str,
) -> Result<Vec<Change>, Response> {
    let mut out = Vec::new();
    for c in rows {
        if c.state != "open" {
            continue;
        }
        let Some(repo_row) = repo_of.get(&c.repo_id) else {
            continue;
        };
        let latest = match changes::latest_patchset(&state.db, &c.id) {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(e) => return Err(internal(e)),
        };
        let review = review_state(state, &c, repo_row, &latest)
            .await
            .map_err(reads::err_to_response)?;
        if review.verdict.landable {
            continue;
        }
        if review.approvals.iter().any(|a| a.user_id == me) {
            continue;
        }
        if review.required.iter().any(|u| u.id == me) {
            out.push(c);
        }
    }
    Ok(out)
}

/// GET /changes?q=&state=&after=&limit= — newest first.
///
/// `?q=` is the query bar's grammar ([`parse_q`]); `?after=` is a keyset
/// cursor and the response's `next` is the value to send back.
pub async fn list(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let f = match filters(&params, &principal, &state.db, 100) {
        Ok(f) => f,
        Err(r) => return r,
    };
    // Refused rather than ignored, for the same reason an unknown term
    // is: a client that sent `repo:web` here believes it is filtering,
    // and this list is one repository already.
    if let Some(name) = &f.repo {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "repo:{name} only means something on the org-wide list \
                 GET /v1/orgs/{org}/changes; this list is already one repository"
            ),
        );
    }
    let author = match &f.author {
        Who::Nobody => return empty_page(),
        Who::Someone(id) => Some(id.as_str()),
        Who::Anyone => None,
    };
    let listing = match changes::list_in_org(
        &state.db,
        std::slice::from_ref(&repo_row.id),
        &changes::Page {
            state: f.state.as_deref(),
            author_user_id: author,
            after: f.after.as_deref(),
            limit: f.limit,
        },
    ) {
        Ok(l) => l,
        Err(e) => return internal(e),
    };
    let rows = match &f.needs {
        None => listing.rows,
        Some(me) => {
            let one = HashMap::from([(repo_row.id.clone(), repo_row.clone())]);
            match only_awaiting(&state, listing.rows, &one, me).await {
                Ok(r) => r,
                Err(r) => return r,
            }
        }
    };
    match page_json(&state, rows, listing.next, |_, _| {}) {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

/// A page with nothing in it and nowhere to go next — what a term that
/// resolves to nobody and no repository answers.
fn empty_page() -> Response {
    Json(serde_json::json!({ "changes": [], "next": serde_json::Value::Null })).into_response()
}

/// One page on the wire: the rows, plus the cursor for the next one.
///
/// `next` is the cursor the *query* stopped at, never the last surviving
/// row's. With `needs:my-approval` the two differ — every row that
/// filtered out sits between them — and deriving the cursor from what
/// survived would walk those rows again on the next request, forever, on
/// a page that is entirely filtered out.
fn page_json(
    state: &SharedState,
    rows: Vec<Change>,
    next: Option<String>,
    decorate: impl Fn(&Change, &mut serde_json::Value),
) -> Result<serde_json::Value, Response> {
    let mut out = Vec::with_capacity(rows.len());
    for c in &rows {
        let latest = changes::latest_patchset(&state.db, &c.id).map_err(internal)?;
        let mut v = change_json(c, latest.as_ref(), source_name(state, c));
        decorate(c, &mut v);
        out.push(v);
    }
    Ok(serde_json::json!({ "changes": out, "next": next }))
}

/// GET /v1/orgs/:org/changes?q=&state=&after=&limit= — every change in
/// the organization the caller may read, newest first.
///
/// The per-repo list is the same page for one repository; this exists
/// because the changeset picker needs *all* of them at once, and asking
/// per repository is a round trip per repository before the person can
/// even see what they might compose.
///
/// # Authority
///
/// There is no org-level scope check here, and that is on purpose: a
/// `repo:read` token is bound to one repository and cannot pass an
/// `org:read` gate, yet it plainly may see its own repository's changes.
/// So every repository is put through the *same* `rest_repo_auth` the
/// per-repo route uses, and a repository that answers anything but Ok is
/// simply not in the page. That makes this list agree with the per-repo
/// list by construction — a per-repo grant lowering someone the way it
/// does everywhere else — rather than by a second opinion about
/// visibility written in SQL, which is how the two would drift apart.
///
/// Every repository is private to its organization, so a caller whose
/// credential does not authenticate — none at all, or one that does not
/// resolve — may read none of them, and is told to sign in (401) rather
/// than handed an empty page: the per-repo list answers each repository
/// that way, and an empty 200 here would be the one door in the org a
/// signed-out caller could open. A token belonging to another
/// organization is put through the per-repository gate and comes out
/// reading nothing, which is the page `GET …/changesets` gives it too.
pub async fn list_in_org(
    State(state): State<SharedState>,
    Path(org_name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let org = match crate::app::org_or_404(&state, &org_name) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = crate::authx::require_authenticated(&state.db, &headers) {
        return r;
    }
    // Who is asking, for the two `?q=` terms that are about a person —
    // and **only** for those. Every authority question below still goes
    // through `rest_repo_auth` per repository with the raw headers, so
    // this resolves an identity and never a permission.
    //
    // The session fallback is not optional: the per-repo list gets its
    // principal from `rest_repo_auth`, which falls back to a browser
    // session, and a route that read the bearer token alone would tell
    // the dashboard — where nobody holds a token — to sign in on a page
    // it is already signed in on.
    let principal =
        match crate::authx::principal_opt(&state.db, &headers, crate::authx::Challenge::None) {
            Ok(Some(p)) => p,
            Ok(None) => match crate::authx::session_principal(&state.db, &headers, &org.id, None) {
                // Signed in, and not a member of this organisation. Still a
                // person: `author:@me` asks who they are, not what they may
                // do, and what they may see is decided per repository.
                Ok(crate::authx::SessionAuth::NoAccess(user_id)) => {
                    stratum_control::auth::Principal::for_user(&org.id, &user_id, Vec::new())
                }
                Ok(crate::authx::SessionAuth::Principal(p)) => p,
                // The session authenticated a moment ago, in
                // `require_authenticated`, and has expired or been revoked
                // since. The answer that check gives a signed-out caller.
                Ok(crate::authx::SessionAuth::None) => {
                    return crate::authx::unauthorized(crate::authx::Challenge::None)
                }
                Err(r) => return r,
            },
            Err(r) => return r,
        };
    let f = match filters(&params, &principal, &state.db, 100) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let author = match &f.author {
        Who::Nobody => return empty_page(),
        Who::Someone(id) => Some(id.as_str()),
        Who::Anyone => None,
    };
    // Which repositories, before which changes: the page has to be
    // filled from repositories the caller may read, or a limit of 100
    // spent on invisible rows would return an almost empty page and read
    // as "you have no open changes".
    let repos = match stratum_control::registry::list_repos(&state.db, &org.id, None, 1000) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut rows_of: HashMap<String, stratum_control::Repo> = HashMap::new();
    let mut visible: Vec<String> = Vec::new();
    // Once per repository, not once per row: the picker greys out a
    // change the caller could compose but never land, and asks the
    // repository row's own question (`repos::viewer_write`) to find out.
    let mut writable: HashMap<String, bool> = HashMap::new();
    for repo in repos {
        // `repo:` narrows to one repository, and does it *here* rather
        // than in SQL so that a name naming something this caller may not
        // read is masked exactly as it is everywhere else: an empty page,
        // never a refusal that confirms the repository exists.
        if f.repo.as_deref().is_some_and(|want| want != repo.name) {
            continue;
        }
        if crate::app::rest_repo_auth(&state, &headers, &org_name, &repo.name, Scope::RepoRead)
            .is_err()
        {
            continue;
        }
        writable.insert(
            repo.id.clone(),
            crate::api::repos::viewer_write(&state, &headers, &repo),
        );
        visible.push(repo.id.clone());
        rows_of.insert(repo.id.clone(), repo);
    }
    let listing = match changes::list_in_org(
        &state.db,
        &visible,
        &changes::Page {
            state: f.state.as_deref(),
            author_user_id: author,
            after: f.after.as_deref(),
            limit: f.limit,
        },
    ) {
        Ok(l) => l,
        Err(e) => return internal(e),
    };
    let rows = match &f.needs {
        None => listing.rows,
        Some(me) => match only_awaiting(&state, listing.rows, &rows_of, me).await {
            Ok(r) => r,
            Err(r) => return r,
        },
    };
    // One query for the whole page, not one per row: what the picker
    // does with this field is grey out the changes already spoken for.
    let ids: Vec<String> = rows.iter().map(|c| c.id.clone()).collect();
    let held = match stratum_control::changesets::holding_changeset_keys(&state.db, &ids) {
        Ok(h) => h,
        Err(e) => return internal(e.to_string()),
    };
    let page = page_json(&state, rows, listing.next, |c, v| {
        v["repo"] = serde_json::json!(rows_of.get(&c.repo_id).map(|r| &r.name));
        v["changeset"] = serde_json::json!(held.get(&c.id));
        v["viewer_write"] = serde_json::json!(writable.get(&c.repo_id).copied().unwrap_or(false));
    });
    match page {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

/// Re-run the composed CI of the changeset this change belongs to, if it
/// belongs to one and is still open.
///
/// Only `open`: a `landing` changeset has a plan made against the tips
/// it was judged at, and starting a build of a combination the lander is
/// already halfway through would produce a verdict about a state that no
/// longer exists by the time it reports. A landed, failed or abandoned
/// one has nothing left to check.
///
/// A missing binding is the ordinary case — most changes are not in a
/// changeset — and a database error here is logged rather than reported:
/// the caller has just recorded a patchset or an approval, and neither is
/// less true because the composed run could not be started.
async fn retrigger_changeset(state: &SharedState, change_id: &str) {
    match stratum_control::changesets::binding(&state.db, change_id) {
        Ok(Some(cs)) if cs.state == "open" => {
            crate::workflow::trigger::on_changeset(state, &cs).await
        }
        Ok(_) => {}
        Err(e) => eprintln!("weft: changeset binding for change {change_id}: {e}"),
    }
}

/// Look up a change by key with existence masking: an invalid or unknown
/// key answers 404 like any other absent thing.
fn change_or_404(state: &SharedState, repo_id: &str, key: &str) -> Result<Change, Response> {
    match changes::by_key(&state.db, repo_id, key) {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(json_error(
            StatusCode::NOT_FOUND,
            format!("no change {key:?}"),
        )),
        Err(e) => Err(internal(e)),
    }
}

/// GET /changes/:change — the change, all patchsets, and the approvals
/// on the latest one.
pub async fn get(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let patchsets = match changes::patchsets(&state.db, &change.id) {
        Ok(p) => p,
        Err(e) => return internal(e),
    };
    let latest = patchsets.last();
    let approvals = match latest {
        Some(ps) => match changes::approvals_for(&state.db, &ps.id) {
            Ok(a) => a,
            Err(e) => return internal(e),
        },
        None => Vec::new(),
    };
    // Which changeset holds it, if any — so the change view can say
    // "lands with Ic5000001" up front rather than offering Land and
    // learning about the binding from the 409 it gets back.
    let held = match bound_to(&state, &change) {
        Ok(k) => k,
        Err(r) => return r,
    };
    let mut change_v = change_json(&change, latest, source_name(&state, &change));
    change_v["changeset"] = serde_json::json!(held);
    // Every submitted review, so the conversation can render a
    // reviewer's cover message beside their comments rather than
    // showing twelve remarks with nothing tying them together. Whether
    // any of them *blocks* is not here — that needs OWNERS and lives on
    // `/verdict`, which resolves it.
    let reviews = match changes::reviews_for(&state.db, &change.id) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    Json(serde_json::json!({
        "change": change_v,
        "patchsets": patchsets.iter().map(patchset_json).collect::<Vec<_>>(),
        "reviews": reviews.iter().map(review_json).collect::<Vec<_>>(),
        "approvals": approvals.iter().map(|a| serde_json::json!({
            "email": a.email,
            "name": a.name,
            "patchset_id": a.patchset_id,
            "created_at": a.created_at,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// The acting person, or the refusal that explains why there is none:
/// an approval is a human judgement, and sufficiency counts people, so
/// a service token is refused. Every caller that reaches here has
/// authenticated — `rest_repo_auth`, or `require_authenticated` on the
/// org-wide list, answers a signed-out one 401 first — so a principal
/// with no person behind it can only be a token, and the one sentence
/// says so.
///
/// The two sentences are parameters rather than a fixed pair because a
/// review verdict is the same judgement wearing a different noun, and
/// the same rule has to hold at both doors: a token may report a fact
/// (a check) and may say words (a comment), and may not hold an
/// opinion about whether code should land. One function, so a second
/// door cannot be added with the rule left out.
fn acting_user(
    principal: &stratum_control::auth::Principal,
    not_a_token: &str,
) -> Result<String, Response> {
    match &principal.user_id {
        Some(user) => Ok(user.clone()),
        None => Err(json_error(StatusCode::FORBIDDEN, not_a_token)),
    }
}

/// The acting person for an approval, in the words the API has always
/// answered with.
fn approving_user(principal: &stratum_control::auth::Principal) -> Result<String, Response> {
    acting_user(
        principal,
        "approvals must come from a person, not a service token",
    )
}

/// The acting person for a review verdict. Same rule, same reason: a
/// review says whether code should land, which is not a fact a machine
/// can observe.
fn reviewing_user(principal: &stratum_control::auth::Principal) -> Result<String, Response> {
    acting_user(
        principal,
        "a review verdict must come from a person, not a service token",
    )
}

/// POST /changes/:change/approve — approve the latest patchset.
pub async fn approve(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let user_id = match approving_user(&principal) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if change.state != "open" {
        return json_error(StatusCode::CONFLICT, format!("change is {}", change.state));
    }
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    let actx = AuditCtx::of(&org_row.id, Some(&principal));
    match changes::approve(&state.db, &change.id, &latest.id, &user_id, &actx) {
        Ok(_) => {
            crate::workers::notifier::enqueue(
                &state,
                &change,
                crate::workers::notifier::Event::Approved,
                &user_id,
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal(e),
    }
}

/// DELETE /changes/:change/approve — revoke one's own approval on the
/// latest patchset.
pub async fn unapprove(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let user_id = match approving_user(&principal) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    let actx = AuditCtx::of(&org_row.id, Some(&principal));
    match changes::unapprove(&state.db, &change.id, &latest.id, &user_id, &actx) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_error(StatusCode::NOT_FOUND, "no active approval to revoke"),
        Err(e) => internal(e),
    }
}

/// Which OWNERS rule governs each path a patchset touches.
///
/// The rules are read at the **target branch's tip**, not at the
/// patchset. The patchset is the thing under review; letting it also say
/// who must review it meant a change that deleted `OWNERS`, or rewrote it
/// to name its own author, needed nobody in particular — and a branch
/// started from an empty tree carried no `OWNERS` at all, so no owner
/// was ever required or told. GitHub reads CODEOWNERS from the base
/// branch for the same reason. Only when the target branch does not
/// exist yet — a change that will create it — is there no trunk to ask,
/// and the patchset's own files are the best available answer.
///
/// The paths are still the patchset's diff: what changed is a property
/// of the change, who may approve it is a property of the trunk it lands
/// on.
async fn path_outcomes(
    state: &SharedState,
    change: &Change,
    repo_row: &stratum_control::Repo,
    ps: &Patchset,
) -> Result<Vec<(String, RuleOutcome)>, String> {
    outcomes_at_target(state, change, repo_row, ps, None).await
}

/// The same resolution, over paths the caller names instead of the
/// patchset's diff.
///
/// It is one function and not two because of the warning three doc
/// comments up: two resolutions of the same OWNERS files would
/// eventually disagree, and the disagreement here would be between who
/// may approve a path and who may call a comment on it settled. `only`
/// is the path list, or `None` for "whatever this patchset changed".
async fn outcomes_at_target(
    state: &SharedState,
    change: &Change,
    repo_row: &stratum_control::Repo,
    ps: &Patchset,
    only: Option<Vec<String>>,
) -> Result<Vec<(String, RuleOutcome)>, String> {
    let prefix = repo_row.prefix().as_str().to_string();
    let commit = ps.commit_oid.clone();
    let parent = ps.parent_oid.clone();
    let target = format!("refs/heads/{}", change.target_branch);
    reads::with_reader(state, prefix, move |reader| {
        let paths: Vec<String> = match only {
            Some(p) => p,
            None => treediff::diff_commits(reader, parent.as_deref(), &commit)?
                .into_iter()
                .map(|c| c.path)
                .collect(),
        };
        let rules_at = reader.ref_oid(&target)?.unwrap_or_else(|| commit.clone());
        let files = load::owners_files_for_paths(reader, &rules_at, &paths)?;
        Ok(paths
            .iter()
            .map(|p| (p.clone(), owners::effective_owners(p, &files)))
            .collect())
    })
    .await
}

/// One read of a change's review state: is it landable, who does OWNERS
/// require, and which of those people have already approved.
///
/// The three come out of a single resolution of the target branch's
/// OWNERS against a single read of the approvals, and that is the point
/// of computing them together. Resolving twice would cost a second store
/// read of the same tree on every request, and — worse — the two answers
/// could straddle an approval arriving between them, so the page would
/// say "blocked, waiting on Alice" beside a ✓ against Alice's name.
pub(crate) struct ReviewState {
    pub verdict: Verdict,
    /// The people OWNERS names for the changed paths, resolved and
    /// ordered by display name (ties broken by id). A list that
    /// reordered itself between two reads of the same change reads as
    /// the set having changed when it has not.
    pub required: Vec<stratum_control::users::User>,
    /// Whether at least one changed path is governed by a `*` entry.
    ///
    /// This is the half `required` cannot carry, and leaving it out is
    /// how a reader concludes the wrong thing from an empty list: a
    /// change that requires nobody because nothing about it is owned
    /// looks exactly like one whose paths are all `*`, where somebody
    /// with write access does still have to approve. The API says which,
    /// so the page can say it in words instead of showing an empty card.
    pub anyone_with_write: bool,
    /// Active approvals on this patchset, identities joined in.
    pub approvals: Vec<stratum_control::changes::Approval>,
    /// Every standing `request_changes`, each with whether it actually
    /// stops the change. Both kinds are here: one that blocks and one
    /// that does not are the same act said by two people, and a page
    /// that showed only the first would hide half the review.
    pub blocks: Vec<StandingBlock>,
}

/// A standing request for changes, and whether it is authoritative.
pub(crate) struct StandingBlock {
    pub review: stratum_control::changes::Review,
    /// Whether its author is somebody OWNERS names for a path this
    /// patchset touches — or, where OWNERS says `*` or governs
    /// nothing, somebody with write access, which is what satisfies the
    /// path there. `false` means it is recorded, rendered and advisory.
    pub blocking: bool,
}

/// Does this person's opinion carry weight over anything this patchset
/// touches?
///
/// This is the whole difference between our `request_changes` and
/// GitHub's. There, any passer-by can wedge a pull request; here the
/// reviewer set is *computed* from OWNERS, so we can answer the
/// question honestly — and answer it with the same machinery that
/// decides whether an approval counts, rather than inventing a second
/// permission that would eventually disagree with the first.
///
/// Where OWNERS says `*`, or governs nothing at all, write access is
/// what satisfies the path, so a blocking review from a writer counts
/// there. Two rules for one file would be worse than either.
///
/// A path whose OWNERS file does not parse gives nobody standing: it
/// blocks everything already, and "who owns this" has no answer until
/// somebody fixes the file.
///
/// Pure over the resolved requirements, so every arm is unit-tested
/// with no repository behind it.
fn stands_on_a_touched_path(
    requirements: &[sufficiency::PathRequirement],
    writers: &std::collections::BTreeSet<String>,
    user_id: &str,
) -> bool {
    let is_writer = writers.contains(user_id);
    requirements.iter().any(|req| match &req.requirement {
        sufficiency::Requirement::Owned {
            owner_ids,
            anyone_with_write,
            ..
        } => owner_ids.contains(user_id) || (*anyone_with_write && is_writer),
        sufficiency::Requirement::Ungoverned => is_writer,
        sufficiency::Requirement::OwnersError { .. } => false,
    })
}

/// Fold the standing blocks into the sufficiency verdict.
///
/// A blocking review is a "no" from somebody whose "yes" would have
/// counted, so it belongs in the same answer, at the same seam: this
/// runs inside [`review_state`], which is what the Land button, the
/// changeset preview **and the lander** all ask. Putting it anywhere
/// else would let a change refuse at the button and land from the
/// queue, or the other way round.
///
/// Separate from [`sufficiency::evaluate`] on purpose. That engine is
/// pure "did every path get an owner's approval", it is the thing
/// migration 0045 and the OWNERS docs describe, and rewriting it to
/// know about reviews would make this slice an engine rewrite.
///
/// `per_path` is left exactly as sufficiency wrote it. A block is not a
/// statement about one path — the reviewer read the change — and
/// scattering it across the paths they happen to own would report one
/// objection three times.
fn apply_blocks(verdict: &mut Verdict, blocks: &[StandingBlock]) {
    let mut who: Vec<&str> = blocks
        .iter()
        .filter(|b| b.blocking)
        .map(|b| {
            // The email, because that is what every other explanation
            // string in the verdict names people by, and a reader
            // matching this sentence against `per_path` must not have
            // to reconcile two spellings of one person.
            b.review.author_email.as_deref().unwrap_or("a reviewer")
        })
        .collect();
    if who.is_empty() {
        return;
    }
    who.sort_unstable();
    who.dedup();
    verdict.landable = false;
    verdict.explanation = format!(
        // No "push a revision and it clears" here, because it does not:
        // a block survives the next patchset by design, and a sentence
        // that suggested otherwise would send the author to force-push
        // at it.
        "blocked: {} asked for changes; it stands until they withdraw it",
        who.join(", ")
    );
}

/// The authoritative review state for a change at its latest patchset:
/// the engine the owners preview uses, fed by the recorded approvals.
/// The lander re-runs exactly this before it will move trunk.
pub(crate) async fn review_state(
    state: &SharedState,
    change: &Change,
    repo_row: &stratum_control::Repo,
    ps: &Patchset,
) -> Result<ReviewState, String> {
    let org_id = &change.org_id;
    let outcomes = path_outcomes(state, change, repo_row, ps).await?;
    let requirements = owners_api::requirements_for_outcomes(state, org_id, &outcomes)?;
    let approvals = changes::approvals_for(&state.db, &ps.id)?;
    let approvers = approvals
        .iter()
        .map(|a| Approver {
            user_id: a.user_id.clone(),
            email: a.email.clone(),
        })
        .collect();
    let writers = resolve::writer_ids(&state.db, org_id, &repo_row.id)?;
    let set = ApproverSet { approvers, writers };
    let required = required_from(&requirements);
    // Ids are no use to a reader, so they are resolved here rather than
    // on the wire. An id that no longer names a user is dropped rather
    // than rendered as a blank row: the ids come from a resolution
    // against the users table moments ago, so the only way to get one
    // that does not resolve is an account removed in between.
    let mut people: Vec<stratum_control::users::User> = required
        .ids
        .iter()
        .map(|id| stratum_control::users::by_id(&state.db, id))
        .collect::<Result<Vec<_>, String>>()?
        .into_iter()
        .flatten()
        .collect();
    people.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    // The standing "no"s, judged against the same resolution of OWNERS
    // that judged the "yes"es. Reusing `writers` and `requirements`
    // here rather than resolving again is the same argument as the doc
    // comment above: two resolutions of one file eventually disagree,
    // and the disagreement would be about whether a change can land.
    let blocks: Vec<StandingBlock> = changes::standing_blocks(&state.db, &change.id)?
        .into_iter()
        .map(|review| StandingBlock {
            blocking: stands_on_a_touched_path(&requirements, &set.writers, &review.user_id),
            review,
        })
        .collect();
    let mut verdict = sufficiency::evaluate(&requirements, &set);
    apply_blocks(&mut verdict, &blocks);
    Ok(ReviewState {
        verdict,
        required: people,
        anyone_with_write: required.anyone_with_write,
        approvals,
        blocks,
    })
}

/// The verdict alone, for the callers that only gate on it.
pub(crate) async fn compute_verdict(
    state: &SharedState,
    change: &Change,
    repo_row: &stratum_control::Repo,
    ps: &Patchset,
) -> Result<Verdict, String> {
    Ok(review_state(state, change, repo_row, ps).await?.verdict)
}

/// What OWNERS demands of a change, before the ids are resolved to
/// people: the owner ids the changed paths name, and whether any of
/// those paths is governed by a `*` entry instead.
struct Required {
    ids: std::collections::BTreeSet<String>,
    anyone_with_write: bool,
}

/// Reduce resolved path requirements to the people the change needs.
///
/// Pure, and separate from the store read above, so the rule below —
/// `*` contributes nobody — is unit-testable without a repository.
fn required_from(requirements: &[sufficiency::PathRequirement]) -> Required {
    let mut ids = std::collections::BTreeSet::new();
    let mut anyone_with_write = false;
    for req in requirements {
        if let sufficiency::Requirement::Owned {
            owner_ids,
            anyone_with_write: star,
            ..
        } = &req.requirement
        {
            // `anyone_with_write` means the path names no owner in
            // particular: whoever has write may approve it. Nobody is
            // *required*, so nobody is notified on that basis — the ids
            // in that case are every writer in the namespace, and
            // treating them as required reviewers means mailing the
            // whole organisation about every change to an ungoverned
            // file. Which is what it did, until an e2e noticed a
            // bystander being told a change had been opened.
            //
            // This is the difference between a notification people keep
            // on and one they filter: "the OWNERS file names you" is a
            // reason to interrupt somebody, and "you happen to have
            // commit access here" is not.
            if *star {
                anyone_with_write = true;
            } else {
                ids.extend(owner_ids.iter().cloned());
            }
        }
    }
    Required {
        ids,
        anyone_with_write,
    }
}

/// The people this change *requires*, per the repository's OWNERS file.
///
/// This is what makes "participating" mean more here than it does on
/// GitHub. There, being a participant means you commented or were
/// @mentioned — something you did. Here it also means the change cannot
/// land without you, which is something the repository decided, and it
/// is knowable before anybody has looked at the change at all.
///
/// It reuses the verdict pipeline rather than resolving OWNERS a second
/// way. Two resolutions of the same file would eventually disagree, and
/// the failure would be silent in the worst direction: the person the
/// change needs is the one who never hears about it.
///
/// An unowned path contributes nobody. A broken OWNERS file contributes
/// nobody either — a notification is not the place to report a syntax
/// error, and the verdict endpoint already does that loudly.
pub(crate) async fn required_reviewers(
    state: &SharedState,
    change: &Change,
    repo_row: &stratum_control::Repo,
    ps: &Patchset,
) -> Result<std::collections::BTreeSet<String>, String> {
    let outcomes = path_outcomes(state, change, repo_row, ps).await?;
    let requirements = owners_api::requirements_for_outcomes(state, &change.org_id, &outcomes)?;
    Ok(required_from(&requirements).ids)
}

/// The required reviewer set for the wire: people, not ids, each with
/// whether they have already approved *this* patchset.
///
/// The whole product argument for computing reviewers from OWNERS rather
/// than letting an author nominate them dies quietly if the answer is
/// only ever visible in an email. A client cannot derive this from the
/// verdict's `per_path`: that carries the OWNERS entries **as written**
/// — "@payments", a team — and expanding a team to its members is a
/// resolution only the server can do.
///
/// Pure over the state above, so the shape the dashboard renders is
/// pinned without a repository behind it.
pub(crate) fn reviewers_json(review: &ReviewState) -> serde_json::Value {
    let approved: std::collections::BTreeSet<&str> = review
        .approvals
        .iter()
        .map(|a| a.user_id.as_str())
        .collect();
    serde_json::json!({
        "required": review.required.iter().map(|u| serde_json::json!({
            "user_id": u.id,
            "name": u.name,
            "email": u.email,
            "approved": approved.contains(u.id.as_str()),
        })).collect::<Vec<_>>(),
        "anyone_with_write": review.anyone_with_write,
    })
}

/// One submitted review on the wire.
///
/// No `blocking` here: whether a "no" actually stops the change is a
/// question about OWNERS and the patchset's paths, which this read does
/// not resolve. `blocks_json` adds it where the answer is known, so a
/// client can never be handed a stale or invented one.
fn review_json(r: &stratum_control::changes::Review) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "verdict": r.verdict.as_str(),
        "body": r.body,
        "author": r.author_name.clone().unwrap_or_else(|| "somebody".to_string()),
        "author_email": r.author_email,
        "user_id": r.user_id,
        "patchset_id": r.patchset_id,
        "state": r.state,
        "submitted_at": r.submitted_at,
        "withdrawn_at": r.withdrawn_at,
        "created_at": r.created_at,
    })
}

/// The standing blocks, in the order they were said, each carrying the
/// field that matters.
///
/// `blocking` is deliberately not the same as
/// `verdict == "request_changes"`: a request for changes from somebody
/// with no standing on any touched path is recorded, rendered, and
/// advisory. A client that read the verdict alone would tell the author
/// their change is stuck when it is not.
fn blocks_json(review: &ReviewState) -> Vec<serde_json::Value> {
    review
        .blocks
        .iter()
        .map(|b| {
            let mut v = review_json(&b.review);
            v["blocking"] = serde_json::json!(b.blocking);
            v
        })
        .collect()
}

/// GET /changes/:change/interdiff?from=<n>&to=<n> — the tree diff between
/// two of this change's patchsets, `n` being patchset *numbers*.
///
/// The question a reviewer coming back to revision 4 of a forty-file
/// change actually has is "what moved since I last read this", and it is
/// not the diff of the latest patchset against its parent: a file touched
/// in patchset 2 and put back in patchset 3 is in that diff and is not in
/// the answer. Resolving both ends to the commits they were recorded at
/// and diffing those two trees is.
///
/// **What this is not**, said plainly here because the name invites the
/// stronger reading. It is a two-commit tree diff between the patchsets
/// as they were pushed. When a patchset was rebased, whatever trunk
/// picked up in between is in the result too — this is not a
/// rebase-aware three-way interdiff in Gerrit's sense, which subtracts
/// the trunk move. And nothing migrates comment anchors across the
/// range: a comment stays pinned to the patchset it was written against,
/// which is the property that keeps it honest.
///
/// The response is the shape `/diff` returns, entry for entry, so the
/// client that renders one renders this. Reversed ranges are allowed —
/// `from` newer than `to` is the perfectly sensible "show me what I would
/// be undoing" — because there is nothing here that a direction breaks.
pub async fn interdiff(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let (Some(from), Some(to)) = (params.get("from"), params.get("to")) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "from and to are required, and are patchset numbers",
        );
    };
    // Refused in words rather than coerced. A client that sent a commit
    // oid here wanted `/diff`, and silently answering something is how it
    // never finds that out.
    let (Ok(from), Ok(to)) = (from.parse::<i64>(), to.parse::<i64>()) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("from and to are patchset numbers; got {from:?} and {to:?}"),
        );
    };
    if from == to {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!("from and to are both patchset {from}; there is nothing between a patchset and itself"),
        );
    }
    let patchsets = match changes::patchsets(&state.db, &change.id) {
        Ok(p) => p,
        Err(e) => return internal(e),
    };
    // A patchset number this change never had is an absent thing, and
    // says so — the caller can already read the whole patchset list, so
    // naming the number leaks nothing and saves them a guess.
    let commit_at = |n: i64| {
        patchsets
            .iter()
            .find(|p| p.number == n)
            .map(|p| p.commit_oid.clone())
            .ok_or_else(|| {
                json_error(
                    StatusCode::NOT_FOUND,
                    format!("change {key:?} has no patchset {n}"),
                )
            })
    };
    let a = match commit_at(from) {
        Ok(oid) => oid,
        Err(r) => return r,
    };
    let b = match commit_at(to) {
        Ok(oid) => oid,
        Err(r) => return r,
    };
    let prefix = repo_row.prefix().as_str().to_string();
    let out = reads::with_reader(&state, prefix, move |reader| {
        let mut v = reads::diff_entries(reader, &a, &b)?;
        // The numbers ride along beside the oids so a client can label
        // the view "patchset 1 → 3" without holding the mapping it just
        // sent us.
        v["from_patchset"] = serde_json::json!(from);
        v["to_patchset"] = serde_json::json!(to);
        Ok(v)
    })
    .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => reads::err_to_response(e),
    }
}

/// GET /changes/:change/verdict — landability now, with the explanation,
/// and who the repository requires.
///
/// The reviewer set rides on the verdict rather than on the change
/// detail read because it is a property of the same OWNERS resolution:
/// the detail read touches no OWNERS file and no store at all, and
/// putting it there would make every change list and every detail render
/// pay for a read of the target branch's tree. The change view already
/// fetches both, so a reader loses nothing.
pub async fn verdict(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    match review_state(&state, &change, &repo_row, &latest).await {
        Ok(r) => Json(serde_json::json!({
            "change": change.change_key,
            "state": change.state,
            "patchset": latest.number,
            "commit": latest.commit_oid,
            "verdict": owners_api::verdict_json(&r.verdict),
            "reviewers": reviewers_json(&r),
            // The standing "no"s ride with the verdict rather than on a
            // route of their own, because they are half of the same
            // answer: the explanation says a review blocks, and this is
            // which one, said by whom, in what words.
            "blocks": blocks_json(&r),
        }))
        .into_response(),
        Err(e) => reads::err_to_response(e),
    }
}

/// The changeset holding this change, by key, or `None` when it is the
/// caller's to land alone.
///
/// One function, two readers: the `changeset` field on the change and
/// the `409` that refuses a solo landing. That is deliberate — the field
/// exists so a client can *show* the binding instead of discovering it
/// by pressing Land and reading the refusal, and a field that answered a
/// slightly different question would put a working Land button in front
/// of a change that cannot land.
fn bound_to(state: &SharedState, change: &Change) -> Result<Option<String>, Response> {
    match stratum_control::changesets::binding(&state.db, &change.id) {
        Ok(cs) => Ok(cs.map(|cs| cs.key)),
        Err(e) => Err(internal(e.to_string())),
    }
}

/// A change in an open changeset lands and closes only through it.
///
/// This is the binding a changeset adds, and the one place it is
/// enforced on the per-change routes: without it, "one landing" would be
/// a sentence in the docs and every member would still have its own
/// Land button.
fn refuse_if_bound(state: &SharedState, change: &Change, otherwise: &str) -> Result<(), Response> {
    match bound_to(state, change)? {
        Some(key) => Err(json_error(
            StatusCode::CONFLICT,
            format!("this change is a member of changeset {key} — {otherwise}"),
        )),
        None => Ok(()),
    }
}

/// POST /changes/:change/land — enqueue the change for landing. The
/// precheck fast-fails with the explanation; the lander re-verifies at
/// claim time, because approvals move between the two.
pub async fn land(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if change.state != "open" {
        return json_error(StatusCode::CONFLICT, format!("change is {}", change.state));
    }
    if let Err(r) = refuse_if_bound(
        &state,
        &change,
        "it lands with the changeset; remove it from the changeset to land it alone",
    ) {
        return r;
    }
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    let verdict = match compute_verdict(&state, &change, &repo_row, &latest).await {
        Ok(v) => v,
        Err(e) => return reads::err_to_response(e),
    };
    if !verdict.landable {
        return json_error(StatusCode::CONFLICT, verdict.explanation);
    }
    // The machine's verdict gates alongside the human one, and the
    // lander re-checks at claim time. `land_gate` — not `failing_check`,
    // which only ever asked "is anything failing *right now*" — because
    // the two answers it separates deserve different answers here:
    //
    // - `Blocked` refuses in words, at the button. That now includes a
    //   required check that has not reported at all, which the old
    //   precheck read as "nothing has failed" and waved through to a
    //   202 the lander ejected seconds later. A person pressing Land
    //   got an accepted request and, a moment on, a change back in
    //   `open` with a verdict they had to go looking for.
    //
    // - `Waiting` is **accepted**, and this is the one worth arguing.
    //   Refusing would be defensible — the change cannot land yet — but
    //   it makes the author come back and press Land again once CI goes
    //   green, which is precisely the polling a land queue exists to
    //   abolish. Nothing is being asserted falsely by the 202: the
    //   change genuinely will land, unattended, when the checks it is
    //   waiting on report green, and the lander (already correct, and
    //   the authority either way) holds it in the queue rather than
    //   ejecting it. `Blocked` is different in kind — waiting cannot
    //   rescue a change something has already said no to — which is why
    //   only one of the two is refused.
    //
    // Both answers name the checks: `reason` lists every blocking check
    // and why, and `waiting_on` is the list of names still to report, so
    // a merge box can render either without a second round trip.
    let waiting_on = match changes::land_gate(&state.db, &change.id) {
        Ok(changes::LandGate::Ready) => Vec::new(),
        Ok(changes::LandGate::Waiting { on }) => on,
        Ok(changes::LandGate::Blocked { reason }) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("blocked: {reason}"),
                    "gate": "blocked",
                    "reason": reason,
                })),
            )
                .into_response()
        }
        Err(e) => return internal(e),
    };
    let payload = serde_json::json!({ "change_id": change.id }).to_string();
    let job = match stratum_control::jobs::create(
        &state.db,
        &org_row.id,
        Some(&repo_row.id),
        "land",
        Some(&payload),
    ) {
        Ok(j) => j,
        Err(e) => return internal(e),
    };
    match changes::set_landing(&state.db, &change.id, &job.id) {
        Ok(true) => {}
        // Someone else enqueued or moved it between our read and now;
        // their landing is already driving, so this job must not.
        Ok(false) => {
            let _ = stratum_control::jobs::complete(
                &state.db,
                &job.id,
                Some("no-op: lost the enqueue race"),
            );
            return json_error(StatusCode::CONFLICT, "change is landing");
        }
        Err(e) => return internal(e),
    }
    let actx = AuditCtx::of(&org_row.id, Some(&principal));
    crate::api::record_or_warn(
        &state.db,
        &actx,
        Some(&repo_row.id),
        "change.land",
        Some(&serde_json::json!({ "change_key": change.change_key, "job": job.id })),
    );
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "queued": true,
            "job": job.id,
            "change": change.change_key,
            "gate": if waiting_on.is_empty() { "ready" } else { "waiting" },
            "waiting_on": waiting_on,
        })),
    )
        .into_response()
}

/// POST /changes/:change/workflows/approve — let a fork's workflows run
/// at this change's **current** tip.
///
/// The gate the `blocked` runs are waiting on. A change from a fork
/// carries a workflow file the contributor wrote, and running it hands a
/// stranger a `repo:read` token and a machine — so it does not run until
/// somebody who could land the change says so.
///
/// **Per tip, not per change.** A new patchset from the fork is blocked
/// again, because the file the maintainer read is not the file the next
/// push contains. This is GitHub's "Approve and run" for exactly the
/// reason GitHub does it that way: an approval that survived new commits
/// would be a standing offer to run whatever arrives next.
///
/// The same door as landing (`repo:write` through `rest_repo_auth`), and
/// deliberately not the approval door beside it: `POST …/approve` is a
/// review opinion, which a reviewer with read access may hold, and it
/// must not also start compute. Two words, two authorizations.
///
/// The fork's objects are already readable here — a change from a fork
/// transplants its tip into this repository when it is opened, and pins
/// `refs/patchsets/<sha>` so nothing collects it — so approval starts
/// runs and nothing else has to move.
pub async fn approve_workflows(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if change.state != "open" {
        return json_error(StatusCode::CONFLICT, format!("change is {}", change.state));
    }
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    let at_tip = match stratum_control::workflows::runs_for_change_commit(
        &state.db,
        &repo_row.id,
        &change.change_key,
        &latest.commit_oid,
    ) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let blocked: Vec<_> = at_tip
        .into_iter()
        .filter(|r| r.state == "blocked")
        .collect();
    if blocked.is_empty() {
        // Everything else that could be true here — already approved,
        // never blocked, no workflow files at all — is the same answer
        // to the caller: there is nothing for this button to start.
        // Distinguishing them would mean reporting on runs to somebody
        // who cannot see anything new from it.
        return json_error(
            StatusCode::CONFLICT,
            "no workflows are waiting for approval at this change's current tip",
        );
    }
    // The placeholder has to be gone before the trigger runs, in both
    // tables. `run_for_commit` — the trigger's idempotency guard —
    // matches a run in any state, so a placeholder left behind means the
    // real run is never created; and the placeholder's mirrored check
    // row is keyed on the **run** id, where the real jobs each mirror a
    // row keyed on a **job** id, so one left behind is a permanently
    // queued check holding the land gate for a run that was replaced.
    for run in &blocked {
        if let Err(e) = stratum_control::checks::delete_external(
            &state.db,
            &repo_row.id,
            stratum_control::checks::HOSTED_PROVIDER,
            &run.id,
        ) {
            return internal(e);
        }
        if let Err(e) = stratum_control::workflows::delete_blocked_run(&state.db, &run.id) {
            return internal(e);
        }
    }
    crate::workflow::trigger::start(
        &state,
        &repo_row,
        &crate::workflow::trigger::Cause {
            event: "change",
            sha: &latest.commit_oid,
            ref_name: Some(&change.target_branch),
            change_key: Some(&change.change_key),
            fork_hold: None,
            composed: None,
        },
    )
    .await;
    // The approval is for this tip, and it covers the composed run too:
    // retriggering the changeset now lets `trigger::from_fork` see the
    // change run that was just started at this commit and admit the
    // member, where a moment ago it would have blocked it again.
    retrigger_changeset(&state, &change.id).await;
    let actx = AuditCtx::of(&org_row.id, Some(&principal));
    crate::api::record_or_warn(
        &state.db,
        &actx,
        Some(&repo_row.id),
        "workflow.approved",
        // The placeholder rows are gone by now, so this event is the
        // only surviving record that these files were held and who let
        // them go. Name the files: "approved the workflows" without
        // saying which is not a trail anybody can audit.
        Some(&serde_json::json!({
            "change_key": change.change_key,
            "commit": latest.commit_oid,
            "files": blocked.iter().map(|r| r.file.clone()).collect::<Vec<_>>(),
        })),
    );
    // Read back rather than reporting what we asked for: the trigger is
    // the authority on what a commit's `.weft/` actually produced,
    // and it may legitimately have settled a run instead of starting one
    // — an organisation out of minutes, a file over the timeout cap. A
    // caller that was told "running" about a run that is `blocked` would
    // wait for a build that is not coming.
    let now = match stratum_control::workflows::runs_for_change_commit(
        &state.db,
        &repo_row.id,
        &change.change_key,
        &latest.commit_oid,
    ) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut out = Vec::with_capacity(now.len());
    for run in &now {
        let jobs = match stratum_control::workflows::jobs_of(&state.db, &run.id) {
            Ok(j) => j,
            Err(e) => return internal(e),
        };
        out.push(crate::api::workflow_runs_api::run_json(&state, run, &jobs));
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "runs": out })),
    )
        .into_response()
}

/// POST /changes/:change/abandon — close without landing.
pub async fn abandon(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, _, principal, change) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite) {
            Ok((o, r, p)) => match change_or_404(&state, &r.id, &key) {
                Ok(c) => (o, r, p, c),
                Err(r) => return r,
            },
            // Not a writer. The author may still withdraw what they opened;
            // anyone else gets the refusal a writer's check produced, so a
            // stranger learns nothing from the difference.
            Err(refused) => match own_change(&state, &headers, &org, &repo, &key) {
                Some(x) => x,
                None => return refused,
            },
        };
    if let Err(r) = refuse_if_bound(
        &state,
        &change,
        "remove it from the changeset first, or abandon the changeset",
    ) {
        return r;
    }
    let actx = AuditCtx::of(&org_row.id, Some(&principal));
    match changes::abandon(&state.db, &change.id, &actx) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_error(StatusCode::CONFLICT, format!("change is {}", change.state)),
        Err(e) => internal(e),
    }
}

/// The change named by `key`, if the caller opened it and can still read
/// the repository it is on.
///
/// Abandoning is otherwise a writer's action, but a change is its author's
/// to withdraw: a first-time contributor who proposed from a fork holds only
/// `repo:read` on the upstream, and without this they could open a change
/// they had no way to close. `created_by` is a person's id, so only a person
/// — never a service token, which opens nothing as itself — matches it.
fn own_change(
    state: &SharedState,
    headers: &HeaderMap,
    org: &str,
    repo: &str,
    key: &str,
) -> Option<(
    stratum_control::registry::Org,
    stratum_control::registry::Repo,
    stratum_control::auth::Principal,
    Change,
)> {
    let (o, r, p) = crate::app::rest_repo_auth(state, headers, org, repo, Scope::RepoRead).ok()?;
    let user = p.user_id.clone()?;
    let change = change_or_404(state, &r.id, key).ok()?;
    (change.created_by.as_deref() == Some(user.as_str())).then_some((o, r, p, change))
}

/// GET /land-queue — what is landing right now, oldest first.
pub async fn land_queue(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let rows = match changes::list(&state.db, &repo_row.id, Some("landing"), 500) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let mut queue = Vec::with_capacity(rows.len());
    // Oldest first: the row order is newest-first for feeds, but a queue
    // reads in the order it will be served.
    for c in rows.iter().rev() {
        let latest = match changes::latest_patchset(&state.db, &c.id) {
            Ok(p) => p,
            Err(e) => return internal(e),
        };
        queue.push(change_json(c, latest.as_ref(), source_name(&state, c)));
    }
    Json(serde_json::json!({ "queue": queue })).into_response()
}

#[derive(Deserialize)]
pub struct AddComment {
    pub body: String,
    /// Optionally anchor the comment to a file the change touches.
    pub path: Option<String>,
    /// With `path`: anchor to a 1-based line in that file as of the
    /// patchset — GitHub-style inline review.
    pub line: Option<i64>,
    /// With `line`: the last line of a multi-line anchor, inclusive.
    /// A remark about a loop is about the loop.
    pub line_end: Option<i64>,
    /// `new` (the default) or `old`: which half of the diff `line`
    /// counts in. `old` is how you say "you should not have deleted
    /// this" about a line that is no longer there.
    pub side: Option<String>,
    /// Reply into an existing thread. The reply inherits that thread's
    /// anchor and must not carry `path`/`line`/`line_end`/`side`.
    pub parent_id: Option<String>,
    /// Draft this into the caller's pending review instead of
    /// publishing it. Nobody else sees it — not the author, not an
    /// admin, not any other reader — until the review is submitted,
    /// and nobody is mailed about it at all.
    pub pending: Option<bool>,
}

/// POST /changes/:change/comments — say why, not just whether. Anyone
/// who can read the repo can comment, people and service principals
/// alike: "the perf suite regressed" from CI is review too.
pub async fn add_comment(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<AddComment>,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    if let Some(p) = &body.path {
        if !crate::review::owners::valid_repo_path(p) {
            return json_error(StatusCode::BAD_REQUEST, format!("invalid path {p:?}"));
        }
    }
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    let side = match body.side.as_deref() {
        None => changes::Side::New,
        Some(s) => match changes::Side::parse(s) {
            Some(side) => side,
            // Never coerced to `new`: a typo'd side would silently move
            // the anchor to the other half of the diff, where the same
            // line number is a different line.
            None => return json_error(StatusCode::BAD_REQUEST, format!("unknown side {s:?}")),
        },
    };
    // A pending comment needs a review to belong to, and starting one
    // is the caller's own act either way, so it is opened here rather
    // than demanded of the client: a draft that failed because the
    // client forgot a setup call would be a lost remark, and the client
    // has nothing to do with the answer.
    //
    // A service token asks for `pending` and is refused, for the reason
    // a token cannot approve: a draft belongs to a person, and there is
    // no "later" in which a token submits it.
    let review_id = if body.pending.unwrap_or(false) {
        let Some(user_id) = principal.user_id.as_deref() else {
            return json_error(
                StatusCode::FORBIDDEN,
                "a pending comment belongs to a person's review, not a service token",
            );
        };
        match changes::open_review(&state.db, &change.id, &latest.id, user_id, None) {
            Ok(Ok(r)) => Some(r.id),
            Ok(Err(e)) => return json_error(StatusCode::BAD_REQUEST, e),
            Err(e) => return internal(e),
        }
    } else {
        None
    };
    let out = changes::add_comment(
        &state.db,
        &changes::NewComment {
            review_id: review_id.as_deref(),
            change_id: &change.id,
            patchset_number: latest.number,
            author_principal: &principal.audit_id(),
            author_user_id: principal.user_id.as_deref(),
            parent_id: body.parent_id.as_deref(),
            path: body.path.as_deref(),
            line: body.line,
            line_end: body.line_end,
            side,
            // Deliberately not settable over this route. `external_id`
            // is an importer's identity, and a client that could choose
            // one could squat the id a later import needs and turn its
            // dedupe into a refusal. See migration 0050.
            external_id: None,
            body: &body.body,
        },
    );
    match out {
        Ok(Ok(c)) => {
            let actx = AuditCtx::of(&org_row.id, Some(&principal));
            crate::api::record_or_warn(
                &state.db,
                &actx,
                Some(&repo_row.id),
                "change.comment",
                Some(&serde_json::json!({
                    "change_key": change.change_key,
                    "patchset": latest.number,
                })),
            );
            // A drafted comment tells nobody anything: it has not been
            // said yet, and mailing about it would publish it in the
            // one place the reviewer cannot take it back from. Its
            // review's submission is what notifies, once, for the lot.
            if let (Some(actor), None) = (principal.user_id.as_deref(), &review_id) {
                crate::workers::notifier::enqueue(
                    &state,
                    &change,
                    crate::workers::notifier::Event::Commented,
                    actor,
                );
            }
            (StatusCode::CREATED, Json(comment_json(&c))).into_response()
        }
        Ok(Err(e)) => json_error(StatusCode::BAD_REQUEST, e),
        Err(e) => internal(e),
    }
}

/// GET /changes/:change/comments — the conversation, oldest first.
pub async fn comments(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    // Everything published, plus the caller's own drafts. The filter is
    // one `WHERE` clause down in `comments_for_viewer` and nowhere
    // else — see the warning on it. Nothing about this handler decides
    // visibility, and nothing about the next one should either.
    let viewer = principal.user_id.as_deref();
    let rows = match changes::comments_for_viewer(&state.db, &change.id, viewer) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    Json(serde_json::json!({
        "comments": rows.iter().map(comment_json).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// The body a review is started or saved with.
#[derive(Deserialize, Default)]
pub struct StartReview {
    /// The cover message, saved with the draft. Optional: most reviews
    /// begin with a comment on a line, not a summary.
    pub body: Option<String>,
}

/// The caller's own pending review, or the refusal for a caller who
/// cannot hold one.
///
/// One helper for all five routes below, because the door has to be the
/// same at each: read access to the repository, a person rather than a
/// token, and a change that exists here. Five copies of that would be
/// four chances to leave one condition out.
fn review_door(
    state: &SharedState,
    headers: &HeaderMap,
    org: &str,
    repo: &str,
    key: &str,
) -> Result<
    (
        stratum_control::Org,
        stratum_control::Repo,
        stratum_control::auth::Principal,
        String,
        Change,
    ),
    Response,
> {
    let (org_row, repo_row, principal) =
        crate::app::rest_repo_auth(state, headers, org, repo, Scope::RepoRead)?;
    let user_id = reviewing_user(&principal)?;
    let change = change_or_404(state, &repo_row.id, key)?;
    Ok((org_row, repo_row, principal, user_id, change))
}

/// POST /changes/:change/review — start (or save) the caller's pending
/// review.
///
/// Idempotent: calling it twice hands back the same draft. A client
/// drafting its first comment does not have to know whether a review is
/// already open, and two tabs racing must not produce two half-reviews
/// — the database says so too, through `reviews_one_draft`.
pub async fn start_review(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Option<Json<StartReview>>,
) -> Response {
    let (_, _, _, user_id, change) = match review_door(&state, &headers, &org, &repo, &key) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if change.state != "open" {
        return json_error(StatusCode::CONFLICT, format!("change is {}", change.state));
    }
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    let Json(body) = body.unwrap_or_default();
    match changes::open_review(
        &state.db,
        &change.id,
        &latest.id,
        &user_id,
        body.body.as_deref(),
    ) {
        Ok(Ok(r)) => (StatusCode::OK, Json(review_json(&r))).into_response(),
        Ok(Err(e)) => json_error(StatusCode::BAD_REQUEST, e),
        Err(e) => internal(e),
    }
}

/// GET /changes/:change/review — the caller's pending review and the
/// comments drafted into it.
///
/// `{"review": null}` rather than a 404 when there is none. A client
/// asking "do I have a review open" is asking a question with a
/// perfectly good negative answer, and answering it with an error makes
/// every page load look like a failure in the log.
pub async fn get_review(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, _, _, user_id, change) = match review_door(&state, &headers, &org, &repo, &key) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let review = match changes::pending_review(&state.db, &change.id, &user_id) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let drafts: Vec<serde_json::Value> = match &review {
        Some(r) => match changes::comments_for_viewer(&state.db, &change.id, Some(&user_id)) {
            Ok(rows) => rows
                .iter()
                .filter(|c| c.published_at.is_none() && c.review_id.as_deref() == Some(&r.id))
                .map(comment_json)
                .collect(),
            Err(e) => return internal(e),
        },
        None => Vec::new(),
    };
    Json(serde_json::json!({
        "review": review.as_ref().map(review_json),
        "comments": drafts,
    }))
    .into_response()
}

/// DELETE /changes/:change/review — throw the pending review away,
/// drafted comments and all. Nothing anybody else ever saw is lost,
/// which is the entire point of a draft.
pub async fn discard_review(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, repo_row, principal, user_id, change) =
        match review_door(&state, &headers, &org, &repo, &key) {
            Ok(x) => x,
            Err(r) => return r,
        };
    match changes::discard_review(&state.db, &change.id, &user_id) {
        Ok(true) => {
            let actx = AuditCtx::of(&org_row.id, Some(&principal));
            crate::api::record_or_warn(
                &state.db,
                &actx,
                Some(&repo_row.id),
                "change.review.discard",
                Some(&serde_json::json!({ "change_key": change.change_key })),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => json_error(StatusCode::NOT_FOUND, "no pending review to discard"),
        Err(e) => internal(e),
    }
}

/// The body a review is submitted with.
#[derive(Deserialize)]
pub struct SubmitReviewBody {
    /// `approve`, `comment` or `request_changes`.
    pub verdict: String,
    /// The cover message. Required for `request_changes` unless the
    /// review carries comments of its own.
    pub body: Option<String>,
}

/// POST /changes/:change/review/submit — the whole review, as one act.
///
/// Publishing the drafts, recording the verdict, writing or revoking
/// the approval and enqueuing the notification all happen in one
/// control-plane transaction (see `changes::submit_review`), and **one**
/// notification is enqueued for the lot.
///
/// That last part is the actual fix for the twelve-emails problem.
/// Migration 0049 stopped a twelve-comment review queuing twelve
/// *identical* jobs, but only while they were still queued: the sender
/// claiming one between the third comment and the fourth started a
/// fresh one, so the count was a race rather than a number. Drafted
/// comments notify nobody at all, and submitting sends exactly one
/// mail because there is exactly one act to report.
pub async fn submit_review(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<SubmitReviewBody>,
) -> Response {
    let (org_row, _repo_row, principal, user_id, change) =
        match review_door(&state, &headers, &org, &repo, &key) {
            Ok(x) => x,
            Err(r) => return r,
        };
    if change.state != "open" {
        return json_error(StatusCode::CONFLICT, format!("change is {}", change.state));
    }
    let Some(verdict) = changes::ReviewVerdict::parse(&body.verdict) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "unknown verdict {:?}: one of approve, comment, request_changes",
                body.verdict
            ),
        );
    };
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    let actx = AuditCtx::of(&org_row.id, Some(&principal));
    let done = match changes::submit_review(
        &state.db,
        &changes::SubmitReview {
            change_id: &change.id,
            patchset_id: &latest.id,
            user_id: &user_id,
            verdict,
            body: body.body.as_deref(),
        },
        &actx,
    ) {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return json_error(StatusCode::BAD_REQUEST, e),
        Err(e) => return internal(e),
    };
    // One notification for the whole pass, in the words of what was
    // said. `approve` reuses the event the approve route sends, because
    // it is the same fact through a different door and a reader must
    // not have to know which one was used.
    let event = match verdict {
        changes::ReviewVerdict::Approve => crate::workers::notifier::Event::Approved,
        changes::ReviewVerdict::Comment => crate::workers::notifier::Event::Reviewed,
        changes::ReviewVerdict::RequestChanges => crate::workers::notifier::Event::ChangesRequested,
    };
    crate::workers::notifier::enqueue(&state, &change, event, &user_id);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "review": review_json(&done.review),
            "published": done.published,
            "approved": done.approved,
            "approval_revoked": done.approval_revoked,
        })),
    )
        .into_response()
}

/// POST /changes/:change/review/withdraw — take back one's standing
/// request for changes.
///
/// Only its author, which is not a permission check here so much as the
/// shape of the call: it withdraws *the caller's* block and cannot name
/// anybody else's. A block somebody else could clear is not a block,
/// and the argument for `request_changes` surviving a new patchset is
/// that it ends when the person who raised it says it does.
pub async fn withdraw_review(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (org_row, _repo_row, principal, user_id, change) =
        match review_door(&state, &headers, &org, &repo, &key) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let actx = AuditCtx::of(&org_row.id, Some(&principal));
    match changes::withdraw_review(&state.db, &change.id, &user_id, &actx) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => json_error(
            StatusCode::NOT_FOUND,
            "no standing request for changes to withdraw",
        ),
        Err(e) => internal(e),
    }
}

/// May this person call that thread settled?
///
/// Its author always may: withdrawing your own remark needs nobody's
/// permission. Otherwise it takes **satisfying the commented path under
/// OWNERS** — and that is the whole argument for doing it this way
/// rather than checking for write access. The repository already has an
/// answer to "whose opinion counts about this file", it is computed
/// rather than configured, and it follows team membership without
/// anybody editing a list. Resolution is the same question as approval
/// asked about one remark, so it gets the same answer from the same
/// engine: the sufficiency judge, with this person as the sole approver.
///
/// Two consequences worth stating out loud, because both are the point:
/// the **change's author** cannot silence a comment on a file they do
/// not own — a review you can dismiss yourself is not a review — and a
/// writer with no standing on that path cannot either. Where OWNERS
/// says `*`, or governs nothing, write access *is* what satisfies the
/// path, and resolution follows: two rules for one file would be worse
/// than either.
///
/// A comment with no path is about the change as a whole; satisfying
/// any one of its governed paths is standing enough to settle it.
async fn may_resolve(
    state: &SharedState,
    change: &Change,
    repo_row: &stratum_control::Repo,
    ps: &Patchset,
    comment: &changes::Comment,
    user_id: &str,
) -> Result<bool, String> {
    if comment.author_user_id.as_deref() == Some(user_id) {
        return Ok(true);
    }
    let only = comment.path.clone().map(|p| vec![p]);
    let outcomes = outcomes_at_target(state, change, repo_row, ps, only).await?;
    let requirements = owners_api::requirements_for_outcomes(state, &change.org_id, &outcomes)?;
    let set = ApproverSet {
        approvers: vec![Approver {
            user_id: user_id.to_string(),
            // The judge matches on ids; the email only ever reaches an
            // explanation string, and this call wants the boolean.
            // Looking the user up to fill it in would buy a database
            // round trip and an arm — "the account vanished between the
            // session check and here" — that no test can reach.
            email: String::new(),
        }],
        writers: resolve::writer_ids(&state.db, &change.org_id, &repo_row.id)?,
    };
    Ok(sufficiency::evaluate(&requirements, &set)
        .per_path
        .iter()
        .any(|v| v.satisfied))
}

/// POST /changes/:change/comments/:comment/resolve — "this is dealt
/// with", said by somebody the repository says may say it.
pub async fn resolve_comment(
    State(state): State<SharedState>,
    Path((org, repo, key, comment)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    set_comment_resolved(state, org, repo, key, comment, headers, true).await
}

/// POST /changes/:change/comments/:comment/unresolve — reopening it,
/// under exactly the same rule. Whoever could close it can reopen it,
/// which is what keeps "resolved" from being a one-way silencer.
pub async fn unresolve_comment(
    State(state): State<SharedState>,
    Path((org, repo, key, comment)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    set_comment_resolved(state, org, repo, key, comment, headers, false).await
}

async fn set_comment_resolved(
    state: SharedState,
    org: String,
    repo: String,
    key: String,
    comment_id: String,
    headers: HeaderMap,
    resolved: bool,
) -> Response {
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    // A service token has no standing here on purpose. Resolution is an
    // opinion about whether a remark has been addressed — the same kind
    // of judgement as an approval, which service tokens are already
    // barred from — where posting a check is a fact a machine observed.
    let Some(user_id) = principal.user_id.clone() else {
        return json_error(
            StatusCode::FORBIDDEN,
            "resolving a thread is a person's judgement, not a token's",
        );
    };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let comment = match changes::comment_by_id(&state.db, &comment_id) {
        Ok(Some(c)) if c.change_id == change.id => c,
        // A comment on another change is masked as absent here, the same
        // as a cross-org probe: an id leaks nothing by answering 404.
        Ok(_) => return json_error(StatusCode::NOT_FOUND, "no such comment"),
        Err(e) => return internal(e),
    };
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    match may_resolve(&state, &change, &repo_row, &latest, &comment, &user_id).await {
        Ok(true) => {}
        Ok(false) => {
            return json_error(
                StatusCode::FORBIDDEN,
                "only the comment's author or an owner of that path may resolve it",
            )
        }
        Err(e) => return internal(e),
    }
    match changes::set_resolved(&state.db, &comment.id, Some(&user_id), resolved) {
        Ok(Ok(Some(c))) => {
            let actx = AuditCtx::of(&org_row.id, Some(&principal));
            crate::api::record_or_warn(
                &state.db,
                &actx,
                Some(&repo_row.id),
                if resolved {
                    "change.comment.resolve"
                } else {
                    "change.comment.unresolve"
                },
                Some(&serde_json::json!({
                    "change_key": change.change_key,
                    "comment": c.id,
                    "path": c.path,
                })),
            );
            (StatusCode::OK, Json(comment_json(&c))).into_response()
        }
        // It was loaded a moment ago; gone now means deleted in between.
        Ok(Ok(None)) => json_error(StatusCode::NOT_FOUND, "no such comment"),
        Ok(Err(e)) => json_error(StatusCode::BAD_REQUEST, e),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct PostCheck {
    /// CI-suite label, e.g. "ci/tests"; re-posting updates in place.
    pub name: String,
    /// pending | passing | failing — failing blocks landing.
    pub state: String,
    /// Where the run's detail lives (http/https).
    pub url: Option<String>,
}

/// POST /changes/:change/checks — external CI reports its verdict on
/// the latest patchset. A service token is the right credential here:
/// reporting a build is a machine's job, unlike approving.
pub async fn post_check(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<PostCheck>,
) -> Response {
    let (_, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    // A check on a closed change is a report about nothing: the
    // patchset either landed already or never will.
    if change.state != "open" && change.state != "landing" {
        return json_error(StatusCode::CONFLICT, format!("change is {}", change.state));
    }
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    match changes::set_check(
        &state.db,
        &change.id,
        &latest.id,
        &body.name,
        &body.state,
        body.url.as_deref(),
        &principal.audit_id(),
    ) {
        Ok(Ok(new)) => {
            let status = if new {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(serde_json::json!({
                    "name": body.name,
                    "state": body.state,
                    "url": body.url,
                    "patchset": latest.number,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => json_error(StatusCode::BAD_REQUEST, e),
        Err(e) => internal(e),
    }
}

/// GET /changes/:change/checks — the latest patchset's checks.
pub async fn checks(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (_, repo_row, _) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let latest = match changes::latest_patchset(&state.db, &change.id) {
        Ok(Some(p)) => p,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "change has no patchsets"),
        Err(e) => return internal(e),
    };
    // **The merged view, because the land gate decides on the merged
    // view.** This read was `checks_for(patchset)` — the intake's rows
    // and nothing else — while `land_gate` has always decided on
    // `merged_checks`: the intake unioned with the runs polled from the
    // provider for that patchset's commit. On a mirrored project the two
    // disagreed exactly where it hurts most. A failing GitHub Actions run
    // blocked the land and named itself in the reason, and the list the
    // author was reading did not contain it: "required check 'build' is
    // failing" above a table with no `build` in it, and no way from here
    // to find out where it ran.
    //
    // The merge is the same call the gate makes, so the page and the gate
    // cannot drift apart again — and `source` says which system to go and
    // look at, which is the whole reason the row carries it.
    //
    // **`required_checks` rides along, and is not derivable from `checks`.**
    // A required check that has never reported has no row to carry a
    // `required` flag on — that is what never-reported means — so a page
    // reading the rows alone counts only what did report and says "all
    // checks have passed" over a change the gate is holding. That was the
    // live defect: the queue answered `waiting on ci/tests` while the
    // review page showed one green row and an enabled Land button. The
    // names come from the same `merged_checks` call that marked the rows,
    // so the two halves cannot describe different moments.
    let (rows, required) = match changes::checks_and_required_for_change(&state.db, &change.id) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    Json(serde_json::json!({
        "patchset": latest.number,
        "required_checks": required,
        "checks": rows
            .iter()
            .map(|k| serde_json::json!({
                "name": k.name,
                "state": k.state,
                "url": k.detail_url,
                "required": k.required,
                "source": k.source.as_str(),
                "posted_by": k.posted_by,
                "updated_at": k.updated_at,
            }))
            .collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct ApplySuggestions {
    /// The comments whose suggestion blocks to apply, as **one** new
    /// patchset. A reviewer leaves five remarks and the author takes
    /// them together; a commit per click would put five revisions on the
    /// change and start CI five times for one act.
    pub comments: Vec<String>,
}

/// How many suggestions one call may apply.
///
/// Every one of them costs a blob read and a line-by-line rewrite inside
/// a single blocking task, and a review with more than fifty applicable
/// suggestions on one patchset is a rewrite somebody should push rather
/// than a set of remarks to take.
const MAX_SUGGESTIONS: usize = 50;

/// POST /changes/:change/suggestions/apply — take the reviewer's
/// suggestions and make them the next patchset.
///
/// **One patchset, through the same door a push uses.** The commit is
/// built here and then registered by [`register_patchset`], which is
/// what pins it, gives it its CI, retriggers the changeset and tells the
/// people OWNERS names. There is deliberately no second way to make a
/// patchset: a route that wrote its own change row would agree with the
/// push path today and disagree with it after the next edit to either.
///
/// **Why this is clean under fast-forward-only landing.** Applying a
/// suggestion does not rewrite anything: it is a new commit on top of
/// the patchset the reviewer read, registered as the next patchset of
/// the same change. Nothing about history changes, the previous patchset
/// stays pinned and readable, and the comments that were written against
/// it stay pinned to it.
///
/// **Write access, and the refusal says who does have it.** The button
/// makes a commit, so it needs the credential a commit needs. A reader
/// who may not push here is told that the author applies it, rather
/// than being handed a control that leads nowhere.
pub async fn apply_suggestions(
    State(state): State<SharedState>,
    Path((org, repo, key)): Path<(String, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ApplySuggestions>,
) -> Response {
    // Read first, so a repository this caller may not see is masked as
    // absent before the sentence below could confirm it exists.
    let (org_row, repo_row, principal) =
        match crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoRead) {
            Ok(x) => x,
            Err(r) => return r,
        };
    if crate::app::rest_repo_auth(&state, &headers, &org, &repo, Scope::RepoWrite).is_err() {
        return json_error(
            StatusCode::FORBIDDEN,
            format!(
                "applying a suggestion commits it to {org}/{repo}, which needs write \
                 access — the change's author applies it, or anybody else who can push here"
            ),
        );
    }
    let change = match change_or_404(&state, &repo_row.id, &key) {
        Ok(c) => c,
        Err(r) => return r,
    };
    // A landing, landed or abandoned change has no next patchset to
    // make, and a commit made against one would sit on no change at all.
    if change.state != "open" {
        return json_error(StatusCode::CONFLICT, format!("change is {}", change.state));
    }
    if body.comments.is_empty() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "name at least one comment to apply",
        );
    }
    if body.comments.len() > MAX_SUGGESTIONS {
        return json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "{} comments is more than the {MAX_SUGGESTIONS} one call applies; \
                 take them in batches",
                body.comments.len()
            ),
        );
    }
    let patchsets = match changes::patchsets(&state.db, &change.id) {
        Ok(p) => p,
        Err(e) => return internal(e),
    };
    let Some(latest) = patchsets.last().cloned() else {
        return json_error(StatusCode::NOT_FOUND, "change has no patchsets");
    };
    // **The commit must belong to this change, and only the Change-Id
    // trailer says so.** `register_patchset` keys a commit by its
    // trailer and falls back to the commit oid, so a patchset built from
    // a message with no trailer would open a *new* change under a key
    // nobody asked for — the reviewer's suggestion would vanish from the
    // change they left it on. The derived key is `g<oid>`, which is not
    // a legal trailer value, so there is nothing to write in either.
    // This is the same property the docs already state: a change with no
    // Change-Id does not survive a rewrite.
    if change_id::parse_change_id(&latest.message).is_none() {
        return json_error(
            StatusCode::CONFLICT,
            format!(
                "change {key} has no Change-Id trailer, so a commit made from it would \
                 open a new change rather than a patchset of this one — put a \
                 Change-Id in the commit message and push again"
            ),
        );
    }

    let mut apps: Vec<crate::review::suggestion::Application> =
        Vec::with_capacity(body.comments.len());
    for id in &body.comments {
        let c = match changes::comment_by_id(&state.db, id) {
            Ok(Some(c)) if c.change_id == change.id => c,
            // A comment on another change is masked as absent, the same
            // as a cross-org probe: an id leaks nothing by answering 404.
            Ok(_) => return json_error(StatusCode::NOT_FOUND, format!("no comment {id:?} here")),
            Err(e) => return internal(e),
        };
        // A draft is not a remark yet. Committing one would publish it in
        // the one place its author cannot take it back from.
        if c.published_at.is_none() {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!("comment {id:?} is still a draft; submit the review first"),
            );
        }
        let (Some(path), Some(line), Some(line_end)) = (c.path.clone(), c.line, c.line_end) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "comment {id:?} is not anchored to a line, so there is nothing for a \
                     suggestion to replace"
                ),
            );
        };
        // `side: old` names a line the patchset deleted. There is no such
        // line in the file to put anything in place of, and applying it
        // to the same number on the new side would edit whatever code
        // happens to sit there now.
        if c.side != changes::Side::New {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "comment {id:?} is on the old side of the diff, which has no line \
                     for a suggestion to replace"
                ),
            );
        }
        let mut blocks = crate::review::suggestion::parse(&c.body);
        if blocks.len() > 1 {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "comment {id:?} carries {} suggestion blocks for one anchor; they \
                     cannot all be lines {line}-{line_end}",
                    blocks.len()
                ),
            );
        }
        let Some(block) = blocks.pop() else {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!("comment {id:?} carries no suggestion block"),
            );
        };
        apps.push(crate::review::suggestion::Application {
            comment: id.clone(),
            path,
            patchset: c.patchset_number,
            edit: crate::review::suggestion::Edit {
                start: line,
                end: line_end,
                lines: block.lines,
            },
        });
    }
    // Refused in words rather than resolved. Two suggestions over one
    // line are two reviewers disagreeing, and quietly picking either
    // would commit a hybrid neither of them proposed.
    let anchors: Vec<crate::review::suggestion::Anchor> = apps
        .iter()
        .map(|a| crate::review::suggestion::Anchor {
            comment: a.comment.clone(),
            path: a.path.clone(),
            start: a.edit.start,
            end: a.edit.end,
        })
        .collect();
    if let Some((a, b)) = crate::review::suggestion::first_overlap(&anchors) {
        return json_error(
            StatusCode::CONFLICT,
            format!(
                "comments {:?} and {:?} both suggest changes to {} — lines {}-{} and \
                 {}-{} overlap. Apply one, then the other against the patchset it makes",
                a.comment, b.comment, a.path, a.start, a.end, b.start, b.end
            ),
        );
    }

    let store_url = state.store_url.clone();
    let prefix = repo_row.prefix().as_str().to_string();
    let parent = latest.commit_oid.clone();
    let latest_number = latest.number;
    // The commit each patchset was recorded at, so the staleness check
    // can ask whether the file is byte-identical to what the reviewer
    // read. A number this change does not have cannot occur — it is
    // written from the patchset the comment was made on — so it falls
    // back to the latest commit rather than inventing a refusal nothing
    // can reach.
    let at: HashMap<i64, String> = patchsets
        .iter()
        .map(|p| (p.number, p.commit_oid.clone()))
        .collect();
    // A machine may apply a suggestion — it is a mechanical act, not the
    // judgement an approval is — so a service token signs with its label
    // exactly as it does on the commit API.
    let author = crate::api::commits::acting_author(&state, Some(&principal), None);
    let message = latest.message.clone();
    let out = tokio::task::spawn_blocking({
        let parent = parent.clone();
        let message = message.clone();
        move || -> Result<
            Result<crate::review::suggestion::Made, (String, crate::review::suggestion::Stale)>,
            String,
        > {
            let store =
                stratum_store::ObjectStore::new(&store_url, stratum_store::LatencyModel::None);
            let mbytes = store.get(&format!("{prefix}/manifest.json"))?;
            let manifest: stratum_store::Manifest =
                serde_json::from_slice(&mbytes).map_err(|e| format!("manifest: {e}"))?;
            let made = match crate::review::suggestion::make(
                &store,
                &prefix,
                &manifest,
                &parent,
                latest_number,
                &|n| at.get(&n).cloned().unwrap_or_else(|| parent.clone()),
                &apps,
                &author,
                &message,
            )? {
                Ok(m) => m,
                Err(refusal) => return Ok(Err(refusal)),
            };
            // The objects and the pin in one transaction: an object
            // written with nothing naming it is collectable, and a ref
            // naming an object that is not there does not `fsck`. It is
            // the same `refs/patchsets/<oid>` pin `register_patchset`
            // writes, and it writes it again idempotently a moment later
            // — this one carries the pack.
            stratum_engine::refops::transact(
                &store,
                &prefix,
                &[stratum_engine::refops::RefUpdate {
                    name: format!("refs/patchsets/{}", made.commit),
                    expect: stratum_engine::refops::Expect::Any,
                    new: Some(made.commit.clone()),
                }],
                Some(&made.pack),
            )
            .map_err(crate::review::suggestion::txn_message)?;
            Ok(Ok(made))
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")));
    let made = match out {
        Ok(Ok(m)) => m,
        Ok(Err((comment, why))) => {
            return json_error(
                StatusCode::CONFLICT,
                format!("comment {comment:?} cannot be applied: {}", why.sentence()),
            )
        }
        Err(e) => return reads::err_to_response(e),
    };

    match register_patchset(
        &state,
        &org_row,
        &repo_row,
        Some(&principal),
        &change.target_branch,
        &made.commit,
        Some(&made.parent),
        &message,
        change.source_repo_id.as_deref(),
    )
    .await
    {
        Ok((change, patchset, _)) => {
            let actx = AuditCtx::of(&org_row.id, Some(&principal));
            crate::api::record_or_warn(
                &state.db,
                &actx,
                Some(&repo_row.id),
                "change.suggestions.apply",
                // Name the comments and the files: "applied some
                // suggestions" is not a trail anybody can audit, and this
                // is the one record that says whose words went in.
                Some(&serde_json::json!({
                    "change_key": change.change_key,
                    "patchset": patchset.number,
                    "comments": body.comments,
                    "paths": made.paths,
                })),
            );
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "change": change_json(&change, Some(&patchset), source_name(&state, &change)),
                    "patchset": patchset_json(&patchset),
                    "applied": body.comments,
                    "paths": made.paths,
                })),
            )
                .into_response()
        }
        Err(r) => r,
    }
}

fn comment_json(c: &changes::Comment) -> serde_json::Value {
    serde_json::json!({
        "id": c.id,
        "patchset": c.patchset_number,
        // A person shows as themselves; a service principal is honest
        // about being one rather than borrowing a human name.
        "author": c.author_name.clone().unwrap_or_else(|| "service".to_string()),
        "author_email": c.author_email,
        "author_principal": c.author_principal,
        "path": c.path,
        "line": c.line,
        "line_end": c.line_end,
        "side": c.side.as_str(),
        "parent_id": c.parent_id,
        // The grouping key, computed here rather than left to each
        // reader: a root's thread is itself, and two renderers that work
        // that out separately will one day work it out differently.
        "thread_id": c.thread_id(),
        // The anchor as first written. A client that finds `line` no
        // longer plausible against the patchset it is rendering can say
        // "written on patchset 2, line 41" instead of quietly drawing
        // the comment somewhere it never was.
        "original_line": c.original_line,
        "original_patchset": c.original_patchset,
        "external_id": c.external_id,
        // Whether this is still only the caller's. Nobody else can see
        // a row with `pending: true`, so a client that renders the flag
        // is telling its own user "this is not sent yet" — the one
        // thing a draft UI must never get wrong.
        "pending": c.published_at.is_none(),
        "review_id": c.review_id,
        "published_at": c.published_at,
        // Root rows only; a reply's thread carries the state.
        "resolved": c.resolved_at.is_some(),
        "resolved_at": c.resolved_at,
        "resolved_by": c.resolved_by_name,
        "body": c.body,
        "created_at": c.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::sufficiency::{PathRequirement, Requirement};

    fn owned(path: &str, owner_ids: &[&str], star: bool) -> PathRequirement {
        PathRequirement {
            path: path.to_string(),
            requirement: Requirement::Owned {
                owner_ids: owner_ids.iter().map(|s| s.to_string()).collect(),
                display: vec![if star { "*".into() } else { "someone".into() }],
                anyone_with_write: star,
            },
        }
    }

    fn user(id: &str, name: &str) -> stratum_control::users::User {
        stratum_control::users::User {
            id: id.to_string(),
            email: format!("{name}@acme.test").to_lowercase(),
            name: name.to_string(),
            created_at: 0,
            disabled_at: None,
            handle: None,
            verified_at: None,
        }
    }

    fn approval(user_id: &str) -> stratum_control::changes::Approval {
        stratum_control::changes::Approval {
            patchset_id: "p1".to_string(),
            user_id: user_id.to_string(),
            email: "who@acme.test".to_string(),
            name: "Who".to_string(),
            created_at: 0,
        }
    }

    /// A `*` path names nobody in particular, so it contributes no
    /// required reviewer — but it must still be *reported*, or a page
    /// with an empty list cannot tell "nothing here is owned" apart from
    /// "anyone with write may approve this".
    /// Every term of the query bar's grammar, and what each one means.
    ///
    /// Pure, so the whole grammar is pinned without a server: the wire
    /// tests then only have to prove that each parsed term reaches the
    /// query it claims to.
    #[test]
    fn the_query_grammar_reads_every_term_it_documents() {
        assert_eq!(parse_q("").unwrap(), Terms::default());
        assert_eq!(parse_q("   ").unwrap(), Terms::default());
        for state in ["open", "landing", "landed", "abandoned"] {
            assert_eq!(
                parse_q(&format!("is:{state}")).unwrap().state.as_deref(),
                Some(state)
            );
        }
        assert_eq!(
            parse_q("is:open author:@me needs:my-approval repo:web").unwrap(),
            Terms {
                state: Some("open".into()),
                author: Some("@me".into()),
                needs_my_approval: true,
                repo: Some("web".into()),
            },
            "the terms compose, in any order, and each is read once"
        );
        assert_eq!(
            parse_q("author:alice@acme.test").unwrap().author.as_deref(),
            Some("alice@acme.test")
        );
    }

    /// An unrecognised term is a refusal that **names it**.
    ///
    /// This is the whole contract: a filter that quietly did nothing
    /// would let somebody read an unfiltered list, conclude no work is
    /// waiting for them, and close the tab — and the same URL, shared,
    /// would mean something different to whoever opened it next. So
    /// every arm below asserts that the offending text is in the
    /// sentence, not merely that something was refused.
    #[test]
    fn an_unknown_term_is_refused_by_name() {
        for (q, quoted) in [
            // A filter nobody implements.
            ("assignee:me", "assignee"),
            // A word with no colon at all is not a term; there is no
            // free-text search here to fall back to.
            ("gateway", "gateway"),
            ("is:open gateway", "gateway"),
            // Known filter, unknown value.
            ("is:merged", "merged"),
            ("needs:review", "review"),
            ("author:not-an-email", "not-an-email"),
            ("repo:../etc", "../etc"),
            // Said twice: answering with the second silently discards
            // the first, and two states cannot both hold.
            ("is:open is:landed", "is"),
            ("author:@me author:bob@acme.test", "author"),
            ("repo:api repo:web", "repo"),
            // Even where the second says the same thing: a query typed
            // twice is a query nobody read back.
            ("needs:my-approval needs:my-approval", "needs"),
        ] {
            let e = parse_q(q).expect_err(&format!("{q:?} was accepted"));
            assert!(
                e.contains(quoted),
                "refusing {q:?} did not name {quoted:?}: {e}"
            );
        }
    }

    #[test]
    fn a_star_path_requires_nobody_but_is_still_announced() {
        let r = required_from(&[owned("docs/readme.md", &["u1", "u2"], true)]);
        assert!(r.ids.is_empty());
        assert!(r.anyone_with_write);
    }

    #[test]
    fn owned_paths_contribute_their_owners_and_ungoverned_ones_contribute_nothing() {
        let r = required_from(&[
            owned("pay/a.rs", &["u2", "u1"], false),
            owned("pay/b.rs", &["u1"], false),
            PathRequirement {
                path: "scratch.txt".to_string(),
                requirement: Requirement::Ungoverned,
            },
            PathRequirement {
                path: "broken/x.rs".to_string(),
                requirement: Requirement::OwnersError {
                    dir: "broken".to_string(),
                    line: 1,
                    message: "nope".to_string(),
                },
            },
        ]);
        assert_eq!(
            r.ids.iter().cloned().collect::<Vec<_>>(),
            vec!["u1".to_string(), "u2".to_string()],
            "the set is deduplicated across paths and ordered"
        );
        assert!(
            !r.anyone_with_write,
            "neither an ungoverned path nor a broken OWNERS file is a `*` rule"
        );
    }

    /// The tick against a name is the *reviewer's* approval, not any
    /// approval: a change approved by a bystander with write access must
    /// not show the owner it is still waiting on as satisfied.
    #[test]
    fn the_wire_marks_only_the_reviewers_who_actually_approved() {
        let review = ReviewState {
            verdict: sufficiency::evaluate(&[], &ApproverSet::default()),
            required: vec![user("u1", "Alice"), user("u2", "Bob")],
            anyone_with_write: false,
            approvals: vec![approval("u2"), approval("u9")],
            blocks: Vec::new(),
        };
        let j = reviewers_json(&review);
        assert_eq!(j["anyone_with_write"], serde_json::json!(false));
        assert_eq!(j["required"][0]["user_id"], serde_json::json!("u1"));
        assert_eq!(j["required"][0]["name"], serde_json::json!("Alice"));
        assert_eq!(
            j["required"][0]["email"],
            serde_json::json!("alice@acme.test")
        );
        assert_eq!(j["required"][0]["approved"], serde_json::json!(false));
        assert_eq!(j["required"][1]["user_id"], serde_json::json!("u2"));
        assert_eq!(j["required"][1]["approved"], serde_json::json!(true));
        assert_eq!(j["required"].as_array().unwrap().len(), 2);
    }

    fn writers(ids: &[&str]) -> std::collections::BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn standing(user_id: &str, email: &str, blocking: bool) -> StandingBlock {
        StandingBlock {
            blocking,
            review: stratum_control::changes::Review {
                id: "r1".into(),
                change_id: "c1".into(),
                patchset_id: "p1".into(),
                user_id: user_id.into(),
                verdict: stratum_control::changes::ReviewVerdict::RequestChanges,
                body: Some("not like this".into()),
                state: "submitted".into(),
                submitted_at: Some(1),
                withdrawn_at: None,
                external_id: None,
                created_at: 1,
                author_email: Some(email.into()),
                author_name: Some("Who".into()),
            },
        }
    }

    /// Who may wedge a change, path by path. This is the rule that
    /// separates our `request_changes` from GitHub's, so every arm of
    /// it is pinned here rather than only through a server.
    #[test]
    fn standing_to_block_follows_owners_and_nothing_else() {
        let owned_path = [owned("pay/a.rs", &["u1"], false)];
        assert!(stands_on_a_touched_path(&owned_path, &writers(&[]), "u1"));
        assert!(
            !stands_on_a_touched_path(&owned_path, &writers(&["u9"]), "u9"),
            "write access is not standing on a path OWNERS gives to somebody else"
        );

        // A `*` rule, and an ungoverned path, both say the same thing:
        // write access is what satisfies this, so write access is what
        // blocks it. Two rules for one file would be worse than either.
        let star = [owned("docs/x.md", &[], true)];
        assert!(stands_on_a_touched_path(&star, &writers(&["u9"]), "u9"));
        assert!(!stands_on_a_touched_path(&star, &writers(&[]), "u9"));
        let ungoverned = [PathRequirement {
            path: "scratch.txt".to_string(),
            requirement: Requirement::Ungoverned,
        }];
        assert!(stands_on_a_touched_path(
            &ungoverned,
            &writers(&["u9"]),
            "u9"
        ));
        assert!(!stands_on_a_touched_path(&ungoverned, &writers(&[]), "u9"));

        // A broken OWNERS file gives nobody standing: it blocks
        // everything already, and "who owns this" has no answer until
        // somebody fixes it.
        let broken = [PathRequirement {
            path: "broken/x.rs".to_string(),
            requirement: Requirement::OwnersError {
                dir: "broken".to_string(),
                line: 1,
                message: "nope".to_string(),
            },
        }];
        assert!(!stands_on_a_touched_path(&broken, &writers(&["u9"]), "u9"));

        // One touched path is enough: a reviewer who owns any of them
        // read the change, not a file.
        let mixed = [
            owned("pay/a.rs", &["u1"], false),
            owned("x/b.rs", &["u2"], false),
        ];
        assert!(stands_on_a_touched_path(&mixed, &writers(&[]), "u2"));
        assert!(!stands_on_a_touched_path(&mixed, &writers(&[]), "u3"));
    }

    /// An advisory block is recorded and rendered and moves nothing; an
    /// authoritative one refuses in the words the author reads.
    #[test]
    fn only_an_authoritative_block_turns_the_verdict() {
        let ok = || sufficiency::evaluate(&[], &ApproverSet::default());
        let mut v = ok();
        apply_blocks(&mut v, &[standing("u9", "passerby@acme.test", false)]);
        assert!(v.landable, "a passer-by must not be able to wedge a change");
        assert_eq!(v.explanation, ok().explanation);

        let mut v = ok();
        apply_blocks(
            &mut v,
            &[
                standing("u9", "passerby@acme.test", false),
                standing("u1", "alice@acme.test", true),
                standing("u2", "bo@acme.test", true),
            ],
        );
        assert!(!v.landable);
        assert_eq!(
            v.explanation,
            "blocked: alice@acme.test, bo@acme.test asked for changes; \
             it stands until they withdraw it",
            "the explanation names who, and does not promise a new \
             patchset will clear it"
        );
        assert!(
            v.per_path.is_empty(),
            "a block is about the change, not scattered over the paths \
             its author happens to own"
        );
    }
}
