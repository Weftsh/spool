// Typed client for the Weft control-plane API.
//
// Two ways to be signed in, and the difference is one field. A person
// signs in with email and password and the server sets an HttpOnly
// session cookie, which the browser attaches by itself — nothing about
// the credential is reachable from script, so `token` is empty. A
// service caller pastes an API token instead, which travels in the
// Authorization header and lives in localStorage (a per-viewer
// convenience, wrapped in try/catch — storage can be absent or blocked).

import { SseDecoder, decodeLogEvent, type LogEvent } from "@/lib/log-stream";

/// Re-exported so that a caller of `streamJobLog` needs one import
/// rather than two. The parsing itself lives in `lib/log-stream` because
/// it is testable without a socket and nothing here is.
export type { LogEvent };

export interface Session {
  org: string;
  /** Empty when the browser holds an HttpOnly session cookie instead. */
  token: string;
}

export interface OrgMembership {
  id: string;
  name: string;
  role: Role;
}

export type Role = "owner" | "admin" | "member" | "viewer";

export interface Me {
  id: string;
  email: string;
  name: string;
  created_at: number;
  /// The account's handle, which is also the name of its personal
  /// namespace. Person-shaped routes are addressed by it, so a client
  /// that does not know its own handle cannot ask about itself.
  /// `null` for an account that has none.
  handle: string | null;
  orgs: OrgMembership[];
}

/// What last touched an entry, when the listing was asked for history.
/// Absent when nothing in the walked window touched it — which is a real
/// answer, and better than a wrong one.
export interface LastCommit {
  commit: string;
  message: string;
  /// The raw git identity line, the same shape `/log` returns, parsed by
  /// `parseIdent` rather than by a second parser here.
  author: string;
}

export interface TreeEntry {
  name: string;
  mode: string;
  kind: "tree" | "blob";
  oid: string;
  /// Bytes, for blobs. `null` for directories: the size of a listing is
  /// not what anybody reading that column would assume.
  size: number | null;
  /// Present only when the listing was asked for history.
  last_commit?: LastCommit | null;
}

export interface Tree {
  commit: string;
  entries: TreeEntry[];
  /// Present only when history was asked for: `true` when the walk hit
  /// its time budget before every entry was attributed, so a `null`
  /// `last_commit` may mean "not reached" rather than "never touched".
  history_truncated?: boolean;
}

/// Every path in a repository at one commit, flat, for the tree beside a
/// file. Directories end in `/`; the server walks in tree order so a
/// directory precedes what it holds. Bounded: past the server's cap the
/// list stops and `truncated` says so.
export interface TreePaths {
  commit: string;
  paths: string[];
  truncated: boolean;
}

export interface FileContent {
  /// Decoded when the server said it was text; empty when it did not.
  text: string;
  binary: boolean;
  contentType: string;
  commit: string;
  size: number;
}

export interface RefName {
  name: string;
  full: string;
  oid: string;
  default: boolean;
}

/// One commit, in the shape `/log` actually returns.
///
/// The field is `commit`, not `oid`, and `author` is a full git identity
/// line — `Name <email> <unix-seconds> <offset>` — not a name and a
/// timestamp. Getting this wrong crashed the commit log, and every test
/// agreed with the mistake because the mock had been written from the
/// same assumption as the client. The server is the contract.
export interface LogEntry {
  commit: string;
  message: string;
  author: string;
  committer: string;
  parents: string[];
  tree: string;
  /// Only present on a path-filtered log: what this commit did to that
  /// path. An unfiltered walk says nothing about any particular file.
  change?: "added" | "modified" | "deleted";
}

/// Pull a display name and a time out of a git identity line.
///
/// Tolerant on purpose: a malformed ident should cost a missing
/// timestamp, not a blank page.
export function parseIdent(ident: string): { name: string; time: number } {
  const stamp = /<[^>]*>\s*(\d+)/.exec(ident);
  const before = ident.split("<")[0].trim();
  // No name before the angle brackets: show the address, the way a git
  // client does. Showing `<only@address>` brackets and all, or nothing at
  // all, are both worse than the address itself.
  const inside = /<([^>]*)>/.exec(ident)?.[1]?.trim();
  return {
    name: before || inside || ident.trim(),
    time: stamp ? Number(stamp[1]) * 1000 : 0,
  };
}

export interface Member {
  user_id: string;
  email: string;
  name: string;
  role: Role;
  disabled: boolean;
  created_at: number;
}

export interface Invite {
  id: string;
  email: string;
  role: Role;
  created_at: number;
  expires_at: number;
  accepted_at: number | null;
}

export interface AuditEntry {
  seq: number;
  at: number;
  org_id: string;
  repo_id: string | null;
  principal: string;
  user_id: string | null;
  user_email: string | null;
  user_name: string | null;
  action: string;
  context: unknown;
}

export interface Team {
  id: string;
  name: string;
  description: string | null;
  created_at: number;
  member_count: number;
}

export interface TeamMember {
  user_id: string;
  email: string;
  name: string;
  created_at: number;
}

/// One person's access to a repo, and which of the three rules produced
/// it. `source` is the answer to "why can Alice write here?".
export interface AccessRow {
  user_id: string;
  email: string;
  name: string;
  role: Role;
  source: "org_role" | "direct_grant" | "team";
  team_id: string | null;
  team_name: string | null;
}

export interface TeamAccessRow {
  team_id: string;
  team_name: string;
  role: Role;
  member_count: number;
}

export interface Token {
  id: string;
  label: string | null;
  scopes: string[];
  repo_id: string | null;
  user_id: string | null;
  created_at: number;
  revoked_at: number | null;
  /** When it stops working by itself; `null` for never. Distinct from
   *  `revoked_at`: nobody decided this, a deadline passed. An expired
   *  token is still listed, because expiry is enforced by the
   *  authentication path rather than by a sweep — so the row being there
   *  says nothing about whether the credential works. */
  expires_at: number | null;
}

/// One language and how many bytes of it the server counted.
///
/// Bytes rather than a percentage, deliberately: the share depends on
/// how many languages the bar names before it aggregates, which is a
/// property of the palette (three series colours) and not of the
/// repository. Sending a percentage would bake a rendering decision
/// into the wire.
export interface Language {
  name: string;
  bytes: number;
}

/// The licence, or as much of it as can be said honestly.
///
/// `recognised: false` with a null `spdx` is a real answer and the
/// common one: the file is there, and the fingerprint table is not
/// certain what it is. The whole object is null only when the
/// repository has no licence file at all.
///
/// Several files is its own answer, not silence. Naming one of them
/// would be the most misleading thing this panel could say, and saying
/// nothing makes a dual-licensed project read as unlicensed — so the
/// list is returned and the reader decides.
export interface License {
  /// The one licence file, when there is exactly one. `null` when the
  /// project carries several — there is no single file to link to, and
  /// `files` is what a reader needs then.
  path: string | null;
  spdx: string | null;
  name: string | null;
  recognised: boolean;
  /// Every licence file found; always at least one when this object
  /// exists. A dual-licensed project (`LICENSE-APACHE` beside
  /// `LICENSE-MIT`, the convention across the Rust ecosystem) lists
  /// both rather than being reported as having none.
  files: string[];
}

/// A community health file the server found in the tree.
export interface CommunityFile {
  kind: "contributing" | "code_of_conduct" | "security";
  path: string;
}

/// What a repository says about itself, beside its files.
export interface RepoMeta {
  topics: string[];
  /// Biggest first. Empty for a repository with no recognised source.
  languages: Language[];
  /// True when the tree walk hit its bound, so the byte counts are of
  /// what was measured rather than of the whole tree. The panel says so
  /// rather than presenting a partial count as a complete one.
  languages_truncated: boolean;
  license: License | null;
  community: CommunityFile[];
  /// The path of this project's README, or `null` when it has none.
  ///
  /// A scalar rather than a fourth `community` kind, because the client
  /// does something different with it: the other three are linked to
  /// and this one is fetched and rendered. `null` rather than `""` is
  /// load-bearing — the About rail draws all six health rows
  /// present-or-absent, so absence is a fact it renders rather than a
  /// file named nothing.
  readme: string | null;
}

/// Where a check run got to.
///
/// Six states and no seventh. `queued` is also what an unrecognised
/// provider verdict degrades to — GitHub adds conclusions, and one we
/// have never heard of must never read as green.
export type RunState =
  "queued" | "running" | "passing" | "failing" | "cancelled" | "skipped";

/// One CI run, as reported by whatever actually ran it.
///
/// Rows arrive from three places now and the page deliberately branches
/// on none of them: a GitHub App installation we poll, a third party
/// posting to the signed intake endpoint, and **Weft's own runners**,
/// which execute a repository's `.weft/*.yml` workflows and mirror
/// each job here as a check row. Which one is `provider`; the day there
/// is a fourth, the UI must not need a fourth arm.
///
/// `detail_url` is where the log actually lives, and it is no longer
/// always somebody else's site: a workflow run's row points at our own
/// [`WorkflowRun`] page, and a third party's points out to theirs.
/// `DetailLink` is what tells those apart, because the first must
/// navigate in-app and the second must not.
export interface CheckRun {
  id: string;
  repo_id: string;
  commit_sha: string;
  ref_name: string | null;
  provider: string;
  external_id: string | null;
  /// The workflow's name — what a reader recognises, and what the left
  /// rail lists.
  name: string;
  run_number: number | null;
  event: string | null;
  state: RunState;
  detail_url: string | null;
  actor: string | null;
  started_at: number | null;
  completed_at: number | null;
  created_at: number;
  updated_at: number;
}

/// Filters for the runs list. Every one of them is also a query-string
/// parameter, so a filtered list is a link somebody can send.
export interface RunQuery {
  branch?: string;
  state?: RunState;
  event?: string;
  actor?: string;
  workflow?: string;
  limit?: number;
  /// The cursor: runs strictly older than this `created_at`.
  before?: number;
}

/// One outbound push subscription. The secret is absent on a read: it
/// is returned once, when the subscription is made, and never again.
export interface Webhook {
  id: string;
  org_id: string;
  repo_id: string;
  url: string;
  created_at: number;
}

/// How the last GitHub Actions poll went, and — the field that earns
/// this type — whether we were allowed to look.
///
/// An App installation without `actions: read` fails in the single way
/// that renders identically to success: an empty Checks tab. A page
/// that cannot tell those apart tells a maintainer their CI is not set
/// up when the truth is that we may not read it. `denied` is that
/// distinction. `connected` is the third state and neither of the other
/// two — there is no GitHub origin here to poll at all, which is the
/// ordinary case for a native repository posting to the intake.
export interface ChecksPoll {
  provider: string;
  connected: boolean;
  polled: boolean;
  denied: boolean;
  error: string | null;
  high_water: number | null;
  resuming_from: string | null;
  retry_in_ms: number | null;
}

/// Where a workflow run got to.
///
/// A different vocabulary from [`RunState`], and deliberately not
/// reconciled at the type level: these are our own words for our own
/// runner, and `CheckRun.state` is the six-word language every provider's
/// verdict is translated into. A workflow run also produces a check row —
/// that translation is the server's — so the two exist side by side and
/// a client that assumed one type covered both would read `passed` as
/// an unknown state and draw it grey.
///
/// `blocked` is the one that is not about execution: the run exists and
/// may not start yet, which is what a fork's change looks like while it
/// waits for a maintainer to approve running it.
export type WorkflowRunState =
  "running" | "passed" | "failed" | "cancelled" | "blocked";

/// Where one job of a workflow run got to. The same words as a run, plus
/// `queued` — a run is `running` from the moment it exists, but a job
/// spends real time waiting for a runner and a reader is owed the
/// difference.
export type WorkflowJobState = WorkflowRunState | "queued";

export interface WorkflowJob {
  /// The row id, and the id the log routes are addressed by. **Not**
  /// `job_id`, which is the name the workflow file gave it.
  id: string;
  /// The identifier in the `.weft/*.yml` file — `build`, `test`.
  job_id: string;
  /// What to call this job on screen: the `job_id` with its matrix
  /// coordinates folded in, so two legs of one matrix are distinguishable.
  key: string;
  /// The matrix coordinates, already an object — the server parses it so
  /// that no client has to `JSON.parse` a field out of a JSON document.
  matrix: Record<string, unknown>;
  /// A `WorkflowJobState`, typed as `string` for the same reason
  /// `classifyCheck` takes one: a server newer than this bundle may
  /// write a word we have never heard of, and a union that cannot
  /// express one makes the code that handles it untestable.
  state: string;
  attempts: number;
  /// Why this job ended the way it did, in the runner's own words.
  error: string | null;
  detail_url: string | null;
  /// The highest log chunk uploaded for the current attempt.
  log_chunks: number;
  started_at: number | null;
  completed_at: number | null;
  /// The `runs-on` list, in file order — which is the order the
  /// server's own refusal sentence quotes them in, so it is the order
  /// they are shown in. Optional because a fixture or an older server
  /// does not send it.
  labels?: string[];
  /// The runner that actually took this job, or `null` for one nothing
  /// picked up yet.
  runner?: { id: string; name: string } | null;
}

/// The org-wide answer to "where may jobs run".
export interface RunnerPolicy {
  self_hosted: "all" | "selected" | "disabled";
  /// Repository **names**, not ids — the same currency every other
  /// repo-shaped route on this client speaks.
  self_hosted_repos: string[];
}

/// A pool of runners, and which repositories may reach it.
export interface RunnerGroup {
  id: string;
  name: string;
  repo_access: "all" | "selected";
  /// The default group cannot be renamed or deleted; every removed
  /// group's runners land in it.
  is_default: boolean;
  repos: string[];
  /// How many live runners are in it, which is what makes deleting one
  /// a decision rather than a click.
  runners: number;
  created_at: number;
  updated_at: number;
}

/// One registered machine.
export interface Runner {
  id: string;
  name: string;
  /// `self-hosted`, the OS, the arch and whatever the runner offered.
  /// Rendered through `orderedLabels` so the chips do not reshuffle.
  labels: string[];
  os: string;
  arch: string;
  version: string;
  ephemeral: boolean;
  group: { id: string; name: string };
  /// `online` | `busy` | `offline`, derived by the server from the last
  /// heartbeat. Typed as `string` for the reason `WorkflowJob.state` is:
  /// a server newer than this bundle may say a word we have never heard
  /// of, and a union that cannot express one makes the code that
  /// handles it untestable.
  state: string;
  last_seen_at: number;
  created_at: number;
  /// What it is running, when it is busy.
  job: {
    run_id: string;
    job_id: string;
    key: string;
    /// The repository the run belongs to. **Optional**: the contract as
    /// written does not include it, and without it the job can only be
    /// named, never linked — see `runnerJobHref`.
    repo?: string;
  } | null;
}

/// A single-use credential an operator pastes into a terminal.
export interface RunnerRegistrationToken {
  token: string;
  expires_at: number;
  group: string;
  /// The server's own composed command. Read for nothing: the panel
  /// builds the command from this browser's origin instead — see
  /// `registrationCommands` for why.
  command: string;
}

/// One run of a `.weft/*.yml` workflow, on the organization's runners.
///
/// `error` is the field this type exists for. A workflow file we refused
/// to run — bad YAML, an unknown key, a dependency cycle — produces a
/// run that failed before any job existed, and the reason lives here and
/// nowhere else. Rendering it verbatim is the difference between "your
/// build failed" and a sentence somebody can act on.
export interface WorkflowRun {
  id: string;
  /// The workflow file's path, e.g. `.weft/ci.yml`.
  file: string;
  /// The workflow's `name:`, or the file's if it has none.
  name: string;
  commit_sha: string;
  ref_name: string | null;
  /// What caused it — `push`, `change`, and so on.
  event: string;
  /// The change this run belongs to, when it was a change that caused
  /// it. `null` for a plain push.
  change_key: string | null;
  /// For a composed run — `event: "changeset"` — the changeset it was
  /// started for, by the key a person types, and which combination of
  /// member tips it built (a hash; see `Changeset.composition`). `null`
  /// on a push or change run, and **absent** from a server older than
  /// the fields. `commit_sha` is one member's; the build saw all of
  /// them, and this is the only thing on the run that says so.
  changeset?: { key: string } | null;
  composition?: string | null;
  /// A `WorkflowRunState`; `string` for the reason `WorkflowJob.state`
  /// gives.
  state: string;
  error: string | null;
  /// Why a `blocked` run is blocked, as a code rather than as prose —
  /// `null` when it is not blocked, and **absent** from a server older
  /// than the field. `"fork"` is the one code waiting on a person who is
  /// looking at it, so it is the only one answered with a button; any
  /// other code is shown by its sentence and nothing more. Typed as a
  /// bare string for the reason `state` is. Never decided by matching
  /// `error`, which is written to be read and will be reworded.
  blocked_reason?: string | null;
  created_at: number;
  updated_at: number;
  completed_at: number | null;
  jobs: WorkflowJob[];
}

/// One person's share of a repository, biggest first.
export interface Contributor {
  user_id: string;
  handle: string;
  commits: number;
  /// Epoch ms of their most recent contributing day here.
  last_at: number;
}

/// One repository forked directly from this one.
export interface ForkEntry {
  org: string;
  name: string;
}

export interface Repo {
  id: string;
  org_id: string;
  name: string;
  description: string | null;
  /// The project's own address on the web, or `null`.
  ///
  /// Always absolute http(s) — the server refuses anything else, and
  /// refuses a scheme-less host rather than repairing it, so the client
  /// never has to guard this string before it becomes an `href`.
  /// Optional on the type for the same reason as the counts below: a
  /// fixture written before the field existed must render as absent
  /// rather than as `undefined`.
  homepage?: string | null;
  kind: "native" | "mirror";
  default_branch: string;
  origin_url: string | null;
  /// The GitHub App installation this mirror was connected through.
  ///
  /// `null` for a mirror connected by bare URL, which can still sync
  /// commits but cannot import issues — reading issues needs the App.
  /// The distinction is why a control gated on `kind === "mirror"`
  /// alone offers an import the server will refuse.
  origin_installation: string | null;
  last_sync_at: number | null;
  last_synced_commit: string | null;
  sync_error: string | null;
  created_at: number;
  clone_url: string;
  /** Null unless the deployment exposes an SSH endpoint. */
  ssh_clone_url: string | null;
  /// The namespace this repository is in, by name — so an action that
  /// creates one somewhere can say where it went.
  org?: string;
  /// `pending` while the fork is being prepared, `ready` once it can be
  /// cloned. Absent when the repository is not a fork.
  fork_state?: string | null;
  /// `owner/name` of what this was forked from, when the viewer may see
  /// it. Absent for a repository that is not a fork, one whose upstream
  /// is gone, and one whose upstream is private — three different facts
  /// the server deliberately reports the same way.
  fork_parent?: string | null;
  /// How many commits reach the default branch's tip — every parent of
  /// every commit, as GitHub counts — as of the last time the job that
  /// follows a write counted them. `null` until it has. `commits_tip` is
  /// the commit the number was true for: the page shows the number only
  /// while the tip it is looking at agrees, so a count a push behind is
  /// never printed as current. `commits_exact` is `false` when the walk
  /// stopped at its cap, and the number is a floor.
  ///
  /// Optional for the reason the counts below are: a fixture written
  /// before the field existed must render as absent, not as `undefined`.
  commits?: number | null;
  commits_tip?: string | null;
  commits_exact?: boolean;
  fork_count?: number;
  /// People subscribed to everything that happens here.
  ///
  /// On the repository row rather than on `…/watch`, because that
  /// endpoint answers "what did *you* choose" and not "how many". The
  /// masthead needs the number with the row, or the control draws with
  /// no count and then grows one.
  ///
  /// Optional for the same reason `fork_count` is: an older server, or
  /// a fixture written before this field existed, must render a zero
  /// rather than `undefined`.
  watcher_count?: number;
  /// Whether the caller may administer this repository — change its
  /// visibility, its default branch, its protections, who may reach it.
  ///
  /// Always sent, `false` rather than absent when the answer is no, so
  /// there is nothing here to distinguish "not admin" from "not told".
  /// It is the server's own answer, from the same `authx` refinement the
  /// writes use, which is why no surface has to infer authority from
  /// whether some admin-only endpoint happened to reply.
  viewer_admin: boolean;
  /// Whether the caller holds a role *here* — an org membership or a
  /// per-repo grant.
  ///
  /// Every other `viewer_*` answer is about what somebody may *do*; this
  /// one is about whether they are on the inside at all, and Insights is
  /// gated on it. It is the server's answer rather than an inference
  /// from the row having arrived, so the gate cannot drift from the rule
  /// the server applies.
  ///
  /// Weaker than `viewer_write` — a `viewer`-role member may not push
  /// and is exactly who this includes. Optional on the type for the
  /// same reason the counts are: a fixture written before the field
  /// existed must read as "no" rather than as `undefined`.
  viewer_member?: boolean;
  /// Whether the caller may push here, and so whether they may open a
  /// change whose commits are already in this repository.
  ///
  /// Not the same question as `viewer_admin` — a member writes without
  /// administering — and it is the one the Changes tab needs, because
  /// write is the scope the server checks before refusing a change with
  /// no `source`. Optional on the type for the same reason the counts
  /// are: a fixture written before the field existed must read as "no"
  /// rather than as `undefined`.
  viewer_write?: boolean;
  /// For a mirror: whether a push here reaches the origin, and if not
  /// why. `null` for a native repository; absent from a server older
  /// than write-through mirrors, which the page must not read as
  /// "forwarding".
  push?: MirrorPush | null;
  /// Bytes this repository holds in storage. `null` when the server has
  /// not measured it yet; absent from a server older than the field.
  /// Either way the page shows nothing rather than "0 GB".
  stored_bytes?: number | null;
}

/// One search result. Search crosses namespaces, so every row carries
/// the namespace: a bare repo name is ambiguous and cannot be clicked.
export interface RepoHit {
  id: string;
  org_id: string;
  org: string;
  name: string;
  description: string | null;
  kind: "native" | "mirror";
  created_at: number;
}

/// What the origin probe found. `private` is not a failure: it is the
/// answer that leads to connecting GitHub.
export interface Probe {
  reachable: boolean;
  private: boolean;
  default_branch: string | null;
  refs: number;
  reason: string | null;
}

export interface SyncStatus {
  state: "syncing" | "ready" | "failed";
  origin: string | null;
  provider: string | null;
  last_sync_at: number | null;
  commit: string | null;
  error: string | null;
  clone_url: string;
  /// Whether a push to the mirror reaches its origin; see `MirrorPush`.
  push?: MirrorPush | null;
}

/// Whether a push to a mirror is forwarded to its origin, as the server
/// says it. `blocked` is the sentence a push would be refused with;
/// `needs_permission` with `approve_url` is the one case a person fixes
/// on GitHub — an installation that predates `Contents: write`.
export interface MirrorPush {
  forwarding: boolean;
  blocked: string | null;
  needs_permission: boolean;
  approve_url: string | null;
}

export interface PendingInstall {
  installation_id: string;
  account: string | null;
  expires_at: number;
}

export interface GithubInstallation {
  installation_id: string;
  provider: string;
  account: string | null;
  created_at: number;
  /// What GitHub says about the installation right now, asked live
  /// when the list is read. Three shapes and a fourth absence, each a
  /// different fact: the permissions it holds; `{ gone: true }` when
  /// the App has been uninstalled there; `null` when GitHub could not
  /// be asked at all; and **absent** from an older server, which the
  /// page treats like `null`.
  detail?: GithubInstallationDetail | { gone: true } | null;
}

/// What an installation may do, as GitHub reports it.
export interface GithubInstallationDetail {
  account: string;
  target_type: "Organization" | "User";
  /// `Contents: write` — what forwarding a push to the origin needs.
  /// Absent from a server older than write-through mirrors.
  contents_write?: boolean;
  /// The server's reading of `contents_write`, sent so the picker and
  /// the forward agree.
  push_ready?: boolean;
  /// Where on GitHub the missing permissions are approved.
  approve_url: string;
  suspended: boolean;
}

export interface RemoteRepo {
  full_name: string;
  private: boolean;
  default_branch: string | null;
  description: string | null;
  size: number | null;
}

export interface SshKey {
  id: string;
  token_id: string | null;
  user_id: string | null;
  algo: string;
  public_key: string;
  fingerprint_sha256: string;
  label: string | null;
  created_at: number;
  revoked_at: number | null;
}

export interface KindSummary {
  count: number;
  bytes: number;
  ms_sum: number;
  p50_ms: number | null;
  p99_ms: number | null;
}

export interface RepoMetrics {
  repo: string;
  from: number;
  to: number;
  kinds: Record<string, KindSummary>;
  sync: { last_sync_at: number | null; sync_error: string | null };
}

export interface UsageDay {
  day: string;
  active_repos: number;
  total_repos: number;
  requests: number;
  bytes_out: number;
}

export interface Usage {
  days: UsageDay[];
}

/// One commit's review identity. `patchset` is the latest revision.
export interface Change {
  key: string;
  title: string;
  target_branch: string;
  /// `owner/name` of the fork these commits live in, or null when they
  /// are already in the target repository. A reviewer reading a change
  /// from a fork is reading code that arrived from outside, which is
  /// the first thing they need to know about it.
  source?: string | null;
  state: "open" | "landing" | "landed" | "abandoned";
  land_verdict: string | null;
  landed_commit: string | null;
  created_at: number;
  updated_at: number;
  patchset: Patchset | null;
  /// The key of the open changeset this change is a member of, or null.
  /// A member lands and closes only through its changeset; the change
  /// view turns its own land button off and points there. Optional
  /// because older servers did not send it — absent reads as free.
  changeset?: string | null;
}

export interface Patchset {
  number: number;
  commit: string;
  parent: string | null;
  message: string;
  created_at: number;
}

export interface ApprovalRow {
  email: string;
  name: string;
  patchset_id: string;
  created_at: number;
}

/// What one review said. Three words, and no fourth: a reviewer either
/// signs the code off, says something about it, or asks for changes.
export type ReviewVerdictKind = "approve" | "comment" | "request_changes";

/// One recorded review — a whole pass over a change, said once.
///
/// A review is not a comment: it is the act that publishes a batch of
/// comments, records a verdict about the patchset, moves the approval to
/// match and sends exactly one notification. `body` is the cover
/// message, which may be null when the comments said everything.
export interface Review {
  id: string;
  verdict: ReviewVerdictKind;
  body: string | null;
  /** The author's display name; the server sends "somebody" for an
   *  account it can no longer resolve rather than a blank. */
  author: string;
  author_email: string | null;
  user_id: string;
  /** The patchset the verdict was about — a verdict describes the code
   *  it was about, exactly as an approval does. */
  patchset_id: string;
  /** `draft` until submitted, and never back again. */
  state: string;
  submitted_at: number | null;
  /** When a standing `request_changes` was taken back by its author. */
  withdrawn_at: number | null;
  created_at: number;
}

/// A standing `request_changes`, and the server's judgement of whether
/// it actually stops the change landing.
///
/// `blocking` is deliberately **not** the same question as "is the
/// verdict `request_changes`". Here the reviewer set is computed from
/// OWNERS, so the server can answer honestly whether this person's "no"
/// carries weight over anything the patchset touches — and one from
/// somebody with no standing is recorded, rendered, and advisory. A
/// client that read the verdict alone would tell an author their change
/// is stuck when it is not, which is the failure GitHub ships by
/// design: there, any passer-by can wedge a pull request.
export interface StandingBlock extends Review {
  blocking: boolean;
}

export interface ChangeDetail {
  change: Change;
  patchsets: Patchset[];
  /** Approvals on the latest patchset only — older ones don't count. */
  approvals: ApprovalRow[];
  /// Every submitted review, oldest first. **Absent** — not empty —
  /// from a server older than migration 0051, which is the whole
  /// capability signal the batched-review surface is gated on: see
  /// `batchedReviewSupported` in `views/change/review.ts`. A modern
  /// server with no reviews yet still sends the key, so absent and
  /// empty stay two different claims.
  reviews?: Review[];
}

export interface PathVerdict {
  path: string;
  satisfied: boolean;
  owners: string[];
  explanation: string;
}

export interface Verdict {
  landable: boolean;
  explanation: string;
  per_path: PathVerdict[];
}

/// One person the repository's OWNERS file requires for this change.
///
/// Not a nomination and not a request: nobody asked this person, the
/// file did. `approved` is about *this* patchset only — a new patchset
/// starts the count over — so a row is a live answer to "is this change
/// still waiting on them", never a record of what they once thought.
export interface RequiredReviewer {
  user_id: string;
  name: string;
  email: string;
  approved: boolean;
}

/// Who the change is waiting on, computed rather than nominated.
export interface ReviewerSet {
  required: RequiredReviewer[];
  /// At least one changed path is governed by a `*` entry: anyone with
  /// write access may approve it, so nobody in particular is required.
  ///
  /// Read this before concluding anything from an empty `required`.
  /// "Nothing here is owned" and "anyone with write may sign this off"
  /// produce the same empty list and are different sentences to a
  /// reader deciding whether they can help.
  anyone_with_write: boolean;
}

export interface ChangeVerdict {
  change: string;
  state: Change["state"];
  patchset: number;
  commit: string;
  verdict: Verdict;
  /// Absent from an older server, which is why every reader goes
  /// through `requiredReviewers` below rather than touching this
  /// directly: a page that assumed the field would throw on the first
  /// deployment that had not caught up.
  reviewers?: ReviewerSet;
  /// The standing "no"s, riding with the verdict rather than on a route
  /// of their own: the explanation says a review blocks, and this is
  /// which one, said by whom, in what words. Absent from a server older
  /// than migration 0051.
  blocks?: StandingBlock[];
}

/// The reviewer set as the page should read it, with an older server's
/// silence rendered as "nothing to say" rather than as "nobody is
/// required" — the two look identical in the data and are opposite
/// claims to a person deciding whether they may approve.
export function requiredReviewers(v: ChangeVerdict): ReviewerSet | null {
  return v.reviewers ?? null;
}

/// One change as the org-wide list returns it: the change itself, the
/// repository it belongs to — the per-repo list never needed to say —
/// and the changeset already holding it, or null. The picker reads the
/// last field to grey a row out rather than let somebody build a
/// changeset the server will refuse member by member.
export interface OrgChange extends Change {
  repo: string;
  changeset: string | null;
  /// Whether the caller may push to `repo` — the repository row's own
  /// `viewer_write`, carried per row so the picker can grey out a change
  /// the caller could tick but never compose or land.
  viewer_write?: boolean;
}

export type ChangesetState =
  "open" | "landing" | "landed" | "abandoned" | "failed";

/// A member named, not carried: `repo` and the change's key.
export interface ChangesetRef {
  repo: string;
  change: string;
}

export interface ChangesetMember {
  repo: string;
  change: Change;
}

/// One member's step of a landing, in landing order. `note` is the
/// server's own sentence — why a step failed, which revert commit put a
/// landed one back — and is rendered verbatim.
export interface ChangesetLandingStep extends ChangesetRef {
  ref: string;
  old: string | null;
  new: string;
  state: "pending" | "done" | "failed" | "reverted";
  note: string | null;
}

export interface ChangesetLanding {
  id: string;
  attempt: number;
  started_at: number;
  finished_at: number | null;
  /// `null` while landing, then which way it went.
  outcome: "landed" | "failed" | null;
  members: ChangesetLandingStep[];
}

/// A composed CI verdict: one job of a member's `on: changeset`
/// workflow, run against every member at once. Named by repo because
/// the gate's sentence names the repo and the two must agree.
export interface ChangesetCheck {
  repo: string;
  name: string;
  state: string;
  detail_url: string | null;
  run: string;
}

export interface Changeset {
  key: string;
  title: string;
  body: string | null;
  state: ChangesetState;
  created_at: number;
  updated_at: number;
  /// Key of the changeset this one was made to revert, or null.
  reverts: string | null;
  /// Keys of the changesets made to revert this one, oldest first.
  reverted_by: string[];
  members: ChangesetMember[];
  edges: { from: ChangesetRef | null; to: ChangesetRef | null }[];
  /// Landing order: dependencies first.
  order: ChangesetRef[];
  /// Hash naming this set of members at these commits; null when a
  /// member has no patchset yet or none is left.
  composition: string | null;
  checks: ChangesetCheck[];
  landing: ChangesetLanding | null;
  /// Whether the caller may land, revert, abandon or edit this set:
  /// write on every member repository. The server's own answer, asked
  /// for the same reason `RepoView.viewer_write` is — the page used to
  /// offer a viewer "Revert…" and answer the press with `no changeset`.
  viewer_write?: boolean;
}

export type ChangesetGate = "ready" | "waiting" | "blocked";

export interface ChangesetMemberVerdict extends ChangesetRef {
  state: Change["state"];
  patchset: number;
  commit: string;
  verdict: Verdict;
  approvals: { email: string; name: string; created_at: number }[];
  landable: boolean;
  explanation: string;
  gate: ChangesetGate;
  waiting_on: string[];
  reason: string | null;
}

export interface ChangesetVerdict {
  changeset: string;
  state: ChangesetState;
  landable: boolean;
  gate: ChangesetGate;
  explanation: string;
  waiting_on: string[];
  members: ChangesetMemberVerdict[];
}

/// The read-only repository served for a changeset: one commit whose
/// tree is a submodule per member, so one recursive clone checks the
/// whole proposal out.
export interface DiffstatTotals {
  files: number;
  insertions: number;
  deletions: number;
  truncated: boolean;
}

export interface ChangesetDiffstat {
  changeset: string;
  members: (DiffstatTotals & {
    repo: string;
    change: string;
    patchset: number;
  })[];
  total: DiffstatTotals;
}

export interface ChangesetWorkspace {
  key: string;
  title: string;
  state: ChangesetState;
  composition: string | null;
  tip: string | null;
  clone_url: string;
  ssh_clone_url: string | null;
  members: {
    repo: string;
    change: string;
    title: string;
    path: string;
    commit: string;
    fetch_ref: string;
    clone_url: string;
    ssh_clone_url: string | null;
  }[];
  note: string | null;
}

/// Why one member of a landed changeset could not be reverted cleanly.
/// `why` is the server's sentence; the flags say which kind it was.
export interface RevertConflict extends ChangesetRef {
  why: string;
  changed?: string[];
  no_branch?: boolean;
  branch_exists?: boolean;
}

export interface ChangeComment {
  id: string;
  patchset: number;
  author: string;
  author_email: string | null;
  author_principal: string;
  path: string | null;
  /** With path: the 1-based line the comment anchors to. */
  line: number | null;
  body: string;
  created_at: number;

  // Threading, resolution and anchoring — migration 0050. Every one of
  // these is optional, and that is load-bearing rather than cautious: a
  // server older than 0050 answers without them, and *absent* must not
  // collapse into *empty* the way it nearly did for the reviewer set.
  // A comment with no `parent_id` is a thread root; a comment from a
  // server that has never heard of `parent_id` is also a thread root,
  // and both render as a flat conversation with no resolution controls
  // rather than as `undefined` on the page.

  /** The root of this thread when this row is a reply; null on a root. */
  parent_id?: string | null;
  /**
   * The server's own grouping key — the root's id, which for a root is
   * its own. Preferred over recomputing it so that the client and the
   * server cannot disagree about what one thread is.
   */
  thread_id?: string;
  /**
   * Which half of the diff `line` counts in. `"old"` anchors to a line
   * the patchset deleted, where the same number is a different line —
   * which is why the server refuses to coerce an unknown side to `new`.
   */
  side?: CommentSide | null;
  /** Inclusive last line of a multi-line anchor. Null for one line. */
  line_end?: number | null;
  /**
   * The anchor as first written, kept beside the live one so a comment
   * on a since-rewritten file can be placed rather than guessed at.
   */
  original_line?: number | null;
  original_patchset?: number | null;
  /** Root rows only: a thread resolves, a sentence inside one does not. */
  resolved?: boolean;
  resolved_at?: number | null;
  /** Who called it settled, as a display name. Null while open. */
  resolved_by?: string | null;

  // The batched review — migration 0051. Optional for the same reason
  // the threading fields above are: a server older than 0051 sends none
  // of them, and a page that read `pending` as false there would offer
  // to "finish a review" through routes that answer 404.

  /**
   * Still only the caller's own. Nobody else can see a row with this
   * true — not the change's author, not an admin, not an anonymous
   * reader — so a client that renders the flag is telling its own user
   * "this is not sent yet", which is the one thing a draft surface must
   * never get wrong.
   */
  pending?: boolean;
  /** The pending review this comment was drafted into, or null. */
  review_id?: string | null;
  /** When it became visible to everybody. Null while it is a draft. */
  published_at?: number | null;
}

/// Which half of the diff an anchor counts in.
export type CommentSide = "new" | "old";

/// The threading and anchoring a new comment may carry.
///
/// A **reply sets none of the anchor fields**: the server gives it its
/// root's path, line, range and side whole and refuses one that sends
/// its own, because a reply that could re-anchor is how a thread ends up
/// describing two places at once.
export interface NewCommentOptions {
  parent_id?: string;
  side?: CommentSide;
  /** Inclusive last line of a range. Needs `line`. */
  line_end?: number;
  /**
   * Draft this into the caller's pending review instead of publishing
   * it. Nobody else sees it and nobody is mailed about it until the
   * review is submitted.
   *
   * The client never has to open a review first: the server opens one
   * on the caller's behalf, because a draft that failed over a setup
   * call the client forgot would be a lost remark.
   */
  pending?: boolean;
}

/// How a reader should weigh an opinion: where its author stands with
/// this repository. Derived by the server on every read from membership
/// and from what this person has landed here — never stored, because a
/// stored copy goes stale the moment somebody is promoted or leaves.
export type AuthorAssociation =
  "owner" | "member" | "contributor" | "first-time";

/// Associations for one change, keyed by the same `author_principal`
/// string comments carry, so the client joins on what it already has.
/// A service principal is simply absent: a build robot has no standing.
export interface ChangeAssociations {
  /** The change's own author, when there is one. */
  author: AuthorAssociation | null;
  /** Which principal that is, so the author's own comments are marked. */
  author_principal: string | null;
  authors: Record<string, AuthorAssociation>;
}

/// The caller's own viewed paths, and the patchset they apply to. Marks
/// made against an older patchset are already filtered by the server:
/// what comes back is what is still true of the code that is there now.
export interface ChangeViews {
  patchset: number | null;
  viewed: string[];
}

export interface BranchProtection {
  branch: string;
  created_at: number;
}

/// One check the repo requires on a branch before a change may land.
///
/// The *name* is the whole of it — matched against `check_runs.name` and
/// `change_checks.name`, which are the same namespace by design. Nothing
/// here says the check exists: requiring a name nothing reports is legal
/// and is how a maintainer prepares for a pipeline that is not wired up
/// yet, and also how a typo holds every change on the branch until the
/// queue's wait budget runs out.
export interface RequiredCheck {
  name: string;
  created_at: number;
}

/// How much of a repository's activity somebody wants to hear about.
///
/// `participating` is everybody's default and the server returns it when
/// no explicit choice has been recorded, so there is no fourth "unset"
/// state to render — a person who has never touched the control and a
/// person who deliberately chose the default look the same, because they
/// want the same thing.
export interface ProfileLink {
  label: string | null;
  url: string;
}

/// What a namespace says about itself. Exactly what `profile_json()`
/// publishes — no email.
export interface Profile {
  handle: string;
  name: string;
  display_name: string | null;
  bio: string | null;
  location: string | null;
  company: string | null;
  pronouns: string | null;
  kind: string;
  profile_repo: string | null;
  created_at: number;
  links: ProfileLink[];
}

/// Where an issue import has got to.
///
/// Per phase rather than one percentage, because the phases are what
/// resume — a single number would be invented.
export interface ImportStatus {
  labels: string | null;
  milestones: string | null;
  issues: string | null;
  /// The job's own outcome: `queued` | `running` | `done` | `failed`, or
  /// null when no import has been asked for. The phases cannot answer
  /// this — a refused import and one still walking its first page leave
  /// identical cursors — which is why a failure used to be invisible
  /// here and the panel polled forever waiting for a `done` that was
  /// never coming.
  state: string | null;
  /// Why it failed, in the words the worker recorded. For a refusal that
  /// is a sentence naming the permission to grant, not a status code.
  error: string | null;
  updated_at: number | null;
}

/// One milestone on a repository, keeping the number it came with.
export interface Milestone {
  number: number;
  title: string;
  description: string;
  state: "open" | "closed";
  due_on: number | null;
  /// Both counts, never one fraction: an empty milestone and a finished
  /// one compute to the same number and mean opposite things.
  open_issues: number;
  closed_issues: number;
}

/// One address on an account. `verified_at` is when it was proved, and
/// `null` until it has been: anybody can put anybody's address in
/// `git config user.email`, so an unproved address claims nothing.
export interface EmailRow {
  address: string;
  verified_at: number | null;
  private: boolean;
  primary: boolean;
  created_at: number;
}

export type WatchLevel = "all" | "participating" | "ignore";

/** One CI verdict bearing on a change, from the **merged** read: rows
 *  posted against the latest patchset unioned with the runs reported
 *  against that patchset's commit. This is the same view the land gate
 *  decides on, so the panel and the blocked-land reason cannot disagree.
 */
export interface ChangeCheck {
  name: string;
  /** Free text, because two vocabularies meet here. A patchset row says
   *  `pending|passing|failing`; a commit row adds
   *  `queued|running|cancelled|skipped`. Narrowing this to the first
   *  three would have made every polled state a type error at best and a
   *  silently unrendered row at worst — see `classifyCheck`. */
  state: string;
  url: string | null;
  /** Whether the repo's policy requires this check on the target branch. */
  required: boolean;
  /** What the row is a statement about: `"patchset"` (scoped to this
   *  revision) or `"commit"` (keyed on the sha). When one name reports on
   *  both sides the patchset row wins, so this never disambiguates two
   *  rows — it tells a reader how far the verdict actually reaches. */
  source: "patchset" | "commit";
  /** Who reported it — an intake principal, or the provider for a
   *  commit-scoped run. This is the "where do I go and look" field. */
  posted_by: string;
  updated_at: number;
}

export interface DiffEntry {
  status: "added" | "modified" | "deleted";
  path: string;
  old_oid: string | null;
  new_oid: string | null;
}

const KEY = "stratum-session";

export function loadSession(): Session | null {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return null;
    const s = JSON.parse(raw);
    if (typeof s.org === "string" && typeof s.token === "string") return s;
  } catch {
    /* storage unavailable */
  }
  return null;
}

export function saveSession(s: Session | null) {
  try {
    if (s) localStorage.setItem(KEY, JSON.stringify(s));
    else localStorage.removeItem(KEY);
  } catch {
    /* storage unavailable */
  }
}

/// One label, as the server stores it.
///
/// `color` is a **token name** from `web/shared/tokens.css` — `series-1`,
/// not `#059669`. A hex in the database renders wrong in the other theme
/// and no test can see it, which is why the server validates the name.
/// The one exception is a label carried in from GitHub, whose hex the
/// project chose and which `LabelPill` mutes rather than discards.
export interface IssueLabel {
  id: string;
  name: string;
  color: string;
  description: string;
}

/// One issue. `author` is a resolved handle, not an id — a row that has
/// to look up who wrote it is a row that renders an id when the lookup
/// is missing.
///
/// `author_label` carries the name an imported issue had on GitHub when
/// that person has no account here. It is deliberately separate from
/// `author`: attributing somebody's imported issue to a local account
/// that merely shares a name is a claim we do not get to make.
export interface Issue {
  id: string;
  number: number;
  title: string;
  body: string;
  state: string;
  author: string | null;
  /// The author's user id.
  ///
  /// **Never rendered.** It exists so the server can answer "is the
  /// caller this issue's author" without keying authorization on a
  /// display name, which is mutable — a handle change must not hand
  /// somebody else the right to close an issue, or take it away from
  /// its author.
  ///
  /// **Never rendered**, and that still holds: putting an id in front of
  /// a person is how a byline turns into `01j7f2…`. It is compared, not
  /// shown — the issue page needs it to decide whether to draw the edit
  /// and close controls at all, since the server allows both to the
  /// author as well as to a writer.
  author_id: string | null;
  author_label: string | null;
  created_at: number;
  updated_at: number;
  closed_at: number | null;
  labels: IssueLabel[];
  comment_count: number;
}

/// One comment. Ordered by `seq`, never by `id` and never by
/// `created_at`: ULID tails are random within a millisecond, so two
/// comments posted in the same millisecond would render in an order
/// nobody typed. `seq` is the insertion order the database assigned.
export interface IssueComment {
  id: string;
  seq: number;
  body: string;
  author: string | null;
  author_label: string | null;
  created_at: number;
  updated_at: number;
}

/// Both counts, always.
///
/// The index shows "12 Open / 40 Closed" as a pair, and the closed
/// number cannot be derived from a list that was filtered to the open
/// ones. Computing either from the rows on screen is how that number
/// goes wrong — quietly, and only once a repository is big enough that
/// nobody is counting by hand any more.
export interface IssueCounts {
  open: number;
  closed: number;
}

export interface IssueList {
  issues: Issue[];
  counts: IssueCounts;
  /// The `before` cursor for the next page, or null at the end.
  next: number | null;
}

/// What the list endpoint accepts. `state` defaults to `open` server
/// side, which is what the index asks for first.
export interface IssueQuery {
  state?: "open" | "closed" | "all";
  label?: string;
  author?: string;
  q?: string;
  /// `newest` (the default) or `oldest`, and only those two.
  ///
  /// Not `updated`: the page cursor is an issue number and the page is
  /// `number < before`, so ordering by `updated_at` while paging by
  /// `number` puts a page boundary in the wrong place and skips or
  /// repeats rows. The server refuses it with a 400 that says so.
  sort?: "newest" | "oldest";
  limit?: number;
  before?: number;
}

/// What the server said when it refused.
///
/// The status is carried rather than left to be recovered from the
/// message, because recovering it from the message is a bug we have
/// already shipped twice: a repository named `rfc-403` once got reported
/// to users as private, and the org overview once signed people out
/// because a quota error mentioned "401 repositories". An authorization
/// outcome is a status code. `message` stays exactly what it was — the
/// server's own sentence — so everything that displays one is unchanged.
export class ApiError extends Error {
  readonly status: number;
  /// The refusal's JSON body, when it had one. `message` is its `error`
  /// sentence; a few refusals say more than a sentence — a changeset
  /// that will not land names the gate and what it is waiting on, a
  /// revert that cannot be made lists the members that conflict — and a
  /// page that can only print the sentence would be hiding the answer.
  readonly body: unknown;
  constructor(status: number, message: string, body?: unknown) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }
}

/// Is this the server saying "not you"? `401` is no credential or a dead
/// one; `403` is a live credential without the right. Only the first is
/// a reason to end a session.
export function isUnauthenticated(e: unknown): boolean {
  return e instanceof ApiError && e.status === 401;
}

/// One request. `Authorization` is sent only when there is a token to
/// send: with a cookie session the header would be `Bearer `, which the
/// server reads as a malformed credential rather than as none at all.
async function raw<T>(
  path: string,
  token: string,
  init?: { method?: string; body?: unknown },
): Promise<T> {
  return (await rawWithStatus<T>(path, token, init)).body;
}

/// `raw`, keeping the status. Almost nothing needs it — the body says
/// what happened — but a route that answers 200 for "here is the one you
/// already had" and 202 for "made you one" is telling the caller
/// something the body alone does not.
async function rawWithStatus<T>(
  path: string,
  token: string,
  init?: { method?: string; body?: unknown },
): Promise<{ status: number; body: T }> {
  const resp = await fetch(path, {
    method: init?.method ?? "GET",
    headers: {
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...(init?.body !== undefined
        ? { "Content-Type": "application/json" }
        : {}),
    },
    ...(init?.body !== undefined ? { body: JSON.stringify(init.body) } : {}),
  });
  if (!resp.ok) {
    let msg = `${resp.status}`;
    let body: unknown;
    try {
      body = await resp.json();
      const err = (body as { error?: unknown } | null)?.error;
      if (err) msg = `${err}`;
    } catch {
      /* non-JSON error body */
    }
    throw new ApiError(resp.status, msg, body);
  }
  if (resp.status === 204) return { status: 204, body: undefined as T };
  return { status: resp.status, body: (await resp.json()) as T };
}

/// The session a forge page should make: the namespace it is about, and
/// the credential of whoever is looking.
///
/// Both halves matter. On a cookie session the second is invisible —
/// the browser attaches the cookie itself — so a page that forgot it
/// only ever broke for somebody signed in with an API token, and it
/// broke *silently*: the server filters by who is asking, so a person
/// looking at their own repository was shown less and told nothing.
export function viewerSession(owner: string, token: string | null): Session {
  return { org: owner, token: token ?? "" };
}

function call<T>(
  session: Session,
  path: string,
  init?: { method?: string; body?: unknown },
): Promise<T> {
  return raw<T>(
    `/v1/orgs/${encodeURIComponent(session.org)}${path}`,
    session.token,
    init,
  );
}

export const api = {
  /// Sign in as a person. The server sets the session cookie; nothing
  /// about the credential is readable from here, which is the point.
  login(email: string, password: string): Promise<Me> {
    return raw("/v1/auth/login", "", {
      method: "POST",
      body: { email, password },
    });
  },
  /// Create an organization, owned by the signed-in person. Ready at
  /// once; `detail` is the server's sentence about what it holds.
  createOrg(name: string): Promise<{
    id: string;
    name: string;
    detail: string;
  }> {
    return raw("/v1/orgs", "", { method: "POST", body: { name } });
  },
  forgotPassword(email: string): Promise<{ status: string }> {
    return raw("/v1/auth/forgot-password", "", {
      method: "POST",
      body: { email },
    });
  },
  /// Redeem a reset link. Signs in, and ends every other session.
  resetPassword(token: string, new_password: string): Promise<Me> {
    return raw("/v1/auth/reset-password", "", {
      method: "POST",
      body: { token, new_password },
    });
  },
  /// What an invitation is for — which namespace, at what role. Every
  /// dead link answers 404 identically, so this cannot be used to probe
  /// which invitations exist.
  previewInvite(
    invite: string,
  ): Promise<{ org: string; role: Role; email: string; expires_at: number }> {
    return raw("/v1/auth/invite/preview", "", {
      method: "POST",
      body: { invite },
    });
  },
  /// Accept an invitation — the only way anybody gets an account here
  /// short of an operator making one on the server. Creates the account
  /// when the address is new; an existing account is attached to the
  /// org instead, and its password and handle are not needed or changed.
  ///
  /// `handle` is sent only when the person typed one. Left out, the
  /// server makes it from the invited address (`handleFrom` shows the
  /// same name before anybody submits), suffixing it if that name is
  /// taken. Asked for and taken, it is a `409` and nothing was written:
  /// the same link accepts again with another.
  acceptInvite(
    invite: string,
    name: string,
    password?: string,
    handle?: string,
  ): Promise<Me> {
    return raw("/v1/auth/accept-invite", "", {
      method: "POST",
      body: { invite, name, password, handle },
    });
  },
  logout(): Promise<void> {
    return raw("/v1/auth/logout", "", { method: "POST" });
  },
  /// How this server lets a person sign in. Asked with no credential and
  /// answered to anybody — it is what the sign-in screen is drawn from.
  /// The body is read by `authMethodsOf`, which is also what decides
  /// that a failure means today's screen rather than an empty one.
  authMethods(): Promise<unknown> {
    return raw("/v1/auth/methods", "");
  },
  me(): Promise<Me> {
    return raw("/v1/auth/me", "");
  },
  changePassword(
    current_password: string,
    new_password: string,
  ): Promise<void> {
    return raw("/v1/auth/password", "", {
      method: "POST",
      body: { current_password, new_password },
    });
  },
  async members(session: Session): Promise<Member[]> {
    const out = await call<{ members: Member[] }>(session, "/members");
    return out.members;
  },
  setRole(session: Session, userId: string, role: Role): Promise<void> {
    return call(session, `/members/${encodeURIComponent(userId)}`, {
      method: "PATCH",
      body: { role },
    });
  },
  removeMember(session: Session, userId: string): Promise<void> {
    return call(session, `/members/${encodeURIComponent(userId)}`, {
      method: "DELETE",
    });
  },
  async invites(session: Session): Promise<Invite[]> {
    const out = await call<{ invites: Invite[] }>(session, "/invites");
    return out.invites;
  },
  tree(
    session: Session,
    repo: string,
    path: string,
    at?: string,
    /// Ask for the commit that last touched each entry. Opt-in because
    /// it is a history walk server-side; the plain listing stays cheap.
    history?: boolean,
  ): Promise<Tree> {
    const suffix = path
      ? `/${path.split("/").map(encodeURIComponent).join("/")}`
      : "";
    const q = new URLSearchParams();
    if (at) q.set("at", at);
    if (history) q.set("history", "1");
    const query = q.toString();
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/tree${suffix}${query ? `?${query}` : ""}`,
    );
  },
  /// The whole repository as paths, one request. Only the file page asks
  /// — a listing has its own, cheaper answer — and the server caps the
  /// walk, so a monorepo answers with its first fifty thousand paths and
  /// `truncated: true` rather than a timeout.
  treePaths(session: Session, repo: string, at?: string): Promise<TreePaths> {
    const q = new URLSearchParams({ recursive: "1" });
    if (at) q.set("at", at);
    return call(session, `/repos/${encodeURIComponent(repo)}/tree?${q}`);
  },
  /// A file's content, plus what the server said about it.
  ///
  /// Not `call`, because this one is not JSON: the body is whatever was
  /// committed, and the interesting facts are in the headers.
  async file(
    session: Session,
    repo: string,
    path: string,
    at?: string,
  ): Promise<FileContent> {
    const encoded = path.split("/").map(encodeURIComponent).join("/");
    const url =
      `/v1/orgs/${encodeURIComponent(session.org)}/repos/${encodeURIComponent(repo)}` +
      `/files/${encoded}${at ? `?at=${encodeURIComponent(at)}` : ""}`;
    const res = await fetch(url, {
      headers: session.token
        ? { Authorization: `Bearer ${session.token}` }
        : undefined,
    });
    if (!res.ok) {
      let msg = `${res.status}`;
      try {
        const body = await res.json();
        if (body?.error) msg = `${body.error}`;
      } catch {
        /* non-JSON error body */
      }
      throw new Error(msg);
    }
    const binary = res.headers.get("x-weft-binary") === "true";
    const buf = await res.arrayBuffer();
    return {
      // A binary file is never decoded: turning a PNG into mojibake and
      // showing it as source is worse than saying it is binary.
      text: binary ? "" : new TextDecoder().decode(buf),
      binary,
      contentType: res.headers.get("content-type") ?? "",
      commit: res.headers.get("x-weft-commit") ?? "",
      size: buf.byteLength,
    };
  },
  async branches(session: Session, repo: string): Promise<RefName[]> {
    const out = await call<{ branches: RefName[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/branches`,
    );
    return out.branches;
  },
  async tags(session: Session, repo: string): Promise<RefName[]> {
    const out = await call<{ tags: RefName[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/tags`,
    );
    return out.tags;
  },
  async log(
    session: Session,
    repo: string,
    opts: {
      rev?: string;
      limit?: number;
      after?: string;
      /// Only commits that changed this path, each saying what it did.
      path?: string;
    } = {},
  ): Promise<{ entries: LogEntry[]; next_after: string | null }> {
    const q = new URLSearchParams();
    if (opts.rev) q.set("rev", opts.rev);
    if (opts.limit) q.set("limit", String(opts.limit));
    if (opts.after) q.set("after", opts.after);
    if (opts.path) q.set("path", opts.path);
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/log${q.toString() ? `?${q}` : ""}`,
    );
  },
  invite(
    session: Session,
    email: string,
    role: Role,
  ): Promise<{
    id: string;
    invite_link: string;
    mail: { sent: boolean; error?: string };
  }> {
    return call(session, "/invites", { method: "POST", body: { email, role } });
  },
  revokeInvite(session: Session, id: string): Promise<void> {
    return call(session, `/invites/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
  },
  /// Tokens, plus the scopes this caller may put on a new one. The
  /// server is the authority on that — a per-repo grant can raise it —
  /// so the dashboard asks rather than keeping its own copy of the rule.
  tokens(
    session: Session,
  ): Promise<{ tokens: Token[]; mintable_scopes: string[] }> {
    return call(session, "/tokens");
  },
  mintToken(
    session: Session,
    body: { scopes: string[]; label?: string },
  ): Promise<{ id: string; token: string }> {
    return call(session, "/tokens", { method: "POST", body });
  },
  revokeToken(session: Session, id: string): Promise<void> {
    return call(session, `/tokens/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
  },
  audit(
    session: Session,
    filters: Record<string, string>,
  ): Promise<{
    entries: AuditEntry[];
    next_after: number | null;
    next_before: number | null;
  }> {
    const q = new URLSearchParams(filters).toString();
    return call(session, `/audit?${q}`);
  },
  /// The CSV, fetched WITH credentials — see metricsCsv below for why a
  /// plain download link cannot work here.
  async auditCsv(
    session: Session,
    filters: Record<string, string>,
  ): Promise<Blob> {
    const q = new URLSearchParams({ ...filters, format: "csv" }).toString();
    const resp = await fetch(
      `/v1/orgs/${encodeURIComponent(session.org)}/audit?${q}`,
      {
        ...(session.token
          ? { headers: { Authorization: `Bearer ${session.token}` } }
          : {}),
      },
    );
    if (!resp.ok) throw new Error(`${resp.status}`);
    return resp.blob();
  },
  async teams(session: Session): Promise<Team[]> {
    const out = await call<{ teams: Team[] }>(session, "/teams");
    return out.teams;
  },
  createTeam(
    session: Session,
    name: string,
    description?: string,
  ): Promise<Team> {
    return call(session, "/teams", {
      method: "POST",
      body: { name, description: description || null },
    });
  },
  renameTeam(session: Session, id: string, name: string): Promise<void> {
    return call(session, `/teams/${encodeURIComponent(id)}`, {
      method: "PATCH",
      body: { name },
    });
  },
  deleteTeam(session: Session, id: string): Promise<void> {
    return call(session, `/teams/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
  },
  async teamMembers(session: Session, id: string): Promise<TeamMember[]> {
    const out = await call<{ members: TeamMember[] }>(
      session,
      `/teams/${encodeURIComponent(id)}/members`,
    );
    return out.members;
  },
  addTeamMember(session: Session, id: string, user: string): Promise<void> {
    return call(
      session,
      `/teams/${encodeURIComponent(id)}/members/${encodeURIComponent(user)}`,
      { method: "PUT" },
    );
  },
  removeTeamMember(session: Session, id: string, user: string): Promise<void> {
    return call(
      session,
      `/teams/${encodeURIComponent(id)}/members/${encodeURIComponent(user)}`,
      { method: "DELETE" },
    );
  },
  access(
    session: Session,
    repo: string,
  ): Promise<{ people: AccessRow[]; teams: TeamAccessRow[] }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/access`);
  },
  /// Grant several people, or a whole team, in one call — never both:
  /// naming people replaces their org role here, a team grant only ever
  /// raises, and the server refuses a body that mixes the two.
  grant(
    session: Session,
    repo: string,
    subject: { user_ids: string[] } | { team_id: string },
    role: Role,
  ): Promise<void> {
    return call(session, `/repos/${encodeURIComponent(repo)}/grants`, {
      method: "POST",
      body: { ...subject, role },
    });
  },
  revokeGrant(session: Session, repo: string, user: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/grants/${encodeURIComponent(user)}`,
      { method: "DELETE" },
    );
  },
  revokeTeamGrant(session: Session, repo: string, team: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/team-grants/${encodeURIComponent(team)}`,
      { method: "DELETE" },
    );
  },
  async repos(session: Session): Promise<Repo[]> {
    const out = await call<{ repos: Repo[] }>(session, "/repos?limit=200");
    return out.repos;
  },
  /// Ask an origin what it is, before creating anything from it.
  probeOrigin(session: Session, origin: string): Promise<Probe> {
    return call(session, "/origins/probe", {
      method: "POST",
      body: { origin },
    });
  },
  createRepo(
    session: Session,
    body: {
      name: string;
      default_branch?: string;
      description?: string;
    },
  ): Promise<Repo> {
    return call(session, "/repos", { method: "POST", body });
  },
  /// Edit what a repo says about itself. An absent field is left alone.
  patchRepo(
    session: Session,
    name: string,
    body: {
      description?: string | null;
      /// Absent leaves it alone; `null` or `""` clears it. Anything
      /// else must be an absolute http(s) URL or the server refuses it
      /// by name — the client does not pre-validate, because a second
      /// copy of that rule here is a second place for it to drift, and
      /// the client's copy always becomes the stricter one.
      homepage?: string | null;
      /// The GitHub App installation a mirror forwards pushes through.
      /// Org admin, GitHub mirrors only, and the org must have connected
      /// it — an id it never earned is a 404.
      installation_id?: string;
    },
  ): Promise<Repo> {
    return call(session, `/repos/${encodeURIComponent(name)}`, {
      method: "PATCH",
      body,
    });
  },
  /// Delete a repository. 204 and nothing back — the row is gone, so
  /// there is nothing honest to return. Forks of it survive: they are
  /// promoted onto storage of their own rather than following it.
  deleteRepo(session: Session, name: string): Promise<void> {
    return call(session, `/repos/${encodeURIComponent(name)}`, {
      method: "DELETE",
    });
  },
  /// Search across namespaces. Sent with the session cookie and no org
  /// in the path — the answer depends on who is asking, not on which
  /// namespace the dashboard happens to be showing.
  /// Find repositories by free text, optionally narrowed to one topic.
  ///
  /// `q` and `topic` do different jobs on purpose. `q` is what somebody
  /// typed and matches a topic as loosely as it matches a description;
  /// `topic` is what a *pill* means and is exact. Sending a topic as
  /// text would make every pill also return the repositories that merely
  /// mention the word, which is the one thing a facet must not do.
  searchRepos(
    session: Session,
    q: string,
    after?: string,
    topic?: string,
  ): Promise<{ repos: RepoHit[]; next: string | null }> {
    const params = new URLSearchParams({ q, limit: "25" });
    if (after) params.set("after", after);
    if (topic) params.set("topic", topic);
    return raw(`/v1/search/repos?${params}`, session.token);
  },
  createMirror(
    session: Session,
    body: {
      name: string;
      origin: string;
      provider?: string;
      installation_id?: string;
    },
  ): Promise<{ repo: Repo; clone_url: string }> {
    return call(session, "/mirrors", { method: "POST", body });
  },
  syncStatus(session: Session, repo: string): Promise<SyncStatus> {
    return call(session, `/repos/${encodeURIComponent(repo)}/sync-status`);
  },
  /// Where to send somebody to install the GitHub App, with the
  /// single-use state already in it.
  startGithubInstall(
    session: Session,
  ): Promise<{ url: string; state: string; expires_in: number }> {
    return call(session, "/github/install", {
      method: "POST",
      body: {},
    });
  },
  /// The installation this browser's claim cookie is parked for — an
  /// install that began on GitHub and came back with no org to bind to.
  /// 404 when there is none, which the caller reads as "nothing waiting".
  pendingInstall(): Promise<PendingInstall> {
    return raw("/v1/github/pending-install", "");
  },
  /// Connect the parked installation to this org. The cookie is the
  /// proof; the org is the choice.
  claimInstall(
    org: string,
  ): Promise<{ org: string; installation_id: string; account: string | null }> {
    return raw(
      `/v1/orgs/${encodeURIComponent(org)}/github/install/claim`,
      "",
      { method: "POST" },
    );
  },
  async githubInstallations(session: Session): Promise<GithubInstallation[]> {
    const out = await call<{ installations: GithubInstallation[] }>(
      session,
      "/github/installations",
    );
    return out.installations;
  },
  async githubRepos(
    session: Session,
    installation: string,
  ): Promise<RemoteRepo[]> {
    const out = await call<{ repositories: RemoteRepo[] }>(
      session,
      `/github/installations/${encodeURIComponent(installation)}/repos?per_page=100`,
    );
    return out.repositories;
  },
  usage(session: Session): Promise<Usage> {
    return call(session, "/usage");
  },
  repo(session: Session, name: string): Promise<Repo> {
    return call(session, `/repos/${encodeURIComponent(name)}`);
  },
  metrics(session: Session, repo: string): Promise<RepoMetrics> {
    return call(session, `/repos/${encodeURIComponent(repo)}/metrics`);
  },
  async sshKeys(session: Session): Promise<SshKey[]> {
    const out = await call<{ keys: SshKey[] }>(session, "/ssh-keys");
    return out.keys;
  },
  addSshKey(
    session: Session,
    key: { public_key: string; token_id?: string; label?: string },
  ): Promise<SshKey> {
    return call(session, "/ssh-keys", { method: "POST", body: key });
  },
  revokeSshKey(session: Session, id: string): Promise<void> {
    return call(session, `/ssh-keys/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
  },
  runnerPolicy(session: Session): Promise<RunnerPolicy> {
    return call(session, "/runner-policy");
  },
  /// Any subset of the three keys. The object, **not** a JSON string:
  /// `raw` stringifies the body itself, and passing
  /// `JSON.stringify(...)` here is a bug this client has shipped once —
  /// every field arrived double-encoded.
  updateRunnerPolicy(
    session: Session,
    patch: Partial<RunnerPolicy>,
  ): Promise<RunnerPolicy> {
    return call(session, "/runner-policy", { method: "PATCH", body: patch });
  },
  async runnerGroups(session: Session): Promise<RunnerGroup[]> {
    const out = await call<{ groups: RunnerGroup[] }>(
      session,
      "/runner-groups",
    );
    return out.groups;
  },
  createRunnerGroup(
    session: Session,
    body: {
      name: string;
      repo_access?: RunnerGroup["repo_access"];
      repos?: string[];
    },
  ): Promise<RunnerGroup> {
    return call(session, "/runner-groups", { method: "POST", body });
  },
  updateRunnerGroup(
    session: Session,
    id: string,
    body: {
      name?: string;
      repo_access?: RunnerGroup["repo_access"];
      repos?: string[];
    },
  ): Promise<RunnerGroup> {
    return call(session, `/runner-groups/${encodeURIComponent(id)}`, {
      method: "PATCH",
      body,
    });
  },
  deleteRunnerGroup(session: Session, id: string): Promise<void> {
    return call(session, `/runner-groups/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
  },
  async runners(session: Session): Promise<Runner[]> {
    const out = await call<{ runners: Runner[] }>(session, "/runners");
    return out.runners;
  },
  removeRunner(session: Session, id: string): Promise<void> {
    return call(session, `/runners/${encodeURIComponent(id)}`, {
      method: "DELETE",
    });
  },
  /// Mint a single-use registration token. Every call mints a fresh one
  /// and the previous one keeps its own hour — nothing here revokes it,
  /// which matters because an operator who pressed the button twice has
  /// two terminals open.
  mintRunnerRegistrationToken(
    session: Session,
    group?: string,
  ): Promise<RunnerRegistrationToken> {
    return call(session, "/runners/registration-token", {
      method: "POST",
      body: group ? { group } : {},
    });
  },
  /// The CSV export, fetched WITH the Authorization header.
  ///
  /// This cannot be a plain `<a href download>`: a link navigation carries
  /// no headers, so the browser would request the URL unauthenticated and
  /// the user would get a 401 page instead of a download. Fetch it as an
  /// API call and hand the bytes to the browser as a blob instead.
  async metricsCsv(session: Session, repo: string): Promise<Blob> {
    const resp = await fetch(
      `/v1/orgs/${encodeURIComponent(session.org)}/repos/${encodeURIComponent(repo)}/metrics?format=csv`,
      session.token
        ? { headers: { Authorization: `Bearer ${session.token}` } }
        : {},
    );
    if (!resp.ok) throw new Error(`${resp.status}`);
    return resp.blob();
  },
  async changes(session: Session, repo: string): Promise<Change[]> {
    const out = await call<{ changes: Change[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/changes`,
    );
    return out.changes;
  },
  changeDetail(
    session: Session,
    repo: string,
    key: string,
  ): Promise<ChangeDetail> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}`,
    );
  },
  /// Open a change against `repo`.
  ///
  /// The four parts are the same four GitHub's compare page collects —
  /// base repository, base branch, head repository, compare branch —
  /// and the server has accepted all four since forks landed:
  ///
  ///   * base repository is `repo`;
  ///   * `target` is the branch it intends to land on, the repository's
  ///     default when omitted;
  ///   * `source` is the **fork** the commits live in, `owner/name`,
  ///     omitted when they are already in `repo`; and
  ///   * `from` is the branch whose tip becomes the patchset.
  ///
  /// This client sent only `from` for as long as forks have existed, so
  /// the entire contribute-without-write-access path — the one the
  /// server refuses you into by name, and the one `fork_pr_e2e.rs`
  /// covers end to end — was reachable only by hand-writing REST calls.
  createChange(
    session: Session,
    repo: string,
    from: string,
    opts: { source?: string; target?: string } = {},
  ): Promise<{ change: Change; patchset: Patchset }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/changes`, {
      method: "POST",
      // Omitted rather than sent null: `source` absent is what tells the
      // server the commits are already here, and an explicit null would
      // be a different assertion for `serde` to read.
      body: {
        from,
        ...(opts.source ? { source: opts.source } : {}),
        ...(opts.target ? { target: opts.target } : {}),
      },
    });
  },
  approve(session: Session, repo: string, key: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/approve`,
      { method: "POST" },
    );
  },
  unapprove(session: Session, repo: string, key: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/approve`,
      { method: "DELETE" },
    );
  },
  changeVerdict(
    session: Session,
    repo: string,
    key: string,
  ): Promise<ChangeVerdict> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/verdict`,
    );
  },
  landChange(
    session: Session,
    repo: string,
    key: string,
  ): Promise<{ queued: boolean; job: string; change: string }> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/land`,
      { method: "POST" },
    );
  },
  abandonChange(session: Session, repo: string, key: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/abandon`,
      { method: "POST" },
    );
  },
  /// Every change across the org the caller may read, newest first —
  /// what the changeset picker offers. One request, not one per repo.
  async orgChanges(
    session: Session,
    opts: { state?: Change["state"]; limit?: number } = {},
  ): Promise<OrgChange[]> {
    const q = new URLSearchParams();
    if (opts.state) q.set("state", opts.state);
    if (opts.limit) q.set("limit", String(opts.limit));
    const qs = q.toString();
    const out = await call<{ changes: OrgChange[] }>(
      session,
      `/changes${qs ? `?${qs}` : ""}`,
    );
    return out.changes;
  },
  async changesets(
    session: Session,
    state?: ChangesetState,
  ): Promise<Changeset[]> {
    const out = await call<{ changesets: Changeset[] }>(
      session,
      `/changesets${state ? `?state=${encodeURIComponent(state)}` : ""}`,
    );
    return out.changesets;
  },
  changeset(session: Session, key: string): Promise<Changeset> {
    return call(session, `/changesets/${encodeURIComponent(key)}`);
  },
  createChangeset(
    session: Session,
    body: {
      key: string;
      title: string;
      body?: string;
      members: ChangesetRef[];
      edges?: { from: ChangesetRef; to: ChangesetRef }[];
    },
  ): Promise<Changeset> {
    return call(session, "/changesets", { method: "POST", body });
  },
  changesetVerdict(session: Session, key: string): Promise<ChangesetVerdict> {
    return call(session, `/changesets/${encodeURIComponent(key)}/verdict`);
  },
  /// `+N −M` per member and in total, counted server-side over every
  /// member's latest patchset. 404 while any member has no patchset;
  /// `truncated` when a file was too large or too different to count,
  /// in which case the numbers are over the files that were.
  changesetDiffstat(session: Session, key: string): Promise<ChangesetDiffstat> {
    return call(session, `/changesets/${encodeURIComponent(key)}/diffstat`);
  },
  changesetWorkspace(
    session: Session,
    key: string,
  ): Promise<ChangesetWorkspace> {
    return call(session, `/changesets/${encodeURIComponent(key)}/workspace`);
  },
  addChangesetMember(
    session: Session,
    key: string,
    member: ChangesetRef,
  ): Promise<Changeset> {
    return call(session, `/changesets/${encodeURIComponent(key)}/members`, {
      method: "POST",
      body: member,
    });
  },
  removeChangesetMember(
    session: Session,
    key: string,
    member: ChangesetRef,
  ): Promise<void> {
    return call(
      session,
      `/changesets/${encodeURIComponent(key)}/members/${encodeURIComponent(member.repo)}/${encodeURIComponent(member.change)}`,
      { method: "DELETE" },
    );
  },
  setChangesetEdges(
    session: Session,
    key: string,
    edges: { from: ChangesetRef; to: ChangesetRef }[],
  ): Promise<Changeset> {
    return call(session, `/changesets/${encodeURIComponent(key)}/edges`, {
      method: "PUT",
      body: { edges },
    });
  },
  /// 202 with the plan when every gate is green; a 409 otherwise, whose
  /// body says which gate and what it is waiting on — see
  /// `ChangesetView`, which reads that off the `ApiError`.
  landChangeset(
    session: Session,
    key: string,
  ): Promise<{
    queued: boolean;
    job: string;
    changeset: string;
    landing: string;
    plan: (ChangesetRef & { ref: string; old: string | null; new: string })[];
  }> {
    return call(session, `/changesets/${encodeURIComponent(key)}/land`, {
      method: "POST",
    });
  },
  /// Makes the reverting changeset and returns it; a 409 carries
  /// `conflicts`, one per member that could not be put back cleanly.
  revertChangeset(
    session: Session,
    key: string,
    body: { key: string; title?: string; body?: string },
  ): Promise<Changeset> {
    return call(session, `/changesets/${encodeURIComponent(key)}/revert`, {
      method: "POST",
      body,
    });
  },
  abandonChangeset(session: Session, key: string): Promise<void> {
    return call(session, `/changesets/${encodeURIComponent(key)}/abandon`, {
      method: "POST",
    });
  },
  async comments(
    session: Session,
    repo: string,
    key: string,
  ): Promise<ChangeComment[]> {
    const out = await call<{ comments: ChangeComment[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/comments`,
    );
    return out.comments;
  },
  addComment(
    session: Session,
    repo: string,
    key: string,
    body: string,
    path?: string,
    line?: number,
    opts?: NewCommentOptions,
  ): Promise<ChangeComment> {
    const payload: {
      body: string;
      path?: string;
      line?: number;
      parent_id?: string;
      side?: CommentSide;
      line_end?: number;
      pending?: boolean;
    } = { body };
    if (path) payload.path = path;
    if (path && line !== undefined) payload.line = line;
    // A reply carries the thread and nothing else. Sending an anchor
    // beside `parent_id` is a 400 from the server, and the client must
    // not be the thing that produces one: the caller passes no anchor
    // with a reply, and this shape is what makes that readable.
    if (opts?.parent_id) payload.parent_id = opts.parent_id;
    // Only sent when there is a line to count it against. `side` on a
    // change-wide comment means nothing, and `line_end` without `line`
    // is refused.
    if (payload.line !== undefined) {
      if (opts?.side) payload.side = opts.side;
      if (opts?.line_end !== undefined) payload.line_end = opts.line_end;
    }
    // Sent only when it is true. `pending: false` and no `pending` at
    // all mean the same thing to the server, and the older deployments
    // this client still talks to have no field to read — sending one
    // unconditionally would put a key on the wire that says nothing.
    if (opts?.pending) payload.pending = true;
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/comments`,
      { method: "POST", body: payload },
    );
  },
  /// Call a thread settled, or reopen it.
  ///
  /// One method for both routes rather than two, because they are one
  /// decision with a boolean in it and the server enforces the same rule
  /// for each: whoever could close it can reopen it, which is what keeps
  /// "resolved" from being a one-way silencer.
  ///
  /// Who may is **not** decided here. It is an OWNERS question about the
  /// commented path, answered by the server; the page hides the control
  /// where it can be sure of the answer and repeats the server's own
  /// refusal where it cannot.
  setCommentResolved(
    session: Session,
    repo: string,
    key: string,
    comment: string,
    resolved: boolean,
  ): Promise<ChangeComment> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}` +
        `/comments/${encodeURIComponent(comment)}/` +
        (resolved ? "resolve" : "unresolve"),
      { method: "POST" },
    );
  },
  /// Submit the caller's review: publish every drafted comment, record
  /// the verdict, move the approval to match, and send **one**
  /// notification for the lot.
  ///
  /// One request, and that is the whole feature. Twelve remarks used to
  /// be twelve posts and — for a while — twelve mails; the count was a
  /// race rather than a number, because the sender claiming one job
  /// between the third comment and the fourth started a fresh one.
  /// There is exactly one act to report here, so there is exactly one
  /// mail.
  ///
  /// `body` is the cover message. The server refuses `request_changes`
  /// with neither words nor drafted comments — "a block with nothing in
  /// it is a wall with no door" — and `reviewRefusal` says the same
  /// thing in the sheet before the trip.
  submitReview(
    session: Session,
    repo: string,
    key: string,
    review: { verdict: ReviewVerdictKind; body?: string },
  ): Promise<{
    review: Review;
    /** How many drafted comments became visible. */
    published: number;
    approved: boolean;
    approval_revoked: boolean;
  }> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/review/submit`,
      // An object, never a pre-stringified one: `call` serialises, and
      // a body handed over as a string arrives double-encoded. This
      // repository has shipped that bug once already.
      { method: "POST", body: review },
    );
  },
  /// Take back one's own standing request for changes.
  ///
  /// It cannot name anybody else's, which is the shape of the route
  /// rather than a check on top of it: a block somebody else could
  /// clear is not a block, and the argument for one surviving a new
  /// patchset is precisely that it ends when the person who raised it
  /// says so.
  withdrawReview(session: Session, repo: string, key: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/review/withdraw`,
      { method: "POST" },
    );
  },
  /// Throw the pending review away, drafted comments and all. Nothing
  /// anybody else ever saw is lost, which is the entire point of a
  /// draft — and a draft that could not be abandoned would be a trap.
  discardReview(session: Session, repo: string, key: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/review`,
      { method: "DELETE" },
    );
  },
  /// Take the named comments' suggestions and make them **one** new
  /// patchset.
  ///
  /// One request however many suggestions, and that is the whole point
  /// of the route. A reviewer leaves five remarks and the author takes
  /// them together; a commit per click would put five revisions on the
  /// change, start CI five times and notify everybody OWNERS names five
  /// times, for one act.
  ///
  /// The refusals come back as sentences written to be shown to the
  /// person who pressed the button — who has write access and who does
  /// not, which two comments overlap on which lines, which file has
  /// moved since the patchset a comment anchors to — so a caller
  /// renders `ApiError.message` verbatim rather than dressing it in
  /// "Could not apply".
  applySuggestions(
    session: Session,
    repo: string,
    key: string,
    comments: string[],
  ): Promise<{
    change: Change;
    patchset: Patchset;
    /** The comment ids that went in, echoed back. */
    applied: string[];
    /** The files the new patchset touched. */
    paths: string[];
  }> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/suggestions/apply`,
      // An object, never a pre-stringified one: `call` serialises, and
      // a body handed over as a string arrives double-encoded. This
      // repository has shipped that bug once already.
      { method: "POST", body: { comments } },
    );
  },
  /// The checks bearing on a change, **and the names the target branch
  /// requires**.
  ///
  /// Both halves, because a required check that has never reported has no
  /// row in `checks` to be found in. Returning the rows alone is how a
  /// review page ends up counting one green check and announcing that
  /// everything passed while the land queue holds the change on a name
  /// nothing has ever posted.
  ///
  /// `required_checks` defaults to `[]` rather than being required on the
  /// wire: a server older than this field answers without it, and the
  /// honest degradation is the behaviour we had before — blockers drawn
  /// from the rows that did report — not a page that fails to load.
  async checks(
    session: Session,
    repo: string,
    key: string,
  ): Promise<{ checks: ChangeCheck[]; required: string[] }> {
    const out = await call<{
      checks: ChangeCheck[];
      required_checks?: string[];
    }>(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/checks`,
    );
    return { checks: out.checks, required: out.required_checks ?? [] };
  },
  /// The reviewer's own per-file viewed marks on a change.
  async changeViews(
    session: Session,
    repo: string,
    key: string,
  ): Promise<ChangeViews> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/views`,
    );
  },
  /// Tick or untick one path, against whatever the latest patchset is.
  /// The patchset is the server's to decide — a client that sent one
  /// could tick a file against a revision nobody is looking at.
  setChangeViewed(
    session: Session,
    repo: string,
    key: string,
    path: string,
    viewed: boolean,
  ): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/views`,
      { method: "PUT", body: { path, viewed } },
    );
  },
  changeAssociations(
    session: Session,
    repo: string,
    key: string,
  ): Promise<ChangeAssociations> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(key)}/associations`,
    );
  },
  async protections(
    session: Session,
    repo: string,
  ): Promise<BranchProtection[]> {
    const out = await call<{ protections: BranchProtection[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/protections`,
    );
    return out.protections;
  },
  protect(session: Session, repo: string, branch: string): Promise<unknown> {
    return call(session, `/repos/${encodeURIComponent(repo)}/protections`, {
      method: "POST",
      body: { branch },
    });
  },
  /// The checks a branch requires before a change may land on it.
  ///
  /// The branch rides in the path as a trailing catch-all — a branch name
  /// contains slashes (`release/2.0`) — so it is **not**
  /// `encodeURIComponent`d, which would turn those into `%2F` and address
  /// a branch nobody has. `encodeURI` leaves the separators and escapes
  /// everything else, which is what the route's wildcard expects. The
  /// check's own name contains slashes too (`ci/tests`), so it cannot be
  /// a path segment either: it rides in the body on POST and as a query
  /// parameter on DELETE.
  async requiredChecks(
    session: Session,
    repo: string,
    branch: string,
  ): Promise<RequiredCheck[]> {
    const out = await call<{ required_checks: RequiredCheck[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/required-checks/${encodeURI(branch)}`,
    );
    return out.required_checks;
  },
  requireCheck(
    session: Session,
    repo: string,
    branch: string,
    name: string,
  ): Promise<unknown> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/required-checks/${encodeURI(branch)}`,
      { method: "POST", body: { name } },
    );
  },
  unrequireCheck(
    session: Session,
    repo: string,
    branch: string,
    name: string,
  ): Promise<unknown> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/required-checks/${encodeURI(
        branch,
      )}?name=${encodeURIComponent(name)}`,
      { method: "DELETE" },
    );
  },
  /// What this caller has asked to hear about this repository.
  ///
  /// Answers 401 to a caller the server cannot identify: a subscription
  /// is a property of a person, and there is nobody to have one. The
  /// control treats that as "do not render", not as an error worth
  /// putting on the page.
  /// `GET /v1/users/{handle}` — what a namespace says about itself.
  /// Sent with whatever credential the viewer has, like every read.
  getProfile(session: Session, handle: string): Promise<Profile> {
    return raw(`/v1/users/${encodeURIComponent(handle)}`, session.token);
  },
  /// Begin importing this repository's issues from its GitHub origin.
  ///
  /// 202, not 201: nothing is imported yet, a job exists. Refused with
  /// 409 if the tracker already has issues — an import keeps the numbers
  /// it came with, so it can only go into an empty one.
  startImport(session: Session, repo: string): Promise<{ status: string }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/import`, {
      method: "POST",
    });
  },
  /// How far an import has got, per phase. `null` means not started,
  /// `"done"` means finished, and anything else is where it resumes.
  importStatus(session: Session, repo: string): Promise<ImportStatus> {
    return call(session, `/repos/${encodeURIComponent(repo)}/import`);
  },
  /// Every milestone on a repository, by the number it came with.
  async milestones(session: Session, repo: string): Promise<Milestone[]> {
    const out = await call<{ milestones: Milestone[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/milestones`,
    );
    return out.milestones;
  },
  /// The addresses on an account, and whether each is proved.
  ///
  /// Self only — the server refuses anybody else, and rightly: a list of
  /// somebody's addresses is exactly what a stranger must not be able
  /// to ask for.
  emails(session: Session, handle: string): Promise<{ emails: EmailRow[] }> {
    return raw(`/v1/users/${encodeURIComponent(handle)}/emails`, session.token);
  },
  /// Claim an address. The server mails it a link; the claim is worth
  /// nothing until that link is spent.
  addEmail(session: Session, handle: string, email: string): Promise<unknown> {
    return raw(
      `/v1/users/${encodeURIComponent(handle)}/emails`,
      session.token,
      {
        method: "POST",
        body: { email },
      },
    );
  },
  /// Spend the link that proves an **added** address.
  ///
  /// The address an account signs in with needs no proving: it was
  /// proved when the account was made, by the invitation reaching it or
  /// by the operator who made it. This is for the others. It is signed
  /// in, names the account, and is how somebody claims the address their
  /// commits already carry.
  ///
  /// Retroactive: the server re-walks the repositories this person can
  /// reach, so commits they authored under the address before today
  /// start counting. That is the migration promise — mirror a decade of
  /// work, prove the address you wrote it under, watch it appear.
  proveEmail(
    session: Session,
    handle: string,
    token: string,
  ): Promise<{ address: string }> {
    return raw(
      `/v1/users/${encodeURIComponent(handle)}/emails/verify`,
      session.token,
      { method: "POST", body: { token } },
    );
  },
  /// Drop an address. The server refuses the sign-in credential: an
  /// account with no reachable address is one nobody can recover.
  removeEmail(
    session: Session,
    handle: string,
    address: string,
  ): Promise<void> {
    return raw(
      `/v1/users/${encodeURIComponent(handle)}/emails/${encodeURIComponent(address)}`,
      session.token,
      { method: "DELETE" },
    );
  },
  /// Who has contributed to this repository, biggest share first.
  ///
  /// Derived from the commits themselves, which is what makes a project
  /// that mirrored in yesterday show its real contributors rather than
  /// an empty grid.
  /// The server clamps `limit`; the client does not second-guess it.
  async contributors(
    session: Session,
    repo: string,
    limit?: number,
  ): Promise<Contributor[]> {
    const q = limit === undefined ? "" : `?limit=${limit}`;
    const out = await call<{ contributors: Contributor[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/contributors${q}`,
    );
    return out.contributors;
  },
  /// The verdicts standing beside **one commit**: one row per workflow,
  /// the newest of each.
  ///
  /// Not the same question as `checkRuns` filtered to a sha. That returns
  /// the *history* — every report, including the four a provider sent
  /// while one build moved from queued to passing — and a commit page
  /// showing four contradictory rows for one workflow is worse than one
  /// showing none, because a reader cannot tell which is current.
  ///
  /// Refuses nothing a caller can act on: a commit nothing has reported on
  /// answers `200` with an empty list, which is the same shape as a commit
  /// whose CI is green and simply not wired to us. The caller renders
  /// nothing in both cases rather than inventing a "no CI" claim it cannot
  /// support.
  async commitChecks(
    session: Session,
    repo: string,
    sha: string,
  ): Promise<CheckRun[]> {
    const out = await call<{ runs: CheckRun[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/commits/${encodeURIComponent(
        sha,
      )}/checks`,
    );
    return out.runs;
  },
  /// This repository's CI runs, newest first.
  ///
  /// `workflows` rides along with the first page so the left rail can be
  /// drawn without a second round trip — the names are a `DISTINCT` over
  /// an index and cost far less than the request would.
  checkRuns(
    session: Session,
    repo: string,
    query: RunQuery = {},
  ): Promise<{
    runs: CheckRun[];
    workflows: string[];
    next_before: number | null;
  }> {
    const q = new URLSearchParams();
    if (query.branch) q.set("branch", query.branch);
    if (query.state) q.set("state", query.state);
    if (query.event) q.set("event", query.event);
    if (query.actor) q.set("actor", query.actor);
    if (query.workflow) q.set("workflow", query.workflow);
    if (query.limit !== undefined) q.set("limit", String(query.limit));
    if (query.before !== undefined) q.set("before", String(query.before));
    const s = q.toString();
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/checks/runs${s ? `?${s}` : ""}`,
    );
  },
  /// Where this repository's pushes are announced.
  ///
  /// The other half of the CI loop for a repository hosted here: the
  /// intake below carries the verdict *back*, and this is what tells
  /// somebody's build system there is something to build. Nothing on
  /// Weft runs a build, so without one of these a hosted repository's
  /// CI is never triggered by anything.
  async webhooks(session: Session, repo: string): Promise<Webhook[]> {
    // `subscriptions`, not `webhooks` — the server's key, read from
    // `webhooks_api::list` rather than guessed from the route name.
    // Unwrapping the wrong one does not fail loudly: it resolves to
    // `undefined`, and the first `.map` or `.length` on it throws
    // somewhere else entirely.
    const out = await call<{ subscriptions: Webhook[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/webhooks`,
    );
    return out.subscriptions;
  },
  /// Subscribe a URL. The response carries the delivery `secret` in
  /// plaintext and is the only time it exists outside the database —
  /// the same once-only shape as the intake secret and an API token,
  /// and it must be treated the same way.
  createWebhook(
    session: Session,
    repo: string,
    url: string,
  ): Promise<{ id: string; url: string; secret: string }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/webhooks`, {
      method: "POST",
      body: { url },
    });
  },
  /// Unsubscribe. The pushes stop being announced, so whatever this was
  /// driving stops being driven — quietly, which is why removing one
  /// belongs behind a confirmation.
  deleteWebhook(session: Session, repo: string, id: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/webhooks/${encodeURIComponent(id)}`,
      { method: "DELETE" },
    );
  },
  /// Whether this repository has an intake secret, and when it last
  /// moved. **Never the secret itself** — the server does not return it
  /// on a read, deliberately, so there is exactly one moment it exists
  /// outside the database and it is the moment somebody asked for it.
  ciSecretStatus(
    session: Session,
    repo: string,
  ): Promise<{ configured: boolean; rotated_at?: number }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/ci/secret`);
  },
  /// Mint or rotate the intake secret. The response carries the secret
  /// in plaintext and is the **only** time it is ever available: rotating
  /// replaces the old one, so a pipeline still holding it stops being
  /// believed. Show it once, say so, and never store it.
  rotateCiSecret(
    session: Session,
    repo: string,
  ): Promise<{ secret: string; rotated_at: number }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/ci/secret`, {
      method: "POST",
    });
  },
  /// Revoke it. The next delivery from whoever held it is answered like
  /// any stranger's — which is to say the project's CI silently stops
  /// reporting, so this belongs behind a confirmation.
  revokeCiSecret(session: Session, repo: string): Promise<void> {
    return call(session, `/repos/${encodeURIComponent(repo)}/ci/secret`, {
      method: "DELETE",
    });
  },
  /// How the last poll of this repository's GitHub Actions runs went.
  ///
  /// Read by the Checks tab so an empty list can say *why* it is empty.
  /// Refused for a viewer who may not read the repository, like every
  /// other repo-scoped read.
  checksPoll(session: Session, repo: string): Promise<ChecksPoll> {
    return call(session, `/repos/${encodeURIComponent(repo)}/ci/poll`);
  },
  /// Ask for a poll now. `repo:write`, because it restates verdicts a
  /// provider already reached rather than creating anything.
  pollChecks(session: Session, repo: string): Promise<void> {
    return call(session, `/repos/${encodeURIComponent(repo)}/ci/poll`, {
      method: "POST",
    });
  },
  /// This repository's workflow runs, newest first, jobs included.
  ///
  /// Jobs ride along rather than costing a request each: a run without
  /// its jobs is a row that can say "failed" and nothing about what
  /// failed, and every caller wants both.
  /// `commitSha` and `changeKey` narrow the window to one revision.
  /// A server that does not yet filter on them ignores them and answers
  /// the same page it always did, so a caller that wants one commit's
  /// runs must **still** filter what comes back — see
  /// `@/lib/workflow-runs`. Sending them is what makes the answer exact
  /// once the server can: without a filter the runs for the commit under
  /// review fall out of the newest-first window on a busy repository,
  /// and a page that reads "nothing is blocked" from a truncated list is
  /// wrong in the direction that looks fine.
  async workflowRuns(
    session: Session,
    repo: string,
    opts: {
      limit?: number;
      commitSha?: string;
      changeKey?: string;
      /// `changeset` for the composed runs alone — the Checks tab's
      /// question, asked of the server so a busy repository's push runs
      /// cannot push them out of the window.
      event?: string;
    } = {},
  ): Promise<WorkflowRun[]> {
    const params = new URLSearchParams();
    if (opts.limit !== undefined) params.set("limit", String(opts.limit));
    if (opts.commitSha !== undefined) params.set("commit_sha", opts.commitSha);
    if (opts.changeKey !== undefined) params.set("change_key", opts.changeKey);
    if (opts.event !== undefined) params.set("event", opts.event);
    const q = params.size === 0 ? "" : `?${params}`;
    const out = await call<{ runs: WorkflowRun[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/workflow-runs${q}`,
    );
    return out.runs;
  },
  /// One workflow run. A run belonging to another repository answers 404,
  /// not 403 — an id that resolves differently for a stranger is an
  /// existence oracle — so a caller must render "no such run" for both.
  workflowRun(
    session: Session,
    repo: string,
    id: string,
  ): Promise<WorkflowRun> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/workflow-runs/${encodeURIComponent(id)}`,
    );
  },
  /// Stop a run. `repo:write`, because cancelling somebody's build
  /// destroys work.
  ///
  /// Answers `409` when the run has already settled, and that is worth
  /// surfacing rather than swallowing: it means the page the person is
  /// looking at is out of date. The run as it now stands comes back, so
  /// the caller can render the truth instead of asking again.
  cancelWorkflowRun(
    session: Session,
    repo: string,
    id: string,
  ): Promise<WorkflowRun> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/workflow-runs/${encodeURIComponent(
        id,
      )}/cancel`,
      { method: "POST" },
    );
  },
  /// Approve the blocked workflow runs on a change from a fork, and start
  /// them.
  ///
  /// Deliberately **not** named `approve`: that one is the review
  /// approval on a patchset, and the two say very different things about
  /// what a person has agreed to. Approving a patchset is a statement
  /// about the code; approving its workflows is a statement about
  /// running a stranger's code on our runners, and a reviewer who
  /// confuses them has been asked the wrong question.
  ///
  /// Per **tip**, like GitHub's "Approve and run": a new patchset from
  /// the fork is blocked again, because the thing that was approved is
  /// not what would now run.
  ///
  /// `202` with the runs that started. `409` when nothing is blocked at
  /// the current tip — usually the page is stale, or somebody else
  /// approved it a moment ago — and `403` for a person who may not land
  /// the change. Both carry the server's own sentence, which the caller
  /// should show rather than replace: it distinguishes "already running"
  /// from "not yours to start".
  async approveWorkflows(
    session: Session,
    repo: string,
    key: string,
  ): Promise<WorkflowRun[]> {
    const out = await call<{ runs: WorkflowRun[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(
        key,
      )}/workflows/approve`,
      { method: "POST" },
    );
    return out.runs;
  },
  /// One job's log as it stands — complete once the job is over, so far
  /// while it is not.
  ///
  /// Addressed by the **job row's** id under `workflow-jobs`, not by
  /// `run/{id}/jobs/{job}`: the runner reports against a job id it was
  /// handed and the read side uses the same address, so there is one
  /// name for a job rather than two.
  ///
  /// Not `call`, because the body is `text/plain` — it is a build log,
  /// and a reader may well be `curl`ing it.
  async jobLog(session: Session, repo: string, job: string): Promise<string> {
    const resp = await fetch(
      `/v1/orgs/${encodeURIComponent(session.org)}/repos/${encodeURIComponent(
        repo,
      )}/workflow-jobs/${encodeURIComponent(job)}/log`,
      session.token
        ? { headers: { Authorization: `Bearer ${session.token}` } }
        : {},
    );
    if (!resp.ok) throw new ApiError(resp.status, `${resp.status}`);
    return resp.text();
  },
  /// The same log, as it arrives.
  ///
  /// Deliberately **not** `EventSource`, which is the obvious spelling
  /// and cannot work: it sends no headers, so a person signed in with an
  /// API token would ask for the feed unauthenticated and be refused —
  /// and refused *invisibly*, since an `EventSource` failure is an
  /// `onerror` with no status in it. `fetch` carries `Authorization` and
  /// gives a real status back.
  ///
  /// **The feed replays.** It starts at chunk one of the current
  /// attempt, so it is a superset of `jobLog` and not a continuation of
  /// it: a caller that fetched the log and then appended everything this
  /// hands back would render the whole log twice. Use one or the other —
  /// this for a job still going, `jobLog` for a settled one.
  ///
  /// Resolves when the feed closes. `signal` is how a caller stops
  /// listening — an unmounting page must abort, or the socket outlives
  /// the component that opened it.
  async streamJobLog(
    session: Session,
    repo: string,
    job: string,
    on: (event: LogEvent) => void,
    signal?: AbortSignal,
  ): Promise<void> {
    const resp = await fetch(
      `/v1/orgs/${encodeURIComponent(session.org)}/repos/${encodeURIComponent(
        repo,
      )}/workflow-jobs/${encodeURIComponent(job)}/log/stream`,
      {
        headers: {
          Accept: "text/event-stream",
          ...(session.token
            ? { Authorization: `Bearer ${session.token}` }
            : {}),
        },
        signal,
      },
    );
    if (!resp.ok) throw new ApiError(resp.status, `${resp.status}`);
    const body = resp.body;
    // No streaming body at all. Not an error: a `fetch` polyfill or a
    // proxy that buffers gives a complete response instead, and the
    // right answer is to say the feed is over rather than to throw at a
    // caller whose page is fine without it.
    if (!body) return;
    const reader = body.getReader();
    const bytes = new TextDecoder();
    const sse = new SseDecoder();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        // `{ stream: true }` because a chunk boundary can land in the
        // middle of a UTF-8 sequence, and decoding each read
        // independently turns one multi-byte character into two
        // replacement glyphs in the middle of somebody's log.
        for (const frame of sse.push(bytes.decode(value, { stream: true }))) {
          const event = decodeLogEvent(frame);
          if (event) on(event);
        }
      }
    } finally {
      // Releases the socket when the caller aborted mid-read. Ignored on
      // failure: cancelling an already-cancelled reader throws, and
      // there is nothing left to do about it.
      reader.cancel().catch(() => {});
    }
  },
  /// Repositories forked directly from this one that the caller may see.
  ///
  /// `count` is the length of the list and never the stored total:
  /// publishing a larger number than the list would say exactly how many
  /// forks exist that the caller cannot read.
  forks(
    session: Session,
    repo: string,
  ): Promise<{ forks: ForkEntry[]; count: number }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/forks`);
  },
  repoMeta(session: Session, repo: string): Promise<RepoMeta> {
    return call(session, `/repos/${encodeURIComponent(repo)}/meta`);
  },
  /// Replace the topics with exactly this set — the whole list, not an
  /// addition. Needs write access; the server lowercases and refuses
  /// anything that is not a case difference away from storable, so the
  /// UI does not have to pre-validate and must not pretend it did.
  setTopics(
    session: Session,
    repo: string,
    topics: string[],
  ): Promise<{ topics: string[] }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/topics`, {
      method: "PUT",
      // The object, not a string. Passing `JSON.stringify(...)` here is
      // the double-encoding bug the Playwright suite caught once
      // already: `raw` stringifies the body itself.
      body: { topics },
    });
  },
  /// Fork a repository into a namespace — the caller's own when `org`
  /// is omitted. Answers 202: the repository exists immediately and
  /// `fork_state` says whether it can be cloned yet.
  /// Fork a repository. `existing` is true when the server answered
  /// with the fork this person already had (200) rather than a new one
  /// (202): pressing Fork twice is how most people find their fork
  /// again, and the page should say that is what happened rather than
  /// let it read as a fresh copy.
  async forkRepo(
    session: Session,
    repo: string,
    into?: { org?: string; name?: string },
  ): Promise<{ repo: Repo; existing: boolean }> {
    const { status, body } = await rawWithStatus<Repo>(
      `/v1/orgs/${encodeURIComponent(session.org)}/repos/${encodeURIComponent(repo)}/forks`,
      session.token,
      { method: "POST", body: into ?? {} },
    );
    return { repo: body, existing: status === 200 };
  },
  watch(session: Session, repo: string): Promise<{ level: WatchLevel }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/watch`);
  },
  setWatch(
    session: Session,
    repo: string,
    level: WatchLevel,
  ): Promise<{ level: WatchLevel }> {
    return call(session, `/repos/${encodeURIComponent(repo)}/watch`, {
      method: "PUT",
      body: { level },
    });
  },
  unprotect(session: Session, repo: string, branch: string): Promise<unknown> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/protections/${encodeURIComponent(branch)}`,
      { method: "DELETE" },
    );
  },
  setDefaultBranch(
    session: Session,
    repo: string,
    branch: string,
  ): Promise<Repo> {
    return call(session, `/repos/${encodeURIComponent(repo)}`, {
      method: "PATCH",
      body: { default_branch: branch },
    });
  },
  /// The issues index for one repository.
  ///
  /// Every parameter is omitted when unset rather than sent empty:
  /// `?author=` is a filter for the person whose handle is the empty
  /// string, which is nobody, and the server would honour it.
  issues(
    session: Session,
    repo: string,
    query: IssueQuery = {},
  ): Promise<IssueList> {
    const q = new URLSearchParams();
    if (query.state) q.set("state", query.state);
    if (query.label) q.set("label", query.label);
    if (query.author) q.set("author", query.author);
    if (query.q) q.set("q", query.q);
    if (query.sort) q.set("sort", query.sort);
    if (query.limit !== undefined) q.set("limit", String(query.limit));
    if (query.before !== undefined) q.set("before", String(query.before));
    const s = q.toString();
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/issues${s ? `?${s}` : ""}`,
    );
  },
  issue(session: Session, repo: string, number: number): Promise<Issue> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/issues/${encodeURIComponent(number)}`,
    );
  },
  /// File one. `RepoRead` is enough on the server, deliberately: gating
  /// this on write scope would make issues useless on open source, which
  /// is the entire point of the feature.
  openIssue(
    session: Session,
    repo: string,
    title: string,
    body: string,
  ): Promise<Issue> {
    // An object, not a string. `raw` stringifies the body itself, and a
    // client that stringified first sent every field double-encoded —
    // which is a bug this repo has already shipped once.
    return call(session, `/repos/${encodeURIComponent(repo)}/issues`, {
      method: "POST",
      body: { title, body },
    });
  },
  /// Change an issue's title or body. The author's or a writer's.
  ///
  /// Separate from `setIssueState` because they are different
  /// authorities on the server — closing is also the author's on their
  /// own issue, editing the text is the author's or a writer's — and
  /// folding them into one call would invite a caller to send both and
  /// discover only one was allowed.
  editIssue(
    session: Session,
    repo: string,
    number: number,
    body: { title?: string; body?: string },
  ): Promise<Issue> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/issues/${encodeURIComponent(number)}`,
      { method: "PATCH", body },
    );
  },
  setIssueState(
    session: Session,
    repo: string,
    number: number,
    state: "open" | "closed",
  ): Promise<Issue> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/issues/${encodeURIComponent(number)}`,
      { method: "PATCH", body: { state } },
    );
  },
  async issueComments(
    session: Session,
    repo: string,
    number: number,
  ): Promise<IssueComment[]> {
    const out = await call<{ comments: IssueComment[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/issues/${encodeURIComponent(number)}/comments`,
    );
    return out.comments;
  },
  commentOnIssue(
    session: Session,
    repo: string,
    number: number,
    body: string,
  ): Promise<IssueComment> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/issues/${encodeURIComponent(number)}/comments`,
      { method: "POST", body: { body } },
    );
  },
  /// Create a label on this repository. Needs write access.
  ///
  /// `color` is a **design-system token name**, never hex — the server
  /// refuses anything else, and the refusal names the seven it takes.
  /// A stored hex would be unfixable when the palette moves; a token
  /// re-resolves. See `LABEL_COLORS` for the list this UI offers.
  createLabel(
    session: Session,
    repo: string,
    body: { name: string; color: string; description?: string },
  ): Promise<IssueLabel> {
    return call(session, `/repos/${encodeURIComponent(repo)}/labels`, {
      method: "POST",
      body,
    });
  },
  /// Remove a label from the repository, and with it from every issue
  /// carrying it. Needs write access.
  deleteLabel(session: Session, repo: string, name: string): Promise<void> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/labels/${encodeURIComponent(name)}`,
      { method: "DELETE" },
    );
  },
  /// Replace an issue's labels with exactly this set — the whole list,
  /// not an addition, which is the same shape `setTopics` uses.
  ///
  /// Needs write access: the server resolves with read and then checks
  /// write explicitly, so a reader without write access gets a 403 that
  /// explains itself rather than a 404 that denies the issue exists.
  setIssueLabels(
    session: Session,
    repo: string,
    number: number,
    labels: string[],
  ): Promise<Issue> {
    return call(
      session,
      `/repos/${encodeURIComponent(repo)}/issues/${encodeURIComponent(number)}/labels`,
      { method: "PUT", body: { labels } },
    );
  },
  async issueLabels(session: Session, repo: string): Promise<IssueLabel[]> {
    const out = await call<{ labels: IssueLabel[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/labels`,
    );
    return out.labels;
  },
  async structuralDiff(
    session: Session,
    repo: string,
    from: string,
    to: string,
  ): Promise<DiffEntry[]> {
    const out = await call<{ changes: DiffEntry[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/diff?from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`,
    );
    return out.changes;
  },
  /// What moved between two patchsets of one change — the same entry
  /// shape as `/diff`, with the revisions named by patchset number. A
  /// reviewer who read patchset 2 wants patchset 3's diff against *that*,
  /// not against the parent all over again.
  async interdiff(
    session: Session,
    repo: string,
    change: string,
    from: number,
    to: number,
  ): Promise<DiffEntry[]> {
    const out = await call<{ changes: DiffEntry[] }>(
      session,
      `/repos/${encodeURIComponent(repo)}/changes/${encodeURIComponent(change)}/interdiff?from=${from}&to=${to}`,
    );
    return out.changes;
  },
  /// A file's text at a rev, fetched WITH the Authorization header (same
  /// reasoning as the CSV exports). Returns null for content the diff
  /// view should not render as text: absent, binary, or too large.
  async fileText(
    session: Session,
    repo: string,
    path: string,
    at: string,
  ): Promise<string | null> {
    const resp = await fetch(
      `/v1/orgs/${encodeURIComponent(session.org)}/repos/${encodeURIComponent(repo)}/files/${path
        .split("/")
        .map(encodeURIComponent)
        .join("/")}?at=${encodeURIComponent(at)}`,
      session.token
        ? { headers: { Authorization: `Bearer ${session.token}` } }
        : {},
    );
    if (resp.status === 404) return null;
    if (!resp.ok) throw new Error(`${resp.status}`);
    const buf = await resp.arrayBuffer();
    if (buf.byteLength > 512 * 1024) return null;
    const bytes = new Uint8Array(buf);
    for (let i = 0; i < Math.min(bytes.length, 8192); i++) {
      if (bytes[i] === 0) return null;
    }
    return new TextDecoder("utf-8", { fatal: false }).decode(buf);
  },
};
