//! Router assembly and shared state: git smart HTTP at
//! `/{org}/{repo}.git/…`, REST at `/v1/…`, one process.

use crate::authx;
use crate::changeset_workspace;
use crate::git_http::{self, GitPermits, RepoCtx};
use crate::metering::MeterSink;
use crate::mirror::freshness;
use crate::mirror::origin::{Generic, GithubApp, OriginProvider};
use crate::mirror::sync::SyncManager;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use std::collections::HashMap;
use std::sync::Arc;
use stratum_control::auth::{Principal, Scope};
use stratum_control::members;
use stratum_control::registry::{self, Org, Repo, RepoKind};
use stratum_control::ControlDb;

/// 64 MB push/request cap, inherited from the research front (bounded
/// hostile input, I13).
pub const BODY_LIMIT: usize = 64 * 1024 * 1024;

pub struct AppState {
    pub db: ControlDb,
    pub store_url: String,
    /// One S3 client for the request path. `ObjectStore::new` builds a
    /// fresh HTTP agent, and an agent is a connection pool: a store built
    /// per request opens a new TCP+TLS connection to the bucket for every
    /// GET it makes and pools nothing. The read API used to do exactly
    /// that, once per request, on top of the reads themselves.
    pub store: Arc<stratum_store::ObjectStore>,
    /// Objects, WAL sidecars and WAL packs remembered across requests.
    /// Everything in it is content-addressed or minted-once, so a hit is
    /// never stale; `STRATUM_READ_CACHE_MB` bounds it.
    pub read_cache: Arc<stratum_engine::read::ReadCache>,
    /// Externally-visible base URL (clone URLs in API responses).
    pub public_url: String,
    pub permits: Arc<GitPermits>,
    pub sync: Arc<SyncManager>,
    /// Bound on the synchronous origin-fetch a freshness miss may trigger.
    pub freshness_timeout: std::time::Duration,
    pub meter: MeterSink,
    /// Where transactional mail goes. Never `None`: an unconfigured
    /// deployment gets [`crate::mail::Null`], which drops but still
    /// validates, so the code path is the same everywhere.
    pub mailer: Arc<dyn crate::mail::Mailer>,
    /// Where to send somebody to install the GitHub App
    /// (`https://github.com/apps/<slug>/installations/new`). None means
    /// this server has no App to connect, and the connect flow says so
    /// rather than offering a dead link.
    pub github_install_url: Option<String>,
    pub data_dir: std::path::PathBuf,
    /// Built dashboard SPA (Vite dist) served at `/dashboard/`.
    pub dashboard_dir: Option<std::path::PathBuf>,
    /// SSH front door bind (None = SSH off).
    pub ssh_bind: Option<String>,
    /// PEM host key for the SSH door; every task in a fleet must present
    /// the same identity or clients see host-key warnings.
    pub ssh_host_key_pem: Option<String>,
    /// Externally-visible SSH base (`ssh://git@host:port`) for
    /// `ssh_clone_url` in repo API responses; None = field is null.
    pub ssh_public_url: Option<String>,
    /// This process's identity, answered on `/healthz` as
    /// `x-weft-instance`. `STRATUM_INSTANCE_ID` when set (the test
    /// harness sets one per spawn so it can prove the server it reaches
    /// is the one it started); otherwise a fresh id at boot.
    pub instance_id: String,
    /// CDN offload for clone bulk (git `packfile-uri`); None = disabled
    /// and the capability is never advertised.
    pub cdn: Option<crate::cdn::CdnConfig>,
    /// The base URL a job's clone URL is built on. The public URL unless
    /// `STRATUM_RUNNER_URL` says otherwise — a deployment behind a CDN
    /// may want runners to bypass it.
    pub runner_url: String,
    /// The longest `timeout-minutes` this fleet will accept
    /// (`STRATUM_RUNNER_MAX_TIMEOUT_MINUTES`, default six hours). A job
    /// asking for more is refused when it is triggered, not silently
    /// clamped: a build that says it needs ten hours and is stopped at
    /// six fails in a way its author cannot explain.
    pub max_timeout_minutes: i64,
    /// How long `POST /v1/runners/claim` holds a self-hosted runner's
    /// poll open before answering 204
    /// (`STRATUM_RUNNER_CLAIM_WAIT_MS`, default 20 s).
    ///
    /// A long poll rather than a short one because the alternative is
    /// every registered machine asking every second forever: at a few
    /// hundred runners that is the busiest query on the instance and
    /// almost all of it answers "nothing". Twenty seconds is under every
    /// default proxy idle timeout worth worrying about, and the runner's
    /// own HTTP timeout is set longer than it so a 204 always arrives as
    /// a 204 rather than as a client-side timeout.
    pub runner_claim_wait_ms: i64,
    /// The lease a job gets the moment a runner claims it
    /// (`STRATUM_RUNNER_START_LEASE_SECS`, default 10 min).
    ///
    /// The same knob the dispatcher uses, and for the same reason: a
    /// runner has to clone the repository before it makes its first
    /// call, and a lease sized for a heartbeating runner would hand the
    /// job to somebody else halfway through the checkout.
    pub runner_start_lease_ms: i64,
    /// Launches a job gets before it is given up on
    /// (`STRATUM_RUNNER_MAX_ATTEMPTS`, default 2). Read here rather than
    /// in the dispatcher so that the dispatcher and the self-hosted
    /// claim cannot disagree about how many tries a job gets.
    pub runner_max_attempts: i64,
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // REST v1
        .route(
            "/v1/orgs/:org/repos",
            post(crate::api::repos::create).get(crate::api::repos::list),
        )
        .route(
            "/v1/orgs/:org/repos/batch/create",
            post(crate::api::repos::batch_create),
        )
        .route(
            "/v1/orgs/:org/repos/batch/delete",
            post(crate::api::repos::batch_delete),
        )
        .route(
            "/v1/orgs/:org/repos/:repo",
            get(crate::api::repos::get)
                .patch(crate::api::repos::patch)
                .delete(crate::api::repos::delete),
        )
        // Forking answers 202, not 201: the row exists synchronously and
        // the storage pointers do not, so `fork_state` is where a caller
        // looks to find out when it is readable.
        .route(
            "/v1/orgs/:org/repos/:repo/forks",
            post(crate::api::repos::fork).get(crate::api::repos::list_forks),
        )
        // Finding a repo you were never sent a link to, across the
        // namespaces the caller belongs to. Signed in only.
        .route("/v1/search/repos", get(crate::api::search::repos))
        // The topics people are actually using, for the discovery page's
        // chips — which were a fixed list of common words until now, and
        // so mostly matched nothing on any real instance.
        .route("/v1/search/topics", get(crate::api::search::topics))
        // Profiles. A new top-level family under `/v1`, whose first
        // segment is already reserved, so nobody can take a namespace
        // that shadows it. The reads are anonymous by design — this is
        // the surface a logged-out visitor arrives at — and the address
        // routes below them are the opposite: nobody but their owner,
        // ever, because they are what authorship is resolved through.
        .route(
            "/v1/users/:handle",
            get(crate::api::profiles_api::get).patch(crate::api::profiles_api::patch),
        )
        .route(
            "/v1/users/:handle/emails",
            get(crate::api::profiles_api::list_emails).post(crate::api::profiles_api::add_email),
        )
        .route(
            "/v1/users/:handle/emails/verify",
            post(crate::api::profiles_api::verify_email),
        )
        .route(
            "/v1/users/:handle/emails/:email",
            delete(crate::api::profiles_api::delete_email),
        )
        .route(
            "/v1/orgs/:org/profile",
            get(crate::api::profiles_api::get_org_profile)
                .patch(crate::api::profiles_api::patch_org_profile),
        )
        // Sessions for the dashboard. Bearer tokens are untouched.
        .route("/v1/auth/login", post(crate::api::auth_api::login))
        .route("/v1/auth/logout", post(crate::api::auth_api::logout))
        .route("/v1/auth/me", get(crate::api::auth_api::me))
        .route(
            "/v1/auth/accept-invite",
            post(crate::api::auth_api::accept_invite),
        )
        .route(
            "/v1/auth/invite/preview",
            post(crate::api::auth_api::preview_invite),
        )
        .route("/v1/orgs", post(crate::api::orgs_api::create_org))
        // GitHub redirects a browser here after an install. No
        // credentials of its own: the `state` is the credential, and it
        // names the org whose admin started the flow.
        .route(
            "/v1/github/setup",
            get(crate::api::github_api::setup_callback),
        )
        // Signing in with GitHub. Unauthenticated for the same reason
        // `/v1/github/setup` is: the caller is a browser mid-redirect,
        // and the anti-CSRF state — a cookie here, a row there — is the
        // only credential either one has.
        .route("/v1/auth/github/start", get(crate::api::github_auth::start))
        .route(
            "/v1/auth/github/callback",
            get(crate::api::github_auth::callback),
        )
        // Install-onboarding: an install begun on GitHub is parked, and
        // claimed against an org once the person has signed in.
        .route(
            "/v1/github/pending-install",
            get(crate::api::github_api::pending_install),
        )
        .route(
            "/v1/orgs/:org/github/install/claim",
            post(crate::api::github_api::claim_install),
        )
        .route("/v1/auth/signup", post(crate::api::auth_api::signup))
        .route("/v1/auth/verify", post(crate::api::auth_api::verify_email))
        .route(
            "/v1/auth/resend-verification",
            post(crate::api::auth_api::resend_verification),
        )
        .route(
            "/v1/auth/forgot-password",
            post(crate::api::auth_api::forgot_password),
        )
        .route(
            "/v1/auth/reset-password",
            post(crate::api::auth_api::reset_password),
        )
        .route(
            "/v1/auth/password",
            post(crate::api::auth_api::change_password),
        )
        .route("/v1/orgs/:org/members", get(crate::api::members_api::list))
        .route(
            "/v1/orgs/:org/members/:user",
            axum::routing::patch(crate::api::members_api::set_role)
                .delete(crate::api::members_api::remove),
        )
        .route(
            "/v1/orgs/:org/invites",
            post(crate::api::members_api::invite).get(crate::api::members_api::list_invites),
        )
        .route(
            "/v1/orgs/:org/invites/:id",
            axum::routing::delete(crate::api::members_api::revoke_invite),
        )
        .route(
            "/v1/orgs/:org/origins/probe",
            post(crate::api::origins_api::probe_origin),
        )
        .route(
            "/v1/orgs/:org/github/install",
            post(crate::api::github_api::start_install),
        )
        .route(
            "/v1/orgs/:org/github/installations",
            get(crate::api::github_api::list_installations),
        )
        .route(
            "/v1/orgs/:org/github/installations/:id/repos",
            get(crate::api::github_api::list_installation_repos),
        )
        .route(
            "/v1/orgs/:org/github/installations/:id",
            delete(crate::api::github_api::forget_installation),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/sync-status",
            get(crate::api::mirrors::sync_status),
        )
        .route(
            "/v1/orgs/:org/teams",
            get(crate::api::teams_api::list).post(crate::api::teams_api::create),
        )
        .route(
            "/v1/orgs/:org/teams/:team",
            axum::routing::patch(crate::api::teams_api::update)
                .delete(crate::api::teams_api::delete),
        )
        .route(
            "/v1/orgs/:org/teams/:team/members",
            get(crate::api::teams_api::list_members),
        )
        .route(
            "/v1/orgs/:org/teams/:team/members/:user",
            axum::routing::put(crate::api::teams_api::add_member)
                .delete(crate::api::teams_api::remove_member),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/access",
            get(crate::api::teams_api::access),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/grants",
            post(crate::api::teams_api::grant_repo),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/team-grants/:team",
            axum::routing::delete(crate::api::teams_api::revoke_team_grant),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/grants/:user",
            axum::routing::delete(crate::api::members_api::revoke_repo_grant),
        )
        .route(
            "/v1/orgs/:org/tokens",
            post(crate::api::tokens::mint).get(crate::api::tokens::list),
        )
        .route(
            "/v1/orgs/:org/tokens/:id",
            delete(crate::api::tokens::revoke),
        )
        .route(
            "/v1/orgs/:org/ssh-keys",
            post(crate::api::sshkeys_api::add).get(crate::api::sshkeys_api::list),
        )
        .route(
            "/v1/orgs/:org/ssh-keys/:id",
            delete(crate::api::sshkeys_api::revoke),
        )
        .route("/v1/orgs/:org/audit", get(crate::api::audit_api::query))
        .route(
            "/v1/orgs/:org/repos/:repo/commits",
            post(crate::api::commits::create),
        )
        // The verdicts standing beside one commit — one row per
        // workflow, the newest of each, which is what a commit page
        // needs and a history is not. A `RepoRead` sibling of the POST
        // above, through the same `rest_repo_auth` door.
        .route(
            "/v1/orgs/:org/repos/:repo/commits/:sha/checks",
            get(crate::api::checks_api::for_commit),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/files/*path",
            get(crate::api::reads::file),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/tree",
            get(crate::api::reads::tree_root),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/tree/*path",
            get(crate::api::reads::tree),
        )
        .route("/v1/orgs/:org/repos/:repo/log", get(crate::api::reads::log))
        .route(
            "/v1/orgs/:org/repos/:repo/branches",
            get(crate::api::reads::branches),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/tags",
            get(crate::api::reads::tags),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/refs",
            get(crate::api::reads::refs),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/diff",
            get(crate::api::reads::diff),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/owners",
            get(crate::api::owners_api::owners),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/owners/check",
            get(crate::api::owners_api::check),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/import",
            post(crate::api::imports_api::start).get(crate::api::imports_api::status),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/issues",
            post(crate::api::issues_api::create).get(crate::api::issues_api::list),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/issues/:number",
            get(crate::api::issues_api::get).patch(crate::api::issues_api::patch),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/issues/:number/comments",
            post(crate::api::issues_api::add_comment).get(crate::api::issues_api::comments),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/issues/:number/labels",
            put(crate::api::issues_api::set_labels),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/labels",
            get(crate::api::issues_api::labels).post(crate::api::issues_api::create_label),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/milestones",
            get(crate::api::issues_api::milestones),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/labels/:label",
            delete(crate::api::issues_api::delete_label),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes",
            post(crate::api::changes_api::create).get(crate::api::changes_api::list),
        )
        // The same page across every repository the caller may read —
        // what the changeset picker asks for, so that composing does not
        // begin with a request per repository.
        .route(
            "/v1/orgs/:org/changes",
            get(crate::api::changes_api::list_in_org),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change",
            get(crate::api::changes_api::get),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/approve",
            post(crate::api::changes_api::approve).delete(crate::api::changes_api::unapprove),
        )
        // Letting a fork's workflows run, at this tip only. Behind the
        // land door rather than the review-approval one: an opinion and
        // a machine are different grants.
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/workflows/approve",
            post(crate::api::changes_api::approve_workflows),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/verdict",
            get(crate::api::changes_api::verdict),
        )
        // "What changed since I last looked" — two patchsets of this
        // change, diffed against each other. A change read rather than a
        // repository read because the range is named in patchset numbers,
        // which only the change knows how to resolve.
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/interdiff",
            get(crate::api::changes_api::interdiff),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/comments",
            post(crate::api::changes_api::add_comment).get(crate::api::changes_api::comments),
        )
        // A review is one act: the caller's pending draft lives here,
        // and the two things that end it — submitting, and taking a
        // standing "no" back — are separate routes for the same reason
        // resolve and unresolve are. They are separate audit actions,
        // and a body that decided which is harder to grant, log and
        // read than a URL that says it.
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/review",
            post(crate::api::changes_api::start_review)
                .get(crate::api::changes_api::get_review)
                .delete(crate::api::changes_api::discard_review),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/review/submit",
            post(crate::api::changes_api::submit_review),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/review/withdraw",
            post(crate::api::changes_api::withdraw_review),
        )
        // Two routes rather than one taking a boolean: resolving and
        // reopening are separate audit actions, and a body that decides
        // which is harder to grant, log and read than a URL that says it.
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/comments/:comment/resolve",
            post(crate::api::changes_api::resolve_comment),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/comments/:comment/unresolve",
            post(crate::api::changes_api::unresolve_comment),
        )
        // Several suggestions, one new patchset. Deliberately not a
        // route under `/comments/:comment` — a suggestion applied on its
        // own would be a revision per click, and a reviewer's five
        // remarks are one act to the author taking them.
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/suggestions/apply",
            post(crate::api::changes_api::apply_suggestions),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/checks",
            post(crate::api::changes_api::post_check).get(crate::api::changes_api::checks),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/land",
            post(crate::api::changes_api::land),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/abandon",
            post(crate::api::changes_api::abandon),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/views",
            get(crate::api::change_views_api::get).put(crate::api::change_views_api::put),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/changes/:change/associations",
            get(crate::api::change_views_api::associations),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/land-queue",
            get(crate::api::changes_api::land_queue),
        )
        // One review and one landing across several repositories of the
        // org. Org-scoped because no single repository owns it; authority
        // is still per member repository, inside the handlers.
        .route(
            "/v1/orgs/:org/changesets",
            post(crate::api::changesets_api::create).get(crate::api::changesets_api::list),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset",
            get(crate::api::changesets_api::get),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset/verdict",
            get(crate::api::changesets_api::verdict),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset/workspace",
            get(crate::api::changesets_api::workspace),
        )
        // How big the review is, member by member. Its own route rather
        // than a field on the changeset read: it opens every member's
        // trees and reads every changed blob, which is far too much to
        // spend on the list nobody asked a size of.
        .route(
            "/v1/orgs/:org/changesets/:changeset/diffstat",
            get(crate::api::changesets_api::diffstat),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset/members",
            post(crate::api::changesets_api::add_member),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset/members/:repo/:change",
            delete(crate::api::changesets_api::remove_member),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset/edges",
            put(crate::api::changesets_api::set_edges),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset/abandon",
            post(crate::api::changesets_api::abandon),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset/land",
            post(crate::api::changesets_api::land),
        )
        .route(
            "/v1/orgs/:org/changesets/:changeset/revert",
            post(crate::api::changeset_revert::revert),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/watch",
            get(crate::api::watch_api::get).put(crate::api::watch_api::put),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/meta",
            get(crate::api::repo_meta::get),
        )
        // Beside `/meta` because they answer the same question from two
        // directions — what this project is, and who made it — and the
        // About rail reads both.
        .route(
            "/v1/orgs/:org/repos/:repo/contributors",
            get(crate::api::contribs_api::contributors),
        )
        // The Checks tab: CI verdicts from whatever actually ran them.
        // A read, on the same `RepoRead` gate as the rest of this block
        // — the writes into `check_runs` are the signed intake below and
        // the GitHub poller, neither of which is a route a person calls.
        .route(
            "/v1/orgs/:org/repos/:repo/checks/runs",
            get(crate::api::checks_api::list),
        )
        // What `.weft/` asks to have run, at a rev — read-only, and
        // it starts nothing. A `RepoRead` sibling of the checks reads
        // above, because a workflow file is repository content and this
        // is a view of it.
        .route(
            "/v1/orgs/:org/repos/:repo/workflows",
            get(crate::api::workflows_api::list),
        )
        // What actually ran. A `RepoRead` sibling of `/workflows` above
        // — the same repository content, one step later — except the
        // cancel, which destroys work and takes `RepoWrite`.
        .route(
            "/v1/orgs/:org/repos/:repo/workflow-runs",
            get(crate::api::workflow_runs_api::list),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/workflow-runs/:id",
            get(crate::api::workflow_runs_api::get),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/workflow-runs/:id/cancel",
            post(crate::api::workflow_runs_api::cancel),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/workflow-jobs/:id/log",
            get(crate::api::workflow_runs_api::log),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/workflow-jobs/:id/log/stream",
            get(crate::api::workflow_runs_api::log_stream),
        )
        // The runner's own door. Deliberately **not** under
        // `/v1/orgs/:org/repos/:repo` and deliberately not on
        // `rest_repo_auth`: a runner is executing untrusted code and
        // holds a token minted for exactly one job, so it addresses that
        // job by id and proves it owns it. Keeping the two surfaces
        // apart is what makes "a job token cannot cancel a run" true by
        // construction rather than by a check somebody has to remember.
        .route("/v1/runner/jobs/:id", get(crate::api::runner_api::spec))
        .route(
            "/v1/runner/jobs/:id/log",
            post(crate::api::runner_api::log_chunk).put(crate::api::runner_api::put_log),
        )
        .route(
            "/v1/runner/jobs/:id/lease",
            post(crate::api::runner_api::lease),
        )
        .route(
            "/v1/runner/jobs/:id/finish",
            post(crate::api::runner_api::finish),
        )
        // Self-hosted runners. Two audiences: the org routes are a
        // person in the dashboard (membership to read, `org:admin` to
        // write), and `/v1/runners/*` is a machine holding a secret that
        // is not a session and not an API token — which is why those two
        // are outside `/v1/orgs/:org/` exactly as the job routes are. A
        // runner should not have to assert which organisation it is in;
        // its credential says so.
        .route(
            "/v1/orgs/:org/runner-policy",
            get(crate::api::runners_api::get_policy).patch(crate::api::runners_api::patch_policy),
        )
        .route(
            "/v1/orgs/:org/runner-groups",
            get(crate::api::runners_api::list_groups).post(crate::api::runners_api::create_group),
        )
        .route(
            "/v1/orgs/:org/runner-groups/:id",
            axum::routing::patch(crate::api::runners_api::patch_group)
                .delete(crate::api::runners_api::delete_group),
        )
        .route("/v1/orgs/:org/runners", get(crate::api::runners_api::list))
        // Static before the parameter, and it has to be: a token mint is
        // a POST and a removal is a DELETE, so the two never collide on
        // a method, but a reader should still see which is which.
        .route(
            "/v1/orgs/:org/runners/registration-token",
            post(crate::api::runners_api::registration_token),
        )
        .route(
            "/v1/orgs/:org/runners/:id",
            delete(crate::api::runners_api::remove),
        )
        .route(
            "/v1/runners/register",
            post(crate::api::runners_api::register),
        )
        .route("/v1/runners/claim", post(crate::api::runners_api::claim))
        .route(
            "/v1/orgs/:org/repos/:repo/checks/runs/:id",
            get(crate::api::checks_api::get),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/topics",
            put(crate::api::repo_meta::put_topics),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/protections",
            post(crate::api::protections_api::protect).get(crate::api::protections_api::list),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/protections/*branch",
            delete(crate::api::protections_api::unprotect),
        )
        // Which named checks must pass before a change lands on a
        // branch. A **sibling** of `/protections/*branch` rather than
        // nested under it, and that is forced rather than chosen: a
        // branch name contains slashes (`release/2.0`), so it has to be
        // the route's trailing catch-all, and a catch-all can only be
        // the last segment — `checks` cannot follow it. Check names
        // contain slashes too (`ci/tests`), so the name cannot be the
        // second half of the path either; it rides in the body on POST
        // and as `?name=` on DELETE.
        .route(
            "/v1/orgs/:org/repos/:repo/required-checks/*branch",
            post(crate::api::protections_api::require_check)
                .get(crate::api::protections_api::list_required)
                .delete(crate::api::protections_api::unrequire_check),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/branches",
            post(crate::api::refops_api::create_branch),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/branches/:name",
            delete(crate::api::refops_api::delete_branch),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/reset",
            post(crate::api::refops_api::reset),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/revert",
            post(crate::api::refops_api::revert),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/tags",
            post(crate::api::refops_api::create_tag),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/tags/:name",
            delete(crate::api::refops_api::delete_tag),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/export",
            post(crate::api::exports::start),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/export/:job",
            get(crate::api::exports::status),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/export/:job/download",
            get(crate::api::exports::download),
        )
        .route("/v1/orgs/:org/export", post(crate::api::exports::bulk))
        .route("/v1/orgs/:org/mirrors", post(crate::api::mirrors::create))
        .route(
            "/v1/orgs/:org/mirrors/:repo/sync",
            post(crate::api::mirrors::sync_now),
        )
        .route("/webhooks/:provider", post(crate::mirror::webhook::receive))
        .route(
            "/v1/orgs/:org/repos/:repo/metrics",
            get(crate::api::metrics_api::repo_metrics),
        )
        .route(
            "/v1/orgs/:org/usage",
            get(crate::api::metrics_api::org_usage),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/compact",
            post(crate::api::metrics_api::compact_now),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/gc",
            post(crate::api::metrics_api::gc_now),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/cdn-pack",
            post(crate::api::metrics_api::cdn_pack_now),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/cdn/:pack",
            get(crate::api::cdn_api::pack),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/webhooks",
            post(crate::api::webhooks_api::create).get(crate::api::webhooks_api::list),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/webhooks/:id",
            delete(crate::api::webhooks_api::delete),
        )
        // The CI on-ramp. The intake is authorized by a signature over
        // the body rather than by a header, so it deliberately carries
        // no Authorization at all — see `api::checks_intake`.
        .route(
            "/v1/orgs/:org/repos/:repo/ci/secret",
            post(crate::api::checks_intake::rotate_secret)
                .get(crate::api::checks_intake::secret_status)
                .delete(crate::api::checks_intake::revoke_secret),
        )
        .route(
            "/v1/orgs/:org/repos/:repo/ci/checks",
            post(crate::api::checks_intake::intake),
        )
        // The other writer into `check_runs`: ask for a poll of this
        // repository's GitHub Actions runs, and read how the last one
        // went. The status route exists for one field — an installation
        // without `actions: read` fails in the one way that renders
        // identically to success, an empty Checks tab, and a page that
        // cannot tell those apart tells a maintainer their CI is not
        // configured when the truth is that we may not look at it.
        .route(
            "/v1/orgs/:org/repos/:repo/ci/poll",
            post(crate::api::checks_intake::poll_now).get(crate::api::checks_intake::poll_status),
        )
        .route("/metrics", get(prometheus))
        // git smart HTTP
        .route("/:org/:repo/info/refs", get(info_refs))
        .route("/:org/:repo/git-upload-pack", post(upload_pack))
        .route("/:org/:repo/git-receive-pack", post(receive_pack))
        // …and the synthetic repository a changeset is cloned as. Five
        // path segments where a repository has four, so the two can
        // never collide — and `changesets` is reserved as a repository
        // name so the four-segment form cannot be created to shadow it.
        .route("/:org/changesets/:key/info/refs", get(changeset_info_refs))
        .route(
            "/:org/changesets/:key/git-upload-pack",
            post(changeset_upload_pack),
        )
        .route(
            "/:org/changesets/:key/git-receive-pack",
            post(changeset_receive_pack),
        )
        // web assets: spec compiled in, dashboard from disk
        .route("/openapi.json", get(crate::webassets::openapi))
        .route("/dashboard", get(crate::webassets::dashboard))
        // axum wildcards need ≥1 char, so the bare trailing-slash form
        // is its own route.
        .route("/dashboard/", get(crate::webassets::dashboard))
        .route("/dashboard/*path", get(crate::webassets::dashboard))
        .fallback(get(crate::webassets::fallback))
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state)
}

/// Liveness, and *which* process is alive. The `x-weft-instance` header
/// carries this instance's id, so a probe can tell one server from
/// another on the same port — which is exactly what the test harness
/// cannot otherwise do: two suites handed the same port by the kernel,
/// the loser exits on "address in use", and its harness's health check
/// is answered by the winner. Every request the losing test then makes
/// goes to a stranger's server with a stranger's database, and fails
/// hundreds of lines later as a 401. A load balancer gets the same
/// benefit for free: the header names the task that answered.
async fn healthz(State(state): State<SharedState>) -> ([(&'static str, String); 1], &'static str) {
    ([("x-weft-instance", state.instance_id.clone())], "ok\n")
}

/// How long a readiness probe may spend on the store before it gives up
/// and reports the instance unready. Deliberately far below the store's
/// default read timeout: a stalled store must make this answer *faster*,
/// not slower.
const READYZ_STORE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Readiness (M7): liveness says the process is up; readiness proves the
/// two dependencies a request needs — the control DB answers a query and
/// the object store answers a (cheap, empty-prefix) LIST. 503 with the
/// failing dependency named so orchestrators can gate traffic.
///
/// The store probe is built with a short timeout rather than the default.
/// A store that accepts connections and then never answers is the failure
/// this endpoint exists to catch, and on the default 300 s timeout the
/// probe would hang for five minutes instead of answering 503 — leaving
/// the load balancer with no signal and the task in rotation. An outage
/// that makes the health check unanswerable is worse than one it reports.
async fn readyz(State(state): State<SharedState>) -> Response {
    use axum::response::IntoResponse;
    let db = state.db.clone();
    let store_url = state.store_url.clone();
    let checks = tokio::task::spawn_blocking(move || {
        if let Err(e) = registry::ping(&db) {
            return Err(format!("db: {e}"));
        }
        let store = stratum_store::ObjectStore::with_timeout(
            &store_url,
            stratum_store::LatencyModel::None,
            READYZ_STORE_TIMEOUT,
        );
        if let Err(e) = store.list("readyz-probe-no-such-prefix/") {
            return Err(format!("store: {e}"));
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(format!("join: {e}")));
    match checks {
        Ok(()) => (axum::http::StatusCode::OK, "ready\n").into_response(),
        Err(why) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {why}\n"),
        )
            .into_response(),
    }
}

async fn prometheus() -> Response {
    use axum::response::IntoResponse;
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        crate::metering::prometheus_text(),
    )
        .into_response()
}

pub fn org_or_404(state: &AppState, org_name: &str) -> Result<Org, Response> {
    match registry::org_by_name(&state.db, org_name) {
        Ok(Some(o)) => Ok(o),
        Ok(None) => Err(authx::not_found()),
        Err(e) => Err(crate::api::internal(e)),
    }
}

/// A namespace's name from its id.
///
/// Only for building a link back into the dashboard after a redirect,
/// where the id is what we have and the name is what a URL wants.
pub fn org_name_of(state: &AppState, org_id: &str) -> Option<String> {
    registry::org_by_id(&state.db, org_id)
        .ok()
        .flatten()
        .map(|o| o.name)
}

/// The person behind a browser session, with no namespace in the
/// question.
///
/// Every other seam resolves a session *against an org*, because
/// authority is per-namespace. Search has no namespace to resolve
/// against — asking which namespaces hold something is the question —
/// so it needs the person alone. `sessions::verify` still refuses a
/// revoked cookie and a disabled account, which is where that check
/// belongs.
pub fn session_user(state: &AppState, headers: &HeaderMap) -> Result<Option<String>, Response> {
    let Some(cookie) = authx::session_from_headers(headers) else {
        return Ok(None);
    };
    let session =
        stratum_control::sessions::verify(&state.db, cookie).map_err(crate::api::internal)?;
    let Some(session) = session else {
        return Ok(None);
    };
    stratum_control::sessions::touch(&state.db, &session.id);
    Ok(Some(session.user_id))
}

/// Look a repo up without deciding how to refuse it.
///
/// `Ok(None)` means "not there"; what to *say* about that is the
/// caller's business and the only thing separating the two wrappers
/// below. Written as one lookup rather than two because it was two, and
/// the coverage gate said so: the same unreachable database-error arm
/// appeared twice, which is the same exemption written down in two
/// places and one of them going stale.
fn resolve_repo(
    state: &AppState,
    org_name: &str,
    repo_name: &str,
) -> Result<Option<(Org, Repo)>, Response> {
    let org = org_or_404(state, org_name)?;
    match registry::repo_by_name(&state.db, &org.id, repo_name.trim_end_matches(".git")) {
        Ok(Some(r)) => Ok(Some((org, r))),
        Ok(None) => Ok(None),
        Err(e) => Err(crate::api::internal(e)),
    }
}

/// Resolve a repo for a caller who has no headers to authenticate with.
///
/// Two routes are like that. The signed CDN pack URL, where the
/// signature in the query string *is* the authorization and a browser
/// following the link sends nothing else; and the signed CI intake,
/// whose body signature is the credential. A 401 there would ask for
/// credentials the client has no way to supply. Every other REST route
/// wants [`repo_or_masked`] instead.
pub fn repo_or_404(
    state: &AppState,
    org_name: &str,
    repo_name: &str,
) -> Result<(Org, Repo), Response> {
    resolve_repo(state, org_name, repo_name)?.ok_or_else(authx::not_found)
}

/// Resolve a repo, or answer in a way that does not say whether it
/// exists.
///
/// The `masked` part is the point. Answering 404 for "no such repo" and
/// 401 for "private" is an existence oracle: anonymously ask for
/// `acme/payments`, read the status code, and you have learned that
/// `acme` has a private repository by that name. The git wire has always
/// answered 401 to both, and REST did not — the two front doors
/// disagreed about the same repository.
///
/// Note what is *not* masked: whether the namespace exists. Namespace
/// names are globally unique and claimed first-come, so signup already
/// answers that question to anyone who asks, and pretending otherwise
/// here would be theatre.
pub fn repo_or_masked(
    state: &AppState,
    headers: &HeaderMap,
    org_name: &str,
    repo_name: &str,
) -> Result<(Org, Repo), Response> {
    resolve_repo(state, org_name, repo_name)?.ok_or_else(|| authx::masked(&state.db, headers))
}

/// Resolve + authorize one REST repo request: a token scoped to the
/// repo, or a browser session whose person holds the scope on it.
///
/// A token wins if one is presented; otherwise the browser session is
/// used. Order matters: a developer with a dashboard session open in the
/// same browser must still be able to test a token by pasting it into a
/// request, and get that token's authority, not their own.
///
/// Anonymous is 401 — there is nothing here anybody may read without
/// signing in — and a caller with no role on this repository gets the
/// masked 404 a missing one gets, so a status code is never an
/// existence oracle.
pub fn rest_repo_auth(
    state: &AppState,
    headers: &HeaderMap,
    org_name: &str,
    repo_name: &str,
    need: Scope,
) -> Result<(Org, Repo, stratum_control::auth::Principal), Response> {
    let (org, repo) = repo_or_masked(state, headers, org_name, repo_name)?;
    let principal = match authx::principal_opt(&state.db, headers, authx::Challenge::None)? {
        // A personal token carries the person, so its authority on *this*
        // repo is their effective role here, not the org-wide one it was
        // minted with. A service token comes back unchanged. A token from
        // another organisation has no role here to refine.
        Some(p) => {
            let here = if p.org_id == org.id {
                stratum_control::members::refine_for_repo(&state.db, &p, &repo.id)
                    .map_err(crate::api::internal)?
            } else {
                None
            };
            // No role here, or their membership ended between
            // authenticating and reaching this repo — an SSH connection
            // authenticates once and can serve much later. Fails closed
            // and masked.
            here.ok_or_else(authx::not_found)?
        }
        None => match authx::session_principal(&state.db, headers, &org.id, Some(&repo.id))? {
            authx::SessionAuth::Principal(p) => p,
            // Signed in but not a member here: masked, exactly like a
            // foreign token.
            authx::SessionAuth::NoAccess(_) => return Err(authx::not_found()),
            authx::SessionAuth::None => return Err(authx::unauthorized(authx::Challenge::None)),
        },
    };
    if principal.org_id != org.id || !principal.allows(need, Some(&repo.id)) {
        return Err(authx::not_found());
    }
    Ok((org, repo, principal))
}

/// Why a wire request from an AUTHENTICATED principal is denied —
/// transport-neutral, so HTTP and SSH front doors render the same
/// decision (status codes vs. in-band pkt ERR).
pub(crate) enum WireDeny {
    /// Missing, or masked (R8): a principal without access can never
    /// distinguish "exists elsewhere" from "does not exist".
    NotFound,
    /// May read, may not push. The repository's existence is no secret
    /// from this caller — they hold read on it — so the refusal can say
    /// what would work instead. Before forks shipped this was `NotFound`
    /// too, and the first thing a would-be contributor met was
    /// "repository not found" for a repository they had just cloned.
    ReadOnly(String),
    Internal(String),
}

/// The sentence a reader who tried to push is given, over either
/// transport. It names the two ways forward because both exist.
pub(crate) fn read_only_msg(org: &str, repo: &str) -> String {
    format!(
        "you can read {org}/{repo} but not push to it; fork it and open a change \
         from your fork, or ask an owner for write access"
    )
}

/// Resolve + authorize one wire request for a verified principal. The
/// core both front doors share: org/repo lookup, org membership, scope
/// check, existence masking.
pub(crate) fn wire_repo_for_principal(
    state: &AppState,
    p: &stratum_control::auth::Principal,
    org_name: &str,
    repo_name: &str,
    need: Scope,
) -> Result<(Repo, RepoCtx), WireDeny> {
    let org = registry::org_by_name(&state.db, org_name)
        .map_err(WireDeny::Internal)?
        .ok_or(WireDeny::NotFound)?;
    let repo = registry::repo_by_name(&state.db, &org.id, repo_name.trim_end_matches(".git"))
        .map_err(WireDeny::Internal)?
        .ok_or(WireDeny::NotFound)?;
    let ctx = RepoCtx {
        store_url: state.store_url.clone(),
        prefix: repo.prefix().as_str().to_string(),
    };
    // Same refinement the REST seam applies: an SSH key or personal token
    // authenticated before the repo was known, and a per-repo grant is
    // exactly the case where the org role is the wrong answer. A
    // principal from another organization has no role here to refine.
    let refined = if p.org_id == org.id {
        members::refine_for_repo(&state.db, p, &repo.id).map_err(WireDeny::Internal)?
    } else {
        None
    };
    let allows = |scope: Scope| {
        refined
            .as_ref()
            .is_some_and(|p| p.allows(scope, Some(&repo.id)))
    };
    if allows(need) {
        return Ok((repo, ctx));
    }
    // A push from somebody who may read: the existence of the repository
    // is already theirs to know, so the refusal says what to do instead.
    if need == Scope::RepoWrite && allows(Scope::RepoRead) {
        return Err(WireDeny::ReadOnly(read_only_msg(
            org_name,
            repo.name.as_str(),
        )));
    }
    Err(WireDeny::NotFound)
}

/// Which door of the smart-HTTP protocol a request came through. The
/// same refusal is rendered differently at each: the advert carries an
/// in-band `ERR` pkt that stock git prints as `remote error: …`, while
/// the RPC behind it answers a plain 403 — git never shows a body there,
/// and a 200 from the RPC is taken as "the push went through".
#[derive(Clone, Copy, PartialEq, Eq)]
enum Door {
    Advert,
    Rpc,
}

fn render_deny(d: WireDeny, door: Door) -> Response {
    match d {
        WireDeny::NotFound => authx::not_found(),
        WireDeny::ReadOnly(msg) => match door {
            Door::Advert => git_http::advert_error(&msg),
            Door::Rpc => authx::forbidden(&msg),
        },
        WireDeny::Internal(e) => crate::api::internal(e),
    }
}

/// Resolve + authorize one git-wire request: a token with the scope on
/// that repo. Without credentials the answer is 401 (so git retries with
/// creds); with valid credentials but no read access it is 404
/// (existence masking, R8); a push by somebody who may read is refused
/// with the read-only sentence, on whichever `door` the client is
/// knocking at.
fn wire_auth(
    state: &AppState,
    headers: &HeaderMap,
    org_name: &str,
    repo_name: &str,
    need: Scope,
    door: Door,
) -> Result<(Repo, RepoCtx, Option<Principal>), Response> {
    // Verify credentials first: an invalid token 401s even for repos that
    // don't exist, and an unauthenticated probe of any unknown/private
    // path 401s so real clients send credentials on retry.
    let principal = authx::principal_opt(&state.db, headers, authx::Challenge::Basic)?;
    match principal {
        Some(p) => {
            let (repo, ctx) = wire_repo_for_principal(state, &p, org_name, repo_name, need)
                .map_err(|d| render_deny(d, door))?;
            Ok((repo, ctx, Some(p)))
        }
        // Nothing is readable without credentials, so there is nothing
        // to look up: every anonymous request is asked for them, the same
        // answer for a repository that exists and one that does not.
        None => Err(authx::unauthorized(authx::Challenge::Basic)),
    }
}

async fn info_refs(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    match params.get("service").map(String::as_str) {
        Some("git-upload-pack") => {
            let (repo_row, ctx, _) =
                match wire_auth(&state, &headers, &org, &repo, Scope::RepoRead, Door::Advert) {
                    Ok(x) => x,
                    Err(r) => return r,
                };
            let cdn = crate::cdn::resolve(&state, &org, &repo, &ctx.prefix);
            let mut resp =
                git_http::upload_pack_advert(&headers, cdn.as_ref().map(|o| &o.pack)).await;
            if repo_row.kind == RepoKind::Mirror {
                freshness::attach(&mut resp, &freshness::staleness_headers(&repo_row));
            }
            resp
        }
        Some("git-receive-pack") => {
            let (repo_row, ctx, _) = match wire_auth(
                &state,
                &headers,
                &org,
                &repo,
                Scope::RepoWrite,
                Door::Advert,
            ) {
                Ok(x) => x,
                Err(r) => return r,
            };
            // A mirror's push is forwarded to its origin, and a mirror
            // with nothing that could push there is told so here — an
            // in-band ERR pkt, which stock git prints as "remote error:
            // <msg>" with the origin named — before it builds a pack.
            if let Some(msg) = mirror_push_refusal(&state, &repo_row) {
                return git_http::advert_error(&msg);
            }
            git_http::receive_pack_advert(ctx, state.permits.clone()).await
        }
        other => git_http::err_response(format!("unhandled service {other:?}")),
    }
}

/// GET `/:org/changesets/:key/info/refs` — the workspace's advert.
///
/// The upload-pack advert is a constant, so authority is the whole of
/// what happens here: the changeset must exist for this caller and every
/// member must be readable by them, or the answer is the same masked
/// 404 a private repository gives.
async fn changeset_info_refs(
    State(state): State<SharedState>,
    Path((org, key)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    match params.get("service").map(String::as_str) {
        Some("git-upload-pack") => {
            if let Err(r) = changeset_workspace::wire_auth(&state, &headers, &org, &key) {
                return r;
            }
            // No CDN: there is no stored pack to hand out for a
            // repository that is assembled per request.
            git_http::upload_pack_advert(&headers, None).await
        }
        Some("git-receive-pack") => {
            // Authorized first, and with read: learning that a workspace
            // is read-only is learning that it exists, so a caller who
            // may not see the changeset gets the masked answer instead
            // of the refusal.
            if let Err(r) = changeset_workspace::wire_auth(&state, &headers, &org, &key) {
                return r;
            }
            git_http::advert_error(changeset_workspace::READ_ONLY)
        }
        other => git_http::err_response(format!("unhandled service {other:?}")),
    }
}

async fn changeset_upload_pack(
    State(state): State<SharedState>,
    Path((org, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let built = match changeset_workspace::wire_auth(&state, &headers, &org, &key) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let body = match decode_rpc_body(&headers, body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // Nothing is metered: no repository's storage was read, and charging
    // a member repo for a clone of the changeset would bill the same
    // bytes to whichever member happened to sort first.
    git_http::upload_pack_workspace(Arc::new(built.ws), state.permits.clone(), body).await
}

/// The push door, which exists only to refuse in words git will print.
///
/// A 405 would be the honest status and the wrong answer: git renders it
/// as a transport failure with no message, and the person is left
/// guessing. `advert_error` puts the sentence in an ERR pkt, which
/// stock git surfaces as `remote error: …`.
async fn changeset_receive_pack(
    State(state): State<SharedState>,
    Path((org, key)): Path<(String, String)>,
    headers: HeaderMap,
    _body: Bytes,
) -> Response {
    if let Err(r) = changeset_workspace::wire_auth(&state, &headers, &org, &key) {
        return r;
    }
    git_http::advert_error(changeset_workspace::READ_ONLY)
}

/// Undo `Content-Encoding: gzip` on a smart-HTTP RPC body.
///
/// Git compresses the request whenever it buffered it and gzip actually
/// made it smaller — see `post_rpc` in remote-curl.c. Whether that
/// happens is a property of the *content*, not of any setting: a handful
/// of refs makes a body too small for gzip to win and it goes over the
/// wire as-is, while a couple of dozen `want` lines for the same oid
/// compress enormously and git switches to gzip without being asked.
///
/// Nothing here decoded it, so the pkt parser read deflate bytes as
/// pkt-lines and answered `bad pkt len` — a 500 on `git clone`, for any
/// repository with enough refs. It looked like a ref-count limit and was
/// really a content-encoding one, which is why it appeared at a
/// threshold rather than at a boundary anybody had chosen.
///
/// Both wire routes decode: `post_rpc` is the same code for a fetch and
/// for a push, so a push of a large ref set was heading for the same
/// answer.
fn decode_rpc_body(headers: &HeaderMap, body: Bytes) -> Result<Bytes, Response> {
    let gzipped = headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|p| p.trim().eq_ignore_ascii_case("gzip")));
    if !gzipped {
        return Ok(body);
    }
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&body[..])
        .take(BODY_LIMIT as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| git_http::err_response(format!("gzip request body: {e}")))?;
    if out.len() > BODY_LIMIT {
        return Err(git_http::err_response(
            "gzip request body exceeds the request limit".into(),
        ));
    }
    Ok(Bytes::from(out))
}

async fn upload_pack(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (repo_row, ctx, who) =
        match wire_auth(&state, &headers, &org, &repo, Scope::RepoRead, Door::Rpc) {
            Ok(x) => x,
            Err(r) => return r,
        };
    let runner = crate::metering::is_runner(&state.db, who.as_ref());
    let body = match decode_rpc_body(&headers, body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let start = std::time::Instant::now();
    let summary = stratum_proto::serve::parse_fetch_summary(&body);
    // Mirror freshness contract (M2): a fetch for commits we don't have
    // triggers a bounded synchronous origin sync; failure modes are
    // explicit (staleness headers / 404-with-explanation) — never a
    // silent stale miss.
    let mut stale = freshness::Staleness::default();
    if repo_row.kind == RepoKind::Mirror {
        match &summary {
            None => stale = freshness::staleness_headers(&repo_row),
            Some(s) if s.wants.is_empty() => stale = freshness::staleness_headers(&repo_row),
            Some(s) => match freshness::ensure_wants(&state, &repo_row, &s.wants).await {
                Ok(st) => stale = st,
                Err(resp) => return resp,
            },
        }
    }
    let cdn = crate::cdn::resolve(&state, &org, &repo, &ctx.prefix);
    // Offloaded only when we actually had a pack to advertise *and* the
    // client opted in — that pair is what sends bytes to the edge.
    let offloaded = cdn.is_some() && summary.as_ref().is_some_and(|s| s.wants_packfile_uris);
    let pack_size = cdn.as_ref().map(|o| o.size).unwrap_or(0);
    let mut resp =
        git_http::upload_pack(ctx, state.permits.clone(), body, cdn.map(|o| o.pack)).await;
    freshness::attach(&mut resp, &stale);
    // M6 metering: clone (fetch with no haves) vs incremental fetch;
    // ls-refs and adverts count as absorbed requests without latency rows.
    // The kind decides whether the bytes bill — see `metering`.
    match summary {
        Some(s) if resp.status().is_success() => {
            let kind = crate::metering::egress_kind(s.haves == 0, offloaded, runner);
            state
                .meter
                .record_offload(&repo_row.id, offloaded, runner, pack_size);
            resp = crate::metering::meter_response(resp, &state.meter, &repo_row.id, kind, start);
        }
        _ => state.meter.record(&repo_row.id, "api", 0, None),
    }
    resp
}

async fn receive_pack(
    State(state): State<SharedState>,
    Path((org, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (repo_row, ctx, who) =
        match wire_auth(&state, &headers, &org, &repo, Scope::RepoWrite, Door::Rpc) {
            Ok(x) => x,
            Err(r) => return r,
        };
    // Belt to the advert's braces: a client that skipped the advert and
    // posted a pack straight here is refused by the same rule.
    if let Some(msg) = mirror_push_refusal(&state, &repo_row) {
        return authx::forbidden(&msg);
    }
    let body = match decode_rpc_body(&headers, body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // Loaded here, where the control plane lives — the wire engine only
    // parses ref names out of the pkt stream, so it does the refusing.
    let protected = match stratum_control::protections::protected_branches(&state.db, &repo_row.id)
    {
        Ok(p) => p,
        Err(e) => return crate::api::internal(e),
    };
    let start = std::time::Instant::now();
    let pushed = body.len() as u64;
    let out = crate::push::receive(&state, &repo_row, &ctx, body, protected).await;
    let (resp, accepted) = git_http::receive_pack_response(out);
    if let Some(updates) = accepted {
        let actx = stratum_control::audit::AuditCtx::of(&repo_row.org_id, who.as_ref());
        crate::push::after_accept(&state, &repo_row, &actx, &updates, "git", pushed, start).await;
    }
    resp
}

/// Why a push to this repository could not be forwarded, when it is a
/// mirror and could not be. `None` for a native repository and for a
/// mirror whose origin can be pushed to.
pub(crate) fn mirror_push_refusal(state: &SharedState, repo: &Repo) -> Option<String> {
    if repo.kind != RepoKind::Mirror {
        return None;
    }
    state
        .sync
        .push_credential(repo)
        .err()
        .map(|r| r.sentence().unwrap_or_default().to_string())
}

pub async fn serve(state: SharedState, bind: &str) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| format!("bind {bind}: {e}"))?;
    eprintln!(
        "stratum-server listening on {}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    // Say it once at boot, because nothing else will.
    //
    // Accounts made by `admin user-create` before it was fixed have no
    // handle and no personal namespace. Both symptoms are silent: their
    // issues render with no author, and forking with no target answers
    // "no personal namespace to fork into". An operator has no way to
    // discover that from the product — the accounts work, they are just
    // quietly less than whole.
    //
    // A warning rather than a repair. The repair claims globally unique
    // names and can collide with an existing org, which is a decision
    // somebody has to make; doing it silently at boot would pick for
    // them, and failing at boot would refuse to start over a cosmetic
    // defect. Neither is right, so it says what is wrong and names the
    // one command that fixes it.
    match stratum_control::users::without_handle(&state.db) {
        Ok(pending) if !pending.is_empty() => eprintln!(
            "stratum-server: {} account(s) have no handle or personal namespace, so their \
             issues show no author and forking has nowhere to go — run: stratum-server admin \
             repair-identities (--dry-run to see what it would do)",
            pending.len()
        ),
        // A read failure here must not stop the server: this is a
        // courtesy, and the database is about to be exercised properly
        // by the first request either way.
        Ok(_) => {}
        Err(e) => eprintln!("stratum-server: could not check account handles: {e}"),
    }
    // One signal watcher fans out to every front door. SIGINT (operators,
    // dev) and SIGTERM (orchestrators — ECS stops tasks with TERM) both
    // drain: stop accepting, finish in-flight requests. SIGKILL after the
    // orchestrator's stop timeout is the hard bound.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        let _ = stop_tx.send(true);
    });
    let mut http_stop = stop_rx.clone();
    let http = axum::serve(listener, router(state.clone())).with_graceful_shutdown(async move {
        let _ = http_stop.changed().await;
    });
    match (&state.ssh_bind, &state.ssh_host_key_pem) {
        (Some(ssh_bind), Some(pem)) => {
            // Fail the boot on SSH misconfiguration, then run both doors
            // until the shared stop signal drains them.
            let front = crate::ssh::prepare(state.clone(), ssh_bind, pem).await?;
            let (h, s) = tokio::join!(http, front.run(stop_rx));
            h.map_err(|e| e.to_string())?;
            s
        }
        _ => http.await.map_err(|e| e.to_string()),
    }
}

pub fn state_from_env() -> Result<SharedState, String> {
    let store_url = std::env::var("STRATUM_STORE_URL")
        .map_err(|_| "STRATUM_STORE_URL must be set (e.g. https://s3.../bucket)".to_string())?;
    let db = open_db_from_env()?;
    let bind = std::env::var("STRATUM_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let public_url =
        std::env::var("STRATUM_PUBLIC_URL").unwrap_or_else(|_| format!("http://{bind}"));
    let public_url_for_runners = public_url.trim_end_matches('/').to_string();

    let webhook_secret = std::env::var("STRATUM_WEBHOOK_SECRET").unwrap_or_default();
    let mut providers: HashMap<String, Arc<dyn OriginProvider>> = HashMap::new();
    let mut github_app: Option<Arc<GithubApp>> = None;
    providers.insert(
        "generic".into(),
        Arc::new(Generic {
            webhook_secret: webhook_secret.clone(),
        }),
    );
    if let Ok(app_id) = std::env::var("STRATUM_GITHUB_APP_ID") {
        let pem = match std::env::var("STRATUM_GITHUB_APP_KEY_PEM") {
            Ok(path) => std::fs::read_to_string(&path)
                .map_err(|e| format!("read {path}: {e}"))?,
            Err(_) => std::env::var("STRATUM_GITHUB_APP_KEY")
                .map_err(|_| "GitHub app configured but no key (STRATUM_GITHUB_APP_KEY_PEM                               or STRATUM_GITHUB_APP_KEY)".to_string())?,
        };
        let mut app = GithubApp::new(
            app_id,
            pem,
            std::env::var("STRATUM_GITHUB_API_BASE")
                .unwrap_or_else(|_| "https://api.github.com".into()),
            std::env::var("STRATUM_GITHUB_GIT_BASE")
                .unwrap_or_else(|_| "https://github.com".into()),
            std::env::var("STRATUM_GITHUB_WEBHOOK_SECRET").unwrap_or(webhook_secret),
        );
        // The App's OAuth client, both halves or neither: one without the
        // other is a deployment that believes it verifies installers and
        // does not, which is the one misconfiguration worth refusing to
        // boot on. Blank counts as unset — a task definition can empty a
        // variable but not remove it.
        let client_id = std::env::var("STRATUM_GITHUB_CLIENT_ID")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let client_secret = std::env::var("STRATUM_GITHUB_CLIENT_SECRET")
            .ok()
            .filter(|v| !v.trim().is_empty());
        app.user_auth = match (client_id, client_secret) {
            (Some(client_id), Some(client_secret)) => Some(crate::mirror::origin::UserAuth {
                client_id,
                client_secret,
                oauth_base: std::env::var("STRATUM_GITHUB_OAUTH_BASE")
                    .unwrap_or_else(|_| "https://github.com".into()),
            }),
            (None, None) => None,
            _ => {
                return Err(
                    "GitHub app: STRATUM_GITHUB_CLIENT_ID and STRATUM_GITHUB_CLIENT_SECRET \
                            are set together or not at all"
                        .into(),
                )
            }
        };
        let app = Arc::new(app);
        github_app = Some(app.clone());
        providers.insert("github".into(), app);
    }
    let data_dir = std::path::PathBuf::from(
        std::env::var("STRATUM_DATA_DIR").unwrap_or_else(|_| "stratum-data".into()),
    );
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    let mut sync_mgr = SyncManager::new(db.clone(), store_url.clone(), data_dir.clone(), providers);
    if let Some(app) = github_app {
        sync_mgr = sync_mgr.with_github(app);
    }
    let sync = Arc::new(sync_mgr);
    let freshness_timeout = std::time::Duration::from_secs(
        std::env::var("STRATUM_FRESHNESS_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8),
    );
    let meter = crate::metering::spawn_writer(db.clone());
    let cdn = cdn_config_from_env()?;
    let ssh_bind = std::env::var("STRATUM_SSH_BIND")
        .ok()
        .filter(|s| !s.is_empty());
    let ssh_host_key_pem = std::env::var("STRATUM_SSH_HOST_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    if ssh_bind.is_some() && ssh_host_key_pem.is_none() {
        // A generated-per-boot key would make every task in a fleet (and
        // every restart) look like a MITM to clients; require a pinned one.
        return Err(
            "STRATUM_SSH_BIND is set but STRATUM_SSH_HOST_KEY is not — provide the \
             fleet-stable host key PEM (ssh-keygen -t ed25519)"
                .into(),
        );
    }
    // Megabytes, because that is the unit an operator sizes a task in.
    // `0` is a legitimate setting — a cache that remembers nothing — and
    // the suite uses it to prove no read *depends* on the cache.
    let read_cache_bytes = std::env::var("STRATUM_READ_CACHE_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(256)
        .saturating_mul(1024 * 1024);
    Ok(Arc::new(AppState {
        db,
        store: Arc::new(stratum_store::ObjectStore::new(
            &store_url,
            stratum_store::LatencyModel::None,
        )),
        read_cache: Arc::new(stratum_engine::read::ReadCache::new(read_cache_bytes)),
        store_url,
        public_url,
        permits: Arc::new(GitPermits::default()),
        sync,
        freshness_timeout,
        meter,
        mailer: crate::mail::from_env()?,
        github_install_url: std::env::var("STRATUM_GITHUB_INSTALL_URL").ok(),
        data_dir,
        dashboard_dir: std::env::var("STRATUM_DASHBOARD_DIR")
            .ok()
            .map(std::path::PathBuf::from),
        ssh_bind,
        ssh_host_key_pem,
        ssh_public_url: std::env::var("STRATUM_SSH_PUBLIC_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end_matches('/').to_string()),
        instance_id: std::env::var("STRATUM_INSTANCE_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(stratum_control::ids::ulid),
        cdn,
        runner_url: std::env::var("STRATUM_RUNNER_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| public_url_for_runners.clone()),
        max_timeout_minutes: env_i64("STRATUM_RUNNER_MAX_TIMEOUT_MINUTES")
            .filter(|&n| n > 0)
            .unwrap_or(stratum_control::workflows::DEFAULT_TIMEOUT_MINUTES),
        // Clamped rather than trusted: a zero or negative wait would
        // turn the long poll into a hot loop across every registered
        // machine, which is precisely the load it exists to prevent.
        runner_claim_wait_ms: env_i64("STRATUM_RUNNER_CLAIM_WAIT_MS")
            .filter(|&n| n > 0)
            .unwrap_or(20_000),
        runner_start_lease_ms: crate::workers::lease_ms("STRATUM_RUNNER_START_LEASE_SECS", 600),
        runner_max_attempts: i64::try_from(crate::workers::env_secs(
            "STRATUM_RUNNER_MAX_ATTEMPTS",
            2,
        ))
        .unwrap_or(i64::MAX)
        .max(1),
    }))
}

/// A whole-number knob from the environment.
///
/// Anything unparseable is `None` — the default — rather than a boot
/// failure. These are budget and timeout ceilings: refusing to start
/// over a typo in one of them would take a deployment down to protect
/// it from running builds for slightly too long.
fn env_i64(key: &str) -> Option<i64> {
    std::env::var(key).ok()?.trim().parse::<i64>().ok()
}

/// CDN offload config. Unset base = feature off. A base *with* signing
/// half-configured is a boot error rather than a silent fallback to
/// unsigned URLs: for a private repo an unsigned URL would be an open
/// door, and the failure must be loud at deploy time, not at clone time.
fn cdn_config_from_env() -> Result<Option<crate::cdn::CdnConfig>, String> {
    if std::env::var("STRATUM_CDN_ENABLED")
        .map(|v| v == "0")
        .unwrap_or(false)
    {
        // Kill switch: turn offload off fleet-wide without a redeploy.
        return Ok(None);
    }
    let Some(base) = std::env::var("STRATUM_CDN_BASE")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let key_pair_id = std::env::var("STRATUM_CDN_KEY_PAIR_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let pem = match std::env::var("STRATUM_CDN_PRIVATE_KEY_PEM") {
        Ok(path) if !path.is_empty() => {
            Some(std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?)
        }
        _ => std::env::var("STRATUM_CDN_PRIVATE_KEY")
            .ok()
            .filter(|s| !s.is_empty()),
    };
    let signing = match (key_pair_id, pem) {
        (Some(key_pair_id), Some(private_key_pem)) => Some(crate::cdn::CdnSigning {
            key_pair_id,
            private_key_pem,
        }),
        (None, None) => None,
        _ => {
            return Err(
                "STRATUM_CDN_KEY_PAIR_ID and STRATUM_CDN_PRIVATE_KEY[_PEM] must be set \
                        together (a half-configured signer would hand out unsigned URLs)"
                    .into(),
            )
        }
    };
    let origin_secret = std::env::var("STRATUM_CDN_ORIGIN_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    if origin_secret.is_some() && signing.is_some() {
        return Err(
            "STRATUM_CDN_ORIGIN_SECRET and STRATUM_CDN_KEY_PAIR_ID select different \
             origins (this server vs. the bucket) — configure exactly one"
                .into(),
        );
    }
    Ok(Some(crate::cdn::CdnConfig {
        base: base.trim_end_matches('/').to_string(),
        signing,
        origin_secret,
        url_ttl_secs: std::env::var("STRATUM_CDN_URL_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600),
    }))
}

/// Ingest shape knobs. STRATUM_REF_PAGE_SIZE opts a deployment into
/// sharded ref pages at a chosen page size (large-ref-count fleets; also
/// how tests exercise the paged paths with tiny counts). Unset = engine
/// defaults (flat refs until a repo exceeds the default page size).
pub fn ingest_config_from_env() -> stratum_engine::IngestConfig {
    let mut cfg = stratum_engine::IngestConfig::default();
    if let Some(n) = std::env::var("STRATUM_REF_PAGE_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n >= 2)
    {
        cfg.page_size = n;
        cfg.paged_refs = true;
    }
    cfg
}

pub fn open_db_from_env() -> Result<ControlDb, String> {
    let url = std::env::var("STRATUM_DB_URL")
        .map_err(|_| "STRATUM_DB_URL (postgres://user@host:port/db) is required".to_string())?;
    ControlDb::open(&url)
}
