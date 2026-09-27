//! PostgreSQL handle + embedded migrations.

use postgres::types::ToSql;
use postgres::{Client, NoTls, Row};
use std::sync::{Arc, Mutex, MutexGuard};

/// The control-plane database. One connection behind a mutex — control
/// operations are point reads/writes measured in microseconds against a
/// local/regional Postgres; contention here is never the bottleneck (the
/// data plane is S3).
#[derive(Clone)]
pub struct ControlDb {
    conn: Arc<Mutex<ClientBox>>,
}

/// Writes that would wait on another session's lock fail after this many
/// milliseconds instead of hanging a request thread. Overridable with
/// `STRATUM_DB_LOCK_TIMEOUT_MS` (tests use a short value to exercise the
/// database-failure arms deterministically).
const DEFAULT_LOCK_TIMEOUT_MS: u64 = 5000;

/// Cluster-wide advisory lock key serializing migrations: two server
/// processes booting against the same database must not interleave DDL.
const MIGRATE_LOCK_KEY: i64 = 0x5354_5241_5455_4D01; // "STRATUM\x01"

const MIGRATIONS: &[&str] = &[
    // 0001: registry + auth + audit
    r#"
    CREATE TABLE orgs (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        created_at BIGINT NOT NULL
    );
    CREATE TABLE repos (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        name TEXT NOT NULL,
        kind TEXT NOT NULL CHECK (kind IN ('native','mirror')),
        state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active','deleted')),
        public BOOLEAN NOT NULL DEFAULT false,
        default_branch TEXT NOT NULL DEFAULT 'main',
        origin_url TEXT,
        origin_provider TEXT,
        origin_installation TEXT,
        last_sync_at BIGINT,
        last_synced_commit TEXT,
        sync_error TEXT,
        created_at BIGINT NOT NULL,
        deleted_at BIGINT
    );
    CREATE UNIQUE INDEX repos_org_name ON repos(org_id, name) WHERE state = 'active';
    CREATE INDEX repos_org_id ON repos(org_id, id);
    CREATE TABLE tokens (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        hash TEXT NOT NULL,
        scopes TEXT NOT NULL,
        repo_id TEXT,
        label TEXT,
        created_at BIGINT NOT NULL,
        revoked_at BIGINT
    );
    CREATE TABLE audit_log (
        seq BIGSERIAL PRIMARY KEY,
        at BIGINT NOT NULL,
        org_id TEXT NOT NULL,
        repo_id TEXT,
        principal TEXT NOT NULL,
        action TEXT NOT NULL,
        context TEXT
    );
    CREATE INDEX audit_org_at ON audit_log(org_id, at);
    "#,
    // 0002: background jobs (export, compaction, GC) — lease-shaped so a
    // multi-node fleet claims work safely (FOR UPDATE SKIP LOCKED).
    r#"
    CREATE TABLE jobs (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL,
        repo_id TEXT,
        kind TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT 'queued'
            CHECK (state IN ('queued','running','done','failed')),
        payload TEXT,
        result TEXT,
        error TEXT,
        attempts BIGINT NOT NULL DEFAULT 0,
        lease_until BIGINT,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL
    );
    CREATE INDEX jobs_state ON jobs(state, lease_until);
    CREATE INDEX jobs_org ON jobs(org_id, id);
    "#,
    // 0003: metrics, billing usage, repo webhooks, worker cursors, plans.
    r#"
    ALTER TABLE orgs ADD COLUMN plan TEXT NOT NULL DEFAULT 'free';
    CREATE TABLE metrics_minute (
        repo_id TEXT NOT NULL,
        minute BIGINT NOT NULL,
        kind TEXT NOT NULL,
        count BIGINT NOT NULL DEFAULT 0,
        bytes BIGINT NOT NULL DEFAULT 0,
        ms_sum BIGINT NOT NULL DEFAULT 0,
        histogram TEXT,
        PRIMARY KEY (repo_id, minute, kind)
    );
    CREATE TABLE usage_daily (
        org_id TEXT NOT NULL,
        day TEXT NOT NULL,
        active_repos BIGINT NOT NULL,
        total_repos BIGINT NOT NULL,
        requests BIGINT NOT NULL,
        bytes_out BIGINT NOT NULL,
        reported_at BIGINT,
        PRIMARY KEY (org_id, day)
    );
    CREATE TABLE webhook_subscriptions (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL,
        repo_id TEXT NOT NULL,
        url TEXT NOT NULL,
        secret TEXT NOT NULL,
        created_at BIGINT NOT NULL
    );
    CREATE INDEX webhook_subs_repo ON webhook_subscriptions(repo_id);
    CREATE TABLE webhook_deliveries (
        id TEXT PRIMARY KEY,
        subscription_id TEXT NOT NULL,
        event TEXT NOT NULL,
        state TEXT NOT NULL,
        attempts BIGINT NOT NULL DEFAULT 0,
        last_error TEXT,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL
    );
    CREATE TABLE meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // 0004: SSH public keys. A key authenticates AS an existing token's
    // principal — scopes, repo binding, and revocation all reuse the token
    // machinery (revoke the key OR its token; access dies on the next
    // connection). Uniqueness is on ACTIVE fingerprints only so a revoked
    // key can be deliberately re-registered.
    r#"
    CREATE TABLE ssh_keys (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        token_id TEXT NOT NULL REFERENCES tokens(id),
        algo TEXT NOT NULL,
        pubkey TEXT NOT NULL,
        fingerprint_sha256 TEXT NOT NULL,
        label TEXT,
        created_at BIGINT NOT NULL,
        revoked_at BIGINT
    );
    CREATE UNIQUE INDEX ssh_keys_active_fingerprint
        ON ssh_keys(fingerprint_sha256) WHERE revoked_at IS NULL;
    CREATE INDEX ssh_keys_org ON ssh_keys(org_id, id);
    "#,
    // 0005 — people.
    //
    // Until now the identity atom was a token owned by an org. A token
    // cannot answer "what did Alice do?", cannot be handed to one person
    // and revoked without disturbing others, and makes an SSH key belong
    // to a credential rather than an owner. Everything below hangs off
    // `users`; tokens and ssh_keys gain a nullable owner so existing
    // org-level service credentials keep working untouched.
    r#"
    CREATE TABLE users (
        id TEXT PRIMARY KEY,
        email TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL,
        -- NULL is legitimate: an account created by OAuth alone has no
        -- password, and one whose password is cleared can still sign in
        -- through a linked identity.
        password_hash TEXT,
        created_at BIGINT NOT NULL,
        disabled_at BIGINT
    );

    -- Linked sign-in providers. Unused today; present so adding OAuth is
    -- a handler, not a migration on a live table.
    CREATE TABLE identities (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL REFERENCES users(id),
        provider TEXT NOT NULL,
        provider_user_id TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        UNIQUE (provider, provider_user_id)
    );

    CREATE TABLE org_members (
        org_id TEXT NOT NULL REFERENCES orgs(id),
        user_id TEXT NOT NULL REFERENCES users(id),
        role TEXT NOT NULL CHECK (role IN ('owner','admin','member','viewer')),
        created_at BIGINT NOT NULL,
        PRIMARY KEY (org_id, user_id)
    );
    CREATE INDEX org_members_user ON org_members(user_id);

    -- Per-repo override of the org role: the contractor who gets exactly
    -- one repo, or the teammate held to read on a sensitive one.
    CREATE TABLE repo_grants (
        repo_id TEXT NOT NULL REFERENCES repos(id),
        user_id TEXT NOT NULL REFERENCES users(id),
        role TEXT NOT NULL CHECK (role IN ('admin','member','viewer')),
        created_at BIGINT NOT NULL,
        PRIMARY KEY (repo_id, user_id)
    );
    CREATE INDEX repo_grants_user ON repo_grants(user_id);

    -- Only the hash is stored, exactly like tokens: an invite link is a
    -- bearer credential for the duration of its life.
    CREATE TABLE invites (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        email TEXT NOT NULL,
        role TEXT NOT NULL CHECK (role IN ('owner','admin','member','viewer')),
        token_hash TEXT NOT NULL,
        created_by TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        accepted_at BIGINT
    );
    CREATE INDEX invites_org ON invites(org_id, id);
    CREATE UNIQUE INDEX invites_pending_email
        ON invites(org_id, lower(email)) WHERE accepted_at IS NULL;

    CREATE TABLE sessions (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL REFERENCES users(id),
        token_hash TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        last_seen_at BIGINT NOT NULL,
        revoked_at BIGINT
    );
    CREATE INDEX sessions_user ON sessions(user_id, id);

    -- NULL owner = an org-level service token, which is what every token
    -- minted before this migration is.
    ALTER TABLE tokens ADD COLUMN user_id TEXT REFERENCES users(id);
    CREATE INDEX tokens_org ON tokens(org_id, id);
    CREATE INDEX tokens_user ON tokens(user_id);

    -- A key may now belong to a person directly rather than only to a
    -- token, so the token binding stops being mandatory.
    ALTER TABLE ssh_keys ADD COLUMN user_id TEXT REFERENCES users(id);
    ALTER TABLE ssh_keys ALTER COLUMN token_id DROP NOT NULL;
    CREATE INDEX ssh_keys_user ON ssh_keys(user_id);
    "#,
    // 0006 — the audit trail names people.
    //
    // `principal` already spells who acted, but as text: "user:01hx…" or
    // "token:01hx…". Answering "what did Alice do?" by matching a prefix
    // works and reads badly, and it cannot join to her name. A real
    // column can, and is indexed for the query the dashboard actually
    // runs — one org, one person, newest first.
    //
    // Existing rows keep their NULL: they were written before people
    // existed and the log is append-only, so they are not rewritten to
    // claim an author they never had.
    r#"
    ALTER TABLE audit_log ADD COLUMN user_id TEXT REFERENCES users(id);
    CREATE INDEX audit_org_user ON audit_log(org_id, user_id, seq);
    CREATE INDEX audit_org_action ON audit_log(org_id, action, seq);
    "#,
    // 0007 — teams.
    //
    // Per-repo access was one row per (repo, person), granted one person
    // at a time through the API alone. That does not survive a real org:
    // "the payments squad can write here" is one statement, and repeating
    // it per person means it drifts the moment somebody joins.
    //
    // Team grants live in their own table rather than as a nullable
    // subject column on `repo_grants`, because the two rules are
    // deliberately different and should not be able to be confused for
    // one another: a per-user grant *replaces* the org role in either
    // direction, while a team grant only ever raises. Two tables keep
    // that visible in the schema instead of hiding it in a WHERE clause.
    //
    // Team names are case-folded like emails already are: `Payments` and
    // `payments` naming two teams in one org is a mistake, not a feature.
    // ON DELETE CASCADE on both membership tables is the point of
    // deleting a team — access has to go with it, on the next request.
    r#"
    CREATE TABLE teams (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        name TEXT NOT NULL,
        description TEXT,
        created_at BIGINT NOT NULL
    );
    CREATE UNIQUE INDEX teams_org_name ON teams(org_id, lower(name));
    CREATE TABLE team_members (
        team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
        user_id TEXT NOT NULL REFERENCES users(id),
        created_at BIGINT NOT NULL,
        PRIMARY KEY (team_id, user_id)
    );
    CREATE INDEX team_members_user ON team_members(user_id);
    CREATE TABLE repo_team_grants (
        repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
        role TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        PRIMARY KEY (repo_id, team_id)
    );
    CREATE INDEX repo_team_grants_team ON repo_team_grants(team_id);
    "#,
    // 0008 — a personal SSH key names a person, not an org.
    //
    // A defect shipped with per-user keys: `ssh_keys.org_id` was NOT
    // NULL, and the fingerprint index is globally unique, so a person in
    // two namespaces registered their laptop key in the first and was
    // refused in the second with "this key is already registered" —
    // able to clone from one namespace only. Survivable while orgs were
    // operator-provisioned and rare; unavoidable the moment everybody
    // has at least two (their own, plus any org they join).
    //
    // The fix is to stop the row carrying a namespace. A personal key
    // resolves to the *person*, and which namespace they are reaching is
    // decided from the repository path, where membership is checked
    // anyway — exactly what every other credential already does. Deploy
    // keys belong to a token, which is org-bound by construction, and
    // keep their org.
    //
    // The global fingerprint index stays. It is not what caused this,
    // and it is load-bearing: two rows for one fingerprint would make
    // "who is this?" ambiguous, and whoever held the private key could
    // choose. Same rule GitHub enforces, for the same reason.
    r#"
    ALTER TABLE ssh_keys ALTER COLUMN org_id DROP NOT NULL;
    UPDATE ssh_keys SET org_id = NULL WHERE user_id IS NOT NULL;
    "#,
    // 0009 — namespaces: personal ones, and names that mean one thing.
    //
    // `orgs` stays the namespace table and grows a `kind`. A personal
    // namespace is an orgs row owned by a person that cannot have
    // members, teams or invites. That is how GitHub models it — both
    // sides are "owners" — and it means every one of the nine
    // org_id-bearing tables, both authorization seams, all three front
    // doors and the object-store prefix keep working untouched. A
    // parallel owner table would have forked `org_or_404`,
    // `repo_or_404`, `wire_repo_for_principal`, `authx::require` and
    // `Repo::prefix` for no gain.
    //
    // Two things that only mattered once strangers pick their own names:
    //
    // * **Case.** `orgs.name` was a plain UNIQUE, so `Acme` and `acme`
    //   were two namespaces — a confusion and a squatting vector.
    //   Case-folded now, the way `invites_pending_email` already was.
    //   The stored name keeps its capitals; only uniqueness and lookup
    //   fold.
    // * **`users.handle`** is a person's namespace name, and it lives in
    //   `orgs` like any other, so the same index does the collision work
    //   for both. The column here is the back-reference.
    r#"
    ALTER TABLE orgs ADD COLUMN kind TEXT NOT NULL DEFAULT 'org'
        CHECK (kind IN ('org','personal'));
    ALTER TABLE orgs ADD COLUMN owner_user_id TEXT REFERENCES users(id);
    -- The old plain-UNIQUE is a *constraint*, and the index behind it
    -- cannot be dropped on its own; the constraint has to go first.
    ALTER TABLE orgs DROP CONSTRAINT IF EXISTS orgs_name_key;
    CREATE UNIQUE INDEX orgs_name_folded ON orgs(lower(name));
    CREATE UNIQUE INDEX orgs_personal_owner ON orgs(owner_user_id)
        WHERE kind = 'personal';
    ALTER TABLE users ADD COLUMN handle TEXT;
    "#,
    // 0010 — proving an address, and getting back in without one.
    //
    // Signup takes an address from a stranger, so it has to be proved
    // before that address can cost anything: an unverified account may
    // sign in and look around, but not create a repo or a namespace.
    // That is the line GitHub draws and it is the one that matters for
    // abuse — everything cheap stays open, everything with a cost does
    // not.
    //
    // `user_tokens` carries both proofs, because they are the same
    // machinery: a random secret, only its hash stored, single-use, and
    // expiring. `kind` keeps them apart, so a verification link cannot
    // be redeemed as a password reset even though both name the same
    // person. The partial index is on the live ones only — a spent or
    // expired row is history, not a credential.
    //
    // **Every account that already exists is marked verified.** They
    // predate this and no message was ever sent to them; leaving them
    // unverified would lock an operator out of the org they bootstrapped
    // with the admin CLI, which is a regression dressed as a security
    // improvement.
    r#"
    ALTER TABLE users ADD COLUMN verified_at BIGINT;
    UPDATE users SET verified_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT;

    CREATE TABLE user_tokens (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL REFERENCES users(id),
        kind TEXT NOT NULL CHECK (kind IN ('verify','reset')),
        token_hash TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        used_at BIGINT
    );
    CREATE INDEX user_tokens_live ON user_tokens(user_id, kind)
        WHERE used_at IS NULL;
    "#,
    // 0011 — organizations are paid for, and a seat is a person.
    //
    // The subscription belongs to the org, not to whoever set it up:
    // owners change, and a bill that follows a person out of the door is
    // a bill nobody can find. One row per org, so "is this org paid for"
    // is a primary-key lookup rather than a scan of somebody's payment
    // history.
    //
    // `plan` stops being unvalidated free text. It has exactly one
    // producer now — the subscription's status — and the CHECK is what
    // keeps a typo in the admin CLI from silently granting a quota. Rows
    // that predate this are normalised first: anything that was not
    // literally `free` was some operator's word for "paid", and the only
    // behaviour ever attached to the column was a repo cap on `free`.
    //
    // `seats` is what the provider has been told to bill for, which is
    // deliberately *not* the same number as `billing::billable_seats`
    // computes. The difference between them is the thing that has to be
    // reconciled, and a schema that stored only one of them could not
    // show it.
    r#"
    UPDATE orgs SET plan = 'paid' WHERE plan <> 'free';
    ALTER TABLE orgs ADD CONSTRAINT orgs_plan_known
        CHECK (plan IN ('free','paid','past_due'));

    CREATE TABLE subscriptions (
        org_id TEXT PRIMARY KEY REFERENCES orgs(id),
        provider TEXT NOT NULL,
        customer_ref TEXT,
        subscription_ref TEXT,
        item_ref TEXT,
        status TEXT NOT NULL,
        seats BIGINT NOT NULL DEFAULT 0,
        current_period_end BIGINT,
        updated_at BIGINT NOT NULL
    );
    -- One org per subscription, both ways: a provider event names the
    -- subscription, and it must resolve to exactly one org.
    CREATE UNIQUE INDEX subscriptions_ref ON subscriptions(subscription_ref)
        WHERE subscription_ref IS NOT NULL;

    -- Provider events arrive more than once, out of order, and
    -- occasionally forged. Recording the ones we have acted on is what
    -- makes replaying one a no-op instead of a second seat change.
    CREATE TABLE billing_events (
        id TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        received_at BIGINT NOT NULL
    );
    "#,
    // 0012 — an org connects GitHub once, not once per repository.
    //
    // `repos.origin_installation` has been there since the mirror SKU
    // shipped, and it is per repository, so the installation id gets
    // typed again for every repo somebody mirrors — an opaque number
    // dug out of a GitHub settings URL, by hand, every time. It stays
    // (a repo does need to know which installation fetches it) and this
    // table is where it comes *from*.
    //
    // The primary key is (org, provider, installation): one org can
    // install the app on several GitHub accounts — a personal account
    // and two organizations is ordinary — and each is a separate source
    // of repositories to pick from.
    //
    // `installation_id` is also unique on its own, per provider. An
    // installation belongs to exactly one GitHub account, and binding
    // it to a second org would let that org read the first org's
    // repositories. The index is what makes that a database error
    // instead of a policy anyone could forget to apply.
    //
    // `states` is the anti-CSRF for the install round trip. GitHub sends
    // the browser back with an `installation_id` and whatever `state` we
    // gave it, and nothing else identifies who started the flow — so a
    // forged callback would bind an attacker's installation to your
    // org, or yours to theirs. Rows are single-use (`spent_at`) and
    // short-lived, and the secret is hashed at rest like every other
    // credential in this schema.
    r#"
    CREATE TABLE org_installations (
        org_id TEXT NOT NULL REFERENCES orgs(id),
        provider TEXT NOT NULL,
        installation_id TEXT NOT NULL,
        account TEXT,
        created_by TEXT REFERENCES users(id),
        created_at BIGINT NOT NULL,
        PRIMARY KEY (org_id, provider, installation_id)
    );
    -- An installation names one GitHub account. Two orgs claiming it is
    -- one org reading the other's code.
    CREATE UNIQUE INDEX org_installations_unique
        ON org_installations(provider, installation_id);
    CREATE INDEX org_installations_org ON org_installations(org_id);

    CREATE TABLE install_states (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        provider TEXT NOT NULL,
        secret_hash TEXT NOT NULL,
        created_by TEXT REFERENCES users(id),
        created_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        spent_at BIGINT
    );
    CREATE INDEX install_states_org ON install_states(org_id);
    "#,
    // 0013: repo search — a sentence about the repo, and the indexes
    // that make "find it by name" fast.
    r#"
    ALTER TABLE repos ADD COLUMN description TEXT;

    -- Search matches case-insensitively, so the index has to be on the
    -- folded value or it is never used.
    CREATE INDEX repos_name_lower ON repos(lower(name)) WHERE state = 'active';

    -- Trigram acceleration, and deliberately best-effort. `LIKE '%q%'`
    -- is correct with or without these; GIN + gin_trgm_ops only stops it
    -- being a sequential scan. A managed PostgreSQL that refuses
    -- CREATE EXTENSION to a non-superuser must not stop the server
    -- booting, and the alternative — a Rust-side try/ignore — puts a
    -- branch nothing can reach in a test around it. The subtransaction
    -- rolls back just this block, so a refusal costs the index and
    -- nothing else.
    DO $mig$
    BEGIN
        CREATE EXTENSION IF NOT EXISTS pg_trgm;
        CREATE INDEX IF NOT EXISTS repos_name_trgm
            ON repos USING GIN (lower(name) gin_trgm_ops);
        CREATE INDEX IF NOT EXISTS repos_description_trgm
            ON repos USING GIN (lower(description) gin_trgm_ops);
    EXCEPTION WHEN OTHERS THEN
        RAISE NOTICE 'weft: trigram search indexes unavailable (%) — repo search still works, by scan', SQLERRM;
    END
    $mig$;
"#,
    // 0014 — changes, patchsets, approvals: stack-native review.
    //
    // The unit of review is one commit, keyed by its Change-Id trailer
    // (or a key derived from the commit oid when there is none), so a
    // change's identity survives rebase and amend while its content
    // moves through numbered patchsets. Approvals attach to a patchset,
    // never to the change: pushing a revision means the recorded
    // approvals no longer describe what would land, so sufficiency reads
    // only the latest patchset's approvals. `revoked_at` keeps revoked
    // rows in place — an approval is authority-moving history, and the
    // partial unique index makes "one active approval per person per
    // patchset" a database fact rather than an application habit.
    //
    // There is no queue table: a change in state `landing` with its
    // `land_job_id` IS the queue entry, and the `jobs` table (0002)
    // supplies claim/lease semantics. One lease implementation is
    // enough. `land_verdict` holds the latest outcome in the exact words
    // the API and dashboard show ("landed", "ejected: not fast-forward
    // from …"); the full history is in the audit log.
    r#"
    CREATE TABLE changes (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        change_key TEXT NOT NULL,
        title TEXT NOT NULL,
        target_branch TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT 'open'
            CHECK (state IN ('open', 'landing', 'landed', 'abandoned')),
        land_job_id TEXT,
        land_verdict TEXT,
        landed_commit TEXT,
        created_by TEXT REFERENCES users(id),
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL
    );
    CREATE UNIQUE INDEX changes_repo_key ON changes(repo_id, change_key);
    CREATE INDEX changes_repo_state ON changes(repo_id, state, id);
    CREATE TABLE patchsets (
        id TEXT PRIMARY KEY,
        change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
        number BIGINT NOT NULL,
        commit_oid TEXT NOT NULL,
        parent_oid TEXT,
        message TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        UNIQUE (change_id, number),
        UNIQUE (change_id, commit_oid)
    );
    CREATE TABLE approvals (
        id TEXT PRIMARY KEY,
        change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
        patchset_id TEXT NOT NULL REFERENCES patchsets(id) ON DELETE CASCADE,
        user_id TEXT NOT NULL REFERENCES users(id),
        created_at BIGINT NOT NULL,
        revoked_at BIGINT
    );
    CREATE UNIQUE INDEX approvals_active
        ON approvals(patchset_id, user_id) WHERE revoked_at IS NULL;
    CREATE INDEX approvals_change ON approvals(change_id);
    "#,
    // 0015 — change comments: the words of a review, not just its verdict.
    //
    // An approval says "yes"; a comment says why not yet — without it, a
    // reviewer's only move is silent refusal, and the author's only move
    // is guessing. Comments belong to the change (the conversation
    // outlives revisions) but record the patchset they were written
    // against, so "this was about patchset 1" stays visible after
    // patchset 2 lands. `path` optionally anchors a comment to a file.
    //
    // `author_user_id` is nullable: agents and service tokens review too
    // (CI posting "the perf suite regressed" is a comment, not an
    // approval), and the principal string always records who spoke.
    r#"
    CREATE TABLE change_comments (
        id TEXT PRIMARY KEY,
        change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
        patchset_number BIGINT NOT NULL,
        author_principal TEXT NOT NULL,
        author_user_id TEXT REFERENCES users(id),
        path TEXT,
        body TEXT NOT NULL,
        created_at BIGINT NOT NULL
    );
    CREATE INDEX change_comments_change ON change_comments(change_id, created_at);
    "#,
    // 0016 — branch protection + line-anchored comments.
    //
    // A land queue whose target anyone can `git push` around is theatre:
    // the verdict says "needs an owner", the push says "or not". A row
    // here means exactly one thing — this branch moves only through the
    // land queue (the lander's ref transaction, which re-checks
    // sufficiency at claim time). Every other write door — wire push,
    // SSH push, the commits API, reset/revert/branch-delete — refuses
    // with the same sentence.
    //
    // `line` turns a file comment into a line comment: "why is this
    // retry unbounded?" belongs on the line with the loop, not in a
    // paragraph that names the file and hopes.
    r#"
    CREATE TABLE branch_protections (
        repo_id TEXT NOT NULL REFERENCES repos(id),
        branch TEXT NOT NULL,
        created_by TEXT,
        created_at BIGINT NOT NULL,
        PRIMARY KEY (repo_id, branch)
    );
    ALTER TABLE change_comments ADD COLUMN line BIGINT;
    "#,
    // 0017 — conversations read in insertion order, not coin-flip order.
    //
    // Comments were ordered by (created_at, id): created_at is
    // millisecond wall-clock and a ULID's tail is random within one
    // millisecond, so two comments posted in the same millisecond could
    // render swapped — CI proved it by losing that coin flip. In a
    // conversation, order *is* meaning; the database's own sequence
    // records who spoke first.
    r#"
    ALTER TABLE change_comments ADD COLUMN seq BIGSERIAL;
    "#,
    // 0018 — CI checks: the machine's verdict beside the human one.
    //
    // External CI hears about work over webhooks, runs whatever it
    // runs, and reports here per patchset — approvals stay human-only,
    // but reporting a build is exactly what a service token is for.
    // One row per (patchset, name): re-posting "ci/tests" updates in
    // place, because a check is current state, not a journal. A new
    // patchset starts with no rows at all — yesterday's green run says
    // nothing about code it never built.
    r#"
    CREATE TABLE change_checks (
        id TEXT PRIMARY KEY,
        change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
        patchset_id TEXT NOT NULL,
        name TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('pending','passing','failing')),
        detail_url TEXT,
        posted_by TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL,
        UNIQUE (patchset_id, name)
    );
    CREATE INDEX change_checks_change ON change_checks(change_id);
    "#,
    // 0019 — one sweep per repo, enforced by the database.
    //
    // The compactor and the CDN packer enqueue with "is one already
    // active?" followed by "then insert one" — two statements with
    // nothing holding between them. Two nodes accepting two pushes in
    // the same instant both read `false` and both insert, and the claim
    // side then does exactly what it should: `FOR UPDATE SKIP LOCKED`
    // hands the two *different* rows to two *different* workers, which
    // fold the same prefix at the same time. The dedup has to be a
    // constraint, not a habit.
    //
    // The index is deliberately not on every kind. It covers the two
    // whose work is a *sweep of the repo's current state* — running it
    // twice is pure waste and running it concurrently is a race, and one
    // run always subsumes an enqueue that arrived while it was queued:
    //
    //   * `compact`  — fold the WAL into a fresh epoch
    //   * `cdnpack`  — rebuild the pack at the current tip
    //
    // Everything else names a unit of its own and must be free to have
    // several rows live at once:
    //
    //   * `export`   — one bundle per request; the caller polls *its*
    //                  job id, so collapsing two into one would hand a
    //                  requester somebody else's job or none at all
    //   * `land`     — one per change, and a repo with two approved
    //                  changes lands both; a singleton index would wedge
    //                  the land queue at one change per repo
    //   * `compact-now`, `cdnpack-now` — the synchronous operator
    //                  triggers, which create a row purely to have
    //                  something to record the outcome on and run it
    //                  inline; two operators must not 500 each other
    //
    // `repo_id` is nullable and the predicate excludes NULL. NULLs are
    // already distinct to a unique index, so org-scoped rows would be
    // unaffected either way — saying so in the predicate keeps the index
    // small and the intent readable.
    //
    // Rows that predate the constraint are reconciled first: the oldest
    // active row per (kind, repo) survives — it is the one a worker may
    // already be running — and the duplicates it subsumes are closed.
    r#"
    UPDATE jobs SET state = 'done',
        result = 'superseded: duplicate active sweep (fleet enqueue race)',
        updated_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
    WHERE kind IN ('compact', 'cdnpack')
      AND repo_id IS NOT NULL
      AND state IN ('queued', 'running')
      AND id NOT IN (
          SELECT DISTINCT ON (kind, repo_id) id FROM jobs
          WHERE kind IN ('compact', 'cdnpack')
            AND repo_id IS NOT NULL
            AND state IN ('queued', 'running')
          ORDER BY kind, repo_id, created_at
      );
    CREATE UNIQUE INDEX jobs_active_per_repo ON jobs (kind, repo_id)
        WHERE repo_id IS NOT NULL
          AND kind IN ('compact', 'cdnpack')
          AND state IN ('queued', 'running');
    "#,
    // 0020 — identity: a person as a public face, and the addresses
    // their commits are signed with.
    //
    // Two different things live here and only one of them is cosmetic.
    //
    // **The profile columns** hang off `users` rather than a side table
    // because there is exactly one row per person either way, and a
    // `user_profiles` table would mean every profile read is a LEFT JOIN
    // that can miss — which is how "this account has no profile" becomes
    // a distinct state from "this profile is empty", and then two code
    // paths render the same page.
    //
    // **`user_emails` is the load-bearing one**, and the reason this
    // migration lands before anything reads commit authorship.
    // `users.email` is exactly one address and it is the *account
    // credential*: it is what sign-in, password reset and every
    // transactional mail use. The addresses in a commit's author line
    // are a different set entirely — a person has one login and half a
    // dozen `git config user.email` values accumulated over a decade,
    // and a migrant's history is only theirs if those map back to the
    // account. Retrofitting this table after a contribution graph
    // existed would mean rebucketing every row it had already counted,
    // so it lands now, empty of consequence.
    //
    // `verified_at` per address is the anti-gaming rule, and it is
    // per-address rather than per-account on purpose: anyone can put any
    // string in `git config user.email`, so an unproved address is a
    // claim, not a fact, and **only rows with `verified_at IS NOT NULL`
    // may ever count toward authorship**. `private` defaults to true
    // because an address is a spam target and nobody adds one here in
    // order to publish it.
    //
    // The verification challenge lives on the row rather than in
    // `user_tokens` beside the signup and reset links, and the
    // difference is the thing being proved. A `user_tokens` row names a
    // *person*; redeeming one proves you can read some mailbox of
    // theirs. That is the right proof for "this account is real" and the
    // wrong one here: with the address supplied by the caller at redeem
    // time, somebody could add `attacker@theirs` and `victim@corp`,
    // click the link that arrives in their own inbox, and name the
    // victim's address when spending it — attaching a colleague's commit
    // history to their account with no proof at all. The token has to
    // bind the address, so it is stored against the address. Everything
    // else about it is the same machinery: a random secret, only its
    // SHA-256 stored, single-use, expiring.
    //
    // The backfill takes each account's credential address as an owned
    // one, carrying `users.verified_at` across: the account's own proof
    // is a proof of that mailbox and re-mailing every existing user to
    // establish something already established would be theatre.
    //
    // `pinned_items` and `org_profiles` key on `orgs`, not on `users`,
    // because a namespace is a namespace — 0009's decision. That is what
    // lets one table serve `/{handle}` and `/{org}` instead of two that
    // drift.
    r#"
    ALTER TABLE users ADD COLUMN display_name TEXT;
    ALTER TABLE users ADD COLUMN bio TEXT;
    ALTER TABLE users ADD COLUMN location TEXT;
    ALTER TABLE users ADD COLUMN company TEXT;
    ALTER TABLE users ADD COLUMN avatar_key TEXT;
    ALTER TABLE users ADD COLUMN pronouns TEXT;
    ALTER TABLE users ADD COLUMN kind TEXT NOT NULL DEFAULT 'human'
        CHECK (kind IN ('human','agent'));
    ALTER TABLE users ADD COLUMN contrib_private_optin BOOLEAN NOT NULL DEFAULT false;
    ALTER TABLE users ADD COLUMN profile_repo TEXT;

    CREATE TABLE user_links (
        id TEXT PRIMARY KEY,
        user_id TEXT NOT NULL REFERENCES users(id),
        label TEXT,
        url TEXT NOT NULL,
        position INT NOT NULL,
        created_at BIGINT NOT NULL
    );
    CREATE INDEX user_links_user ON user_links(user_id, position);

    CREATE TABLE user_emails (
        -- Normalized (lowercased, trimmed) and the primary key, so one
        -- mailbox can belong to at most one account platform-wide. The
        -- collision is a 409 that never says whose it is.
        address TEXT PRIMARY KEY,
        user_id TEXT NOT NULL REFERENCES users(id),
        verified_at BIGINT,
        private BOOLEAN NOT NULL DEFAULT true,
        created_at BIGINT NOT NULL,
        verify_hash TEXT,
        verify_expires_at BIGINT
    );
    CREATE INDEX user_emails_user ON user_emails(user_id, address);
    -- The authorship lookup, and the only index it is allowed to use:
    -- an unverified address is not in it.
    CREATE INDEX user_emails_verified ON user_emails(address)
        WHERE verified_at IS NOT NULL;

    INSERT INTO user_emails (address, user_id, verified_at, private, created_at)
    SELECT email, id, verified_at, true, created_at FROM users;

    CREATE TABLE org_profiles (
        org_id TEXT PRIMARY KEY REFERENCES orgs(id),
        display_name TEXT,
        description TEXT,
        location TEXT,
        website TEXT,
        contact_email TEXT,
        avatar_key TEXT,
        updated_at BIGINT NOT NULL
    );

    CREATE TABLE pinned_items (
        id TEXT PRIMARY KEY,
        owner_org_id TEXT NOT NULL REFERENCES orgs(id),
        kind TEXT NOT NULL CHECK (kind IN ('repo','change')),
        target_id TEXT NOT NULL,
        position INT NOT NULL,
        created_at BIGINT NOT NULL
    );
    CREATE UNIQUE INDEX pinned_items_owner_target
        ON pinned_items(owner_org_id, kind, target_id);
    CREATE INDEX pinned_items_owner ON pinned_items(owner_org_id, position);
    "#,
    // 0021 — who hears about what.
    //
    // A review flow where nobody is told anything happened does not
    // function: a change sits in the land queue until somebody happens
    // to look at it. This is the table that fixes that, and it is
    // deliberately the *first* social feature, ahead of anything with a
    // visible surface.
    //
    // Three levels, and they are GitHub's, because a maintainer already
    // knows what they mean: everything, only what you are involved in,
    // or nothing. **The absence of a row means `participating`** — the
    // default is not stored, so nobody has to be enrolled and a new
    // member is already subscribed to the right amount. Only a
    // deliberate choice writes a row, which also means the table stays
    // small: it holds the people who wanted something other than the
    // sensible thing.
    //
    // What "participating" resolves to is the part GitHub cannot do.
    // There, it means you commented or were @mentioned. Here it also
    // means **the change needs you** — the repository's OWNERS file
    // decides which reviewers a change actually requires, so the people
    // who must act are told, and the people who must not are left alone.
    // That is the whole maintainer-attention argument, expressed as a
    // recipient list.
    //
    // The partial index carries only the `all` watchers because that is
    // the one set that has to be read on every event; participants are
    // computed from the change itself, and `ignore` is a subtraction.
    r#"
    CREATE TABLE repo_watches (
        repo_id    TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        level      TEXT NOT NULL CHECK (level IN ('all','participating','ignore')),
        updated_at BIGINT NOT NULL,
        PRIMARY KEY (repo_id, user_id)
    );
    CREATE INDEX repo_watches_all ON repo_watches(repo_id) WHERE level = 'all';
    CREATE INDEX repo_watches_user ON repo_watches(user_id);
    "#,
    // 0022 — who starred what, and whose number it is.
    //
    // Two counts live on `repos` and they are never added together.
    // `stars` is ours: people who starred this repository here, one row
    // each in `repo_stars`. `origin_stars` is somebody else's: what the
    // upstream said when we last mirrored it. A mirrored project shows
    // both — its own count, and a separately labelled "60.3k on GitHub"
    // pointing at the origin — because the alternatives are both lies.
    // Showing only ours says a project with 116k stars is dead on the
    // day it is imported; summing them invents a number no one can
    // reproduce and quietly claims their reputation as our traffic.
    //
    // That is also why `origin_stars` is NULLABLE and `stars` is NOT
    // NULL DEFAULT 0. `NULL` means "we have no imported number for this
    // repository" — a native repo, or a mirror we have not read a count
    // from — and it is a different fact from `0`, which means the origin
    // told us zero. A column that could not tell those apart would force
    // the UI to render one of them as the other, and the rule this table
    // exists to serve is precisely that an absent count is omitted
    // rather than shown as a zero. `origin_stars_at` is when that number
    // was true, because an imported count is a snapshot and a stale one
    // presented as current is its own small lie.
    //
    // `stars` is denormalised onto `repos` and maintained in the same
    // transaction as the `repo_stars` insert or delete — not by a
    // trigger. This codebase has no triggers and should not grow its
    // first one for a counter: a trigger puts half the invariant
    // somewhere no reader of `stars.rs` will look. The CHECK is the
    // backstop that turns a drift bug into a failed write at the moment
    // it happens, rather than a number that is quietly wrong for months.
    r#"
    ALTER TABLE repos ADD COLUMN stars INT NOT NULL DEFAULT 0 CHECK (stars >= 0);
    ALTER TABLE repos ADD COLUMN origin_stars INT CHECK (origin_stars >= 0);
    ALTER TABLE repos ADD COLUMN origin_stars_at BIGINT;

    CREATE TABLE repo_stars (
        repo_id    TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        created_at BIGINT NOT NULL,
        PRIMARY KEY (repo_id, user_id)
    );
    -- Somebody's own starred list, newest first: the read behind
    -- "things I starred". The repo direction is served by the
    -- denormalised count and needs no index of its own.
    CREATE INDEX repo_stars_user ON repo_stars(user_id, created_at DESC);
    "#,
    // 0023 — which repository is reading whose bytes.
    //
    // A zero-copy fork does not copy anything. It writes its own
    // manifest and an `SLH4` locator pointing at *upstream's* data
    // prefix, and from then on two repositories read one set of
    // immutable objects. That is what makes a fork cost milliseconds
    // instead of a full ingest, and it is also what breaks epoch GC.
    //
    // GC decides what to delete from the live epoch set, and until now
    // that set came entirely from one prefix's own two pointers — the
    // manifest and the locator header. Upstream compacts, publishes a
    // fresh epoch, the old one stops being referenced by anything
    // upstream can see, one grace window passes, and every object under
    // it is deleted. The fork is still pointing at it. Nothing errors;
    // the fork simply stops being able to serve the objects it shares,
    // and the first symptom is a clone that fails `git fsck`.
    //
    // This table is the missing half of that question, and the resolver
    // that reads it is `EpochRefs` in `stratum-engine::gc`. One row per
    // (upstream epoch, forking repo).
    //
    // **The two delete behaviours are deliberately different, and they
    // are the safety argument.**
    //
    // `referencing_repo_id` CASCADEs: when a fork is deleted its claim
    // on upstream's epochs dies with it, and the storage becomes
    // collectable on the next pass. That is the common case and it
    // should need no cleanup job.
    //
    // `owner_repo_id` RESTRICTs, which is the unusual choice and the
    // important one. Deleting an upstream row while forks still read its
    // epochs would silently drop the very rows that were keeping their
    // data alive — the deletion would succeed, GC would find nothing
    // referenced, and the forks would be hollowed out one grace window
    // later. RESTRICT turns that into a failed write at the moment
    // somebody tries it. The plan's rule that `sweep_prefix` must refuse
    // while references exist then stops being a check in code that a
    // future caller can forget to make, and becomes something the
    // database will not let us get wrong. Deleting an upstream that has
    // forks goes through the promotion job, which materializes the
    // dependents and removes their references first; only then does the
    // delete succeed.
    //
    // `created_at` is not decoration: forks pinning old epochs
    // indefinitely is real storage drift, and the re-pointing job that
    // bounds it wants to find the oldest references first.
    r#"
    CREATE TABLE epoch_refs (
        owner_repo_id       TEXT NOT NULL REFERENCES repos(id) ON DELETE RESTRICT,
        epoch               TEXT NOT NULL,
        referencing_repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        created_at          BIGINT NOT NULL,
        PRIMARY KEY (owner_repo_id, epoch, referencing_repo_id)
    );
    -- The GC read: every epoch of one repository that anybody is
    -- reading. Served by the primary key's leading columns, so it needs
    -- no index of its own.
    --
    -- This one is the other direction — everything a fork depends on —
    -- read when a fork is deleted, re-pointed at a newer epoch, or
    -- promoted because its upstream is going away.
    CREATE INDEX epoch_refs_referencing
        ON epoch_refs(referencing_repo_id, created_at);
    "#,
    // 0024 — where a repository came from.
    //
    // Forking is the mechanism by which open source is open: it is how
    // somebody with no push credential contributes at all. Until now a
    // project here could be read by anybody and contributed to by
    // nobody outside its org.
    //
    // `fork_parent_id` is who we forked directly. `fork_root_id` is the
    // top of the chain, and it is **denormalised on purpose**: the
    // "fork network" listing is the one read that would otherwise be a
    // recursive CTE walking a chain of unknown depth on every page view,
    // and a fork of a fork of a fork is not rare among people trying to
    // land one patch. With the root stored, the network is one index
    // scan.
    //
    // Both are `ON DELETE SET NULL` rather than CASCADE, and the
    // difference is the whole point: deleting an upstream must never
    // delete the repositories forked from it. A fork whose parent is
    // gone has been promoted — it stands on its own storage and is its
    // own root — and `NULL` is exactly that statement. CASCADE here
    // would mean deleting your own repository silently destroyed
    // everybody else's work derived from it.
    //
    // `NULL` in both columns therefore means "not a fork", and a
    // repository is never its own parent or root — a self-reference
    // would make the network query return the repository twice and give
    // the promotion job a cycle to walk.
    //
    // `fork_state` tracks the job, because a zero-copy fork is fast but
    // not instant, and a repository that exists but is not yet readable
    // has to be able to say so rather than 404 confusingly. NULL for
    // repositories that were never forks.
    //
    // `fork_count` counts **direct** forks, not the network — it is the
    // number rendered on the repository page, maintained in the same
    // transaction as the fork row like `stars` is, and never by a
    // trigger. The CHECK is the backstop that turns a drift bug into a
    // failed write at the moment it happens rather than a number that is
    // quietly wrong for months.
    //
    // The rule that a fork of a private repository is private, and may
    // only be made public if its root is public, is **not** expressible
    // here — a CHECK cannot read another row. It lives in
    // `api/repos.rs::patch`, next to the audit record that already asks
    // who made this public and when. That is doubly load-bearing for a
    // zero-copy fork, which is literally serving upstream's bytes: a
    // fork flipped public would publish a private repository's objects
    // without ever copying them.
    r#"
    ALTER TABLE repos ADD COLUMN fork_parent_id TEXT
        REFERENCES repos(id) ON DELETE SET NULL;
    ALTER TABLE repos ADD COLUMN fork_root_id TEXT
        REFERENCES repos(id) ON DELETE SET NULL;
    ALTER TABLE repos ADD COLUMN fork_state TEXT
        CHECK (fork_state IN ('pending','ready','failed'));
    ALTER TABLE repos ADD COLUMN fork_count INT NOT NULL DEFAULT 0
        CHECK (fork_count >= 0);

    ALTER TABLE repos ADD CONSTRAINT repos_fork_not_self
        CHECK (fork_parent_id IS DISTINCT FROM id AND fork_root_id IS DISTINCT FROM id);

    -- "Forks of this repository": the count is denormalised, but the
    -- listing still has to find them.
    CREATE INDEX repos_fork_parent ON repos(fork_parent_id)
        WHERE fork_parent_id IS NOT NULL;
    -- The fork network, in one index scan rather than a recursive walk.
    CREATE INDEX repos_fork_root ON repos(fork_root_id)
        WHERE fork_root_id IS NOT NULL;
    "#,
    // 0025 — issues, comments and labels: the place a stranger says
    // something is broken.
    //
    // Filing an issue needs read access, not write, which is why the
    // author column is nullable in two different senses. `author_id`
    // names a local account; `author_label` carries the name an
    // imported issue was filed under by somebody who has no account
    // here. A native issue leaves `author_label` NULL so the join to
    // `users` is the single source of truth, and an unmapped importee is
    // never silently attributed to whichever local account happens to
    // share a display name.
    //
    // `number` is per repository and human-facing (`#42`). It is
    // allocated from `issue_counters` with `UPDATE … RETURNING` inside
    // the same transaction as the insert — never read-then-write, since
    // two people filing in the same second is the ordinary case and a
    // read-then-write would hand them both `#42` and fail one of them on
    // the unique index.
    //
    // `issue_comments.seq` is BIGSERIAL for migration 0017's reason: a
    // ULID's tail is random within a millisecond, so ordering a
    // conversation by (created_at, id) renders two same-millisecond
    // comments in an order nobody typed. In a conversation, order *is*
    // meaning; the database's own sequence records who spoke first.
    //
    // `labels.color` holds a **token name** from web/shared/tokens.css,
    // not a hex colour. A hex here renders wrong in the other theme and
    // no test can see it, because the database is the one place the
    // design-system contract cannot reach; the server validates the
    // name against the published `--series-*` set on the way in.
    r#"
    CREATE TABLE issues (
        id            TEXT PRIMARY KEY,
        repo_id       TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        number        INT  NOT NULL,
        title         TEXT NOT NULL,
        body          TEXT NOT NULL DEFAULT '',
        state         TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','closed')),
        author_id     TEXT REFERENCES users(id) ON DELETE SET NULL,
        author_label  TEXT,
        created_at    BIGINT NOT NULL,
        updated_at    BIGINT NOT NULL,
        closed_at     BIGINT,
        UNIQUE (repo_id, number)
    );
    CREATE INDEX issues_repo_state ON issues(repo_id, state, number DESC);

    CREATE TABLE issue_counters (
        repo_id     TEXT PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
        next_number INT NOT NULL DEFAULT 1
    );

    CREATE TABLE issue_comments (
        id           TEXT PRIMARY KEY,
        issue_id     TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
        seq          BIGSERIAL,
        body         TEXT NOT NULL,
        author_id    TEXT REFERENCES users(id) ON DELETE SET NULL,
        author_label TEXT,
        created_at   BIGINT NOT NULL,
        updated_at   BIGINT NOT NULL
    );
    CREATE INDEX issue_comments_issue ON issue_comments(issue_id, seq);

    CREATE TABLE labels (
        id          TEXT PRIMARY KEY,
        repo_id     TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        color       TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '',
        created_at  BIGINT NOT NULL,
        UNIQUE (repo_id, name)
    );

    CREATE TABLE issue_labels (
        issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
        label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
        PRIMARY KEY (issue_id, label_id)
    );
    "#,
    // 0026 — the two job kinds forks added, and why they are singletons.
    //
    // `jobs_active_per_repo` (0019) deduplicates only the kinds whose
    // work is a *sweep of the repo's current state*, where running twice
    // is waste and running concurrently is a race. Forking added two
    // more of exactly that shape and covered neither, so both could have
    // two live rows and two workers.
    //
    //   * `fork`    — write the fork's pointers, keyed on the **target**.
    //                 Repository-name uniqueness already stops a
    //                 double-clicked button making two forks, so what
    //                 this covers is two nodes claiming two rows for one
    //                 target and both writing its locator.
    //   * `promote` — give a fork storage of its own. Two concurrent runs
    //                 are two materialize-and-re-ingest passes racing a
    //                 manifest CAS: one wins and the other burns a full
    //                 re-ingest to lose.
    //
    // Both subsume a queued duplicate: one run leaves the repository in
    // the state the second was going to put it in.
    //
    // Pre-existing rows are reconciled first, exactly as 0019 did it —
    // the oldest active row per (kind, repo) survives, because it is the
    // one a worker may already be running.
    r#"
    UPDATE jobs SET state = 'done',
        result = 'superseded: duplicate active fork/promote job',
        updated_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
    WHERE kind IN ('fork', 'promote')
      AND repo_id IS NOT NULL
      AND state IN ('queued', 'running')
      AND id NOT IN (
          SELECT DISTINCT ON (kind, repo_id) id FROM jobs
          WHERE kind IN ('fork', 'promote')
            AND repo_id IS NOT NULL
            AND state IN ('queued', 'running')
          ORDER BY kind, repo_id, created_at
      );
    DROP INDEX jobs_active_per_repo;
    CREATE UNIQUE INDEX jobs_active_per_repo ON jobs (kind, repo_id)
        WHERE repo_id IS NOT NULL
          AND kind IN ('compact', 'cdnpack', 'fork', 'promote')
          AND state IN ('queued', 'running');
    "#,
    // ---- The open-source layer, part two ----
    //
    // Allocated in one migration deliberately. `MIGRATIONS` is an
    // index-ordered array, so two people appending at once collide on
    // the next index — and the work below is being built in parallel by
    // several tracks. One migration, written once, means no track needs
    // to open this file again.
    r#"
    -- A change whose patchset commits live in a **fork** rather than in
    -- the repository it targets.
    --
    -- This is the whole contribution path for somebody with no push
    -- credential: they fork (which they own), push there, and open a
    -- change against upstream. NULL means what it has always meant —
    -- the commits are already in the target repo — so every existing
    -- change is unaffected and the enterprise flow does not change.
    --
    -- ON DELETE SET NULL rather than CASCADE: if the fork is deleted the
    -- change is still a real thing that happened, and a landed change
    -- whose source vanished must not vanish with it.
    ALTER TABLE changes ADD COLUMN source_repo_id TEXT
        REFERENCES repos(id) ON DELETE SET NULL;
    CREATE INDEX changes_source_repo ON changes(source_repo_id)
        WHERE source_repo_id IS NOT NULL;

    -- Per-file "I have looked at this" state on a review.
    --
    -- Keyed by patchset as well as path: a file you reviewed at
    -- patchset 3 is *not* reviewed at patchset 4 if it changed, and a
    -- viewed flag that survives a force-push is worse than none —
    -- it tells a reviewer they have seen code they have not.
    CREATE TABLE change_file_views (
      change_id   TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
      user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      path        TEXT NOT NULL,
      patchset    INT  NOT NULL,
      viewed_at   BIGINT NOT NULL,
      PRIMARY KEY (change_id, user_id, path)
    );

    -- Topics, for discovery. Lowercased at the write so that "Rust" and
    -- "rust" are one topic rather than two that split a listing.
    CREATE TABLE repo_topics (
      repo_id    TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      topic      TEXT NOT NULL CHECK (topic = lower(topic)),
      created_at BIGINT NOT NULL,
      PRIMARY KEY (repo_id, topic)
    );
    CREATE INDEX repo_topics_topic ON repo_topics(topic);

    -- Following a person. No reciprocal row and no acceptance step:
    -- following is a public act about public activity, not a
    -- relationship both parties negotiate.
    CREATE TABLE follows (
      follower_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      followed_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      created_at  BIGINT NOT NULL,
      PRIMARY KEY (follower_id, followed_id),
      CHECK (follower_id <> followed_id)
    );
    CREATE INDEX follows_followed ON follows(followed_id);

    -- The contribution graph, stored as raw per-day rows rather than a
    -- rendered total.
    --
    -- Raw because `repos.public` can be flipped: a private repository
    -- made public must retroactively show its contributions in detail,
    -- and a public one made private must stop — neither is possible
    -- from a pre-summed count. `public` is denormalised here so the
    -- read path does not join to `repos` for every square.
    CREATE TABLE contributions (
      user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      repo_id    TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      day        INT  NOT NULL,
      count      INT  NOT NULL DEFAULT 0 CHECK (count >= 0),
      public     BOOLEAN NOT NULL,
      PRIMARY KEY (user_id, repo_id, day)
    );
    CREATE INDEX contributions_user_day ON contributions(user_id, day);

    -- How far the authorship walker has read each ref, so a second run
    -- is incremental rather than a full re-walk. I13: the walk is
    -- bounded per job and re-enqueues rather than running long.
    CREATE TABLE contrib_cursor (
      repo_id    TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      ref        TEXT NOT NULL,
      last_seen  TEXT NOT NULL,
      updated_at BIGINT NOT NULL,
      PRIMARY KEY (repo_id, ref)
    );

    -- Daily counters that a ranking can be computed from later.
    --
    -- Deliberately just counters. There is no trending surface yet and
    -- there must not be one until this table has enough days in it to
    -- rank honestly — a ranking over noise is worse than no ranking,
    -- and `/explore` refuses `sort=trending` by name until then.
    CREATE TABLE repo_signals (
      repo_id  TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      day      INT  NOT NULL,
      stars    INT  NOT NULL DEFAULT 0,
      forks    INT  NOT NULL DEFAULT 0,
      changes  INT  NOT NULL DEFAULT 0,
      PRIMARY KEY (repo_id, day)
    );
    CREATE INDEX repo_signals_day ON repo_signals(day);
    "#,
    // ---- Importing a project's issues from GitHub ----
    r#"
    -- A milestone. One per issue on GitHub, so the link is a column on
    -- `issues` rather than a join table.
    CREATE TABLE milestones (
      id          TEXT PRIMARY KEY,
      repo_id     TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      number      INT  NOT NULL,
      title       TEXT NOT NULL,
      description TEXT NOT NULL DEFAULT '',
      state       TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','closed')),
      due_on      BIGINT,
      created_at  BIGINT NOT NULL,
      UNIQUE (repo_id, number),
      UNIQUE (repo_id, title)
    );
    ALTER TABLE issues ADD COLUMN milestone_id TEXT
        REFERENCES milestones(id) ON DELETE SET NULL;

    -- Who an issue is assigned to.
    --
    -- `user_id` **or** `label`, never neither: an assignee we could map
    -- to an account here is one, and an assignee we could not is a name
    -- we render as text. Silently attributing `octocat` to whoever holds
    -- that handle here would be worse than not importing them at all.
    CREATE TABLE issue_assignees (
      issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
      user_id  TEXT REFERENCES users(id) ON DELETE CASCADE,
      label    TEXT,
      CHECK ((user_id IS NULL) <> (label IS NULL))
    );
    -- A unique *index* rather than a primary key, because the key is an
    -- expression and Postgres does not allow one in a PRIMARY KEY. The
    -- property is the same: one row per assignee per issue, whichever of
    -- the two columns names them.
    CREATE UNIQUE INDEX issue_assignees_one
        ON issue_assignees (issue_id, COALESCE(user_id, label));

    -- Reactions, as counts rather than as rows per person.
    --
    -- GitHub's list endpoint pages the individual reactions, which for a
    -- popular issue is thousands of requests to render a number nobody
    -- clicks through. The summary is on the issue itself and is what a
    -- reader actually sees, so that is what is stored. Native reactions,
    -- when they exist, will need a per-person table to be toggleable —
    -- this one is deliberately the imported shape and says so.
    CREATE TABLE reactions (
      subject_kind TEXT NOT NULL CHECK (subject_kind IN ('issue','comment')),
      subject_id   TEXT NOT NULL,
      content      TEXT NOT NULL,
      count        INT  NOT NULL CHECK (count >= 0),
      PRIMARY KEY (subject_kind, subject_id, content)
    );

    -- Where an imported thing came from, so an old link still resolves.
    --
    -- The point of the whole import: `github.com/acme/widget/issues/4721`
    -- is in commit messages, changelogs and other people's documentation.
    -- A migration that breaks every one of those has moved the data and
    -- lost the references to it.
    CREATE TABLE imported_urls (
      url        TEXT PRIMARY KEY,
      repo_id    TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      kind       TEXT NOT NULL CHECK (kind IN ('issue','comment')),
      number     INT  NOT NULL,
      created_at BIGINT NOT NULL
    );
    CREATE INDEX imported_urls_repo ON imported_urls(repo_id);

    -- What an imported issue was, upstream.
    ALTER TABLE issues ADD COLUMN origin_url TEXT;

    -- The colour GitHub gave a label, kept verbatim.
    --
    -- `labels.color` is a design-system token name and stays one: a hex
    -- is unfixable when the palette changes. But a project's own palette
    -- is real information, so the imported hex is kept beside it and
    -- rendered as the small hue dot the pill already draws — the path in
    -- `label-pill.tsx` that has been written, tested and unreachable
    -- since the day it landed, waiting for exactly this.
    ALTER TABLE labels ADD COLUMN origin_color TEXT;

    -- How far an import has got, so a run interrupted at issue 6,000 of
    -- 10,000 resumes instead of starting again. Same discipline as
    -- `contrib_cursor`: the job is bounded and re-enqueues.
    CREATE TABLE import_cursor (
      repo_id    TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      phase      TEXT NOT NULL,
      cursor     TEXT NOT NULL,
      updated_at BIGINT NOT NULL,
      PRIMARY KEY (repo_id, phase)
    );
    "#,
    // ---- The project's own address ----
    //
    // A repository's homepage, which is the one thing GitHub's About
    // panel carries that we had nowhere to put. Not derivable from the
    // tree: a project's site is not its README and not its origin URL,
    // and asking a maintainer to put it in the description is asking
    // them to spend the sentence a stranger reads first on a URL.
    //
    // Nullable with no default, because "no homepage" and "a homepage
    // that is the empty string" must not both be reachable — the About
    // rail renders the row present-or-absent and an empty string would
    // draw a link to nowhere.
    r#"
    ALTER TABLE repos ADD COLUMN homepage TEXT;
    "#,
    // ---- Check runs: our providers' verdicts, in one shape ----
    //
    // Stratum does not run anybody's code and is not going to — see the
    // doc comment on `api::checks_intake`. What it can do is *show* a
    // person the runs, which today it cannot: `change_checks` (0018)
    // attaches a verdict to a patchset, so a project whose `main` is
    // kept green by GitHub Actions has nothing on its repository page.
    //
    // One table, two writers, and that is the whole design. The poller
    // reads GitHub Actions through the org's existing App installation
    // and writes `provider = 'github'`; the HMAC-signed intake endpoint
    // writes `provider = 'intake'` for everybody else — a project on
    // Buildkite or Jenkins or GitLab CI gets the same page. The reading
    // side never learns which, because the day we support a fourth
    // provider the UI must not need a fourth branch.
    //
    // Keyed on a commit rather than on a patchset, deliberately. A run
    // is about a commit; that a commit is also the head of a change is
    // a fact the change knows. Keying this on patchsets would have made
    // a push to `main` — the thing CI mostly runs on — unrepresentable.
    //
    // `UNIQUE (repo_id, provider, external_id)` is what makes a re-poll
    // idempotent: GitHub hands the same run id every page, and a run
    // that moves from `running` to `passing` is an update, not a second
    // row. `external_id` is nullable for an intake caller who has no id
    // of their own to offer, and the partial index is why that is safe
    // — several such rows do not collide with each other.
    //
    // `state` is one column, not GitHub's two. The mapping from their
    // (`status`, `conclusion`) pair lives in exactly one pure function
    // with an exhaustive test, and an unrecognised verdict degrades to
    // `queued`: a conclusion GitHub adds tomorrow must never read as
    // green here.
    r#"
    CREATE TABLE check_runs (
      id           TEXT PRIMARY KEY,
      repo_id      TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      commit_sha   TEXT NOT NULL,
      ref_name     TEXT,
      provider     TEXT NOT NULL,
      external_id  TEXT,
      name         TEXT NOT NULL,
      run_number   BIGINT,
      event        TEXT,
      state        TEXT NOT NULL CHECK (state IN
                     ('queued','running','passing','failing','cancelled','skipped')),
      detail_url   TEXT,
      actor        TEXT,
      started_at   BIGINT,
      completed_at BIGINT,
      created_at   BIGINT NOT NULL,
      updated_at   BIGINT NOT NULL
    );
    CREATE UNIQUE INDEX check_runs_external
      ON check_runs(repo_id, provider, external_id)
      WHERE external_id IS NOT NULL;
    -- The index the list page reads through: one repository, newest
    -- first, which is every request that surface makes.
    CREATE INDEX check_runs_repo_at ON check_runs(repo_id, created_at DESC);
    -- And the one the commit and change pages read through, to put a
    -- verdict beside a sha without scanning the repository's history.
    CREATE INDEX check_runs_commit ON check_runs(repo_id, commit_sha);
    "#,
    // ---- Signals: the daily rollup Insights reads ----
    //
    // `repo_signals` was created for exactly this and then referenced by
    // no Rust code at all — its own comment says it exists so a ranking
    // can be computed later. Later is now, and Pulse needs more than
    // three counters.
    //
    // The columns added are the ones a weekly digest is made of, and
    // they are stored rather than derived on read for one reason: the
    // org-wide rollup is a sum across every repository in a namespace,
    // and computing that from `changes`, `issues` and `contributions`
    // per request turns one page load into a scan of the whole tenant.
    // Per-repo Pulse reads the same table, so there is one aggregation
    // to keep correct rather than two that can disagree.
    //
    // Deliberately absent: files changed, additions and deletions. We do
    // not store per-commit diffstats, and GitHub's own Pulse rendering
    // "0 files changed, 0 additions" over a week with 222 commits is the
    // failure this omission avoids. A number nobody can check is worse
    // than a line that is not there.
    r#"
    ALTER TABLE repo_signals ADD COLUMN issues_opened   INT NOT NULL DEFAULT 0;
    ALTER TABLE repo_signals ADD COLUMN issues_closed   INT NOT NULL DEFAULT 0;
    ALTER TABLE repo_signals ADD COLUMN changes_merged  INT NOT NULL DEFAULT 0;
    ALTER TABLE repo_signals ADD COLUMN commits         INT NOT NULL DEFAULT 0;
    ALTER TABLE repo_signals ADD COLUMN contributors    INT NOT NULL DEFAULT 0;
    -- When this day's row was last recomputed. A rollup worker that
    -- cannot tell a day it has never summed from one it summed while
    -- the day was still running would either re-sum the whole history
    -- every pass or freeze today's numbers at whatever they were at
    -- 00:05.
    ALTER TABLE repo_signals ADD COLUMN rolled_at BIGINT NOT NULL DEFAULT 0;
    "#,
    // ---- Two findings from building the checks and insights tracks ----
    //
    // **`jobs_active_per_repo` was covering four kinds and needed six.**
    //
    // 0019 and 0026 each added the kinds whose work is a *sweep of the
    // repository's current state*, where running twice is waste and
    // running concurrently is a race, and each covered only the kinds in
    // front of it at the time. `import` has been outside the index since
    // the day it landed, and `checkspoll` would have been. Both call
    // `jobs::enqueue_unique`, which deduplicates **only** through this
    // index — so for those two kinds it has been `create` with extra
    // steps and the `ON CONFLICT DO NOTHING` never fired. A
    // double-pressed "Poll now" made two rows and two nodes polled one
    // repository at once.
    //
    // Both are the right shape for it. A poll is a sweep of the
    // repository's current CI state and one run subsumes a queued
    // duplicate; an import is bounded and resumes from `import_cursor`,
    // so two of them racing would page the same issues twice and fight
    // over one cursor.
    //
    // Pre-existing rows are reconciled first, exactly as 0019 and 0026
    // did it: the oldest active row per (kind, repo) survives, because
    // it is the one a worker may already be running.
    r#"
    UPDATE jobs SET state = 'done',
        result = 'superseded: duplicate active import/checkspoll job',
        updated_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
    WHERE kind IN ('import', 'checkspoll')
      AND repo_id IS NOT NULL
      AND state IN ('queued', 'running')
      AND id NOT IN (
          SELECT DISTINCT ON (kind, repo_id) id FROM jobs
          WHERE kind IN ('import', 'checkspoll')
            AND repo_id IS NOT NULL
            AND state IN ('queued', 'running')
          ORDER BY kind, repo_id, created_at
      );
    DROP INDEX jobs_active_per_repo;
    CREATE UNIQUE INDEX jobs_active_per_repo ON jobs (kind, repo_id)
        WHERE repo_id IS NOT NULL
          AND kind IN ('compact', 'cdnpack', 'fork', 'promote',
                       'import', 'checkspoll')
          AND state IN ('queued', 'running');
    "#,
    // **When a change landed, as its own fact.**
    //
    // `set_landed` writes `state = 'landed'` and stamps `updated_at`,
    // and there was no other record of the moment. So "merged in this
    // window" and time-to-merge both had to read `updated_at`, which is
    // correct the instant a change lands and stays correct only while
    // nothing ever touches an already-landed change. One `UPDATE … SET
    // updated_at` on a landed row — a title edit, a comment counter, an
    // import backfill — would silently move that change into a later
    // period and inflate its time-to-merge, and it would be wrong
    // quietly, in a number nobody can check against anything.
    //
    // **Alone in its element, and so is the backfill below it.** Every
    // element runs in one transaction, so locks *compose*: this
    // `ALTER` takes ACCESS EXCLUSIVE on `changes` and holds it until
    // commit, and while these three statements shared an element that
    // meant `jobs` stayed ACCESS EXCLUSIVE — for a `DROP INDEX` it had
    // nothing to do with — for the entire length of a full rewrite of
    // `changes`. ACCESS EXCLUSIVE blocks plain `SELECT`s, `lock_timeout`
    // is 5s on every connection and bounds only how long *we* wait to
    // acquire, and `jobs::claim` runs on every worker's poll loop. So a
    // large tenant's whole fleet would have errored continuously for
    // the length of the backfill, and a node booting concurrently would
    // have failed on the advisory lock and stalled the deploy.
    //
    // Split, this commits in microseconds: nullable with no default is
    // metadata-only in PostgreSQL 11+, so there is no rewrite here at
    // all.
    r#"
    ALTER TABLE changes ADD COLUMN landed_at BIGINT;
    "#,
    // The backfill, in its own transaction so it holds only ROW
    // EXCLUSIVE — which under MVCC blocks no readers and conflicts only
    // with concurrent writes to the same landed rows, of which there
    // are none: `set_landed`'s `WHERE … state IN ('open','landing')`
    // cannot touch a row that is already landed.
    //
    // It is still a sequential scan and a rewrite of every landed row —
    // no index on `changes` serves `state` without a `repo_id` — but a
    // long ROW EXCLUSIVE is a slow migration, where a long ACCESS
    // EXCLUSIVE is an outage.
    //
    // `updated_at` because it is the best estimate that exists for
    // changes already landed, and a NULL would make every historical
    // change vanish from the insights it belongs in. New rows get the
    // real thing, and the readers `COALESCE` for the rolling-deploy
    // window where an old binary lands a change without stamping it.
    r#"
    UPDATE changes SET landed_at = updated_at WHERE state = 'landed';
    "#,
    // ---- Which checks a branch actually requires ----
    //
    // The land gate was `failing_check(patchset)`: is any check
    // *currently* failing. That is a weaker promise than it reads as,
    // in two directions at once. A patchset with **no** checks passed
    // it. A patchset whose checks were all still `pending` passed it.
    // So a change could land before its build had started, and the tick
    // beside it on the change page meant "nothing has failed yet"
    // rather than "this was tested".
    //
    // It was also blind to half the checks that exist. `change_checks`
    // is keyed on a patchset and written by the intake; `check_runs` is
    // keyed on a commit and is where a mirrored project's GitHub
    // Actions verdicts land. A repository whose CI is Actions had its
    // runs on the Checks tab and gating nothing.
    //
    // A **table rather than a column** on `branch_protections`, for
    // three reasons. A name is a row, so uniqueness is a constraint
    // instead of a habit; adding one does not rewrite the protection
    // row and race an admin editing the branch beside it; and — the
    // reason that decided it — nothing existing reads or writes this
    // table, so no predicate anywhere changes meaning. The last schema
    // change here widened a partial index and silently broke
    // self-re-enqueue in two workers that had never been under it.
    //
    // **Empty is not "require everything".** A protected branch with no
    // rows keeps exactly today's behaviour: a failing check blocks, and
    // nothing else does. Requiring a check is a thing an admin says out
    // loud, because the alternative — inferring it from whatever has
    // ever reported — turns one experimental run somebody posted once
    // into a permanent merge gate, and makes a renamed workflow stop
    // gating without anybody being told.
    //
    // `name` matches on both sides: a `change_checks.name` and a
    // `check_runs.name` are the same namespace deliberately, so a
    // project migrating from the intake to the poller keeps its gate.
    r#"
    CREATE TABLE required_checks (
        repo_id    TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        branch     TEXT NOT NULL,
        name       TEXT NOT NULL,
        created_by TEXT,
        created_at BIGINT NOT NULL,
        PRIMARY KEY (repo_id, branch, name)
    );
    "#,
    // ---- Two foreign keys that made a repository unpurgeable ----
    //
    // `branch_protections` (0016) and `repo_grants` (0005) both
    // reference `repos(id)` with **no `ON DELETE`**, so the default
    // `NO ACTION` applies: a repository that has ever been protected, or
    // ever had a per-repo grant, cannot be `purge_repo`'d at all. Every
    // other repo-scoped table in the schema cascades; these two were
    // written before that was the habit and nothing since has deleted a
    // repository that had either.
    //
    // What made it hard to find is the error. `purge_repo` maps *any*
    // foreign-key violation to "this repository still has forks reading
    // its storage — promote or delete them first", which is a real
    // condition with real advice, and following that advice on a
    // protected repository does nothing at all. A message that names the
    // wrong cause is worse than one that names none: it sends somebody
    // to look in a place where there is nothing to find.
    //
    // Cascade is right for both. A protection and a grant are statements
    // *about* a repository — they have no meaning once it is gone, and
    // nothing reads them for audit (`audit_log` is the audit trail and
    // keeps its own rows, which is why it references nothing).
    //
    // Separate elements, one lock at a time: each is a constraint swap
    // taking ACCESS EXCLUSIVE on its own table, and welding them
    // together would hold both at once for no reason. There is no table
    // rewrite here — dropping and re-adding a foreign key validates the
    // existing rows, which is a scan, not a rewrite.
    r#"
    ALTER TABLE branch_protections DROP CONSTRAINT branch_protections_repo_id_fkey;
    ALTER TABLE branch_protections ADD CONSTRAINT branch_protections_repo_id_fkey
        FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE;
    "#,
    r#"
    ALTER TABLE repo_grants DROP CONSTRAINT repo_grants_repo_id_fkey;
    ALTER TABLE repo_grants ADD CONSTRAINT repo_grants_repo_id_fkey
        FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE;
    "#,
    // ---- The index that makes the signals sweep bounded ----
    //
    // `signals::due` asked "which (repo, day) pairs need rolling" as a
    // six-way UNION across `issues` (twice), `changes` (twice),
    // `contributions`, `repo_stars`, `repos` and `repo_signals`, with the
    // selectivity in a LEFT JOIN *outside* the CTE. Its own comment
    // claimed the filter made this nearly free in steady state; the
    // filter runs last, so the whole candidate set was scanned and
    // deduplicated before a single row was discarded. Ten passes per
    // tick, every five minutes, forever — and the dedup is a
    // HashAggregate that spills to disk past `work_mem`, which arrives
    // at a threshold rather than degrading.
    //
    // The due set decomposes exactly, because the predicate reads
    // `coalesce(rolled_at, 0)`: days that already have a row and are not
    // yet final, plus days in a source table with no row at all. The
    // first is the entire steady state and is expressible over
    // `repo_signals` alone. This index serves it.
    //
    // The predicate is what makes it work, and the property is worth
    // stating: **a row leaves this index permanently the moment its day
    // finalises.** So it holds roughly two entries per active repository
    // no matter how much history the tenant has — where every other
    // shape of this index grows with the past.
    //
    // `day::BIGINT` is load-bearing and not style. `day` is `INT`, so
    // `(day + 1) * 86400000` is int4 arithmetic and overflows around day
    // 19000 — the index would fail to build. The cast makes the
    // expression int8 and is still immutable, so the partial predicate
    // stays legal. It must also match the query's text character for
    // character or the planner will not use it.
    r#"
    CREATE INDEX repo_signals_open ON repo_signals (rolled_at)
        WHERE rolled_at < (day::BIGINT + 1) * 86400000;
    "#,
    // ---- A credential that dies on its own ----
    //
    // `tokens` has had `revoked_at` since 0001, which is somebody
    // *deciding* to end a credential. It has never had a way for one to
    // end by itself, so every token minted so far is immortal until a
    // person or a process remembers it.
    //
    // That is fine for a personal access token, which somebody owns and
    // can see in a list. It is not fine for the shape the build runner
    // needs: a `repo:read` token bound to one repository, minted per
    // job, handed to a container that is about to run somebody else's
    // code. Cleanup for those would be the dispatcher revoking them
    // afterwards — and a dispatcher that is deployed over, OOM-killed,
    // or that simply loses the race between handing the token over and
    // revoking it leaves a live credential inside a build with no
    // deadline on it. Scoping a token to one repository is undone by
    // letting it outlive the job.
    //
    // NULL means "never expires", so every existing token is unaffected
    // and the personal-token flow does not change.
    //
    // Enforced in the authentication queries rather than swept by a
    // worker. A credential that stays valid until something gets round
    // to deleting it is valid for an unbounded time, which is the exact
    // property being fixed; a sweep can only ever be a tidying pass
    // behind an answer the reader already gave correctly.
    //
    // No index, deliberately. Both readers look a token up by primary
    // key and test this column on the row they already have, and nothing
    // scans for expired tokens — an expired row is inert whether or not
    // it is still there.
    r#"
    ALTER TABLE tokens ADD COLUMN expires_at BIGINT;
    "#,
    // ---- Workflow runs: what a push asked to have done ----
    //
    // Two tables and an edge list, and the shape is decided by the one
    // query that matters — the claim, which has to find a job whose
    // dependencies have all passed, in an org that is not already at its
    // concurrency limit, without scanning the world.
    //
    // `workflow_jobs.org_id` is denormalised from the run deliberately.
    // The claim counts an org's running jobs on every attempt, and
    // joining through `workflow_runs` to do it would put a join inside
    // the hot statement of the queue. It is written once and never
    // changes, because a job cannot move between organisations.
    //
    // **Readiness is computed, never stored.** There is no `blocked`
    // state that something has to remember to clear: the claim asks
    // whether any dependency has not passed, so a job becomes runnable
    // the instant its last dependency does, and no missed transition can
    // strand it. A run whose promoter died between "the dep passed" and
    // "the dependent was unblocked" is the classic way a queue wedges,
    // and there is nothing here to miss.
    //
    // `skipped` is a real stored state and not a derived one, because it
    // is a *decision*: GitHub's rule is that a job whose dependency
    // failed does not run, and recording that is what lets a reader see
    // the difference between "did not run because the build failed" and
    // "is still waiting".
    r#"
    CREATE TABLE workflow_runs (
      id            TEXT PRIMARY KEY,
      org_id        TEXT NOT NULL REFERENCES orgs(id),
      repo_id       TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      -- The file it came from, and what the file called itself.
      file          TEXT NOT NULL,
      name          TEXT NOT NULL,
      commit_sha    TEXT NOT NULL,
      ref_name      TEXT,
      -- `push` or `change`, our two triggers.
      event         TEXT NOT NULL,
      -- Set when a change caused this run, so a verdict can be attached
      -- to the review as well as to the commit.
      change_key    TEXT,
      state         TEXT NOT NULL DEFAULT 'running'
                    CHECK (state IN ('running','passed','failed','cancelled')),
      created_at    BIGINT NOT NULL,
      updated_at    BIGINT NOT NULL,
      completed_at  BIGINT
    );
    CREATE INDEX workflow_runs_repo ON workflow_runs(repo_id, created_at DESC);
    CREATE INDEX workflow_runs_commit ON workflow_runs(repo_id, commit_sha);

    CREATE TABLE workflow_jobs (
      id            TEXT PRIMARY KEY,
      run_id        TEXT NOT NULL REFERENCES workflow_runs(id) ON DELETE CASCADE,
      -- Denormalised for the claim; see above.
      org_id        TEXT NOT NULL,
      repo_id       TEXT NOT NULL,
      -- The job as written, and the expanded cell name a person reads.
      job_id        TEXT NOT NULL,
      key           TEXT NOT NULL,
      -- The planner's position for this job, and the only thing that
      -- makes start order deterministic. `created_at` ties for every job
      -- in a run — they are written in one transaction, in one
      -- millisecond — and a ULID tiebreak is random inside that
      -- millisecond, so without this two runs of one workflow start
      -- their jobs in different orders and a reader comparing them is
      -- reading the scheduler's mood rather than the runs.
      ordinal       INT NOT NULL DEFAULT 0,
      -- The cell's matrix bindings, as JSON. Read by the runner to build
      -- the environment; never queried on, so it needs no shape here.
      matrix        TEXT NOT NULL DEFAULT '{}',
      state         TEXT NOT NULL DEFAULT 'queued'
                    CHECK (state IN ('queued','running','passed','failed','skipped','cancelled')),
      attempts      BIGINT NOT NULL DEFAULT 0,
      lease_until   BIGINT,
      -- Where the runner reported from, for a reader chasing a red job.
      detail_url    TEXT,
      error         TEXT,
      created_at    BIGINT NOT NULL,
      updated_at    BIGINT NOT NULL,
      started_at    BIGINT,
      completed_at  BIGINT,
      UNIQUE (run_id, key)
    );
    -- The claim's index: queued jobs oldest first, and the org counter.
    CREATE INDEX workflow_jobs_claim ON workflow_jobs(state, created_at, ordinal)
        WHERE state IN ('queued','running');
    CREATE INDEX workflow_jobs_org_running ON workflow_jobs(org_id)
        WHERE state = 'running';
    CREATE INDEX workflow_jobs_run ON workflow_jobs(run_id);

    -- The DAG. A row per edge rather than a list on the job, so the
    -- claim can ask "does any unfinished dependency exist" as an index
    -- lookup instead of parsing an array.
    CREATE TABLE workflow_job_deps (
      job_id    TEXT NOT NULL REFERENCES workflow_jobs(id) ON DELETE CASCADE,
      needs_id  TEXT NOT NULL REFERENCES workflow_jobs(id) ON DELETE CASCADE,
      PRIMARY KEY (job_id, needs_id)
    );
    CREATE INDEX workflow_job_deps_needs ON workflow_job_deps(needs_id);

    -- How many jobs one organisation may have running at once.
    --
    -- On `orgs` rather than in a config file because it is per-tenant and
    -- has to be changeable without a deploy. NULL means the default,
    -- which the server owns: a number here is an override somebody chose,
    -- and storing the default in every row would freeze today's default
    -- into every organisation created before it changed.
    ALTER TABLE orgs ADD COLUMN ci_concurrency INT;
    "#,
    // ---- What a run needs once something actually runs it ----
    //
    // 0028 built the queue; this is everything the dispatcher and the
    // runner need on top of it, and every column here exists because
    // some request could not be answered without it.
    r#"
    -- Why a run ended the way it did, in the run's own words.
    --
    -- A job carries its own `error`, but a run can be settled with no
    -- job at all — a workflow file we refused, a fork change waiting for
    -- approval, a deployment with no runner configured — and without
    -- this there is nowhere to put the sentence that explains it. A red
    -- check with no reason is the worst kind: it stops a land and tells
    -- nobody what to fix.
    ALTER TABLE workflow_runs ADD COLUMN error TEXT;

    -- `blocked`: a run that exists, is not running, and has not reached
    -- a verdict — a change pushed from a fork, waiting for a maintainer
    -- to say it may run. It is a real stored state rather than a
    -- derived one for the same reason `skipped` is on jobs: it is a
    -- decision somebody has to be able to see and act on.
    --
    -- Dropped by the name Postgres generated for 0028's inline
    -- `CHECK (state IN (…))`, which is `<table>_<column>_check`. Not
    -- `IF EXISTS`: if that name is ever wrong, the migration must fail
    -- loudly here rather than quietly add a second constraint and leave
    -- every `blocked` insert refused by the first one.
    ALTER TABLE workflow_runs DROP CONSTRAINT workflow_runs_state_check;
    ALTER TABLE workflow_runs ADD CONSTRAINT workflow_runs_state_check
        CHECK (state IN ('running','passed','failed','cancelled','blocked'));

    -- The highest log chunk the runner has uploaded for this attempt.
    -- The log itself lives in the object store — a build log is
    -- megabytes of text nobody queries on — and this is the only part
    -- the database needs: it is how a reader knows how many objects to
    -- fetch, and how the live stream knows something new has arrived.
    ALTER TABLE workflow_jobs ADD COLUMN log_chunks INT NOT NULL DEFAULT 0;
    -- What is executing it: an ECS task ARN, or `pid:<n>` for the
    -- process executor. Kept so a cancellation can stop the thing that
    -- is actually burning money, not just mark a row.
    ALTER TABLE workflow_jobs ADD COLUMN task_ref TEXT;
    -- The job token minted for **this attempt**. The runner API compares
    -- the presented token's id against it, so a token from a previous
    -- attempt — or from another job entirely — cannot report a verdict.
    ALTER TABLE workflow_jobs ADD COLUMN token_id TEXT;
    -- The `check_runs` row that mirrors this job, so every transition
    -- updates one row rather than accumulating one per report.
    ALTER TABLE workflow_jobs ADD COLUMN check_run_id TEXT;
    -- The JobSpec the runner is handed: image, timeout, env, matrix,
    -- steps, as JSON. Frozen at dispatch on purpose — a job must run
    -- what the commit said, not what `.weft/` says by the time a
    -- retry gets to it.
    ALTER TABLE workflow_jobs ADD COLUMN spec TEXT NOT NULL DEFAULT '{}';

    -- "Is anything already running for this ref?" — asked on every push,
    -- to supersede the run the previous push started.
    CREATE INDEX workflow_runs_live_ref ON workflow_runs (repo_id, ref_name, event)
        WHERE state = 'running';
    "#,
    // ---- What stops a hosted fleet being somebody else's free compute ----
    //
    // Three columns, all on `orgs` for the same reason `ci_concurrency`
    // is: they are per-tenant, an operator has to be able to change them
    // without a deploy, and NULL has to keep meaning "whatever the
    // server's default is today" rather than freezing today's default
    // into every row.
    r#"
    -- How many hosted-runner minutes this organisation may burn in a
    -- rolling thirty days. NULL is the deployment default
    -- (`STRATUM_RUNNER_MINUTES_PER_MONTH`, unset or 0 = unlimited).
    --
    -- Minutes rather than jobs or runs because minutes are what the
    -- fleet is billed in: a thousand ten-second jobs cost less than one
    -- job left spinning for a day, and a job count would price them the
    -- other way round.
    ALTER TABLE orgs ADD COLUMN ci_minutes_per_month INT;

    -- Why hosted workflows are switched off for this organisation, in
    -- the words a member will read, and when it happened.
    --
    -- A reason column rather than a boolean because the only useful
    -- thing to tell somebody whose builds have stopped is *why*, and a
    -- suspension with no reason is indistinguishable from a broken
    -- deployment. NULL in `ci_suspended_reason` is the whole "not
    -- suspended" test; `ci_suspended_at` is for the operator deciding
    -- whether to clear it, not for any decision the code makes.
    --
    -- Clearing is an operator action by SQL for now — there is no
    -- operator role on this server to hang a route off, and inventing
    -- one here would be a security surface built in passing. See
    -- `docs/` next to `ci_concurrency`.
    ALTER TABLE orgs ADD COLUMN ci_suspended_reason TEXT;
    ALTER TABLE orgs ADD COLUMN ci_suspended_at BIGINT;

    -- "What has this organisation burned lately?" — asked at every
    -- trigger and every claim, over a thirty-day window.
    CREATE INDEX workflow_jobs_org_started ON workflow_jobs (org_id, started_at)
        WHERE started_at IS NOT NULL;

    -- Which of the refusals put this run in `blocked`, as a word a
    -- program may branch on: `fork`, `budget`, `suspended`.
    --
    -- The sentence in `error` is written for a person and will be
    -- rephrased; a dashboard that decides whether to offer "approve
    -- these workflows" by matching that prose breaks silently the day
    -- somebody improves the wording. NULL for every run that is not
    -- blocked, which is the state's own invariant rather than a
    -- convention: see `create_settled_run`.
    ALTER TABLE workflow_runs ADD COLUMN blocked_reason TEXT;
    "#,
    // ---- Somebody else's machines: self-hosted runners ----
    //
    // A second pool. Everything here exists so that a job can be routed
    // to hardware this deployment does not own, and so that the person
    // who owns it can say which repositories are allowed to reach it —
    // which is the whole security question, because a workflow file
    // arrives from any repository anybody forked and a self-hosted
    // runner is somebody's laptop or build box.
    r#"
    -- What the organisation allows, at the organisation level.
    --
    -- Two independent switches rather than one policy, because they
    -- answer opposite questions: `runner_hosted` is "may our work run on
    -- Stratum's machines" (an org that only trusts its own turns it
    -- off), and `runner_self_hosted` is "may our work run on machines we
    -- registered" (an org that does not want a fork's PR near its
    -- hardware turns *that* off). Defaults keep every existing
    -- organisation exactly as it was: hosted allowed, self-hosted open
    -- to every repository — which changes nothing until somebody
    -- registers a runner, since a pool with no runners in it refuses at
    -- trigger time anyway.
    ALTER TABLE orgs ADD COLUMN runner_hosted TEXT NOT NULL DEFAULT 'allowed'
        CHECK (runner_hosted IN ('allowed','disabled'));
    ALTER TABLE orgs ADD COLUMN runner_self_hosted TEXT NOT NULL DEFAULT 'all'
        CHECK (runner_self_hosted IN ('all','selected','disabled'));

    -- Which repositories `runner_self_hosted = 'selected'` names. Empty
    -- under 'all' and under 'disabled', where it is not consulted.
    CREATE TABLE org_self_hosted_repos (
      org_id   TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
      repo_id  TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      PRIMARY KEY (org_id, repo_id)
    );

    -- A group is the unit a repository is admitted by, and every runner
    -- is in exactly one. GitHub's shape, for GitHub's reason: the
    -- alternative is a per-runner repository list, and an operator with
    -- thirty runners then has thirty lists to keep in step.
    --
    -- `allow_public` defaults FALSE and that default is the security
    -- property, not a preference. A public repository can be forked, a
    -- fork's change carries its own workflow file, and admitting public
    -- repositories by default would mean the first stranger to open a
    -- change gets a shell on somebody's build box. The fork gate stands
    -- in front of it as well; this is the second lock.
    CREATE TABLE runner_groups (
      id            TEXT PRIMARY KEY,
      org_id        TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
      name          TEXT NOT NULL,
      repo_access   TEXT NOT NULL DEFAULT 'all'
                    CHECK (repo_access IN ('all','selected')),
      allow_public  BOOLEAN NOT NULL DEFAULT FALSE,
      is_default    BOOLEAN NOT NULL DEFAULT FALSE,
      created_at    BIGINT NOT NULL,
      updated_at    BIGINT NOT NULL,
      UNIQUE (org_id, name)
    );
    -- One default per organisation, enforced rather than asserted: the
    -- default group is where a registration with no group named lands
    -- and where a deleted group's runners are moved to, and two of them
    -- would make both of those ambiguous.
    CREATE UNIQUE INDEX runner_groups_one_default ON runner_groups (org_id)
        WHERE is_default;

    CREATE TABLE runner_group_repos (
      group_id  TEXT NOT NULL REFERENCES runner_groups(id) ON DELETE CASCADE,
      repo_id   TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      PRIMARY KEY (group_id, repo_id)
    );

    -- The machines themselves.
    --
    -- `labels` is a real TEXT[] rather than JSON because routing asks
    -- "does this runner's label set contain every label the job asked
    -- for", which is one `@>` against a GIN index, and the same question
    -- over JSON is a scan.
    --
    -- Removal is a tombstone (`removed_at`), never a delete: a job row
    -- points at the runner that ran it, and a reader looking at last
    -- week's build should still be told which machine it ran on. That is
    -- why the name uniqueness is **partial** — re-registering under a
    -- name whose runner was removed is an ordinary registration, and
    -- re-registering under a *live* name is how a credential is rotated
    -- (the old row is tombstoned in the same transaction).
    CREATE TABLE runners (
      id               TEXT PRIMARY KEY,
      org_id           TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
      group_id         TEXT NOT NULL REFERENCES runner_groups(id),
      name             TEXT NOT NULL,
      labels           TEXT[] NOT NULL,
      os               TEXT NOT NULL,
      arch             TEXT NOT NULL,
      version          TEXT NOT NULL,
      ephemeral        BOOLEAN NOT NULL DEFAULT FALSE,
      -- Only the hash. The plaintext is shown once, at registration, and
      -- is never recoverable — the same rule the API tokens follow.
      credential_hash  TEXT NOT NULL,
      last_seen_at     BIGINT NOT NULL,
      created_at       BIGINT NOT NULL,
      removed_at       BIGINT
    );
    CREATE UNIQUE INDEX runners_live_name ON runners (org_id, name)
        WHERE removed_at IS NULL;
    -- The claim's index: authenticate by hash, then route by labels.
    CREATE UNIQUE INDEX runners_credential ON runners (credential_hash);
    CREATE INDEX runners_org_live ON runners (org_id) WHERE removed_at IS NULL;
    CREATE INDEX runners_labels ON runners USING GIN (labels);

    -- The one-hour, single-use secret an operator carries to the machine.
    --
    -- Hashed at rest for the same reason the runner credential is: it is
    -- a bearer secret that mints another bearer secret, and it travels
    -- through a shell history and a terminal scrollback on its way.
    CREATE TABLE runner_registration_tokens (
      id          TEXT PRIMARY KEY,
      org_id      TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
      group_id    TEXT NOT NULL REFERENCES runner_groups(id) ON DELETE CASCADE,
      token_hash  TEXT NOT NULL,
      created_by  TEXT NOT NULL,
      expires_at  BIGINT NOT NULL,
      used_at     BIGINT,
      created_at  BIGINT NOT NULL
    );
    CREATE UNIQUE INDEX runner_registration_tokens_hash
        ON runner_registration_tokens (token_hash);

    -- Which pool a job belongs to, what it asked for, and what took it.
    --
    -- `pool` defaults to 'hosted' so every row written before this
    -- migration is what it always was, and so the hosted claim's new
    -- `AND j.pool = 'hosted'` does not silently strand the queue on
    -- deploy. `labels` is the `runs-on` list as written (lowercased,
    -- deduped, file order); a hosted job's is its one hosted label.
    ALTER TABLE workflow_jobs ADD COLUMN pool TEXT NOT NULL DEFAULT 'hosted'
        CHECK (pool IN ('hosted','self_hosted'));
    ALTER TABLE workflow_jobs ADD COLUMN labels TEXT[] NOT NULL DEFAULT '{}';
    -- The runner that claimed this attempt. NULL for every hosted job
    -- and for a self-hosted job nobody has taken yet. Not a foreign key
    -- on purpose: runners are tombstoned rather than deleted, but an
    -- organisation being deleted takes its runners with it and must not
    -- be blocked by a job row that is only evidence.
    ALTER TABLE workflow_jobs ADD COLUMN runner_id TEXT;
    -- "What is this runner doing right now?" — asked once per runner on
    -- every listing, and once per claim to refuse a busy one.
    CREATE INDEX workflow_jobs_runner_running ON workflow_jobs (runner_id)
        WHERE state = 'running';
    -- The self-hosted claim's index: the pool is the first thing it
    -- narrows by, and without this it shares the hosted claim's index
    -- and scans every hosted job in the queue to find none of them.
    CREATE INDEX workflow_jobs_pool_claim ON workflow_jobs (pool, created_at, ordinal)
        WHERE state IN ('queued','running');
    "#,
    // 0044 — a card before anything else; public is free.
    //
    // The plan vocabulary grows `pending`: an organization that exists
    // so the payment provider has an id to hand back, and holds nothing
    // until a payment method is confirmed. `free` changes meaning from
    // "holds nothing" to "public repositories, any number of people",
    // which is what the free tier had always claimed to be on the
    // marketing site and never was in the code.
    //
    // The saved card lives on the subscription row because the
    // subscription is opened *on* it, server-side, the first time the
    // organization needs one — that is the whole reason to collect it
    // up front.
    //
    // Nothing is live yet, so rows are not migrated between meanings:
    // an existing `free` org simply has no card recorded, and the first
    // thing it is asked to pay for sends it through the setup checkout
    // as if it were new.
    r#"
    ALTER TABLE orgs DROP CONSTRAINT orgs_plan_known;
    ALTER TABLE orgs ADD CONSTRAINT orgs_plan_known
        CHECK (plan IN ('pending','free','paid','past_due'));
    ALTER TABLE subscriptions ADD COLUMN payment_method_ref TEXT;
    -- Events named by customer resolve here; the column was only ever
    -- read by primary key before.
    CREATE INDEX subscriptions_customer ON subscriptions(customer_ref)
        WHERE customer_ref IS NOT NULL;
    -- The sweep deletes by age.
    CREATE INDEX billing_events_received ON billing_events(received_at);
    "#,
    // 0045 — changesets: one review, one verdict, one landing across
    // several repositories of one organization.
    //
    // A changeset owns nothing a change does not already have. Its
    // members are existing `changes` rows, one per repository, and its
    // edges say which member lands before which. What it adds is the
    // binding: while a change is a member of an open changeset it lands
    // and is abandoned only through the changeset, so the "one landing"
    // the word promises is a database fact and not an API habit.
    //
    // `active` is that binding. It mirrors "the changeset is open or
    // landing" and exists so the one-open-changeset-per-change rule can
    // be a partial unique index rather than a read-then-write the
    // second creator wins. A landed or abandoned changeset flips its
    // members inactive in the same statement that moves its state, and
    // the change is free to be composed again.
    r#"
    CREATE TABLE changesets (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        key TEXT NOT NULL,
        title TEXT NOT NULL,
        body TEXT NOT NULL DEFAULT '',
        state TEXT NOT NULL DEFAULT 'open'
            CHECK (state IN ('open', 'landing', 'landed', 'abandoned', 'failed')),
        created_by TEXT REFERENCES users(id),
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL,
        UNIQUE (org_id, key)
    );
    CREATE INDEX changesets_org_state ON changesets(org_id, state, id);
    CREATE TABLE changeset_members (
        changeset_id TEXT NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
        change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
        position BIGINT NOT NULL,
        active BOOLEAN NOT NULL DEFAULT TRUE,
        PRIMARY KEY (changeset_id, change_id)
    );
    CREATE UNIQUE INDEX changeset_members_one_open
        ON changeset_members(change_id) WHERE active;
    CREATE TABLE changeset_edges (
        changeset_id TEXT NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
        from_change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
        to_change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
        PRIMARY KEY (changeset_id, from_change_id, to_change_id),
        CHECK (from_change_id <> to_change_id)
    );
    "#,
    // 0046 — the changeset landing's commit point.
    //
    // A changeset lands across several manifests and there is no
    // transaction that spans two of them, so the row here is what makes
    // the landing all-or-nothing anyway: it is written, with the whole
    // plan, in the same transaction that moves the changeset to
    // `landing`, and from that row on the landing *will* finish — every
    // member lands, or every member that landed is reverted. Whoever
    // drives it (the job named, or a reaper after that job died) reads
    // the plan back and continues from what the store actually says.
    //
    // `progress` is the plan and the record of it in one JSON document
    // — one entry per member, in landing order, each carrying the ref,
    // the tip it was expected at, the tip it moves to, and how far it
    // got. TEXT rather than JSONB because nothing queries inside it; it
    // is read whole by the one worker driving the landing, the same way
    // `jobs.payload` is.
    //
    // One live landing per changeset, by index: a second `begin` while
    // the first is unfinished is a bug in a caller, and the index turns
    // it into a unique violation rather than two landers on one plan.
    r#"
    CREATE TABLE changeset_landings (
        id TEXT PRIMARY KEY,
        changeset_id TEXT NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
        job_id TEXT,
        attempt BIGINT NOT NULL DEFAULT 1,
        started_at BIGINT NOT NULL,
        finished_at BIGINT,
        outcome TEXT CHECK (outcome IN ('landed', 'failed')),
        progress TEXT NOT NULL,
        CHECK ((finished_at IS NULL) = (outcome IS NULL))
    );
    CREATE UNIQUE INDEX changeset_landings_live
        ON changeset_landings(changeset_id) WHERE finished_at IS NULL;
    CREATE INDEX changeset_landings_by_changeset
        ON changeset_landings(changeset_id, started_at DESC);
    "#,
    // 0047 — a revert changeset remembers what it reverts.
    //
    // A revert is a new changeset — new changes, reviewed and landed like
    // any other — so nothing about the landed one changes; but the link
    // between the two is what a reader of either wants first: "was this
    // ever reverted?" on the landed one, "what does this undo?" on the
    // revert. One nullable column, no cascade: deleting the original
    // would orphan the record of what was undone, and nothing deletes a
    // changeset today.
    r#"
    ALTER TABLE changesets ADD COLUMN reverts TEXT REFERENCES changesets(id);
    CREATE INDEX changesets_reverts ON changesets(reverts) WHERE reverts IS NOT NULL;
    "#,
    // 0048 — composed CI: a run that belongs to a changeset, and the
    // verdicts it produces.
    //
    // `composition` is what the changeset *was* when the run started:
    // the hash of every member's repository and tip. It is on the run
    // rather than derived on read because that is the whole supersession
    // rule — a member gets a new patchset, the composition changes, and
    // every run made against the old one is stale by comparison rather
    // than by anybody remembering to look. Nullable, because a push or a
    // change run has no changeset and never will.
    //
    // `member_token_ids` is on the job because the credentials are the
    // job's: a composed job is handed a read token per member repository
    // at spec time, and something has to be able to revoke them when the
    // job ends. An array rather than a table because they are only ever
    // read and written whole, by the one job that owns them.
    //
    // `changeset_checks` is a second table rather than more rows in
    // `check_runs`, and that is the load-bearing decision here. A file
    // with `on: [change, changeset]` produces the *same check name* from
    // both events at the same commit; put the composed verdict in
    // `check_runs` and the per-change land gate reads it as the member's
    // own answer — so a change would be held by, or released by, a build
    // of a combination it is only one part of. The composed verdicts are
    // the changeset's and reach nothing else.
    //
    // `external_id` is the job's id, or the run's for a run that settled
    // without one (a refused file, a blocked fork member): unique, so the
    // upsert is by identity and one job keeps one row however many times
    // it reports, exactly as `check_runs` does.
    r#"
    ALTER TABLE workflow_runs
      ADD COLUMN changeset_id TEXT REFERENCES changesets(id) ON DELETE CASCADE,
      ADD COLUMN composition TEXT;
    CREATE INDEX workflow_runs_changeset ON workflow_runs (changeset_id)
      WHERE changeset_id IS NOT NULL;

    ALTER TABLE workflow_jobs ADD COLUMN member_token_ids TEXT[] NOT NULL DEFAULT '{}';

    CREATE TABLE changeset_checks (
      id           TEXT PRIMARY KEY,
      changeset_id TEXT NOT NULL REFERENCES changesets(id) ON DELETE CASCADE,
      composition  TEXT NOT NULL,
      repo_id      TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
      run_id       TEXT NOT NULL REFERENCES workflow_runs(id) ON DELETE CASCADE,
      external_id  TEXT NOT NULL UNIQUE,
      name         TEXT NOT NULL,
      state        TEXT NOT NULL
                   CHECK (state IN ('queued','running','passing','failing','cancelled','skipped')),
      detail_url   TEXT,
      created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
      updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
    );
    CREATE INDEX changeset_checks_current ON changeset_checks (changeset_id, composition);
    "#,
    // 0049 — the notification queue's dedup, which had never existed.
    //
    // `jobs::enqueue_unique` deduplicates **only** through a partial
    // unique index, and `notify-mail` was covered by none: not by
    // `jobs_active_per_repo`, whose predicate names the sweep kinds and
    // never named this one. So the `ON CONFLICT DO NOTHING` matched
    // nothing, every enqueue inserted, and a twelve-comment review sent
    // twelve emails — from the feature whose whole argument is that a
    // forge which mails everybody about everything trains people to
    // filter it out.
    //
    // **Not by widening `jobs_active_per_repo`.** That index is keyed on
    // `(kind, repo_id)`, which for a notification means "one pending
    // mail per repository": "Ada opened change A" would swallow "Bo
    // commented on change B", and the second event would never be sent
    // at all. Losing a notification is worse than repeating one, and the
    // repository already says a sweep and an announcement are different
    // shapes of work. They need different keys, so they get different
    // indexes.
    //
    // What makes two notifications the same is that they would say the
    // same sentence about the same thing, and the payload — change or
    // changeset, event, actor — *is* that sentence. Hence the key, and
    // `md5` over it rather than the text: these payloads are a handful
    // of ids, but a btree entry is capped at ~2704 bytes and an oversized
    // payload must not turn "queue a notification" into an error.
    //
    // `state = 'queued'` and not `IN ('queued','running')`: a job that
    // has been claimed is already being sent, and it cannot carry an
    // event that happened after the send began. Collapsing into it would
    // be the silent loss above.
    //
    // Pre-existing duplicates are reconciled first, exactly as 0019,
    // 0026 and 0037 did it — a live queue holds them by construction,
    // and `CREATE UNIQUE INDEX` on a table that has them fails the
    // migration and the deploy with it. The oldest of each group
    // survives, because it is the one the next claim would take.
    r#"
    UPDATE jobs SET state = 'done',
        result = 'superseded: duplicate queued notification',
        updated_at = (EXTRACT(EPOCH FROM now()) * 1000)::BIGINT
    WHERE kind IN ('notify-mail', 'notify-changeset')
      AND state = 'queued'
      AND id NOT IN (
          SELECT DISTINCT ON (kind, org_id, COALESCE(repo_id, ''), md5(COALESCE(payload, '')))
              id FROM jobs
          WHERE kind IN ('notify-mail', 'notify-changeset')
            AND state = 'queued'
          ORDER BY kind, org_id, COALESCE(repo_id, ''), md5(COALESCE(payload, '')), created_at
      );
    CREATE UNIQUE INDEX jobs_pending_notify
        ON jobs (kind, org_id, COALESCE(repo_id, ''), md5(COALESCE(payload, '')))
        WHERE kind IN ('notify-mail', 'notify-changeset')
          AND state = 'queued';
    "#,
    // 0050 — a review is a conversation, not twelve orphan paragraphs.
    //
    // What this table has supported since 0017 is a flat list of
    // line-anchored and file-anchored remarks: twelve comments with no
    // way for the author to say "done", and no way for the reviewer to
    // see what is left. Each column below answers one part of that.
    //
    // `parent_id` is a reply. **One level only, and enforced above this
    // table** — a reply may not itself have children. Postgres cannot
    // say that in a column constraint without a trigger, and a trigger
    // would be a second place the rule lives; `changes::add_comment`
    // refuses a reply whose parent already has a parent, and the unit
    // test named for it is the gate. A tree is a forum, and a forum is
    // the thing we are deliberately not building: a review has to stay
    // readable top to bottom by somebody who was not in it.
    //
    // `resolved_at` / `resolved_by` live on the **root** only. A thread
    // resolves; a sentence inside one does not, and storing the state
    // per row would let a reply claim a state its thread does not have.
    // Who may set it is an OWNERS question rather than a write-access
    // one — see `changes_api::may_resolve` — and it is deliberately
    // **not** a land blocker. `changes::land_gate` is untouched here on
    // purpose: the gate blocks on things somebody said deliberately, and
    // a stray unresolved nit stopping a landing is how "resolve
    // everything" turns from information into ceremony.
    //
    // `side` is which half of the diff the anchor sits in — the
    // post-image line, or a line the change deleted. The default is
    // `'new'`, and deciding it now rather than later is the whole point
    // of putting it in this migration: every existing row was written
    // against the post-image, because that is the only side the API
    // could name. `'new'` is therefore a fact about the corpus, not a
    // guess. Leave the column nullable and let each reader guess
    // instead, and half the archive has an ambiguous anchor with
    // nothing left to reconstruct the answer from.
    //
    // `line_end` is a range. A remark about a loop is about the loop,
    // and pinning it to the `for` line makes the reader hunt for the
    // rest. Existing rows mean `line_end = line`, backfilled here
    // rather than coalesced on read, so that a NULL keeps its one
    // meaning: this comment has no line anchor at all.
    //
    // `original_line` / `original_patchset` are the anchor as first
    // written, kept beside the live one. A new patchset rewrites the
    // file under an old comment; without the original, that comment
    // either vanishes or silently points at the wrong line, and nothing
    // on the row says which of the two happened. Gerrit and GitHub both
    // keep the original for exactly this. Backfilled from the current
    // anchor, because for a row nothing has re-anchored yet the
    // original *is* the live one.
    //
    // `external_id` is nullable text carrying an argument, not dead
    // machinery. A review import with nothing to dedupe on cannot be run
    // twice — and the second run is the ordinary one, because the first
    // attempt at an import is always incomplete. It is also the string
    // that is already in people's chat logs
    // (`github.com/acme/widget/pull/4721#discussion_r123`), so it is the
    // identity worth keeping rather than one we mint. Unique where
    // present, so a re-run updates rather than doubles; NULL where a
    // comment was spoken here, and NULLs do not collide.
    r#"
    ALTER TABLE change_comments
        ADD COLUMN parent_id TEXT REFERENCES change_comments(id) ON DELETE CASCADE,
        ADD COLUMN resolved_at BIGINT,
        ADD COLUMN resolved_by TEXT REFERENCES users(id),
        ADD COLUMN side TEXT NOT NULL DEFAULT 'new' CHECK (side IN ('new','old')),
        ADD COLUMN line_end BIGINT,
        ADD COLUMN original_line BIGINT,
        ADD COLUMN original_patchset BIGINT,
        ADD COLUMN external_id TEXT;

    UPDATE change_comments
       SET line_end = line,
           original_line = line,
           original_patchset = patchset_number;

    CREATE INDEX change_comments_thread
        ON change_comments(parent_id) WHERE parent_id IS NOT NULL;
    CREATE UNIQUE INDEX change_comments_external
        ON change_comments(external_id) WHERE external_id IS NOT NULL;
    "#,
    // 0051 — a review is one act, and it can say no.
    //
    // Two holes, and they are the same hole. Every comment was posted
    // the instant it was typed, so a reviewer reading a forty-file
    // change published their half-formed first reaction and then argued
    // with it eleven comments later, in public, while the author
    // watched. And `approvals` is binary: the only negative signal the
    // product had was *silence*, which is indistinguishable from
    // "hasn't looked yet". A team cannot run a review culture where the
    // reviewer's second-most-common act has no representation at all.
    //
    // `verdict` is the three things a reviewer actually says. It is a
    // CHECK rather than a lookup table because the set is closed by
    // design: a fourth verdict is a product decision that should cost a
    // migration and an argument, not an INSERT.
    //
    // `state` is `draft` until the reviewer submits. **The invisibility
    // of a draft is not enforced here** — a column cannot filter a
    // SELECT — it is enforced in exactly one query, `changes::comments_for`,
    // and the partial index below is what makes that query cheap. Put
    // the filter in the handlers instead and the next handler forgets;
    // a leaked unpublished comment is the worst bug this feature can
    // have, so it gets one place to be wrong.
    //
    // `patchset_id`, like an approval: a verdict about code describes
    // the code it was about. "Alice approved" without saying what she
    // read is the thing patchset-scoped approvals exist to prevent, and
    // a blocking review is a stronger statement, not a weaker one.
    //
    // `withdrawn_at` is the whole argument for `request_changes` being
    // durable. An approval dies structurally when new code arrives —
    // that is right, the approver never saw it. A **block** must not,
    // or the author clears an objection by force-pushing over it, which
    // is precisely the move the objection existed to stop. So a
    // standing `request_changes` carries forward across patchsets and
    // ends only when its author says it has: `withdrawn_at`, set by
    // them, or superseded by their own later verdict on the change.
    //
    // What it *blocks* is decided above this table, in
    // `changes_api::review_state`, because it needs OWNERS: a block
    // counts against the land gate only when its author is somebody
    // OWNERS names for a path the patchset touches (or, where OWNERS
    // says `*` or governs nothing, somebody with write access — the
    // same rule that satisfies the path). GitHub lets any passer-by
    // wedge a pull request; we can do better precisely because the
    // reviewer set is computed rather than nominated. Every other block
    // is still recorded and rendered, labelled advisory.
    //
    // `external_id` carries the same argument as 0050's: a review
    // import with nothing to dedupe on cannot be run twice, and the
    // second run is the ordinary one.
    //
    // On `change_comments`: `review_id` is the review a comment was
    // drafted into, `published_at` is when it became visible. Existing
    // rows get `review_id NULL` and `published_at = created_at` —
    // backfilled rather than coalesced on read, so NULL keeps exactly
    // one meaning: this comment is somebody's unsubmitted draft. Every
    // comment written before this migration was published the moment it
    // was written, which is a fact about the corpus and not a guess.
    r#"
    CREATE TABLE reviews (
        id TEXT PRIMARY KEY,
        change_id TEXT NOT NULL REFERENCES changes(id) ON DELETE CASCADE,
        patchset_id TEXT NOT NULL,
        user_id TEXT NOT NULL REFERENCES users(id),
        verdict TEXT NOT NULL CHECK (verdict IN ('approve','comment','request_changes')),
        body TEXT,
        state TEXT NOT NULL CHECK (state IN ('draft','submitted')),
        submitted_at BIGINT,
        withdrawn_at BIGINT,
        external_id TEXT,
        created_at BIGINT NOT NULL
    );
    CREATE UNIQUE INDEX reviews_one_draft
        ON reviews(change_id, user_id) WHERE state = 'draft';
    CREATE INDEX reviews_change ON reviews(change_id, created_at, id);
    CREATE UNIQUE INDEX reviews_external
        ON reviews(external_id) WHERE external_id IS NOT NULL;

    ALTER TABLE change_comments
        ADD COLUMN review_id TEXT REFERENCES reviews(id) ON DELETE CASCADE,
        ADD COLUMN published_at BIGINT;

    UPDATE change_comments SET published_at = created_at;

    CREATE INDEX change_comments_unpublished
        ON change_comments(review_id) WHERE published_at IS NULL;
    "#,
    // 0051 — no card before anything; the provider is the merchant of
    // record.
    //
    // `pending` existed for one reason: a setup-mode Checkout that
    // saved a card and charged nothing, so a subscription could later
    // be opened on it server-side. Stripe's Managed Payments — Stripe
    // as merchant of record, which is what this fleet runs — has no
    // such page: a card is only ever met on the subscription page
    // itself. So an organization is free the moment it exists, holds
    // public repositories and people at once, and meets the card and
    // the price together the first time it wants something private.
    //
    // The one organization created `pending` before this landed (the
    // first real one, whose card page Stripe refused) becomes `free`,
    // which is what it would have been had the page worked.
    // `subscriptions.payment_method_ref` stays as a column nothing
    // writes: dropping it buys nothing and the row shape is read by
    // name.
    r#"
    UPDATE orgs SET plan = 'free' WHERE plan = 'pending';
    ALTER TABLE orgs DROP CONSTRAINT orgs_plan_known;
    ALTER TABLE orgs ADD CONSTRAINT orgs_plan_known
        CHECK (plan IN ('free','paid','past_due'));
    "#,
    // 0104: a job may ask not to be claimed yet.
    //
    // Everything in this table was claimable the instant it was queued,
    // so a worker that wanted to wait had nowhere to say so. The importer
    // wanted to: GitHub answers a rate limit with `Retry-After`, and the
    // only way to honour it was to hold the whole worker, which parks
    // every other organization's import behind one repository's limit.
    // So it did not honour it at all — see `importer::requeue`, which
    // took the value and dropped it.
    //
    // Default 0 means "now", so every existing row and every caller that
    // does not care is unchanged.
    r#"
    ALTER TABLE jobs ADD COLUMN not_before BIGINT NOT NULL DEFAULT 0;
    "#,
    // 0105: usage past the pool is metered.
    //
    // A seat used to buy an entitlement — minutes, and only minutes —
    // enforced as a ceiling and never invoiced. Now it buys a pool of
    // hosted minutes, transfer out of private repositories and storage
    // in them, and what an organization uses past the pool is reported
    // to the provider's meters and invoiced, up to a spend limit the
    // organization sets. Every table here exists so that reporting is
    // exactly-once across a crashed tick and so that the number a
    // person sees on the billing page is the number that was sent.
    //
    // `current_period_start`: the allowance resets with the provider's
    // billing period, and only the end of it was ever stored.
    // `metered_items`: which subscription items carry the metered
    // prices, as a JSON map of price id to item id; empty means the
    // subscription was opened before metering existed and sells seats
    // only.
    // `spend_limit_cents`: how much past the pool this org will pay for
    // in a period. Zero is the default and means "stop at the pool".
    // `usage_ledger`: one row per org, period and meter — the allowance
    // the seats bought, what was used, and how much overage has already
    // been reported. `reported` is cumulative; the next report is the
    // difference.
    // `meter_events`: the outbox. A row is written in the same
    // transaction that advances `reported`, and stamped `sent_at` when
    // the provider acknowledges it; the identifier doubles as the
    // provider's idempotency key, so a tick that died between the POST
    // and the stamp retries the same event and is deduplicated.
    // `storage_usage`: what each repository (later: each package) holds,
    // as the manifest counts it, SET on every write and by the sweep —
    // never incremented, so a crash anywhere converges on the next pass.
    // `storage_daily`: hourly samples of an org's private bytes folded
    // per day; the period's GB-month is the average of the days.
    r#"
    ALTER TABLE subscriptions ADD COLUMN current_period_start BIGINT;
    ALTER TABLE subscriptions ADD COLUMN metered_items TEXT NOT NULL DEFAULT '{}';
    ALTER TABLE orgs ADD COLUMN spend_limit_cents BIGINT NOT NULL DEFAULT 0
        CONSTRAINT orgs_spend_limit_nonnegative CHECK (spend_limit_cents >= 0);
    CREATE TABLE usage_ledger (
        org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        period_start BIGINT NOT NULL,
        meter TEXT NOT NULL CHECK (meter IN ('minutes','egress','storage')),
        allowance BIGINT NOT NULL DEFAULT 0,
        used BIGINT NOT NULL DEFAULT 0,
        reported BIGINT NOT NULL DEFAULT 0,
        updated_at BIGINT NOT NULL,
        PRIMARY KEY (org_id, period_start, meter)
    );
    CREATE TABLE meter_events (
        identifier TEXT PRIMARY KEY,
        org_id TEXT NOT NULL,
        meter TEXT NOT NULL,
        hour BIGINT NOT NULL,
        value BIGINT NOT NULL,
        created_at BIGINT NOT NULL,
        sent_at BIGINT
    );
    CREATE INDEX meter_events_unsent ON meter_events (created_at) WHERE sent_at IS NULL;
    CREATE TABLE storage_usage (
        owner_kind TEXT NOT NULL CHECK (owner_kind IN ('repo','package')),
        owner_id TEXT NOT NULL,
        org_id TEXT NOT NULL,
        private BOOLEAN NOT NULL,
        logical_bytes BIGINT NOT NULL DEFAULT 0,
        physical_bytes BIGINT,
        sampled_at BIGINT NOT NULL,
        inventoried_at BIGINT,
        PRIMARY KEY (owner_kind, owner_id)
    );
    CREATE INDEX storage_usage_org ON storage_usage (org_id, private);
    CREATE TABLE storage_daily (
        org_id TEXT NOT NULL,
        day TEXT NOT NULL,
        samples BIGINT NOT NULL DEFAULT 0,
        private_bytes_sum BIGINT NOT NULL DEFAULT 0,
        private_bytes_max BIGINT NOT NULL DEFAULT 0,
        total_bytes_last BIGINT NOT NULL DEFAULT 0,
        PRIMARY KEY (org_id, day)
    );
    "#,
    // 0106: the day's rollup says what reached the bill.
    //
    // `bytes_out` is everything served, public included, and the org
    // page draws it as "requests absorbed". What the meters count is
    // narrower — bytes out of *private* repositories, hosted minutes,
    // and the day's private storage sample — and a person reading the
    // usage page next to the invoice has to be able to see which is
    // which. Three columns, defaulted to zero so every existing day
    // reads as "nothing billable" rather than as unknown.
    r#"
    ALTER TABLE usage_daily ADD COLUMN hosted_minutes BIGINT NOT NULL DEFAULT 0;
    ALTER TABLE usage_daily ADD COLUMN private_bytes_out BIGINT NOT NULL DEFAULT 0;
    ALTER TABLE usage_daily ADD COLUMN private_bytes_stored BIGINT NOT NULL DEFAULT 0;
    "#,
    // 0107: GitHub Actions jobs that ran on our runners.
    //
    // A `workflow_job` webhook from a bound installation whose labels
    // ask for a Weft runner becomes a row here, and the row is what the
    // budget, the dashboard and the sweepers read. It is deliberately
    // **not** a `workflow_jobs` row: that table needs a Weft run, a Weft
    // repository and a frozen spec behind every job, and every reader of
    // it — the claim, the cancel cascade, the overdue sweep — assumes
    // one. A GitHub job has none of those; the only place the two must
    // meet is the minutes sum, which reads both.
    //
    // `multiplier` is stored rather than derived from `size`, so that
    // re-pricing a label later never rewrites what was already billed.
    // `(installation_id, github_job_id, run_attempt)` is the idempotency
    // key: GitHub redelivers, and a redelivered `queued` must not start a
    // second runner. `runner_name` is unique because the lifecycle after
    // launch is keyed on it — GitHub matches a runner to a job by label,
    // not by id, so the job that *reached* a runner (`ran_github_job_id`)
    // may not be the one that triggered it. `jit_config` holds the
    // runner's registration credential between mint and boot and is
    // nulled on its one read (`take_jit_config`).
    r#"
    CREATE TABLE github_jobs (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        installation_id TEXT NOT NULL,
        repo_full_name TEXT NOT NULL,
        repo_private BOOLEAN NOT NULL DEFAULT TRUE,
        github_job_id BIGINT NOT NULL,
        github_run_id BIGINT NOT NULL,
        run_attempt INT NOT NULL DEFAULT 1,
        job_name TEXT,
        html_url TEXT,
        labels TEXT[] NOT NULL DEFAULT '{}',
        size TEXT NOT NULL CHECK (size IN ('base','2x','4x')),
        multiplier INT NOT NULL,
        state TEXT NOT NULL CHECK (state IN
            ('refused','queued','launching','running','completed','failed','abandoned')),
        refusal TEXT,
        cancelled_on_github BOOLEAN NOT NULL DEFAULT FALSE,
        error TEXT,
        conclusion TEXT,
        runner_name TEXT,
        runner_id BIGINT,
        task_ref TEXT,
        jit_config TEXT,
        jit_token_id TEXT,
        ran_github_job_id BIGINT,
        attempts INT NOT NULL DEFAULT 0,
        lease_until BIGINT,
        not_before BIGINT NOT NULL DEFAULT 0,
        queued_at BIGINT NOT NULL,
        launched_at BIGINT,
        started_at BIGINT,
        completed_at BIGINT,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL,
        UNIQUE (installation_id, github_job_id, run_attempt)
    );
    CREATE UNIQUE INDEX github_jobs_runner_name ON github_jobs (runner_name)
        WHERE runner_name IS NOT NULL;
    CREATE INDEX github_jobs_claim ON github_jobs (state, not_before, queued_at);
    CREATE INDEX github_jobs_org_started ON github_jobs (org_id, started_at);
    "#,
    // 0108: the build cache, and the credential a runner reaches it with.
    //
    // One row per archive a job saved: `(org, repository, key, version)`
    // is the identity `actions/cache` restores by, unique so that two
    // jobs saving one key at once resolve to one winner (see
    // `cache::reserve`). `blocks` records the store objects in order,
    // because a large archive arrives as blocks and is served back as
    // one stream. `expires_at` mirrors the S3 lifecycle rule on `cache/`
    // so the rows and the objects go together.
    //
    // `github_jobs.cache_token_id` is the token the runner uses for the
    // cache for the length of its job — minted when the registration is
    // collected, so it never rides in RunTask either.
    r#"
    CREATE TABLE cache_entries (
        id TEXT PRIMARY KEY,
        org_id TEXT NOT NULL REFERENCES orgs(id),
        repo_full_name TEXT NOT NULL,
        key TEXT NOT NULL,
        version TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('reserving','ready')),
        size_bytes BIGINT NOT NULL DEFAULT 0,
        blocks JSONB NOT NULL DEFAULT '[]'::jsonb,
        uploaded JSONB NOT NULL DEFAULT '{}'::jsonb,
        created_at BIGINT NOT NULL,
        ready_at BIGINT,
        last_hit_at BIGINT,
        expires_at BIGINT NOT NULL,
        UNIQUE (org_id, repo_full_name, key, version)
    );
    CREATE INDEX cache_entries_lookup ON cache_entries (org_id, repo_full_name, version, key);
    CREATE INDEX cache_entries_expiry ON cache_entries (expires_at);
    ALTER TABLE github_jobs ADD COLUMN cache_token_id TEXT;
    "#,
    // 0109 — how many commits a repository has.
    //
    // Everything reachable from the default branch's tip, as GitHub
    // counts it, computed by the compaction job after each write and
    // stored beside the tip it was true for. Never computed on a
    // request: the dashboard used to walk the log for it, capped and
    // first-parent, nine seconds on the production mirror for a number
    // that was wrong. `exact` is false when the walk stopped at its cap.
    r#"
    CREATE TABLE repo_commit_counts (
        repo_id     TEXT PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
        tip         TEXT NOT NULL,
        count       BIGINT NOT NULL CHECK (count >= 0),
        exact       BOOLEAN NOT NULL,
        computed_at BIGINT NOT NULL
    );
    "#,
    // 0110 — static site hosting: what a repository publishes, and the
    // deploys it has published.
    //
    // A deploy for a site that needs no build stores **no bytes**. It is
    // the commit and the tree of the published directory, and the blobs
    // under that tree are already in this repository's store because
    // somebody pushed them. That is why publishing is instant, why the
    // free public tier is nearly free to run, and why there is no
    // storage column here: there is nothing to charge for that the
    // repository was not already charged for.
    //
    // `host` is the DNS label, derived once and stored rather than
    // computed per request — our names admit `_`, `.` and 100
    // characters, and a DNS label admits none of those. The derivation
    // is lossy, so the unique constraint is what actually decides
    // between two repositories that want the same label. See
    // `site::host`.
    //
    // The config is snapshotted onto the deploy rather than read from
    // the repository at serve time. A request must not depend on a tree
    // read to know whether this site wants SPA fallback, and a deploy
    // that was published under one config should keep being served
    // under it after somebody edits the file.
    r#"
    CREATE TABLE site_deploys (
        id          TEXT PRIMARY KEY,
        -- Insertion order, and the only thing history is ordered by.
        --
        -- Not `created_at`: that is a millisecond from whichever node
        -- handled the push, so two deploys in one millisecond order
        -- arbitrarily (ULID suffixes are random within a millisecond,
        -- not monotonic), and two nodes with skewed clocks can order
        -- *wrongly* — a rollback that appears to predate the deploy it
        -- rolls back is a history nobody can read.
        seq         BIGSERIAL NOT NULL UNIQUE,
        repo_id     TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
        commit_oid  TEXT NOT NULL,
        tree_oid    TEXT NOT NULL,
        publish     TEXT NOT NULL,
        spa         BOOLEAN NOT NULL,
        not_found   TEXT,
        created_at  BIGINT NOT NULL
    );
    CREATE INDEX site_deploys_repo ON site_deploys(repo_id, seq DESC);

    CREATE TABLE sites (
        repo_id     TEXT PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
        host        TEXT NOT NULL UNIQUE,
        branch      TEXT,
        current     TEXT REFERENCES site_deploys(id) ON DELETE SET NULL,
        created_at  BIGINT NOT NULL
    );

    -- Publishing is a queued job, deduplicated per repository like every
    -- other after-write fold. Two pushes in a second should publish the
    -- tip once, not twice, and the one that runs reads the tip at the
    -- moment it runs rather than the one that enqueued it.
    DROP INDEX jobs_active_per_repo;
    CREATE UNIQUE INDEX jobs_active_per_repo ON jobs (kind, repo_id)
        WHERE repo_id IS NOT NULL
          AND kind IN ('compact', 'cdnpack', 'fork', 'promote',
                       'import', 'checkspoll', 'sitepublish')
          AND state IN ('queued', 'running');
    "#,
    // 0111 — when we last asked GitHub whether a running job is still
    // running.
    //
    // The dispatcher reconciles a `running` row against GitHub because a
    // `workflow_job` completion that never lands leaves the row — and the
    // ECS task — alive until the fleet's six-hour cap. The check has to
    // be rate-limited or it becomes one API call per running job per
    // five-second poll, which is precisely the budget we take care not to
    // spend (see the primary rate-limit misclassification in CLAUDE.md).
    //
    // Its own column rather than `updated_at`, which cannot carry this:
    // the concurrency cap stops counting a row once `updated_at` falls
    // behind the fleet's timeout, so touching it on every check would
    // hold a leaked row against the cap forever — reviving the exact
    // starvation the cap bound was added to end.
    r#"
    ALTER TABLE github_jobs ADD COLUMN reconciled_at BIGINT NOT NULL DEFAULT 0;
    "#,
    // 0112 — an installation that arrived before there was an org to
    // bind it to.
    //
    // The setup callback could bind only when it knew the org: from the
    // `state` a dashboard-started connect carries, or from the one
    // pending connect the signed-in person had. An install that starts
    // on GitHub — the Marketplace listing, the App's own public page —
    // arrives with neither, and often with nobody signed in at all, and
    // used to dead-end on `connect=missing`. Now the callback proves the
    // installation is the person's (the `code` exchange, as before),
    // parks it here, and hands the browser a claim secret; after sign-in
    // or sign-up and an org choice the dashboard spends the claim and
    // the bind happens then. Single-use, short-lived, secret hashed at
    // rest, the same shape as `install_states`.
    r#"
    CREATE TABLE install_claims (
        id TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        installation_id TEXT NOT NULL,
        account TEXT,
        secret_hash TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        expires_at BIGINT NOT NULL,
        claimed_at BIGINT
    );
    "#,
    // 0113 — the package registry: what an organization publishes, what
    // it cached from upstream, and the provenance of each version.
    //
    // A package belongs to the **organization**, not to a repository.
    // A monorepo publishes forty of them and a package's source moves
    // between repositories over its life, so a repository is the wrong
    // owner — but *which* repository and commit produced a version is
    // the question asked during an incident, so it is recorded on the
    // version. That is `repo_id`/`commit_sha`/`job_id` below, and it is
    // the whole reason this is worth hosting beside the code rather
    // than buying.
    //
    // Bytes live in the object store under `o/<org>/pkg/<digest>`,
    // content-addressed, reached only through `registry::PackagePrefix`
    // — the same R8 enforcement point `RepoPrefix` is. `package_blobs`
    // is the control-plane record of what is out there; `package_files`
    // is what references it. Nothing here carries a refcount: a blob is
    // live iff some `package_files` row names its digest, which is a
    // mark-sweep the collector answers with one query, and a refcount
    // that drifts is a silently-deleted layer.
    r#"
    CREATE TABLE packages (
        id              TEXT PRIMARY KEY,
        org_id          TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        ecosystem       TEXT NOT NULL
                        CHECK (ecosystem IN ('npm','maven','pypi','cargo','oci')),
        -- What the publisher called it, and what a lookup matches on.
        -- The normalised form carries the unique index because several
        -- ecosystems fold names: PEP 503 makes `.`, `_` and `-`
        -- equivalent, npm lowercases. Two spellings that differ only
        -- there are one package, and admitting them as two rows is both
        -- a collision and an authorization bypass — the second row
        -- would answer for a name whose grants live on the first.
        name            TEXT NOT NULL,
        normalized_name TEXT NOT NULL,
        private         BOOLEAN NOT NULL DEFAULT TRUE,
        -- 'local' was published here; 'proxied' was cached from an
        -- upstream registry. The distinction decides who may write it
        -- and whether the licence gate ran, and it is what makes
        -- "private always wins" enforceable: a proxied row is never
        -- created for a name that already has a local one.
        origin          TEXT NOT NULL DEFAULT 'local'
                        CHECK (origin IN ('local','proxied')),
        created_at      BIGINT NOT NULL,
        updated_at      BIGINT NOT NULL,
        UNIQUE (org_id, ecosystem, normalized_name)
    );
    CREATE INDEX packages_org ON packages(org_id, ecosystem, normalized_name);

    CREATE TABLE package_versions (
        id                   TEXT PRIMARY KEY,
        package_id           TEXT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
        version              TEXT NOT NULL,
        normalized_version   TEXT NOT NULL,
        -- Yanked, not deleted. A version's bytes never change and a
        -- version never disappears: npm, PyPI, Maven and Cargo all
        -- cache on the assumption that a resolved version is stable,
        -- and a mutable one is a supply-chain hole. Yank hides it from
        -- resolution; it stays fetchable by exact version so a lockfile
        -- that already names it still builds.
        yanked               BOOLEAN NOT NULL DEFAULT FALSE,
        yank_reason          TEXT,
        -- The SPDX expression this version is under, and where we got
        -- it. 'declared' is the ecosystem's own metadata, 'detected' is
        -- a LICENSE file fingerprinted by `repo_meta::detect_license`,
        -- 'unknown' is neither — which is a disposition the policy has
        -- to have an answer for, not an error.
        license_expr         TEXT,
        license_source       TEXT NOT NULL DEFAULT 'unknown'
                             CHECK (license_source IN ('declared','detected','unknown')),
        -- The ecosystem's own version document, as published: npm's
        -- version manifest, a POM, a wheel's METADATA, an OCI config.
        --
        -- Stored verbatim rather than decomposed into columns, because
        -- it is what the resolver on the other end needs and we are not
        -- the authority on its shape. npm's packument must carry each
        -- version's `dependencies`, `bin`, `engines` and the rest; a
        -- registry that answered with only the fields it understood
        -- would install the package and none of its dependencies, and
        -- the failure would look like the package being broken.
        metadata             TEXT NOT NULL DEFAULT '{}',
        size_bytes           BIGINT NOT NULL DEFAULT 0,
        -- Provenance. `repo_id` is SET NULL rather than CASCADE because
        -- deleting the repository must not delete the record of what it
        -- shipped; `commit_sha` is plain text and survives it, which is
        -- what an incident actually needs.
        repo_id              TEXT REFERENCES repos(id) ON DELETE SET NULL,
        commit_sha           TEXT,
        job_id               TEXT,
        published_by_user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
        published_at         BIGINT NOT NULL,
        UNIQUE (package_id, normalized_version)
    );
    CREATE INDEX package_versions_pkg ON package_versions(package_id, published_at DESC);
    CREATE INDEX package_versions_repo ON package_versions(repo_id)
        WHERE repo_id IS NOT NULL;

    -- One row per file a version is made of. npm and Cargo have exactly
    -- one; a Maven version has a jar, a POM and usually sources and
    -- javadoc; an OCI manifest names a config and every layer. The
    -- digest is the join to the bytes, and two versions naming the same
    -- digest share one object — which is most of why an OCI registry
    -- fits in the same storage as a tarball registry at all.
    CREATE TABLE package_files (
        version_id   TEXT NOT NULL REFERENCES package_versions(id) ON DELETE CASCADE,
        filename     TEXT NOT NULL,
        digest       TEXT NOT NULL,
        size_bytes   BIGINT NOT NULL,
        content_type TEXT NOT NULL,
        -- The *other* digests this artifact has, as JSON: npm wants a
        -- SHA-1 `shasum` and a SHA-512 `integrity`, Maven publishes
        -- `.md5` and `.sha1` beside every file, PyPI reports an MD5.
        -- None of them is the SHA-256 the object is addressed by.
        --
        -- Computed once, at publish, from bytes we had just verified,
        -- and stored — rather than recomputed when a packument is read.
        -- Recomputing means one object GET *per version* on every
        -- resolve, which is the single hottest read a registry has; a
        -- package with two hundred versions would do two hundred store
        -- round trips for one `npm install`. They cannot drift from the
        -- object, because the object is content-addressed and a
        -- published version is immutable.
        digests      TEXT NOT NULL DEFAULT '{}',
        PRIMARY KEY (version_id, filename)
    );
    CREATE INDEX package_files_digest ON package_files(digest);

    -- The bytes in the store, per organization. `(org_id, digest)` and
    -- not `digest` alone: two organizations that happen to publish
    -- identical bytes get two objects, because a shared object is a
    -- cross-tenant read of whatever the other one can infer from its
    -- existence, and R8 is worth more than the deduplication.
    CREATE TABLE package_blobs (
        org_id     TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        digest     TEXT NOT NULL,
        size_bytes BIGINT NOT NULL,
        created_at BIGINT NOT NULL,
        PRIMARY KEY (org_id, digest)
    );

    -- Mutable pointers into the immutable set: npm dist-tags
    -- (`latest`, `next`), OCI tags. A tag moves; what it points at
    -- does not.
    CREATE TABLE package_tags (
        package_id TEXT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
        tag        TEXT NOT NULL,
        version_id TEXT NOT NULL REFERENCES package_versions(id) ON DELETE CASCADE,
        updated_at BIGINT NOT NULL,
        PRIMARY KEY (package_id, tag)
    );

    -- Which ecosystems this organization admits, and on what terms.
    -- Absent means 'off': a registry nobody switched on answers 404 for
    -- every ecosystem, so enabling one is a deliberate act with an
    -- audit row behind it.
    --
    -- `license_unknown` is per ecosystem and not one global default for
    -- a reason that only shows up in production: npm, PyPI, Cargo and
    -- Maven publishers declare a licence most of the time, and OCI
    -- images mostly declare nothing at all. One global 'block' would
    -- leave a proxy-enabled organization unable to pull almost any
    -- public image, which reads as the feature being broken rather
    -- than as the policy doing its job.
    CREATE TABLE org_ecosystems (
        org_id          TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        ecosystem       TEXT NOT NULL
                        CHECK (ecosystem IN ('npm','maven','pypi','cargo','oci')),
        mode            TEXT NOT NULL DEFAULT 'off'
                        CHECK (mode IN ('off','private','proxy')),
        license_unknown TEXT NOT NULL DEFAULT 'block'
                        CHECK (license_unknown IN ('block','allow')),
        updated_at      BIGINT NOT NULL,
        PRIMARY KEY (org_id, ecosystem)
    );

    -- The licence policy. `allow_list` admits only what is listed;
    -- `deny_list` admits everything except. Both spellings exist
    -- because they are not the same policy under an expression: an
    -- organization that has approved four licences wants a fifth to be
    -- refused, and one that has banned AGPL wants a licence nobody has
    -- heard of to pass.
    ALTER TABLE orgs ADD COLUMN license_mode TEXT NOT NULL DEFAULT 'deny_list'
        CHECK (license_mode IN ('allow_list','deny_list'));

    CREATE TABLE org_license_rules (
        org_id      TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        -- An SPDX identifier as written, e.g. 'MIT', 'Apache-2.0'.
        -- Stored case-folded for lookup because SPDX ids are compared
        -- case-insensitively and a rule that catches one spelling
        -- catches nothing.
        spdx_id     TEXT NOT NULL,
        disposition TEXT NOT NULL CHECK (disposition IN ('allow','deny')),
        updated_at  BIGINT NOT NULL,
        PRIMARY KEY (org_id, spdx_id)
    );
    "#,
    // 0114 — publishing from a job: the credential, and the one fact
    // that decides whether it may write.
    //
    // `from_fork` is carried forward from the trigger's own gate rather
    // than re-derived at the mint site. The fork gate
    // (`workflow/trigger.rs`) blocks a fork's change outright until a
    // maintainer approves it, and after that approval dispatch runs the
    // *identical* path as an organization's own push — nothing
    // downstream knew where the code came from. That was harmless while
    // the only credential was `repo:read` on one repository; it stops
    // being harmless the moment a job can publish, because "approve
    // this fork's workflow" would silently also mean "publish a release
    // under our name". Two decisions, and the approval door's own
    // documentation insists they stay two.
    //
    // Denormalised onto the job for the same reason `org_id` and
    // `repo_id` already are: the mint happens where the job is, and a
    // security decision behind two joins is a security decision
    // somebody will eventually skip.
    r#"
    ALTER TABLE workflow_runs ADD COLUMN from_fork BOOLEAN NOT NULL DEFAULT FALSE;
    ALTER TABLE workflow_jobs ADD COLUMN from_fork BOOLEAN NOT NULL DEFAULT FALSE;

    -- The registry credential minted for this attempt, recorded so it
    -- can be revoked with the rest when the job ends. A token nothing
    -- remembers is a token that outlives its job.
    ALTER TABLE workflow_jobs ADD COLUMN package_token_id TEXT;

    -- The same, for a GitHub Actions job running on our fleet. It is
    -- handed over with the runner's registration, beside the cache
    -- token, and dies the same way.
    ALTER TABLE github_jobs ADD COLUMN package_token_id TEXT;
    "#,
    // 0115 — the fourth meter: artifact storage, priced apart from git.
    //
    // `storage_daily` folded one number per organization per day, which
    // was right while there was one kind of stored byte. There are two
    // now, at different prices, so the kind joins the key. Every
    // existing row is a repository's — which is what the default says —
    // so the backfill is the default and there is nothing to rewrite.
    //
    // The ledger's CHECK is swapped rather than dropped: a meter name
    // the code does not know must still be refused by the database, or
    // the constraint is decoration.
    r#"
    ALTER TABLE storage_daily ADD COLUMN owner_kind TEXT NOT NULL DEFAULT 'repo'
        CHECK (owner_kind IN ('repo','package'));
    ALTER TABLE storage_daily DROP CONSTRAINT storage_daily_pkey;
    ALTER TABLE storage_daily ADD PRIMARY KEY (org_id, day, owner_kind);

    ALTER TABLE usage_ledger DROP CONSTRAINT usage_ledger_meter_check;
    ALTER TABLE usage_ledger ADD CONSTRAINT usage_ledger_meter_check
        CHECK (meter IN ('minutes','egress','storage','packages'));
    "#,
    // 0116 — the admission policy: what may enter the registry from an
    // upstream, and what happens when something may not.
    //
    // Three questions that get conflated and fail differently, so they
    // are three sets of columns:
    //
    // 1. **What may enter at all** — the licence rules (0111) plus
    //    `org_reserved_namespaces`. That second one closes a hole the
    //    "private always wins" rule does not: it protects a name this
    //    organization has *already published*, so if somebody registers
    //    `@acme/new-service` upstream before we do, a build asking for
    //    it gets theirs. A reserved namespace is never proxied,
    //    published or not.
    //
    // 2. **What may change** — `registry_cooldown_days`. An upstream
    //    version published less than N days ago is not served. Every
    //    compromised-maintainer incident worth naming was detected and
    //    pulled within hours to days, and almost nobody needs a package
    //    the day it ships. A semver gate is what people expect and
    //    protects far less: a malicious patch release is still a patch
    //    release.
    //
    // 3. **What happens on a violation** — `registry_policy_mode`,
    //    defaulting to `audit`. This is the column that decides whether
    //    the feature is ever switched on: a policy that blocks from day
    //    one meets a deadline in week one and gets turned off, and
    //    nobody learns what it would have cost.
    //
    // `package_policy_events` is deduplicated to one row per
    // (org, ecosystem, name, version) with a hit count. A CI run
    // resolving eight hundred dependencies must not write eight hundred
    // rows, and "how many times did this actually come up" is the
    // number that decides whether a rule earns its keep.
    r#"
    ALTER TABLE orgs ADD COLUMN registry_policy_mode TEXT NOT NULL DEFAULT 'audit'
        CHECK (registry_policy_mode IN ('audit','block'));
    -- 0 disables. Days rather than hours: the window is a judgement
    -- about how long a bad release takes to be noticed, and hours
    -- invites false precision about it.
    ALTER TABLE orgs ADD COLUMN registry_cooldown_days INT NOT NULL DEFAULT 0;

    CREATE TABLE org_reserved_namespaces (
        org_id     TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        ecosystem  TEXT NOT NULL
                   CHECK (ecosystem IN ('npm','maven','pypi','cargo','oci')),
        -- A normalised name prefix. `@acme` reserves the whole npm
        -- scope; `com.acme` a Maven groupId. Matched on a segment
        -- boundary, never as a bare substring — see
        -- `packages::namespace_covers`.
        pattern    TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        PRIMARY KEY (org_id, ecosystem, pattern)
    );

    CREATE TABLE package_policy_events (
        org_id      TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        ecosystem   TEXT NOT NULL,
        name        TEXT NOT NULL,
        version     TEXT NOT NULL,
        -- What the policy decided: 'blocked' when it was refused,
        -- 'would_block' when audit mode served it anyway. The second is
        -- the whole point of audit mode.
        disposition TEXT NOT NULL CHECK (disposition IN ('blocked','would_block')),
        -- Which rule decided: 'license', 'cooldown', 'reserved'.
        rule        TEXT NOT NULL,
        -- The sentence a person reads, already assembled.
        reason      TEXT NOT NULL,
        hits        BIGINT NOT NULL DEFAULT 1,
        first_at    BIGINT NOT NULL,
        last_at     BIGINT NOT NULL,
        PRIMARY KEY (org_id, ecosystem, name, version)
    );
    CREATE INDEX package_policy_events_org ON package_policy_events(org_id, last_at DESC);
    "#,
    // 0117 — when the *upstream* published a cached version.
    //
    // `published_at` is when this registry wrote the row, which for a
    // proxied artifact is when somebody first installed it. That is the
    // wrong clock for the cooldown, and wrong in the way that matters:
    // a cached artifact is always zero days old by that measure, so the
    // moment a version is cached it is refused for the next N days —
    // the cooldown would turn a working install into a broken one
    // rather than the other way round.
    //
    // NULL for a version this organization published itself. Admission
    // policy is about what enters from outside and never runs against
    // our own code, so there is no date to keep.
    r#"
    ALTER TABLE package_versions ADD COLUMN upstream_published_at BIGINT;
    "#,
    // 0118 — blobs bigger than one object, and the sessions that build
    // them.
    //
    // `ObjectStore::put` takes a slice: there is no streaming PUT and no
    // multipart upload anywhere in this codebase, so one object is one
    // resident allocation. A tarball, a wheel, a jar and a crate all fit
    // comfortably. **An OCI layer does not** — multi-hundred-megabyte
    // layers are routine, and `docker push` sends one of them as a
    // single request body.
    //
    // So a large blob is stored as an ordered list of blocks, each
    // content-addressed in the same key-space as an ordinary artifact.
    // The request body is cut into blocks as it streams in, so nothing
    // larger than one block is ever resident, on the way in or out.
    //
    // A blob with no rows here is one object, which is every artifact
    // the other four ecosystems store. The block path is not a second
    // scheme to remember: `blobs::get` asks this table first and falls
    // back, so a caller cannot address a blocked blob the wrong way.
    r#"
    CREATE TABLE package_blocks (
        org_id     TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        -- The whole blob's digest: the name the client asks for.
        digest     TEXT NOT NULL,
        seq        INT NOT NULL,
        -- The block's own digest, which is its key in the store. Blocks
        -- dedupe across blobs for free, and a block shared by two blobs
        -- is written once.
        block      TEXT NOT NULL,
        size_bytes BIGINT NOT NULL,
        PRIMARY KEY (org_id, digest, seq)
    );
    CREATE INDEX package_blocks_block ON package_blocks(org_id, block);

    -- An in-flight upload. The OCI protocol lets a client open a
    -- session, PATCH into it over several requests, and finish with a
    -- PUT naming the digest — possibly from a different node of the
    -- fleet, which is why this is a table and not a map in memory.
    CREATE TABLE package_uploads (
        id         TEXT PRIMARY KEY,
        org_id     TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
        -- The repository the session was opened against. Re-checked at
        -- every PATCH: a session id is a bearer capability, and one
        -- that could be finished against a different repository would
        -- be a way to write into a name the opener could not open a
        -- session for.
        package    TEXT NOT NULL,
        -- The blocks written so far, in order, as JSON.
        blocks     TEXT NOT NULL DEFAULT '[]',
        size_bytes BIGINT NOT NULL DEFAULT 0,
        started_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL
    );
    CREATE INDEX package_uploads_stale ON package_uploads(updated_at);
    "#,
];

/// The sync `postgres` client drives its own internal runtime with
/// `block_on`, which panics if the calling thread carries any tokio
/// runtime context — async workers and `spawn_blocking` threads alike.
/// The server calls control functions from both, so every database call
/// routes through here: with a runtime context present, hop to a scoped
/// OS thread (microseconds, against point queries); otherwise run
/// directly. `block_in_place` is NOT sufficient — it only works on
/// worker threads, not in `spawn_blocking` context.
fn run<T, F>(f: F) -> T
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| s.spawn(f).join().expect("db call thread"))
    } else {
        f()
    }
}

/// The live client plus what it takes to make a new one. A lost
/// connection (database restart, failover, terminated backend) is
/// re-established once per call, transparently — a fleet must not need
/// restarting because Postgres did. `postgres::Client`'s Drop also
/// drives its runtime; dropping it on a tokio thread (server shutdown)
/// would panic, so Drop hops threads too.
struct ClientBox {
    client: Option<Client>,
    url: String,
    lock_ms: u64,
}

impl ClientBox {
    fn client(&mut self) -> &mut Client {
        self.client.as_mut().expect("client present until drop")
    }

    fn reconnect(&mut self) -> Result<(), postgres::Error> {
        let mut fresh = Client::connect(&self.url, NoTls)?;
        fresh.batch_execute(&format!("SET lock_timeout = {}", self.lock_ms))?;
        // Replace before the old client drops (its Drop is runtime-safe
        // here: reconnect already runs off the async threads via `run`).
        self.client = Some(fresh);
        Ok(())
    }
}

impl Drop for ClientBox {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                std::thread::spawn(move || drop(client));
            }
        }
    }
}

/// A locked connection: the same query surface as `postgres::Client`,
/// with every call routed through [`run`] and retried once on a lost
/// connection.
pub(crate) struct Conn<'a>(MutexGuard<'a, ClientBox>);

impl Conn<'_> {
    fn call<T, F>(&mut self, f: F) -> Result<T, postgres::Error>
    where
        T: Send,
        F: Fn(&mut Client) -> Result<T, postgres::Error> + Send,
    {
        let boxed: &mut ClientBox = &mut self.0;
        run(move || match f(boxed.client()) {
            Err(e) if connection_lost(&e) => match boxed.reconnect() {
                Ok(()) => f(boxed.client()),
                Err(re) => Err(re),
            },
            other => other,
        })
    }

    pub fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, postgres::Error> {
        self.call(|c| c.execute(sql, params))
    }

    pub fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, postgres::Error> {
        self.call(|c| c.query(sql, params))
    }

    pub fn query_one(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, postgres::Error> {
        self.call(|c| c.query_one(sql, params))
    }

    pub fn query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, postgres::Error> {
        self.call(|c| c.query_opt(sql, params))
    }

    /// Run several statements as one unit, rolling back if the closure
    /// returns `Err`.
    ///
    /// Needed because some control-plane facts are only true together: a
    /// user with no membership belongs to no org and cannot be reached by
    /// any org-scoped query, so it would be an orphan row that the unique
    /// email index then blocks from being recreated. The closure gets a
    /// plain `Transaction`; it is deliberately not the retrying `call`
    /// wrapper, because replaying half a transaction after a reconnect
    /// would be worse than failing.
    pub fn transaction<T, F>(&mut self, f: F) -> Result<T, postgres::Error>
    where
        T: Send,
        F: FnOnce(&mut postgres::Transaction) -> Result<T, postgres::Error> + Send,
    {
        let boxed: &mut ClientBox = &mut self.0;
        run(move || {
            let mut tx = boxed.client().transaction()?;
            let out = f(&mut tx)?;
            tx.commit()?;
            Ok(out)
        })
    }
}

impl ControlDb {
    /// Connect to `postgres://…` and bring the schema current.
    pub fn open(url: &str) -> Result<ControlDb, String> {
        run(|| {
            let mut conn =
                Client::connect(url, NoTls).map_err(|e| format!("connect control db: {e}"))?;
            let lock_ms = std::env::var("STRATUM_DB_LOCK_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(DEFAULT_LOCK_TIMEOUT_MS);
            conn.batch_execute(&format!("SET lock_timeout = {lock_ms}"))
                .map_err(|e| e.to_string())?;
            Self::migrate(&mut conn)?;
            Ok(ControlDb {
                conn: Arc::new(Mutex::new(ClientBox {
                    client: Some(conn),
                    url: url.to_string(),
                    lock_ms,
                })),
            })
        })
    }

    fn migrate(conn: &mut Client) -> Result<(), String> {
        conn.execute("SELECT pg_advisory_lock($1)", &[&MIGRATE_LOCK_KEY])
            .map_err(|e| e.to_string())?;
        let result = Self::migrate_locked(conn);
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&MIGRATE_LOCK_KEY]);
        result
    }

    fn migrate_locked(conn: &mut Client) -> Result<(), String> {
        conn.batch_execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL)",
        )
        .map_err(|e| e.to_string())?;
        let current: i64 = conn
            .query_one(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                &[],
            )
            .map_err(|e| e.to_string())?
            .get(0);
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let version = (i + 1) as i64;
            if version <= current {
                continue;
            }
            let mut tx = conn.transaction().map_err(|e| e.to_string())?;
            tx.batch_execute(sql)
                // `detail`, not `{e}`: a failing migration reported as
                // "migration 28: db error" tells an operator watching a
                // deploy nothing at all, and tells whoever wrote the
                // migration less than that. This is the first thing
                // anybody reads when a release will not start.
                .map_err(|e| format!("migration {version}: {}", detail(&e)))?;
            tx.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES ($1, $2)",
                &[&version, &crate::ids::now_ms()],
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn lock(&self) -> Conn<'_> {
        Conn(self.conn.lock().unwrap())
    }
}

/// Is this a lost connection rather than a server-side answer? The very
/// first call on a terminated session surfaces an io-sourced
/// "error communicating with the server" before `is_closed()` turns
/// true, so both shapes count. A server-side error always carries a
/// DbError and is never retried.
fn connection_lost(e: &postgres::Error) -> bool {
    if e.is_closed() {
        return true;
    }
    if e.as_db_error().is_some() {
        return false;
    }
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        if s.downcast_ref::<std::io::Error>().is_some() {
            return true;
        }
        src = std::error::Error::source(s);
    }
    false
}

/// What a Postgres error actually says.
///
/// `postgres::Error`'s own `Display` renders a server-side failure as
/// the literal string **"db error"** — the message, the constraint name
/// and the SQLSTATE all live on the `DbError` behind it and are never
/// reached by `{e}`. So every `format!("doing the thing: {e}")` in this
/// crate produces "doing the thing: db error", which tells an operator
/// nothing and tells a developer debugging a failing test even less.
///
/// This is not hypothetical: it cost a debugging cycle on the issues
/// tracker, where a real constraint failure surfaced as `set issue
/// state: db error` and had to be reproduced by hand to find out what
/// had gone wrong.
///
/// The detail is safe to surface: it is our own SQL and our own
/// constraint names, not user data. Where a message reaches a client it
/// still goes through the API layer's own choice of status and wording.
pub fn detail(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => {
            let code = d.code().code();
            match d.constraint() {
                Some(c) => format!("{} [{code}, constraint {c}]", d.message()),
                None => format!("{} [{code}]", d.message()),
            }
        }
        None => e.to_string(),
    }
}

/// Did this error come from a UNIQUE/PK constraint? (name-collision arms
/// answer "already exists" instead of a 500).
pub(crate) fn is_unique_violation(e: &postgres::Error) -> bool {
    e.as_db_error()
        .map(|d| *d.code() == postgres::error::SqlState::UNIQUE_VIOLATION)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failing migration says what was wrong with it.
    ///
    /// This is the first thing anybody reads when a release will not
    /// start, and it used to read `migration 28: db error` — which tells
    /// an operator nothing and tells whoever wrote the migration less.
    /// It cost a debugging cycle here within minutes of `detail`
    /// existing: a `PRIMARY KEY` over an expression, which Postgres
    /// refuses, reported as "db error" and diagnosed only after the
    /// message was fixed.
    ///
    /// Asserted through `apply_migrations` on a deliberately broken
    /// statement rather than by reading the format string, because the
    /// format string being right is not the claim — the claim is that
    /// the SQLSTATE reaches the operator.
    #[test]
    fn a_failing_migration_names_what_postgres_objected_to() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("db_migfail")).unwrap();
        let err = db
            .lock()
            .execute(
                "CREATE TABLE broken (a TEXT, PRIMARY KEY (COALESCE(a, a)))",
                &[],
            )
            .expect_err("an expression in a PRIMARY KEY must be refused");
        assert_eq!(err.to_string(), "db error", "the premise has changed");
        let named = detail(&err);
        assert!(named.contains("42601"), "no SQLSTATE in {named:?}");
        assert!(
            named.contains("syntax error"),
            "no reason in {named:?} — an operator has nothing to act on"
        );
    }

    /// `detail` exists because `postgres::Error`'s own `Display` renders
    /// every server-side failure as the literal string **"db error"**.
    /// That is not a hypothetical: it cost a debugging cycle on the
    /// issues tracker, where a real constraint failure surfaced as
    /// `set issue state: db error` and had to be reproduced by hand.
    ///
    /// So the test provokes a **real** constraint violation rather than
    /// asserting over a hand-made error. A fake would prove the
    /// formatting and not the thing that matters — that the message,
    /// the SQLSTATE and the constraint name are actually reachable from
    /// what Postgres hands back.
    #[test]
    fn a_database_error_says_what_went_wrong_rather_than_db_error() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("db_detail")).unwrap();
        let u = crate::users::create(
            &db,
            "ada@example.com",
            "ada",
            Some("a long enough password"),
        )
        .unwrap();

        // Same primary key twice.
        let err = db
            .lock()
            .execute(
                "INSERT INTO users (id, email, name, created_at) VALUES ($1, $2, $3, $4)",
                &[&u.id, &"other@example.com", &"Other", &0i64],
            )
            .expect_err("a duplicate primary key must fail");

        // The thing the plain `{e}` gives you, and the reason this
        // function exists.
        assert_eq!(err.to_string(), "db error");

        let d = detail(&err);
        assert!(d.contains("23505"), "no SQLSTATE in {d:?}");
        assert!(d.contains("users_pkey"), "no constraint name in {d:?}");
        assert_ne!(d, "db error");

        // An error with no `DbError` behind it — a connection failure
        // rather than a server-side refusal — falls back to Display
        // rather than losing the message.
        let conn = match Client::connect("postgres://127.0.0.1:1/nope", NoTls) {
            Ok(_) => panic!("something is listening on port 1"),
            Err(e) => e,
        };
        assert!(conn.as_db_error().is_none());
        assert_eq!(detail(&conn), conn.to_string());
    }

    /// `connection_lost` decides whether a failed statement is retried
    /// on a fresh connection or handed back as the server's answer, and
    /// its three arms each need their own evidence:
    ///
    /// * a transport failure the client has not yet recorded as
    ///   "closed" — the io-sourced shape the very first call on a
    ///   terminated session surfaces — is lost;
    /// * a server-side error, even one that announces the session is
    ///   ending, is an answer and is never retried (retrying a
    ///   constraint violation would be a bug, and retrying `FATAL
    ///   57P01` would re-run the statement the operator was stopping);
    /// * a session the client already knows is closed is lost.
    ///
    /// The first arm used to be covered only by `faults_e2e`'s backend
    /// termination, and whether that test reaches the `source` walk or
    /// `is_closed()` depends on whether the client's connection task
    /// read the server's FIN before the next request was written: on
    /// the 2026-09-08 fleet run it took the other path, and coverage
    /// reported the walk uncovered on a tree that had not touched it.
    /// A refused connect carries its `io::Error` at a depth the harness
    /// can produce every time.
    #[test]
    fn a_transport_failure_is_lost_and_a_server_answer_is_not() {
        let refused = match Client::connect("postgres://127.0.0.1:1/nope", NoTls) {
            Ok(_) => panic!("something is listening on port 1"),
            Err(e) => e,
        };
        assert!(!refused.is_closed(), "the premise has changed: {refused}");
        assert!(refused.as_db_error().is_none());
        assert!(
            connection_lost(&refused),
            "an io-sourced error is a lost connection"
        );

        let mut client =
            Client::connect(&stratum_testkit::pg::test_db_url("db_lost"), NoTls).unwrap();
        let refusal = client
            .execute("SELECT 1 / 0", &[])
            .expect_err("division by zero is refused by the server");
        assert!(refusal.as_db_error().is_some());
        assert!(
            !connection_lost(&refusal),
            "a server-side answer is never retried"
        );

        // The server ends the session itself. The statement that asked
        // is answered (57P01, an answer, not a loss); everything after it
        // finds the session gone, by whichever of the two shapes the
        // client meets first.
        let ended = client
            .execute("SELECT pg_terminate_backend(pg_backend_pid())", &[])
            .expect_err("terminating your own backend ends the session");
        let answered = ended.as_db_error().map(|d| d.code().code()) == Some("57P01");
        let lost = connection_lost(&ended);
        assert!(answered || lost, "{}", detail(&ended));
        let after = client
            .execute("SELECT 1", &[])
            .expect_err("the session is gone");
        assert!(after.as_db_error().is_none(), "{}", detail(&after));
        assert!(connection_lost(&after), "{}", detail(&after));
    }
}
