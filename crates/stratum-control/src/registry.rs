//! Org/repo registry and the multi-tenant addressing boundary.
//!
//! `RepoPrefix` — the object-store key prefix a repo's data lives under —
//! can only be constructed here, after a lookup that is always scoped
//! `WHERE org_id = $1`. There is no code path from a request string to an
//! S3 key that bypasses this module: that is the R8 isolation enforcement
//! point.
//!
//! [`PackagePrefix`] is the same construction for the package registry's
//! key-space, `o/<org>/pkg/…`, and exists here rather than beside the
//! package tables for exactly that reason: R8 is a property of *one*
//! place being able to turn a request into a key, and a second module
//! that could also do it would make the sentence above false. It takes
//! an [`Org`] because a package blob is addressed by its content and
//! owned by the organization — there is no repository in the path.
//!
//! `docs_e2e::only_the_addressing_boundary_builds_store_keys` holds
//! this: it scrapes every `"o/` literal in the workspace and fails on
//! one outside the short allowlist it names.

use crate::db::{is_unique_violation, ControlDb};
use crate::ids::{now_ms, ulid};
use postgres::Row;
use serde::Serialize;

/// The single production layout name. Object keys:
/// `o/<org_ulid>/r/<repo_ulid>/<LAYOUT>/…`
pub const LAYOUT: &str = "prod";

#[derive(Debug, Clone, Serialize)]
pub struct Org {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoKind {
    Native,
    Mirror,
}

impl RepoKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RepoKind::Native => "native",
            RepoKind::Mirror => "mirror",
        }
    }

    fn parse(s: &str) -> RepoKind {
        if s == "mirror" {
            RepoKind::Mirror
        } else {
            RepoKind::Native
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub id: String,
    pub org_id: String,
    pub name: String,
    /// A sentence about what this repository is. Optional, and the only
    /// free text a stranger can find a repo by.
    pub description: Option<String>,
    /// The project's own address on the web.
    ///
    /// Not derivable from anything else we hold: a project's site is
    /// not its README and not its origin URL, and asking a maintainer
    /// to put it in the description spends the one sentence a stranger
    /// reads first on a URL. `None` and `Some("")` are not both
    /// reachable — see [`clean_homepage`].
    pub homepage: Option<String>,
    pub kind: RepoKind,
    pub default_branch: String,
    pub origin_url: Option<String>,
    pub origin_provider: Option<String>,
    pub origin_installation: Option<String>,
    pub last_sync_at: Option<i64>,
    pub last_synced_commit: Option<String>,
    pub sync_error: Option<String>,
    pub created_at: i64,
}

/// Object-store key prefix for one repo's layout. Constructible only by
/// this module (private field), always via an org-scoped lookup.
#[derive(Debug, Clone)]
pub struct RepoPrefix(String);

impl RepoPrefix {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Repo {
    pub fn prefix(&self) -> RepoPrefix {
        RepoPrefix(format!("o/{}/r/{}/{LAYOUT}", self.org_id, self.id))
    }

    fn from_row(row: &Row) -> Repo {
        Repo {
            id: row.get("id"),
            org_id: row.get("org_id"),
            name: row.get("name"),
            description: row.get("description"),
            homepage: row.get("homepage"),
            kind: RepoKind::parse(row.get("kind")),
            default_branch: row.get("default_branch"),
            origin_url: row.get("origin_url"),
            origin_provider: row.get("origin_provider"),
            origin_installation: row.get("origin_installation"),
            last_sync_at: row.get("last_sync_at"),
            last_synced_commit: row.get("last_synced_commit"),
            sync_error: row.get("sync_error"),
            created_at: row.get("created_at"),
        }
    }
}

const REPO_COLS: &str = "id, org_id, name, description, homepage, kind, \
     default_branch, origin_url, origin_provider, origin_installation, last_sync_at, \
     last_synced_commit, sync_error, created_at";

/// Valid org and repo names: DNS-label-ish, no path or key metacharacters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !name.bytes().all(|b| b == b'.')
        && !name.starts_with('.')
}

/// Namespace names nobody may take.
///
/// A namespace is a *top-level path segment* — `/<namespace>/<repo>.git`
/// is the git wire — so any name the router already claims is a name
/// that can never be cloned or pushed to. `dashboard` is the sharp one:
/// `/dashboard/*path` is a catch-all, so an org called `dashboard` would
/// have the SPA answer every git request for it, forever, with no error
/// anyone could act on.
///
/// Four groups, and they matter for different reasons:
///
/// * **Router segments.** Correctness. A test scrapes `app.rs` and fails
///   if a new top-level route appears that is not listed here, so this
///   cannot quietly fall behind the thing it is protecting.
/// * **Site pages.** Correctness, but the router cannot see it. These
///   are Astro files answered by the axum `.fallback`, so no `.route()`
///   literal mentions them and the scraping test finds nothing. That is
///   how `monorepo` — a page the company sells from — went unreserved;
///   `no_site_page_can_be_taken_as_a_namespace` in `docs_e2e` now reads
///   the page directory itself so the two cannot drift again.
/// * **SPA segments.** Correctness, and invisible for the same reason
///   once removed: the dashboard's client-side router claims these
///   first path segments and nothing on the server side declares them.
/// * **The squatting set.** Judgement. `admin`, `support`, `security`
///   and friends are names a stranger taking them could use to look
///   official. Nothing breaks if one is missed; somebody gets phished.
///
/// Held here rather than at any one door because org creation, the
/// admin CLI and personal handles must all obey it, and three copies of
/// a denylist is two too many.
const RESERVED: &[&str] = &[
    // Router segments — kept in step by the scraping test.
    "dashboard",
    "healthz",
    "readyz",
    "metrics",
    "webhooks",
    "openapi.json",
    "v1",
    // The OCI distribution API's root. Not a choice: a container client
    // parses `host/path/image` as registry `host` and repository
    // `path/image`, so the API has to answer at the host's own `/v2/`
    // and cannot live under `/v1/` with the rest. The other four
    // ecosystems take an arbitrary base URL and do live under `/v1/`.
    "v2",
    // The registry's own names, held whether or not each one is a route
    // today: `npm`, `cargo` and the rest are exactly the names somebody
    // would try to take, and a namespace that shadowed one would
    // be unreachable in every client that hard-codes the ecosystem in a
    // URL.
    "cargo",
    "maven",
    "npm",
    "oci",
    "pypi",
    "registry",
    // Served or generated assets that share the namespace.
    "assets",
    "docs",
    "llms.txt",
    "llms-full.txt",
    // The hosted product's site pages. Reserved here too, so a namespace
    // moved between the two editions keeps the same address in both.
    "ai-policy",
    "discover",
    "github-runners",
    "gitfarm",
    "migrate",
    "mirror",
    "monorepo",
    "open-source",
    "packages",
    "privacy",
    "repos",
    "search",
    "supply-chain",
    // Top-level segments the dashboard SPA claims. Held here because
    // nothing declares them to the server: the SPA is served by the
    // fallback, so the router scraper cannot see one appear and a
    // namespace taking one would be shadowed on the web and unreachable
    // in the browser. `login` is already in the squatting set below.
    "explore",
    "feed",
    "issues",
    "notifications",
    "orgs",
    "stars",
    "topics",
    // The usual impersonation set.
    "about",
    "admin",
    "api",
    "billing",
    "blog",
    "contact",
    "help",
    "login",
    "logout",
    "new",
    "pricing",
    "root",
    "security",
    "settings",
    "signup",
    "status",
    "stratum",
    "support",
    "weft",
    "www",
];

/// Is this name taken by the platform itself?
///
/// Case-folded, because the namespace is: `Dashboard` and `dashboard`
/// are the same name, and a denylist that only catches one spelling
/// catches nothing.
pub fn is_reserved(name: &str) -> bool {
    let folded = name.to_ascii_lowercase();
    RESERVED.contains(&folded.as_str())
}

/// Every reserved name, for the test that keeps this in step with the
/// router. Not part of the API otherwise.
pub fn reserved_names() -> &'static [&'static str] {
    RESERVED
}

/// Repository names the platform has taken inside a namespace.
///
/// A shorter list than [`RESERVED`] and a different question: that one
/// is about the *first* path segment, this one about the second.
/// `changesets` is here because `/{org}/changesets/{key}.git` is the git
/// wire address of a changeset's workspace — five segments where a
/// repository has four — and a repository actually named `changesets`
/// would sit one segment short of it, close enough that every mistyped
/// URL lands on the wrong one and nobody can tell which.
const RESERVED_REPO: &[&str] = &["changesets"];

/// A repository name the platform has not already claimed.
///
/// Case-folded like the namespace list, and for the same reason: the
/// router does not care about case, so a denylist that catches one
/// spelling catches nothing.
///
/// The refusal starts with `invalid ` deliberately — that is what
/// `errclass::is_invalid_input` reads, and it is the difference between
/// a 400 that tells somebody to pick another name and a 500 that tells
/// them nothing.
pub fn valid_repo_name(name: &str) -> Result<(), String> {
    let folded = name.to_ascii_lowercase();
    if RESERVED_REPO.contains(&folded.as_str()) {
        return Err(format!(
            "invalid repo name {name:?}: reserved for changeset workspaces"
        ));
    }
    Ok(())
}

/// A namespace name that is well-formed *and* not the platform's.
///
/// The two checks are separate because their answers are: an ill-formed
/// name is definitionally absent and must read as "not found" on lookup,
/// while a reserved one exists as far as the router is concerned and
/// must be refused at creation with a reason.
///
/// `noun` is what the caller calls this thing — "org", "handle" — so the
/// refusal names what the person was actually typing.
pub fn valid_namespace_name(noun: &str, name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("invalid {noun} name {name:?}"));
    }
    if is_reserved(name) {
        return Err(format!("{noun} name {name:?} is reserved"));
    }
    Ok(())
}

pub fn create_org(db: &ControlDb, name: &str) -> Result<Org, String> {
    valid_namespace_name("org", name)?;
    let org = Org {
        id: ulid(),
        name: name.to_string(),
        created_at: now_ms(),
    };
    db.lock()
        .execute(
            "INSERT INTO orgs (id, name, created_at) VALUES ($1, $2, $3)",
            &[&org.id, &org.name, &org.created_at],
        )
        .map_err(|e| {
            if is_unique_violation(&e) {
                format!("org {name:?} already exists")
            } else {
                e.to_string()
            }
        })?;
    Ok(org)
}

/// Create somebody's personal namespace: an `orgs` row they own.
///
/// GitHub's model, and the reason for it is blast radius. Both sides are
/// "owners" of repositories, so keeping one table means every
/// `org_id`-bearing table, both authorization seams, all three front
/// doors and the object-store prefix keep working untouched. A parallel
/// owner table would have forked `org_or_404`, `repo_or_404`,
/// `wire_repo_for_principal`, `authx::require` and `Repo::prefix`.
///
/// The owner is made a member at `owner` role in the same transaction:
/// every existing authorization check asks about membership, and a
/// namespace whose owner is not a member of it would be a namespace they
/// could not use.
///
/// Personal namespaces cannot take members, teams or invites. That is
/// enforced at those call sites rather than here, because "can this
/// namespace have members?" is a question about the namespace, not about
/// its creation.
pub fn create_personal_namespace(
    db: &ControlDb,
    user_id: &str,
    handle: &str,
    audit: Option<&crate::audit::AuditCtx>,
) -> Result<Org, String> {
    valid_namespace_name("handle", handle)?;
    let org = Org {
        id: ulid(),
        name: handle.to_string(),
        created_at: now_ms(),
    };
    let o = org.clone();
    let uid = user_id.to_string();
    let blob = serde_json::json!({ "handle": handle, "user_id": user_id });
    db.lock()
        .transaction(move |tx| {
            claim_personal_namespace_tx(tx, &o, &uid)?;
            if let Some(ctx) = audit {
                crate::audit::record_tx(tx, ctx, None, "namespace.create", Some(&blob))?;
            }
            Ok(())
        })
        .map_err(|e| {
            if is_unique_violation(&e) {
                format!("{handle:?} is taken")
            } else {
                format!("create personal namespace: {e}")
            }
        })?;
    Ok(org)
}

/// The three statements a personal namespace is, inside somebody else's
/// transaction.
///
/// Shared by [`create_personal_namespace`] and by accepting an
/// invitation, which creates the account and its namespace together: a
/// second transaction after the first commits is exactly the window in
/// which an account exists with no handle, and both of the things a
/// handle-less account lacks are silent (see
/// [`crate::users::without_handle`]). A name somebody already holds is a
/// unique violation on `orgs`, which rolls the caller's whole
/// transaction back — see [`is_namespace_taken`].
pub(crate) fn claim_personal_namespace_tx(
    tx: &mut postgres::Transaction,
    org: &Org,
    user_id: &str,
) -> Result<(), postgres::Error> {
    tx.execute(
        "INSERT INTO orgs (id, name, created_at, kind, owner_user_id) \
         VALUES ($1, $2, $3, 'personal', $4)",
        &[&org.id, &org.name, &org.created_at, &user_id],
    )?;
    tx.execute(
        "INSERT INTO org_members (org_id, user_id, role, created_at) \
         VALUES ($1, $2, 'owner', $3)",
        &[&org.id, &user_id, &org.created_at],
    )?;
    tx.execute(
        "UPDATE users SET handle = $2 WHERE id = $1",
        &[&user_id, &org.name],
    )?;
    Ok(())
}

/// Did this statement fail because the namespace name is somebody's
/// already?
///
/// Asked of the constraint, not the message: `orgs.name` is unique both
/// as written and case-folded (`orgs_name_folded`), and a transaction
/// that also inserts a `users` row can fail on *that* table's unique
/// address instead, which is a different refusal with a different fix.
pub(crate) fn is_namespace_taken(e: &postgres::Error) -> bool {
    is_unique_violation(e)
        && e.as_db_error()
            .and_then(|d| d.constraint())
            .is_some_and(|c| c.starts_with("orgs_name"))
}

/// A namespace name made from something a person already answers to —
/// the part of their address before the `@`, or a GitHub login — for
/// when nobody typed one.
///
/// Anything outside the alphabet becomes a dash, so `ada.lovelace`
/// reads as `ada-lovelace` rather than `adalovelace`; never a dot,
/// because a dot-led name is one [`valid_name`] refuses.
///
/// Pure, so it can be tested exhaustively without a database. The
/// output always passes [`valid_name`]: non-empty, well inside the
/// length bound (leaving room for a suffix), and built from the allowed
/// alphabet. It can still be *reserved* or *taken*; that is the caller's
/// to find out, because only the caller knows what to do about it.
pub fn handle_from(seed: &str) -> String {
    let mut out = String::new();
    for c in seed.to_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '-'
        };
        // One dash for a run of anything else: `a..b` is `a-b`.
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let trimmed: String = out.trim_matches(['-', '_']).chars().take(60).collect();
    let trimmed = trimmed.trim_end_matches(['-', '_']);
    if trimmed.is_empty() {
        "user".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A handle nobody holds, made from `seed` for somebody who did not
/// choose one: [`handle_from`]'s name if it is free and not reserved,
/// otherwise the same name with a short random suffix. `None` when both
/// are taken, which a caller reports rather than retrying forever.
///
/// Free *at the time of asking*: the caller claims it inside its own
/// transaction, and a unique violation there — somebody took it in
/// between — is [`is_namespace_taken`].
pub fn free_handle_near(db: &ControlDb, seed: &str) -> Result<Option<String>, String> {
    let base = handle_from(seed);
    let free = |name: &str| -> Result<bool, String> {
        Ok(valid_namespace_name("handle", name).is_ok() && org_by_name(db, name)?.is_none())
    };
    if free(&base)? {
        return Ok(Some(base));
    }
    let alt = format!("{base}-{}", &crate::ids::token_secret()[..6].to_lowercase());
    if free(&alt)? {
        return Ok(Some(alt));
    }
    Ok(None)
}

/// Is this namespace a person's own?
///
/// The one question that separates the two kinds, asked wherever
/// something is meaningless for a personal namespace — inviting somebody
/// into it, giving it teams, billing it for seats.
pub fn is_personal(db: &ControlDb, org_id: &str) -> Result<bool, String> {
    db.lock()
        .query_opt("SELECT kind FROM orgs WHERE id = $1", &[&org_id])
        .map_err(|e| format!("namespace kind: {e}"))
        .map(|r| r.is_some_and(|r| r.get::<_, String>("kind") == "personal"))
}

/// Create an organization with its founder already inside it.
///
/// One transaction, for the same reason a personal namespace is: every
/// authorization check asks about membership, so an org whose creator is
/// not a member of it is an org they cannot use — and cannot fix,
/// because fixing it needs `org:admin` on the thing they are locked out
/// of.
pub fn create_org_owned_by(db: &ControlDb, name: &str, user_id: &str) -> Result<Org, String> {
    valid_namespace_name("organization", name)?;
    let org = Org {
        id: ulid(),
        name: name.to_string(),
        created_at: now_ms(),
    };
    let (id, oname, at, uid) = (
        org.id.clone(),
        org.name.clone(),
        org.created_at,
        user_id.to_string(),
    );
    db.lock()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO orgs (id, name, created_at) VALUES ($1, $2, $3)",
                &[&id, &oname, &at],
            )?;
            tx.execute(
                "INSERT INTO org_members (org_id, user_id, role, created_at) \
                 VALUES ($1, $2, 'owner', $3)",
                &[&id, &uid, &at],
            )?;
            let ctx = crate::audit::AuditCtx {
                principal: format!("user:{uid}"),
                user_id: Some(uid.clone()),
                org_id: id.clone(),
            };
            crate::audit::record_tx(
                tx,
                &ctx,
                None,
                "org.create",
                Some(&serde_json::json!({ "name": oname })),
            )?;
            Ok(())
        })
        .map_err(|e| {
            // The error code, not its text: a `postgres::Error` from
            // inside a transaction stringifies to a bare "db error", so
            // matching on words gave every taken name an unhelpful 400.
            if crate::db::is_unique_violation(&e) {
                format!("organization {name:?} already exists")
            } else {
                e.to_string()
            }
        })?;
    Ok(org)
}

/// Readiness probe: proves the connection answers a query.
pub fn ping(db: &ControlDb) -> Result<(), String> {
    db.lock()
        .query_one("SELECT count(*) FROM orgs", &[])
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn org_by_name(db: &ControlDb, name: &str) -> Result<Option<Org>, String> {
    // A name that can't be a valid identifier names nothing — short-circuit
    // to "not found" rather than round-tripping hostile bytes (NUL, control
    // chars, injection shapes) to Postgres, which would surface as a 500
    // instead of the existence-masking 404 the lookup contract promises.
    if !valid_name(name) {
        return Ok(None);
    }
    // Case-folded: the namespace is, so `Acme` and `acme` must resolve to
    // the same row rather than to two.
    db.lock()
        .query_opt(
            "SELECT id, name, created_at FROM orgs WHERE lower(name) = lower($1)",
            &[&name],
        )
        .map(|row| {
            row.map(|r| Org {
                id: r.get(0),
                name: r.get(1),
                created_at: r.get(2),
            })
        })
        .map_err(|e| e.to_string())
}

/// Lookup by id, for the paths that already hold an org id — a user's
/// memberships name orgs by id, and re-deriving the name for display
/// should not require a second name-based round trip.
pub fn org_by_id(db: &ControlDb, id: &str) -> Result<Option<Org>, String> {
    db.lock()
        .query_opt(
            "SELECT id, name, created_at FROM orgs WHERE id = $1",
            &[&id],
        )
        .map(|row| {
            row.map(|r| Org {
                id: r.get(0),
                name: r.get(1),
                created_at: r.get(2),
            })
        })
        .map_err(|e| e.to_string())
}

pub struct NewRepo<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub kind: RepoKind,
    pub default_branch: &'a str,
    pub origin_url: Option<&'a str>,
    pub origin_provider: Option<&'a str>,
    pub origin_installation: Option<&'a str>,
}

pub fn create_repo(db: &ControlDb, org_id: &str, new: &NewRepo) -> Result<Repo, String> {
    if !valid_name(new.name) {
        return Err(format!("invalid repo name {:?}", new.name));
    }
    valid_repo_name(new.name)?;
    let id = ulid();
    let created_at = now_ms();
    db.lock()
        .execute(
            "INSERT INTO repos (id, org_id, name, description, kind, default_branch, \
             origin_url, origin_provider, origin_installation, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &id,
                &org_id,
                &new.name,
                &new.description,
                &new.kind.as_str(),
                &new.default_branch,
                &new.origin_url,
                &new.origin_provider,
                &new.origin_installation,
                &created_at,
            ],
        )
        .map_err(|e| {
            if is_unique_violation(&e) {
                format!("repo {:?} already exists", new.name)
            } else {
                e.to_string()
            }
        })?;
    repo_by_id(db, org_id, &id)?.ok_or_else(|| "repo vanished after insert".into())
}

pub fn repo_by_name(db: &ControlDb, org_id: &str, name: &str) -> Result<Option<Repo>, String> {
    // As in `org_by_name`: an invalid-shaped name is definitionally absent,
    // so never feed it to the query (NUL bytes would 500).
    if !valid_name(name) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            &format!(
                "SELECT {REPO_COLS} FROM repos \
                 WHERE org_id = $1 AND name = $2 AND state = 'active'"
            ),
            &[&org_id, &name],
        )
        .map(|row| row.as_ref().map(Repo::from_row))
        .map_err(|e| e.to_string())
}

pub fn repo_by_id(db: &ControlDb, org_id: &str, id: &str) -> Result<Option<Repo>, String> {
    db.lock()
        .query_opt(
            &format!(
                "SELECT {REPO_COLS} FROM repos \
                 WHERE org_id = $1 AND id = $2 AND state = 'active'"
            ),
            &[&org_id, &id],
        )
        .map(|row| row.as_ref().map(Repo::from_row))
        .map_err(|e| e.to_string())
}

/// A repository by id alone, without knowing its namespace.
///
/// Deliberately separate from `repo_by_id`, which takes an `org_id` and
/// is the right function almost everywhere: scoping a lookup to the
/// namespace the request is about is what stops one org reading
/// another's rows by guessing an id.
///
/// This one exists for the cases where the id came from **our own
/// column** rather than from a caller — `changes.source_repo_id`, which
/// points at a fork in somebody else's namespace by construction. There
/// is no org to scope to, because crossing namespaces is the point.
/// Never call it with an id a client supplied.
///
/// Tombstoned repositories are excluded, so a deleted fork resolves to
/// `None` rather than to a row nothing else will honour.
pub fn repo_by_id_any(db: &ControlDb, id: &str) -> Result<Option<Repo>, String> {
    if !crate::ids::valid_id(id) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            &format!("SELECT {REPO_COLS} FROM repos WHERE id = $1 AND state = 'active'"),
            &[&id],
        )
        .map(|row| row.as_ref().map(Repo::from_row))
        .map_err(|e| e.to_string())
}

/// Hard-remove a repo row — rollback for a create whose storage init
/// failed. Never used on repos that may hold data.
pub fn purge_repo(db: &ControlDb, org_id: &str, id: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "DELETE FROM repos WHERE org_id = $1 AND id = $2",
            &[&org_id, &id],
        )
        .map(|_| ())
        .map_err(|e| {
            // Postgres renders a constraint violation through `Display`
            // as, literally, "db error" — which is what an operator saw
            // when this refused. That was tolerable while nothing could
            // refuse it; a repository whose epochs another repository's
            // fork is still reading now can, by design, and "db error"
            // is a worse answer than the silence it replaced.
            //
            // The refusal is the feature: those references are what keep
            // the fork's data alive, and deleting the row would drop
            // them and hollow the fork out a grace window later. So it
            // says that, and says what to do about it.
            // Narrowed to the constraint, not to the SQL state. Any
            // foreign key pointing at `repos` produces the same
            // `FOREIGN_KEY_VIOLATION`, and this arm claimed all of them
            // for one cause — so `branch_protections` and `repo_grants`,
            // which referenced `repos(id)` with no `ON DELETE` and made
            // a protected repository unpurgeable outright, both reported
            // "still has forks". A message naming the wrong cause is
            // worse than one naming none: it comes with advice, and
            // following it does nothing.
            //
            // Both of those now cascade, so neither can reach here. This
            // stays keyed on the constraint anyway, because the next
            // table to reference `repos` will be added by somebody who
            // is not thinking about this function, and the failure they
            // get should say "db error" and send them here rather than
            // send them hunting forks that do not exist.
            let db = e.as_db_error();
            let is_fk = db
                .map(|d| *d.code() == postgres::error::SqlState::FOREIGN_KEY_VIOLATION)
                .unwrap_or(false);
            let from_epochs = db
                .and_then(|d| d.constraint())
                .map(|c| c.starts_with("epoch_refs"))
                .unwrap_or(false);
            if is_fk && from_epochs {
                return "this repository still has forks reading its storage — \
                        promote or delete them first, and the storage becomes \
                        collectable"
                    .to_string();
            }
            e.to_string()
        })
}

/// Tombstone: the repo vanishes from routing/auth instantly; the S3 prefix
/// sweep is the GC worker's job.
pub fn delete_repo(db: &ControlDb, org_id: &str, id: &str) -> Result<bool, String> {
    let n = db
        .lock()
        .execute(
            "UPDATE repos SET state = 'deleted', deleted_at = $3 \
             WHERE org_id = $1 AND id = $2 AND state = 'active'",
            &[&org_id, &id, &now_ms()],
        )
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

/// Keyset pagination over (org_id, id): stable under inserts, O(page) at
/// any depth — this is what makes 1M+ repos per org listable.
pub fn list_repos(
    db: &ControlDb,
    org_id: &str,
    after_id: Option<&str>,
    limit: usize,
) -> Result<Vec<Repo>, String> {
    let limit = limit.clamp(1, 1000) as i64;
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {REPO_COLS} FROM repos \
                 WHERE org_id = $1 AND state = 'active' AND id > $2 \
                 ORDER BY id LIMIT $3"
            ),
            &[&org_id, &after_id.unwrap_or(""), &limit],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(Repo::from_row).collect())
}

pub fn count_repos(db: &ControlDb, org_id: &str) -> Result<u64, String> {
    db.lock()
        .query_one(
            "SELECT COUNT(*) FROM repos WHERE org_id = $1 AND state = 'active'",
            &[&org_id],
        )
        .map(|r| r.get::<_, i64>(0) as u64)
        .map_err(|e| e.to_string())
}

/// All active mirror repos whose origin identity matches (webhook fan-out).
pub fn mirrors_by_origin(
    db: &ControlDb,
    provider: &str,
    origin: &str,
) -> Result<Vec<Repo>, String> {
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT {REPO_COLS} FROM repos \
                 WHERE kind = 'mirror' AND state = 'active' \
                   AND origin_provider = $1 AND origin_url = $2"
            ),
            &[&provider, &origin],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(Repo::from_row).collect())
}

/// All active mirrors (poller sweep).
pub fn list_mirrors(db: &ControlDb) -> Result<Vec<Repo>, String> {
    let rows = db
        .lock()
        .query(
            &format!("SELECT {REPO_COLS} FROM repos WHERE kind = 'mirror' AND state = 'active'"),
            &[],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(Repo::from_row).collect())
}

/// All org ids (worker sweeps).
pub fn all_org_ids(db: &ControlDb) -> Result<Vec<String>, String> {
    let rows = db
        .lock()
        .query("SELECT id FROM orgs", &[])
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

/// Every active repo across all orgs (GC sweep).
pub fn all_active_repos(db: &ControlDb) -> Result<Vec<Repo>, String> {
    let rows = db
        .lock()
        .query(
            &format!("SELECT {REPO_COLS} FROM repos WHERE state = 'active'"),
            &[],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(Repo::from_row).collect())
}

/// Deleted repos whose tombstone predates `cutoff_ms` and whose storage
/// hasn't been purged yet: (org_id, repo_id).
pub fn deleted_repos_older_than(
    db: &ControlDb,
    cutoff_ms: i64,
) -> Result<Vec<(String, String)>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT org_id, id FROM repos WHERE state = 'deleted' \
             AND deleted_at IS NOT NULL AND deleted_at < $1 \
             AND sync_error IS DISTINCT FROM 'purged'",
            &[&cutoff_ms],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// Mark a deleted repo's storage as swept (reuses sync_error as the purge
/// marker on tombstoned rows — never read for anything else once deleted).
pub fn mark_purged(db: &ControlDb, repo_id: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE repos SET sync_error = 'purged' WHERE id = $1 AND state = 'deleted'",
            &[&repo_id],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Attach the App installation a mirror forwards pushes through.
pub fn set_origin_installation(
    db: &ControlDb,
    repo_id: &str,
    installation: &str,
) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE repos SET origin_installation = $2 WHERE id = $1",
            &[&repo_id, &installation],
        )
        .map(|_| ())
        .map_err(|e| format!("set origin installation: {e}"))
}

pub fn set_default_branch(db: &ControlDb, repo_id: &str, branch: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE repos SET default_branch = $2 WHERE id = $1",
            &[&repo_id, &branch],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn set_sync_state(
    db: &ControlDb,
    repo_id: &str,
    synced_commit: Option<&str>,
    error: Option<&str>,
) -> Result<(), String> {
    db.lock()
        .execute(
            "UPDATE repos SET last_sync_at = $2, last_synced_commit = \
             COALESCE($3, last_synced_commit), sync_error = $4 WHERE id = $1",
            &[&repo_id, &now_ms(), &synced_commit, &error],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The longest description we will store.
///
/// Not a database limit — `TEXT` has none worth the name — but a limit
/// on what one repository can cost every search response that mentions
/// it. Two lines of prose is the shape this field is for.
pub const MAX_DESCRIPTION: usize = 512;

/// Trim a description into what may be stored, or say why not.
///
/// Empty and whitespace-only both mean "no description", so clearing the
/// field is the same request as never setting it.
pub fn clean_description(raw: &str) -> Result<Option<String>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.chars().count() > MAX_DESCRIPTION {
        return Err(format!(
            "invalid description: at most {MAX_DESCRIPTION} characters"
        ));
    }
    // A newline in a one-line field is how a name gets run into the next
    // column in every table that renders it.
    if t.chars().any(|c| c.is_control()) {
        return Err("invalid description: no control characters".into());
    }
    Ok(Some(t.to_string()))
}

/// The longest homepage we will store. Generous — a URL carrying a
/// path and a query is ordinary — and bounded, because this string is
/// rendered in a 296px rail and stored per repository.
pub const MAX_HOMEPAGE: usize = 512;

/// Trim a homepage into what may be stored, or say why not.
///
/// Same shape as [`clean_description`]: empty and whitespace-only both
/// mean "no homepage", so clearing the field is the same request as
/// never setting it, and `None` is the only encoding of absence that
/// exists. An empty string would be a second one, and the About rail
/// would draw a link to nowhere for it.
///
/// **An allowlist of two schemes, not a denylist.** This string is
/// somebody's typing and it becomes an `href` in a page other people
/// read, so `javascript:` and `data:` are the whole reason this
/// function exists rather than a `trim()` at the call site. The same
/// reasoning, and the same two schemes, as the origin link in the
/// dashboard's `lib/origin.ts` and the profile rail's guard: a list of
/// schemes to refuse is never finished.
///
/// A scheme-less `example.com` is **refused rather than repaired**. We
/// could prepend `https://` and be right nearly always, but the case
/// where we are wrong is a link that silently resolves against our own
/// host — a relative URL pointing into the forge, wearing the
/// project's name. Telling somebody to type the scheme costs them four
/// seconds once.
pub fn clean_homepage(raw: &str) -> Result<Option<String>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.chars().count() > MAX_HOMEPAGE {
        return Err(format!(
            "invalid homepage: at most {MAX_HOMEPAGE} characters"
        ));
    }
    if t.chars().any(|c| c.is_control()) {
        return Err("invalid homepage: no control characters".into());
    }
    let lower = t.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("invalid homepage: must begin with http:// or https://".into());
    }
    Ok(Some(t.to_string()))
}

/// Set the things about a repo a person can change after creating it.
/// `None` for any of them leaves it alone.
///
/// The `CASE WHEN $set THEN $value ELSE column END` shape, once per
/// field, is what makes "leave it alone" and "set it to null" different
/// requests in one statement. It matters most for the two nullable
/// fields: without it, clearing a description and not mentioning a
/// description would be the same wire message.
pub fn update_repo_meta(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    description: Option<Option<&str>>,
    homepage: Option<Option<&str>>,
) -> Result<bool, String> {
    let desc_set = description.is_some();
    let desc = description.flatten();
    let home_set = homepage.is_some();
    let home = homepage.flatten();
    let n = db
        .lock()
        .execute(
            "UPDATE repos SET \
             description = CASE WHEN $3 THEN $4 ELSE description END, \
             homepage = CASE WHEN $5 THEN $6 ELSE homepage END \
             WHERE org_id = $1 AND id = $2 AND state = 'active'",
            &[&org_id, &repo_id, &desc_set, &desc, &home_set, &home],
        )
        .map_err(|e| e.to_string())?;
    Ok(n > 0)
}

/// One search result: a repo, plus the namespace it lives in.
///
/// Search crosses orgs, so a bare repo name is ambiguous — every row has
/// to carry the namespace or the answer cannot be clicked.
#[derive(Debug, Clone, Serialize)]
pub struct RepoHit {
    pub id: String,
    pub org_id: String,
    pub org: String,
    pub name: String,
    pub description: Option<String>,
    pub kind: RepoKind,
    pub created_at: i64,
}

/// Who is asking, and therefore what they may be told exists.
///
/// This enum is the whole of search's authorization, and it is
/// deliberately a closed set rather than a list of org ids a caller
/// hands in: an id in a query parameter would be a request to be shown
/// somebody else's namespace.
pub enum Viewer<'a> {
    /// A person: every namespace they belong to — which is exactly what
    /// `members::effective_role` grants, because a per-repo grant
    /// without membership is not access.
    User(&'a str),
    /// A token not bound to a single repo — a service token *or a
    /// personal one* — seeing the one org it was minted in. A personal
    /// token is scoped to its org rather than read as its person: a token
    /// belongs to one organization, and only a person's session spans
    /// every org they are a member of.
    Org(&'a str),
}

/// A keyset cursor over the search ordering.
///
/// Ordering is `(lower(org), lower(name), id)` — deterministic, stable
/// under inserts, and independent of any relevance score, so paging
/// cannot be made to skip or repeat rows. The cursor is the last row's
/// key, and it is applied *inside* the visibility filter: tampering with
/// it moves the window within what the caller could already see, and
/// cannot widen it.
fn parse_cursor(after: &str) -> Option<(String, String, String)> {
    // `org/name/id`. Neither a namespace nor a repo name may contain a
    // slash (`valid_name`), so two splits are unambiguous.
    let (org, rest) = after.split_once('/')?;
    let (name, id) = rest.split_once('/')?;
    Some((org.to_lowercase(), name.to_lowercase(), id.to_string()))
}

/// The cursor to hand back for a row.
pub fn cursor_of(hit: &RepoHit) -> String {
    format!("{}/{}/{}", hit.org, hit.name, hit.id)
}

/// Escape a user's query for `LIKE`, so `%` matches a literal percent.
///
/// Without this, a query of `%` matches everything — which is only a
/// nuisance — and a query of `_` matches every one-character name, which
/// reads as the search being broken.
fn like_pattern(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for c in q.to_lowercase().chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// The longest query we will run. Longer is refused rather than
/// truncated: silently answering a different question than the one asked
/// is worse than saying no.
pub const MAX_QUERY: usize = 128;

/// Find repositories by name, namespace or description.
///
/// Matching is a case-insensitive substring — `LIKE '%q%'` — which is
/// correct whether or not `pg_trgm` was available to index it. An empty
/// query matches everything visible, which is what makes this the same
/// endpoint the discovery page browses with.
/// Find repositories by free text, optionally narrowed to one topic.
///
/// The two arguments do deliberately different jobs. `q` is fuzzy and
/// inclusive — it matches a substring of the name, the namespace, the
/// description **and any topic**, because somebody typing "kubernetes"
/// into a search box means "anything to do with kubernetes" and does not
/// know or care which field carries the word. `topic` is exact: it is
/// what a topic *pill* means, and a pill that quietly also matched
/// repositories merely mentioning the word in prose would make the
/// facet useless for the one thing facets are for.
///
/// Topics were unsearchable by either route until now, which made the
/// About rail's invitation — "a word or two makes this findable" — false
/// in the only sense that matters: a maintainer could tag a repository
/// `kubernetes`, and searching `kubernetes` returned nothing.
pub fn search_repos(
    db: &ControlDb,
    q: &str,
    topic: Option<&str>,
    viewer: &Viewer,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<RepoHit>, String> {
    if q.chars().count() > MAX_QUERY {
        return Err(format!("invalid query: at most {MAX_QUERY} characters"));
    }
    // A NUL byte is text PostgreSQL refuses outright, and the refusal
    // arrives as a 500 with a fragment of the database layer in it. No
    // repository can be named or described with one, so it matches
    // nothing — say that instead.
    if q.contains('\0') {
        return Ok(Vec::new());
    }
    // Same reasoning for the facet, and the same answer. `normalize`
    // lowercases, so `/topics/Rust` and `/topics/rust` are one page;
    // anything it refuses is a string no repository can be carrying, so
    // it matches nothing rather than erroring — a link somebody typed by
    // hand should come back empty, not 400.
    let topic = match topic {
        None => None,
        Some(t) => match crate::topics::normalize(t) {
            Ok(t) => Some(t),
            Err(_) => return Ok(Vec::new()),
        },
    };
    let limit = limit.clamp(1, 100) as i64;
    let pattern = like_pattern(q);
    let (c_org, c_name, c_id) = after
        .and_then(parse_cursor)
        .unwrap_or_else(|| (String::new(), String::new(), String::new()));
    // Membership is resolved here rather than joined, so the visibility
    // rule is one readable line of SQL and the list of namespaces comes
    // from the same function every authorization seam uses.
    let orgs: Vec<String> = match viewer {
        Viewer::User(user_id) => crate::members::orgs_of(db, user_id)?,
        Viewer::Org(org_id) => vec![(*org_id).to_string()],
    };
    let rows = db
        .lock()
        .query(
            "SELECT r.id, r.org_id, o.name AS org, r.name, r.description, \
             r.kind, r.created_at \
             FROM repos r JOIN orgs o ON o.id = r.org_id \
             WHERE r.state = 'active' \
               AND r.org_id = ANY($1) \
               AND (lower(r.name) LIKE $2 ESCAPE '\\' \
                    OR lower(o.name) LIKE $2 ESCAPE '\\' \
                    OR lower(COALESCE(r.description, '')) LIKE $2 ESCAPE '\\' \
                    OR EXISTS (SELECT 1 FROM repo_topics mt \
                               WHERE mt.repo_id = r.id \
                                 AND mt.topic LIKE $2 ESCAPE '\\')) \
               AND ($7::text IS NULL \
                    OR EXISTS (SELECT 1 FROM repo_topics ft \
                               WHERE ft.repo_id = r.id AND ft.topic = $7)) \
               AND (lower(o.name), lower(r.name), r.id) > ($3, $4, $5) \
             ORDER BY lower(o.name), lower(r.name), r.id \
             LIMIT $6",
            &[&orgs, &pattern, &c_org, &c_name, &c_id, &limit, &topic],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|row| RepoHit {
            id: row.get("id"),
            org_id: row.get("org_id"),
            org: row.get("org"),
            name: row.get("name"),
            description: row.get("description"),
            kind: RepoKind::parse(row.get("kind")),
            created_at: row.get("created_at"),
        })
        .collect())
}

/// Every topic in use, with how many repositories the caller can see
/// carrying it, most-used first.
///
/// Beside `search_repos` and not in `topics.rs` on purpose: it applies
/// the *same* visibility rule, and a visibility rule implemented twice
/// is one that will eventually disagree with itself. The org list is
/// resolved exactly as it is there — the namespaces you belong to — so
/// a topic carried only by another namespace's repositories is invisible
/// to you, and so is the fact that it exists.
///
/// This exists because the discovery page's topic chips were a fixed
/// list of ten common words: they matched whatever a repository happened
/// to say rather than what maintainers had actually filed things under,
/// so most of them returned nothing on any real instance while the
/// topics people *did* use were nowhere on the page.
///
/// Ordered by count and then by name, so the list is stable between
/// calls rather than reshuffling among equal counts on every load.
pub fn topics_in_use(
    db: &ControlDb,
    viewer: &Viewer,
    limit: usize,
) -> Result<Vec<(String, i64)>, String> {
    let limit = limit.clamp(1, 200) as i64;
    let orgs: Vec<String> = match viewer {
        Viewer::User(user_id) => crate::members::orgs_of(db, user_id)?,
        Viewer::Org(org_id) => vec![(*org_id).to_string()],
    };
    let rows = db
        .lock()
        .query(
            "SELECT t.topic, COUNT(*) AS n \
             FROM repo_topics t JOIN repos r ON r.id = t.repo_id \
             WHERE r.state = 'active' \
               AND r.org_id = ANY($1) \
             GROUP BY t.topic \
             ORDER BY n DESC, t.topic ASC \
             LIMIT $2",
            &[&orgs, &limit],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows
        .iter()
        .map(|row| (row.get("topic"), row.get("n")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_or_an_address_becomes_a_legal_namespace_name() {
        assert_eq!(handle_from("ada"), "ada");
        // GitHub allows mixed case; namespaces here are lowercase.
        assert_eq!(handle_from("AdaLovelace"), "adalovelace");
        assert_eq!(handle_from("ada-lovelace"), "ada-lovelace");
        // A leading dash or underscore would be a name that reads as a
        // flag in every command line it appears in.
        assert_eq!(handle_from("-ada-"), "ada");
        assert_eq!(handle_from("_ada_"), "ada");
        // Anything outside the alphabet is a dash, one per run, and never
        // a dot: `valid_name` refuses a dot-led name.
        assert_eq!(handle_from("ada.lovelace"), "ada-lovelace");
        assert_eq!(handle_from("ada+spool"), "ada-spool");
        assert_eq!(handle_from("ada/../root"), "ada-root");
        assert_eq!(handle_from(".ada"), "ada");
        assert_eq!(handle_from("ada_lovelace"), "ada_lovelace");
        assert_eq!(handle_from("Zoë"), "zo");
        // Length is bounded well inside the 100-byte limit, leaving room
        // for the suffix the caller may add — and a cut never leaves a
        // trailing dash.
        assert_eq!(handle_from(&"a".repeat(200)).len(), 60);
        assert_eq!(
            handle_from(&format!("{}.b", "a".repeat(59))),
            "a".repeat(59)
        );
        // Nothing usable left is still a legal name.
        assert_eq!(handle_from("---"), "user");
        assert_eq!(handle_from(""), "user");
        assert_eq!(handle_from("!!!"), "user");
    }

    /// Every output above passes the registry's own rules, which is the
    /// property a caller relies on when it treats a refusal from the
    /// database as "taken" rather than "malformed".
    #[test]
    fn every_derived_name_passes_the_registry_rules() {
        for seed in [
            "ada",
            "AdaLovelace",
            "-ada-",
            "ada.lovelace",
            "ada+spool",
            "ada/../root",
            ".ada",
            "Zoë",
            "---",
            "",
            &"a".repeat(200),
        ] {
            let base = handle_from(seed);
            assert!(
                valid_name(&base),
                "{seed:?} derived {base:?}, which the registry refuses"
            );
        }
    }

    /// A personal namespace is a namespace, and is not an org.
    #[test]
    fn a_personal_namespace_holds_one_person_and_takes_no_others() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("registry-personal")).unwrap();
        let ada =
            crate::users::create(&db, "ada@acme.test", "Ada", Some("a long enough pw")).unwrap();
        let bo = crate::users::create(&db, "bo@acme.test", "Bo", Some("a long enough pw")).unwrap();

        // Created with a trail: a namespace appearing with no record of
        // who made it is the same problem as a credential doing so.
        let ctx = crate::audit::AuditCtx {
            principal: format!("user:{}", ada.id),
            user_id: Some(ada.id.clone()),
            org_id: String::new(),
        };
        let ns = create_personal_namespace(&db, &ada.id, "ada", Some(&ctx)).unwrap();
        assert!(is_personal(&db, &ns.id).unwrap());
        let trail = crate::audit::query(
            &db,
            "",
            &crate::audit::AuditQuery {
                action: Some("namespace.create"),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(trail.len(), 1, "{trail:?}");
        assert_eq!(trail[0].context.as_ref().unwrap()["handle"], "ada");

        // The owner is a member of it, at owner — every authorization
        // check asks about membership, so a namespace whose owner is not
        // a member would be one they could not use.
        assert_eq!(
            crate::members::role_of(&db, &ns.id, &ada.id).unwrap(),
            Some(crate::members::Role::Owner)
        );
        // …and it is reachable by name, like any other namespace.
        assert_eq!(
            org_by_name(&db, "ada").unwrap().map(|o| o.id),
            Some(ns.id.clone())
        );
        assert_eq!(
            org_by_name(&db, "ADA").unwrap().map(|o| o.id),
            Some(ns.id.clone())
        );
        // The handle is recorded on the person too.
        assert_eq!(
            crate::users::by_id(&db, &ada.id)
                .unwrap()
                .unwrap()
                .handle
                .as_deref(),
            Some("ada")
        );

        // Repos work in it exactly as in an org — that is the point of
        // keeping one table.
        let repo = create_repo(
            &db,
            &ns.id,
            &NewRepo {
                description: None,
                name: "notes",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        assert!(repo
            .prefix()
            .as_str()
            .starts_with(&format!("o/{}/r/", ns.id)));

        // Nobody else gets in — not as a member, an invitee, or a team.
        let refused = crate::members::add(&db, &ns.id, &bo.id, crate::members::Role::Member, None)
            .unwrap_err();
        assert!(refused.contains("personal namespace"), "{refused}");
        let refused = crate::invites::create(
            &db,
            &ns.id,
            "bo@acme.test",
            crate::members::Role::Member,
            &ada.id,
            3600,
            None,
        )
        .unwrap_err();
        assert!(refused.contains("personal namespace"), "{refused}");
        let refused = crate::teams::create(&db, &ns.id, "squad", None, None).unwrap_err();
        assert!(refused.contains("personal namespace"), "{refused}");

        // A real org is unaffected by all three.
        let org = create_org(&db, "acme").unwrap();
        assert!(!is_personal(&db, &org.id).unwrap());
        crate::members::add(&db, &org.id, &bo.id, crate::members::Role::Member, None).unwrap();
        crate::teams::create(&db, &org.id, "squad", None, None).unwrap();

        // One namespace per person, and a handle nobody else can take.
        assert!(create_personal_namespace(&db, &ada.id, "ada-two", None).is_err());
        assert!(create_personal_namespace(&db, &bo.id, "Ada", None).is_err());
        assert!(create_personal_namespace(&db, &bo.id, "dashboard", None).is_err());
        assert!(create_personal_namespace(&db, &bo.id, "has space", None).is_err());
        // …and a free one still works, so the refusals above are about
        // the names rather than about personal namespaces being broken.
        assert!(create_personal_namespace(&db, &bo.id, "bo", None).is_ok());
    }

    /// A namespace name means one thing, whoever types it.
    #[test]
    fn namespace_names_are_case_folded_and_the_platform_keeps_its_own() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("registry-names")).unwrap();
        let org = create_org(&db, "Acme").unwrap();
        assert_eq!(org.name, "Acme", "the stored name keeps its capitals");

        // …but only one of it exists, however it is spelled.
        for taken in ["acme", "ACME", "AcMe"] {
            assert!(
                create_org(&db, taken).is_err(),
                "{taken} was accepted alongside Acme"
            );
        }
        // And every spelling finds it. Before this, `Acme` and `acme`
        // were two namespaces — a confusion and a squatting vector.
        for spelling in ["Acme", "acme", "ACME", "aCmE"] {
            let found = org_by_name(&db, spelling).unwrap();
            assert_eq!(
                found.map(|o| o.id),
                Some(org.id.clone()),
                "{spelling} did not resolve to the same namespace"
            );
        }

        // Names the router already answers for cannot be taken. Being
        // able to create one means a namespace that can never be cloned.
        for reserved in reserved_names() {
            assert!(
                create_org(&db, reserved).is_err(),
                "{reserved} was available as a namespace"
            );
            assert!(
                create_org(&db, &reserved.to_uppercase()).is_err(),
                "{reserved} was available in capitals"
            );
        }
        assert!(is_reserved("dashboard") && is_reserved("Dashboard"));
        // The product's own name, and the one it had before, are not for
        // anyone to squat on: pinned by name because the loop above can
        // only check what the list already says.
        assert!(is_reserved("weft") && is_reserved("Weft"));
        assert!(is_reserved("stratum"));
        assert!(!is_reserved("acme"));

        // The two refusals are different, and say so: an ill-formed name
        // is absent, a reserved one exists and is not yours.
        let bad = valid_namespace_name("handle", "has space").unwrap_err();
        assert!(bad.contains("invalid handle name"), "{bad}");
        let taken = valid_namespace_name("handle", "dashboard").unwrap_err();
        assert!(taken.contains("reserved"), "{taken}");
        assert!(valid_namespace_name("handle", "acme").is_ok());
        // The noun is the caller's, so the refusal names what they typed.
        assert!(create_org(&db, "Bad Name!")
            .unwrap_err()
            .contains("invalid org name"));

        // A name that could never be one still reads as absent rather
        // than erroring, so hostile bytes never reach a query.
        for bad in ["", "has space", "\0", "'; DROP TABLE orgs;--", "../etc"] {
            assert!(org_by_name(&db, bad).unwrap().is_none(), "{bad:?}");
        }
    }

    /// A repository may not be called `changesets`, in any spelling.
    ///
    /// `/{org}/changesets/{key}.git` is the git wire address of a
    /// changeset's workspace. A repository by that name sits one path
    /// segment short of it, which makes every mistyped clone URL land on
    /// whichever the router reaches first — and the refusal has to be a
    /// 400 that names the problem, so it starts with `invalid `, which
    /// is what the API layer classifies on.
    #[test]
    fn a_repository_may_not_be_called_changesets() {
        let db = db();
        let org = create_org(&db, &format!("cs-name-{}", ulid())).unwrap();
        for spelling in ["changesets", "Changesets", "CHANGESETS"] {
            let e = create_repo(
                &db,
                &org.id,
                &NewRepo {
                    name: spelling,
                    description: None,
                    kind: RepoKind::Native,
                    default_branch: "main",
                    origin_url: None,
                    origin_provider: None,
                    origin_installation: None,
                },
            )
            .unwrap_err();
            assert!(e.contains("invalid repo name"), "{spelling}: {e}");
            assert!(
                stratum_engine_is_invalid_input(&e),
                "{spelling} must classify as a 400: {e}"
            );
        }
        // The singular is a perfectly ordinary repository name, and a
        // denylist that swallowed it would be taking more than it needs.
        assert!(valid_repo_name("changeset").is_ok());
        assert!(valid_repo_name("app").is_ok());
    }

    /// `errclass::is_invalid_input`, inlined: `stratum-control` does not
    /// depend on the engine, and what is being pinned is the *prefix*
    /// the API layer branches on.
    fn stratum_engine_is_invalid_input(err: &str) -> bool {
        err.starts_with("invalid ")
    }

    fn db() -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url("control-registry")).unwrap()
    }

    #[test]
    fn org_repo_lifecycle_and_isolation_scoping() {
        let db = db();
        let a = create_org(&db, "org-a").unwrap();
        let b = create_org(&db, "org-b").unwrap();
        assert!(create_org(&db, "org-a").is_err());

        let r = create_repo(
            &db,
            &a.id,
            &NewRepo {
                description: None,
                name: "app",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        assert!(r.prefix().as_str().starts_with(&format!("o/{}/r/", a.id)));

        // Same name in another org is fine; cross-org lookup finds nothing.
        create_repo(
            &db,
            &b.id,
            &NewRepo {
                description: None,
                name: "app",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let found = repo_by_name(&db, &b.id, "app").unwrap().unwrap();
        assert_ne!(found.id, r.id);
        assert!(repo_by_id(&db, &b.id, &r.id).unwrap().is_none());

        // Delete tombstones instantly and frees the name.
        assert!(delete_repo(&db, &a.id, &r.id).unwrap());
        assert!(repo_by_name(&db, &a.id, "app").unwrap().is_none());
        create_repo(
            &db,
            &a.id,
            &NewRepo {
                description: None,
                name: "app",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn keyset_pagination_walks_everything_once() {
        let db = db();
        let org = create_org(&db, "org").unwrap();
        for i in 0..25 {
            create_repo(
                &db,
                &org.id,
                &NewRepo {
                    description: None,
                    name: &format!("repo-{i}"),
                    kind: RepoKind::Native,
                    default_branch: "main",
                    origin_url: None,
                    origin_provider: None,
                    origin_installation: None,
                },
            )
            .unwrap();
        }
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = list_repos(&db, &org.id, cursor.as_deref(), 10).unwrap();
            if page.is_empty() {
                break;
            }
            cursor = Some(page.last().unwrap().id.clone());
            seen.extend(page.into_iter().map(|r| r.name));
        }
        assert_eq!(seen.len(), 25);
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 25);
    }

    /// Everything a description may and may not be, with no database in
    /// the way. The control characters case is not hypothetical: a
    /// newline in a one-line field is how a name gets run into the next
    /// column of every table that renders it.
    #[test]
    fn a_description_is_trimmed_bounded_and_single_line() {
        assert_eq!(
            clean_description("  a fast repo  ").unwrap().as_deref(),
            Some("a fast repo")
        );
        // Empty and whitespace-only both mean "no description", so
        // clearing the field is the same request as never setting one.
        assert_eq!(clean_description("").unwrap(), None);
        assert_eq!(clean_description("   \t ").unwrap(), None);
        // Counted in characters, not bytes: a description of accented
        // text must not be refused for being long in UTF-8.
        let accented = "é".repeat(MAX_DESCRIPTION);
        assert_eq!(
            clean_description(&accented).unwrap().as_deref(),
            Some(accented.as_str())
        );
        assert!(clean_description(&"é".repeat(MAX_DESCRIPTION + 1))
            .unwrap_err()
            .contains("at most"));
        for bad in ["two\nlines", "a\rb", "nul\0byte", "tab\there"] {
            assert!(clean_description(bad).is_err(), "{bad:?} should be refused");
        }
    }

    /// A homepage is a URL somebody else's browser will follow.
    ///
    /// This one is a security boundary and not a tidiness rule, which is
    /// why it is an allowlist of exactly two schemes. The About rail
    /// renders this string as an `href` on a page that strangers read,
    /// so `javascript:` here is stored XSS with a project's name on it.
    /// A list of schemes to *refuse* is never finished; the dashboard's
    /// `lib/origin.ts` guards the mirror origin the same way for the
    /// same reason.
    #[test]
    fn a_homepage_is_an_absolute_http_url_or_nothing_at_all() {
        assert_eq!(
            clean_homepage("  https://example.com/docs?a=1  ")
                .unwrap()
                .as_deref(),
            Some("https://example.com/docs?a=1")
        );
        assert_eq!(
            clean_homepage("HTTP://Example.COM").unwrap().as_deref(),
            Some("HTTP://Example.COM"),
            "the scheme test is case-insensitive but the value is stored verbatim"
        );

        // One encoding of absence, not two. An empty string would draw a
        // link to nowhere in the rail, and would make "cleared" and
        // "never set" two states that behave identically.
        assert_eq!(clean_homepage("").unwrap(), None);
        assert_eq!(clean_homepage("   \t ").unwrap(), None);

        // The whole reason this function exists.
        for hostile in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "file:///etc/passwd",
        ] {
            assert!(
                clean_homepage(hostile).is_err(),
                "{hostile:?} was accepted as a homepage"
            );
        }

        // Refused rather than repaired. Prepending `https://` would be
        // right nearly always, and the case where it is wrong is a link
        // that silently resolves against our own host — a relative URL
        // pointing back into the forge, wearing the project's name.
        assert!(clean_homepage("example.com")
            .unwrap_err()
            .contains("http://"));
        assert!(clean_homepage("/settings").is_err());
        assert!(clean_homepage("//evil.example").is_err());

        // Bounded and single-line, like the description beside it.
        assert!(
            clean_homepage(&format!("https://a.example/{}", "x".repeat(MAX_HOMEPAGE)))
                .unwrap_err()
                .contains("at most")
        );
        assert!(clean_homepage("https://a.example/\nx").is_err());
    }

    /// A query is data, never syntax. Without escaping, `%` matches
    /// everything and `_` matches every one-character name — which reads
    /// as the search being broken rather than as an injection.
    #[test]
    fn a_query_is_escaped_into_a_like_pattern() {
        assert_eq!(like_pattern("app"), "%app%");
        assert_eq!(like_pattern("APP"), "%app%");
        assert_eq!(like_pattern("100%"), r"%100\%%");
        assert_eq!(like_pattern("a_b"), r"%a\_b%");
        assert_eq!(like_pattern(r"back\slash"), r"%back\\slash%");
        assert_eq!(like_pattern(""), "%%");
    }

    /// A cursor is three fields, and anything else is no cursor at all —
    /// which starts from the beginning rather than erroring, because a
    /// stale link is not a client bug worth a 400.
    #[test]
    fn a_cursor_is_org_name_id_or_nothing() {
        assert_eq!(
            parse_cursor("Acme/Widget/01H"),
            Some(("acme".into(), "widget".into(), "01H".into()))
        );
        for bad in ["", "acme", "acme/widget", "/"] {
            assert_eq!(parse_cursor(bad), None, "{bad:?} should not parse");
        }
        // A slash inside the id is kept: ULIDs have none, and splitting
        // twice is what makes the first two fields unambiguous.
        assert_eq!(
            parse_cursor("a/b/c/d"),
            Some(("a".into(), "b".into(), "c/d".into()))
        );
    }

    /// Topics are findable, by typing and by clicking.
    ///
    /// They were neither. A maintainer could tag a repository
    /// `kubernetes` — the About rail invites exactly that, in the words
    /// "a word or two makes this findable" — and searching `kubernetes`
    /// returned nothing, because the query read the name, the namespace
    /// and the description and never the topics. The discovery page then
    /// offered a row of topic chips that prefilled that same search, so
    /// every one of them answered "nothing matched" on a corpus that
    /// plainly contained matches.
    ///
    /// The two routes are asserted separately because they are meant to
    /// behave differently: free text is inclusive and matches a topic as
    /// loosely as it matches prose, while the facet is exact and must
    /// *not* pick up a repository that merely says the word.
    #[test]
    fn search_finds_repositories_by_topic_typed_and_by_topic_clicked() {
        let db =
            ControlDb::open(&stratum_testkit::pg::test_db_url("registry-search-topic")).unwrap();
        let acme = create_org(&db, "acme").unwrap();
        let mk = |name: &str, desc: Option<&str>| {
            create_repo(
                &db,
                &acme.id,
                &NewRepo {
                    name,
                    description: desc,
                    kind: RepoKind::Native,
                    default_branch: "main",
                    origin_url: None,
                    origin_provider: None,
                    origin_installation: None,
                },
            )
            .unwrap()
        };
        // Tagged, and says nothing about kubernetes in its prose.
        let tagged = mk("operator", Some("runs things"));
        // The opposite: says the word, carries no topic. This is the row
        // that keeps the facet honest.
        mk("notes", Some("some thoughts on Kubernetes"));
        let ctx = crate::audit::AuditCtx {
            principal: "user:test".to_string(),
            user_id: None,
            org_id: acme.id.clone(),
        };
        crate::topics::set(&db, &tagged.id, &["kubernetes".into(), "rust".into()], &ctx).unwrap();

        let names = |v: Vec<RepoHit>| v.into_iter().map(|h| h.name).collect::<Vec<_>>();

        // Typed: both, because somebody typing a word means "anything to
        // do with this" and does not know which field carries it.
        let mut typed =
            names(search_repos(&db, "kubernetes", None, &Viewer::Org(&acme.id), None, 50).unwrap());
        typed.sort();
        assert_eq!(
            typed,
            ["notes", "operator"],
            "a typed query still does not reach topics"
        );

        // Clicked: only the tagged one. A pill that also returned the
        // repository merely mentioning the word would make the facet
        // useless for the one job it has.
        assert_eq!(
            names(
                search_repos(
                    &db,
                    "",
                    Some("kubernetes"),
                    &Viewer::Org(&acme.id),
                    None,
                    50
                )
                .unwrap()
            ),
            ["operator"]
        );

        // Topics are stored lowercased, so the facet is case-insensitive
        // and `/topics/Rust` is the same page as `/topics/rust`.
        assert_eq!(
            names(search_repos(&db, "", Some("RUST"), &Viewer::Org(&acme.id), None, 50).unwrap()),
            ["operator"]
        );

        // Text and facet compose rather than replace each other.
        assert_eq!(
            names(
                search_repos(
                    &db,
                    "operator",
                    Some("rust"),
                    &Viewer::Org(&acme.id),
                    None,
                    50
                )
                .unwrap()
            ),
            ["operator"]
        );
        assert!(
            names(
                search_repos(&db, "notes", Some("rust"), &Viewer::Org(&acme.id), None, 50).unwrap()
            )
            .is_empty(),
            "the facet did not narrow the text query"
        );

        // A topic no repository carries, and one no repository *could*
        // carry: both are empty rather than an error, because a link
        // somebody typed by hand should come back empty, not 400.
        for t in ["absent", "not a topic", "-leading-hyphen", ""] {
            assert!(
                search_repos(&db, "", Some(t), &Viewer::Org(&acme.id), None, 50)
                    .unwrap()
                    .is_empty(),
                "{t:?} should match nothing"
            );
        }
    }

    /// The whole of search's authorization, exercised against real rows.
    ///
    /// The assertion that matters is the negative one: a repo is absent
    /// for everyone outside its namespace, and *present* for a member —
    /// so the test would fail both if the filter leaked and if it were
    /// simply broken and returned nothing.
    #[test]
    fn search_shows_a_namespaces_repos_only_to_its_members() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("registry-search")).unwrap();
        let acme = create_org(&db, "acme").unwrap();
        let other = create_org(&db, "zzz-other").unwrap();
        let member =
            crate::users::create(&db, "m@acme.test", "M", Some("a long enough pw")).unwrap();
        let stranger =
            crate::users::create(&db, "s@else.test", "S", Some("a long enough pw")).unwrap();
        crate::members::add(
            &db,
            &acme.id,
            &member.id,
            crate::members::Role::Member,
            None,
        )
        .unwrap();
        crate::members::add(
            &db,
            &other.id,
            &stranger.id,
            crate::members::Role::Member,
            None,
        )
        .unwrap();

        let mk = |org: &str, name: &str, desc: Option<&str>| {
            create_repo(
                &db,
                org,
                &NewRepo {
                    name,
                    description: desc,
                    kind: RepoKind::Native,
                    default_branch: "main",
                    origin_url: None,
                    origin_provider: None,
                    origin_installation: None,
                },
            )
            .unwrap()
        };
        mk(&acme.id, "widget", Some("the acme widget"));
        mk(&acme.id, "payments", Some("ledger work"));
        mk(&other.id, "widget-fork", None);

        let names = |v: Vec<RepoHit>| {
            v.into_iter()
                .map(|h| format!("{}/{}", h.org, h.name))
                .collect::<Vec<_>>()
        };
        let as_member = Viewer::User(&member.id);
        let as_stranger = Viewer::User(&stranger.id);

        // A member of acme sees acme's repositories, and no sign that the
        // other namespace's are there at all.
        assert_eq!(
            names(search_repos(&db, "", None, &as_member, None, 50).unwrap()),
            ["acme/payments", "acme/widget"]
        );
        // Someone in a *different* org sees only their own: membership
        // widens nothing outside its own namespace.
        assert_eq!(
            names(search_repos(&db, "", None, &as_stranger, None, 50).unwrap()),
            ["zzz-other/widget-fork"]
        );
        assert_eq!(
            names(search_repos(&db, "payments", None, &as_stranger, None, 50).unwrap()),
            [] as [String; 0]
        );
        // A service token bound to the org stands in for the org.
        assert_eq!(
            names(search_repos(&db, "payments", None, &Viewer::Org(&acme.id), None, 50).unwrap()),
            ["acme/payments"]
        );

        // Matching: name, namespace and description all count.
        assert_eq!(
            names(search_repos(&db, "WIDG", None, &as_member, None, 50).unwrap()),
            ["acme/widget"]
        );
        assert_eq!(
            names(search_repos(&db, "zzz-other", None, &as_stranger, None, 50).unwrap()),
            ["zzz-other/widget-fork"]
        );
        assert_eq!(
            names(search_repos(&db, "acme widget", None, &as_member, None, 50).unwrap()),
            ["acme/widget"]
        );
        // A description only a member can see does not match for anyone
        // else — the text is as private as the repository.
        assert_eq!(
            names(search_repos(&db, "ledger", None, &as_stranger, None, 50).unwrap()),
            [] as [String; 0]
        );
        assert_eq!(
            names(search_repos(&db, "ledger", None, &as_member, None, 50).unwrap()),
            ["acme/payments"]
        );

        // Wildcards are data. `%` matching everything would be the
        // difference between a search and a dump.
        assert_eq!(
            names(search_repos(&db, "%", None, &as_member, None, 50).unwrap()),
            [] as [String; 0]
        );
        assert_eq!(
            names(search_repos(&db, "_", None, &as_member, None, 50).unwrap()),
            [] as [String; 0]
        );
        // A NUL byte is text PostgreSQL refuses outright; nothing can be
        // named with one, so it matches nothing rather than 500ing.
        assert_eq!(
            names(search_repos(&db, "wid\0get", None, &as_member, None, 50).unwrap()),
            [] as [String; 0]
        );
        assert!(
            search_repos(&db, &"a".repeat(MAX_QUERY + 1), None, &as_member, None, 50)
                .unwrap_err()
                .contains("at most")
        );

        // Paging walks the visible set once, and a cursor cannot widen
        // it: replaying the member's cursor as somebody else still hides
        // the repositories behind it that are not theirs.
        let first = search_repos(&db, "", None, &as_member, None, 1).unwrap();
        assert_eq!(names(first.clone()), ["acme/payments"]);
        let cursor = cursor_of(&first[0]);
        assert_eq!(
            names(search_repos(&db, "", None, &as_member, Some(&cursor), 50).unwrap()),
            ["acme/widget"]
        );
        assert_eq!(
            names(search_repos(&db, "", None, &as_stranger, Some(&cursor), 50).unwrap()),
            ["zzz-other/widget-fork"]
        );
        // A cursor pointing before everything is the first page, not an
        // error, and a nonsense one is simply no cursor.
        assert_eq!(
            search_repos(&db, "", None, &as_member, Some("nonsense"), 50)
                .unwrap()
                .len(),
            2
        );

        // Deleting removes it from search on the very next request.
        let doomed = mk(&acme.id, "doomed", None);
        assert_eq!(
            names(search_repos(&db, "doomed", None, &as_member, None, 50).unwrap()),
            ["acme/doomed"]
        );
        delete_repo(&db, &acme.id, &doomed.id).unwrap();
        assert_eq!(
            names(search_repos(&db, "doomed", None, &as_member, None, 50).unwrap()),
            [] as [String; 0]
        );
    }

    /// The contract the trigram indexes are held to: they make search
    /// fast, and nothing else.
    ///
    /// `CREATE EXTENSION` is a superuser act on plenty of managed
    /// PostgreSQL, so the migration creates them inside a subtransaction
    /// that swallows a refusal. That makes it worth proving the search
    /// still answers correctly with them gone — otherwise the first
    /// deployment that could not install `pg_trgm` would find out by
    /// returning nothing.
    #[test]
    fn search_is_correct_with_the_trigram_indexes_dropped() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("registry-notrgm")).unwrap();
        let org = create_org(&db, "acme").unwrap();
        for (name, desc) in [("widget", "the fast one"), ("ledger", "money")] {
            create_repo(
                &db,
                &org.id,
                &NewRepo {
                    name,
                    description: Some(desc),
                    kind: RepoKind::Native,
                    default_branch: "main",
                    origin_url: None,
                    origin_provider: None,
                    origin_installation: None,
                },
            )
            .unwrap();
        }
        for sql in [
            "DROP INDEX repos_name_trgm",
            "DROP INDEX repos_description_trgm",
            "DROP EXTENSION pg_trgm",
        ] {
            db.lock().execute(sql, &[]).unwrap();
        }
        let hit = search_repos(&db, "idg", None, &Viewer::Org(&org.id), None, 50).unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "widget");
        let hit = search_repos(&db, "fast", None, &Viewer::Org(&org.id), None, 50).unwrap();
        assert_eq!(hit.len(), 1, "description search survives too");
    }

    /// Description and homepage are edited independently: a PATCH that
    /// sets one must leave the other alone.
    #[test]
    fn updating_one_field_leaves_the_other_alone() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("registry-meta")).unwrap();
        let org = create_org(&db, "acme").unwrap();
        let repo = create_repo(
            &db,
            &org.id,
            &NewRepo {
                name: "app",
                description: Some("first"),
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        assert_eq!(repo.description.as_deref(), Some("first"));

        assert!(update_repo_meta(&db, &org.id, &repo.id, Some(Some("second")), None).unwrap());
        let now = repo_by_id(&db, &org.id, &repo.id).unwrap().unwrap();
        assert_eq!(now.description.as_deref(), Some("second"));

        // Clearing is `Some(None)`, and is distinct from "leave alone".
        assert!(update_repo_meta(&db, &org.id, &repo.id, Some(None), None).unwrap());
        let now = repo_by_id(&db, &org.id, &repo.id).unwrap().unwrap();
        assert_eq!(now.description, None);

        // The homepage obeys the same three rules, and each of them is a
        // separate `CASE WHEN` arm that can be got wrong on its own.
        // Every repository begins without one.
        assert_eq!(now.homepage, None);
        assert!(update_repo_meta(
            &db,
            &org.id,
            &repo.id,
            None,
            Some(Some("https://example.com")),
        )
        .unwrap());
        let now = repo_by_id(&db, &org.id, &repo.id).unwrap().unwrap();
        assert_eq!(now.homepage.as_deref(), Some("https://example.com"));

        // Not mentioning it leaves it alone — the arm a naive
        // `homepage = $n` would break, silently wiping a project's site
        // every time somebody edited their description.
        assert!(update_repo_meta(&db, &org.id, &repo.id, Some(Some("third")), None).unwrap());
        let now = repo_by_id(&db, &org.id, &repo.id).unwrap().unwrap();
        assert_eq!(
            now.homepage.as_deref(),
            Some("https://example.com"),
            "a description edit cleared the homepage"
        );
        assert_eq!(now.description.as_deref(), Some("third"));

        // And clearing it is its own request, which must not disturb
        // anything beside it.
        assert!(update_repo_meta(&db, &org.id, &repo.id, None, Some(None)).unwrap());
        let now = repo_by_id(&db, &org.id, &repo.id).unwrap().unwrap();
        assert_eq!(now.homepage, None);
        assert_eq!(now.description.as_deref(), Some("third"));

        // A repo in another namespace is not this org's to edit, and a
        // deleted one is gone: both answer "no rows", never a silent
        // success.
        let other = create_org(&db, "other").unwrap();
        assert!(!update_repo_meta(&db, &other.id, &repo.id, Some(Some("x")), None).unwrap());
        delete_repo(&db, &org.id, &repo.id).unwrap();
        assert!(!update_repo_meta(&db, &org.id, &repo.id, Some(Some("x")), None).unwrap());
    }

    #[test]
    fn name_validation_rejects_path_shapes() {
        for bad in ["", "..", ".hidden", "a/b", "a b", "a\\b", &"x".repeat(101)] {
            assert!(!valid_name(bad), "{bad:?} should be invalid");
        }
        for good in ["app", "my-repo", "a.b.c", "x_1"] {
            assert!(valid_name(good), "{good:?} should be valid");
        }
    }
}
