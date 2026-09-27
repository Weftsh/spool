//! The changeset as one repository: built here, served by both front
//! doors, described by the REST view.
//!
//! Everything in this module answers one question — *may this caller see
//! this combination, and what is in it* — so that the HTTP wire, the SSH
//! wire and the JSON view cannot come to three different answers. The
//! objects themselves are [`stratum_proto::workspace`]'s; what lives
//! here is the authority rule and the member set.
//!
//! **The authority rule is the union of the members', not the
//! changeset's.** A changeset has no ACL of its own: it is readable
//! exactly by somebody who may read every repository it touches, because
//! its tree names those repositories, their branches and their proposed
//! commits. One member the caller cannot read and the whole workspace is
//! masked — not filtered down to the readable part, which would hand
//! over a checkout that silently differs from the changeset everybody
//! else is reviewing.

use crate::app::{SharedState, WireDeny};
use crate::authx;
use crate::workflow::trigger::{self, ComposedMember};
use axum::response::Response;
use stratum_control::auth::{Principal, Scope};
use stratum_control::changesets::{self, Changeset};
use stratum_control::registry;
use stratum_proto::workspace::{member_url, Workspace, WorkspaceMember, WorkspaceSpec};

/// What a push to a changeset workspace is told, on both transports.
///
/// It is a refusal with a direction: the combination is assembled from
/// the members and cannot be written to as a unit, but every commit in
/// it has a real repository it belongs in. A bare "read-only" would
/// leave somebody with a checkout and nowhere to put their work.
pub const READ_ONLY: &str = "a changeset workspace is read-only; push to its member repositories";

/// A changeset resolved to its members and to the repository they clone
/// as.
pub struct Built {
    /// The composition hash CI names this combination's runs with, or
    /// `None` when there is nothing to compose.
    pub composition: Option<String>,
    /// The composed members, ordered by repository name — the order the
    /// tree and the view both print, so a person reading one is reading
    /// the other.
    pub members: Vec<ComposedMember>,
    pub ws: Workspace,
}

/// Build the workspace for a changeset, whatever its state.
///
/// Never fails: an un-composable changeset — every member's repository
/// deleted, or a member with no patchset — is an *empty* workspace, and
/// the difference between "no combination" and "you may not look" is one
/// the caller has already settled before it gets here.
pub fn build(state: &SharedState, org_name: &str, cs: &Changeset) -> Built {
    let composed = trigger::compose(state, cs);
    let (composition, mut members) = match composed {
        Some((c, m)) => (Some(c), m),
        None => (None, Vec::new()),
    };
    members.sort_by(|a, b| a.repo.name.cmp(&b.repo.name));
    let ws = Workspace::build(&WorkspaceSpec {
        org: org_name.to_string(),
        key: cs.key.clone(),
        title: cs.title.clone(),
        composition: composition.clone().unwrap_or_default(),
        // The newest member tip, in seconds. Dating the commit by the
        // proposal rather than by the request is what makes the same
        // combination hash to the same commit on every node and on every
        // retry — see `WorkspaceSpec::timestamp_secs`.
        timestamp_secs: members.iter().map(|m| m.patchset_at).max().unwrap_or(0) / 1000,
        members: members
            .iter()
            .map(|m| WorkspaceMember {
                name: m.repo.name.clone(),
                commit: parse_oid(&m.commit_sha),
                url: member_url(&m.repo.name),
            })
            .collect(),
    });
    Built {
        composition,
        members,
        ws,
    }
}

/// A patchset commit as raw bytes.
///
/// A tip that is not 40 hex characters cannot have come from the receive
/// path or the commit API, both of which write what git gave them; the
/// zero oid is the safe reading of a row that has been corrupted anyway,
/// and it produces a gitlink git will simply fail to check out rather
/// than a tree built around a guess.
fn parse_oid(sha: &str) -> [u8; 20] {
    let mut out = [0u8; 20];
    for (i, b) in out.iter_mut().enumerate() {
        match sha
            .get(i * 2..i * 2 + 2)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            Some(v) => *b = v,
            None => return [0u8; 20],
        }
    }
    out
}

/// Where a changeset is cloned from over HTTP.
///
/// `/{org}/changesets/{key}.git` — five path segments where a repository
/// has four, which is what keeps it from colliding with a repository
/// literally named `changesets`. That name is reserved in the registry
/// for exactly this reason.
pub fn clone_url(state: &SharedState, org_name: &str, key: &str) -> String {
    format!("{}/{org_name}/changesets/{key}.git", state.public_url)
}

/// The same over SSH, or `None` on a deployment with no SSH front door.
///
/// `None` rather than a guessed URL: a clone URL that does not work is
/// worse than an absent one, because somebody copies it.
pub fn ssh_clone_url(state: &SharedState, org_name: &str, key: &str) -> Option<String> {
    state
        .ssh_public_url
        .as_ref()
        .map(|base| format!("{base}/{org_name}/changesets/{key}.git"))
}

/// Resolve and authorize a changeset wire request for a verified
/// identity — the core both transports share, in the shape
/// [`crate::app::wire_repo_for_principal`] has for repositories.
///
/// `None` for the principal is a refusal, not an anonymous read: SSH has
/// no anonymous door, and the HTTP one handles its own unauthenticated
/// case below where it can answer 401 and let git retry with
/// credentials.
pub fn wire_for_identity(
    state: &SharedState,
    principal: Option<&Principal>,
    org_name: &str,
    key: &str,
) -> Result<Built, WireDeny> {
    let Some(p) = principal else {
        return Err(WireDeny::NotFound);
    };
    let org = registry::org_by_name(&state.db, org_name)
        .map_err(WireDeny::Internal)?
        .ok_or(WireDeny::NotFound)?;
    // Before the member walk, because a changeset with no members would
    // otherwise pass a walk of nothing: a token minted for another
    // organisation must not learn whether this key exists.
    if p.org_id != org.id {
        return Err(WireDeny::NotFound);
    }
    let cs = changesets::get(&state.db, &org.id, strip_git(key))
        .map_err(|e| WireDeny::Internal(e.to_string()))?
        .ok_or(WireDeny::NotFound)?;
    let built = build(state, org_name, &cs);
    for m in &built.members {
        crate::app::wire_repo_for_principal(state, p, org_name, &m.repo.name, Scope::RepoRead)?;
    }
    Ok(built)
}

/// The HTTP door's version: credentials first, then the members, then
/// the public fallback.
///
/// The order is the one `app::wire_auth` uses for repositories and it is
/// deliberate — an invalid token 401s even for a changeset that does not
/// exist, so a probe cannot use the difference between 401 and 404 to
/// enumerate keys.
pub fn wire_auth(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    org_name: &str,
    key: &str,
) -> Result<Built, Response> {
    let principal = authx::principal_opt(&state.db, headers, authx::Challenge::Basic)?;
    if principal.is_some() {
        return wire_for_identity(state, principal.as_ref(), org_name, key).map_err(|d| match d {
            WireDeny::NotFound => authx::not_found(),
            // The workspace's own read-only refusal is rendered by its
            // push door, not here: this seam only ever decides readability.
            WireDeny::ReadOnly(msg) => authx::forbidden(&msg),
            WireDeny::Internal(e) => crate::api::internal(e),
        });
    }
    // Anonymous. A changeset is public when every repository it touches
    // is: the tree names them all, so one private member makes the whole
    // combination private. A changeset with **no** members is not public
    // either — there is no member set to be public over, and answering
    // it anonymously would confirm the key exists.
    let unauthorized = || authx::unauthorized(authx::Challenge::Basic);
    let org = registry::org_by_name(&state.db, org_name)
        .map_err(crate::api::internal)?
        .ok_or_else(unauthorized)?;
    let cs = changesets::get(&state.db, &org.id, strip_git(key))
        .map_err(|e| crate::api::internal(e.to_string()))?
        .ok_or_else(unauthorized)?;
    let built = build(state, org_name, &cs);
    if built.members.is_empty() || !built.members.iter().all(|m| m.repo.public) {
        return Err(unauthorized());
    }
    Ok(built)
}

/// git appends `.git` to the path it asks for; the key does not have it.
fn strip_git(key: &str) -> &str {
    key.strip_suffix(".git").unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tip is read byte for byte, and anything that is not a full
    /// sha-1 reads as the zero oid rather than as a truncated one — a
    /// half-parsed commit id in a tree is a checkout that fails on the
    /// wrong member.
    #[test]
    fn a_member_tip_is_parsed_whole_or_not_at_all() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let parsed = parse_oid(sha);
        assert_eq!(parsed[0], 0x01);
        assert_eq!(parsed[19], 0x67);
        for bad in ["", "0123", &sha[..39], "zz".repeat(20).as_str()] {
            assert_eq!(parse_oid(bad), [0u8; 20], "{bad:?}");
        }
        // Longer than a sha-1 is not an error: the first 20 bytes are
        // what a gitlink holds, and the row cannot be longer anyway.
        assert_eq!(parse_oid(&format!("{sha}ff")), parsed);
    }

    #[test]
    fn a_clone_path_may_carry_dot_git_or_not() {
        assert_eq!(strip_git("Ic5.git"), "Ic5");
        assert_eq!(strip_git("Ic5"), "Ic5");
        assert_eq!(strip_git(".git"), "");
    }
}
