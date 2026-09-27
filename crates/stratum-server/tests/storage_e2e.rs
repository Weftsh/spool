//! Stored bytes, end to end: the number a repository is billed for
//! follows every write, the sweep corrects anything that lied, and a
//! push that would take an organization past what its seats include is
//! refused at every door with nothing stored.
//!
//! The deal. Private storage is metered per seat, averaged over the
//! period, and capped by the org's spend limit (zero by default: stop
//! at the pool). Public repositories are free. Nothing is deleted for
//! billing; the write that would cross the cap is the thing refused.
//! The allowance is set here through the test-only
//! `STRATUM_PAID_STORAGE_BYTES_PER_SEAT` so a kilobyte of pack crosses
//! it, which is the only way a hermetic suite can see the refusal.
//!
//! Every refusal ends by proving the server is still serving and that
//! a fresh clone still passes `git fsck --full --strict`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use stratum_store::{LatencyModel, Manifest, ObjectStore};
use stratum_testkit::fake_stripe::FakeStripe;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::minio::{ROOT_PASSWORD, ROOT_USER};
use stratum_testkit::wait::wait_until;
use stratum_testkit::{Minio, Server};

const SOON: Duration = Duration::from_secs(15);
/// The sentence, as `stratum_proto::receive::STORAGE_REFUSAL` starts.
const REFUSAL: &str = "quota: this push would take private storage past the pool";
/// A kilobyte-scale allowance: a seed commit fits, a 20 KB blob does not.
const ALLOWANCE: &str = "4096";

/// A server that sells (the fake provider is configured) with the
/// allowance shrunk to something a test can cross.
fn stack(bucket_url: &str, hint: &str, extra: &[(&str, &str)]) -> (Server, FakeStripe) {
    let stripe = FakeStripe::start("whsec_test");
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), bucket_url)
        .db_hint(hint)
        .env("STRATUM_STORAGE_SWEEP_SECS", "0");
    for (k, v) in stripe.env() {
        b = b.env(k, v);
    }
    for (k, v) in extra {
        b = b.env(k, *v);
    }
    (b.start(), stripe)
}

fn pg(server: &Server) -> postgres::Client {
    postgres::Client::connect(&server.db_url, postgres::NoTls).expect("the control plane")
}

fn pg_at(db_url: &str) -> postgres::Client {
    postgres::Client::connect(db_url, postgres::NoTls).expect("the control plane")
}

fn org_id(db: &mut postgres::Client, org: &str) -> String {
    db.query_one("SELECT id FROM orgs WHERE name = $1", &[&org])
        .unwrap()
        .get(0)
}

fn repo_id(db: &mut postgres::Client, org: &str, repo: &str) -> String {
    db.query_one(
        "SELECT r.id FROM repos r JOIN orgs o ON o.id = r.org_id \
         WHERE o.name = $1 AND r.name = $2 AND r.state = 'active'",
        &[&org, &repo],
    )
    .unwrap()
    .get(0)
}

/// `(logical, physical)` as the row says, or `None` for no row.
fn row(db: &mut postgres::Client, repo_id: &str) -> Option<(i64, Option<i64>)> {
    db.query_opt(
        "SELECT logical_bytes, physical_bytes FROM storage_usage \
         WHERE owner_kind = 'repo' AND owner_id = $1",
        &[&repo_id],
    )
    .unwrap()
    .map(|r| (r.get(0), r.get(1)))
}

fn org_private(db: &mut postgres::Client, org_id: &str) -> i64 {
    db.query_one(
        "SELECT COALESCE(SUM(logical_bytes), 0)::BIGINT FROM storage_usage \
         WHERE org_id = $1 AND private",
        &[&org_id],
    )
    .unwrap()
    .get(0)
}

/// Set a row by hand — what a crashed hook, or a person with a database
/// prompt, leaves behind. The sweep is off in the suites that do this,
/// so it sticks until a write replaces it.
fn set_row(db: &mut postgres::Client, org_id: &str, repo_id: &str, private: bool, bytes: i64) {
    db.execute(
        "INSERT INTO storage_usage \
             (owner_kind, owner_id, org_id, private, logical_bytes, sampled_at) \
         VALUES ('repo', $1, $2, $3, $4, 0) \
         ON CONFLICT (owner_kind, owner_id) DO UPDATE SET \
             private = $3, logical_bytes = $4",
        &[&repo_id, &org_id, &private, &bytes],
    )
    .unwrap();
}

/// Every key under the repository's prefix, sorted — what "nothing
/// stored was touched" is checked against.
fn keys(store: &ObjectStore, org_id: &str, repo_id: &str) -> Vec<String> {
    let mut v: Vec<String> = store
        .list(&format!("o/{org_id}/r/{repo_id}/"))
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    v.sort();
    v
}

/// What the manifest says the repository holds under its own prefix.
fn manifest_bytes(store: &ObjectStore, org_id: &str, repo_id: &str) -> (Manifest, String, u64) {
    let key = keys(store, org_id, repo_id)
        .into_iter()
        .find(|k| k.ends_with("/manifest.json"))
        .expect("a manifest");
    let prefix = key.trim_end_matches("/manifest.json").to_string();
    let m: Manifest = serde_json::from_slice(&store.get(&key).unwrap()).unwrap();
    let bytes = m.stored_bytes(&prefix);
    (m, prefix, bytes)
}

fn commit_bytes(clone: &Path, name: &str, content: &[u8]) {
    std::fs::write(clone.join(name), content).unwrap();
    gitcli::git(clone, &["add", "-A"]);
    gitcli::git(clone, &["commit", "-q", "-m", name]);
}

/// Twenty kilobytes that do not compress: what a pack of them weighs is
/// what they weigh.
fn incompressible(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn push_token(server: &Server, admin: &str, org: &str, repo: &str) -> String {
    let (st, minted) = server.post(
        &format!("/v1/orgs/{org}/tokens"),
        admin,
        Some(serde_json::json!({
            "scopes": ["repo:read", "repo:write"], "repo": repo, "label": "push",
        })),
    );
    assert_eq!(st, 201, "{minted}");
    minted["token"].as_str().unwrap().to_string()
}

/// A repository with one seed commit on `main`, pushed over HTTP.
fn seeded(server: &Server, scratch: &Scratch, tok: &str, org: &str, repo: &str) -> PathBuf {
    let work = scratch.path().join(repo);
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            &server.authed_url(tok, org, repo),
            work.to_str().unwrap(),
        ],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    commit_bytes(&work, "seed.txt", b"seed\n");
    gitcli::git(&work, &["push", "-q", "origin", "main"]);
    work
}

fn error_of(body: &serde_json::Value) -> &str {
    body["error"].as_str().unwrap_or_default()
}

/// The row follows every write and the sweep corrects a lie.
///
/// A push sets the row to exactly what the manifest names — the WAL
/// entry's payload plus its oid list — and a second push moves it by
/// the second entry. A value written by hand is replaced by the next
/// sweep, which also inventories the bucket. Visibility moves the bytes
/// between the private pool and the free one, and deleting the
/// repository stops its bill at once.
#[test]
fn stored_bytes_follow_every_write_and_the_sweep_corrects_a_lie() {
    let minio = Minio::shared();
    let bucket = minio.bucket("storage-follow");
    let scratch = Scratch::new("storage-follow");
    let (server, _stripe) = stack(
        &bucket.base_url,
        "storage-follow",
        &[
            ("STRATUM_STORAGE_SWEEP_SECS", "1"),
            ("STRATUM_STORAGE_INVENTORY_SECS", "1"),
        ],
    );
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let admin = server.bootstrap_org("acme");
    server
        .admin(&["admin", "set-plan", "--org", "acme", "--plan", "paid"])
        .unwrap();
    for (name, public) in [("vault", false), ("open", true)] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name, "public": public })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let mut db = pg(&server);
    let acme = org_id(&mut db, "acme");
    let vault_id = repo_id(&mut db, "acme", "vault");
    let open_id = repo_id(&mut db, "acme", "open");

    // The first push: the row is the manifest's number, and that number
    // is the WAL entry's bytes plus twenty per object for its oid list.
    let tok = push_token(&server, &admin, "acme", "vault");
    let vault = seeded(&server, &scratch, &tok, "acme", "vault");
    let (m1, _, bytes1) = manifest_bytes(&store, &acme, &vault_id);
    assert_eq!(m1.wal.len(), 1, "one push, one WAL entry");
    assert_eq!(bytes1, m1.wal[0].bytes + m1.wal[0].entries * 20);
    assert!(bytes1 > 0);
    // Only the logical number: `physical_bytes` is the *inventory's*
    // column, and this test runs the inventory every second on purpose
    // (see the env above), so asserting it is still NULL here asserts
    // that a one-second worker has not ticked yet — true only while the
    // machine gets from server-start to this line inside a second. It
    // lost that race under a full-workspace `cargo llvm-cov` run, where
    // everything is slower. What the test means to prove about physical
    // bytes it proves properly further down, by *waiting* for the
    // inventory rather than racing it.
    assert_eq!(row(&mut db, &vault_id).map(|r| r.0), Some(bytes1 as i64));
    assert_eq!(org_private(&mut db, &acme), bytes1 as i64);

    // A second push adds its entry.
    commit_bytes(&vault, "more.txt", &incompressible(1_000));
    gitcli::git(&vault, &["push", "-q", "origin", "main"]);
    let (m2, _, bytes2) = manifest_bytes(&store, &acme, &vault_id);
    assert_eq!(m2.wal.len(), 2);
    assert!(bytes2 > bytes1 + 1_000, "{bytes2} <= {bytes1} + 1000");
    assert_eq!(row(&mut db, &vault_id).map(|r| r.0), Some(bytes2 as i64));

    // A lie in the row is not believed for long: the sweep SETs it back
    // to the manifest's number, and the inventory records what the
    // bucket physically holds — at least the manifest's bytes, since
    // the manifest is one of the objects.
    set_row(&mut db, &acme, &vault_id, true, 1);
    wait_until("the sweep to correct the row", SOON, || {
        row(&mut db, &vault_id).map(|r| r.0) == Some(bytes2 as i64)
    });
    wait_until("the inventory to run", SOON, || {
        row(&mut db, &vault_id).is_some_and(|r| r.1.is_some())
    });
    let (logical, physical) = row(&mut db, &vault_id).unwrap();
    assert!(
        physical.unwrap() >= logical,
        "physical {physical:?} < logical {logical}"
    );
    // The sweep sampled the day for the org, and the sample is the
    // private sum. Named by kind: the table holds a row per owner kind
    // per day, and a query that did not say which would find two and
    // fail with a row-count error — which is what happened the moment
    // package storage became its own meter.
    let (samples, max): (i64, i64) = db
        .query_one(
            "SELECT samples, private_bytes_max FROM storage_daily \
             WHERE org_id = $1 AND owner_kind = 'repo'",
            &[&acme],
        )
        .map(|r| (r.get(0), r.get(1)))
        .unwrap();
    assert!(samples >= 1);
    assert_eq!(max, bytes2 as i64);

    // …and the package row is its own, at zero, because this
    // organization has published nothing. A row that folded the two
    // together would bill git storage at the package rate.
    let pkg: i64 = db
        .query_one(
            "SELECT private_bytes_max FROM storage_daily \
             WHERE org_id = $1 AND owner_kind = 'package'",
            &[&acme],
        )
        .map(|r| r.get(0))
        .unwrap();
    assert_eq!(pkg, 0, "an org that published nothing holds package bytes");

    // Visibility moves the bytes between pools, both ways.
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/vault",
        &admin,
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(org_private(&mut db, &acme), 0);
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/vault",
        &admin,
        Some(serde_json::json!({ "public": false })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(org_private(&mut db, &acme), bytes2 as i64);

    // A public repository's bytes are recorded and never private.
    let open_tok = push_token(&server, &admin, "acme", "open");
    seeded(&server, &scratch, &open_tok, "acme", "open");
    let (_, _, open_bytes) = manifest_bytes(&store, &acme, &open_id);
    assert_eq!(row(&mut db, &open_id).map(|r| r.0), Some(open_bytes as i64));
    assert_eq!(org_private(&mut db, &acme), bytes2 as i64);
    let total: i64 = db
        .query_one(
            "SELECT COALESCE(SUM(logical_bytes), 0)::BIGINT FROM storage_usage WHERE org_id = $1",
            &[&acme],
        )
        .unwrap()
        .get(0);
    assert_eq!(total, (bytes2 + open_bytes) as i64);

    // Deleting stops the bill now, not when the sweeper reaches the prefix.
    let (st, out) = server.delete("/v1/orgs/acme/repos/vault", &admin);
    assert_eq!(st, 204, "{out}");
    assert_eq!(row(&mut db, &vault_id), None);
    assert_eq!(org_private(&mut db, &acme), 0);
    // The sweep does not resurrect a deleted repository's row.
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(row(&mut db, &vault_id), None);

    gitcli::clone_and_fsck(
        &server.authed_url(&open_tok, "acme", "open"),
        &scratch.path().join("open-again"),
    );
    assert!(server.healthy());
}

/// A sweep that fails does not take the worker with it.
///
/// The sweep runs from a spawned tick and returns an error only when
/// Postgres refuses it after the advisory lock; the tick logs that and
/// waits for the next. The log line used to be ledgered as unreachable
/// and then kept getting reached by accident — a server whose one-second
/// sweep ticked while its test's Postgres was being torn down — so the
/// coverage gate flipped between "stale" and "unledgered" depending on
/// the run. This makes the arm deterministic: the table the sweep's
/// first query touches is hidden, a lie planted in the row survives
/// two ticks, and once the table is back the very next tick corrects
/// the lie. The second half is the assertion that matters: the worker
/// logged and carried on rather than dying with the first error.
#[test]
fn a_sweep_that_fails_logs_and_the_next_tick_still_runs() {
    let minio = Minio::shared();
    let bucket = minio.bucket("storage-sweep-fails");
    let scratch = Scratch::new("storage-sweep-fails");
    let (server, _stripe) = stack(
        &bucket.base_url,
        "storage-sweep-fails",
        &[("STRATUM_STORAGE_SWEEP_SECS", "1")],
    );
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let admin = server.bootstrap_org("acme");
    server
        .admin(&["admin", "set-plan", "--org", "acme", "--plan", "paid"])
        .unwrap();
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "vault", "public": false })),
    );
    assert_eq!(st, 201, "{out}");
    let mut db = pg(&server);
    let acme = org_id(&mut db, "acme");
    let vault_id = repo_id(&mut db, "acme", "vault");
    let tok = push_token(&server, &admin, "acme", "vault");
    seeded(&server, &scratch, &tok, "acme", "vault");
    let (_, _, bytes) = manifest_bytes(&store, &acme, &vault_id);
    assert_eq!(row(&mut db, &vault_id).map(|r| r.0), Some(bytes as i64));

    // The sweep's first query after the lock prunes storage_usage; with
    // the table gone every sweep fails there and touches nothing else.
    db.execute(
        "ALTER TABLE storage_usage RENAME TO storage_usage_hidden",
        &[],
    )
    .unwrap();
    db.execute(
        "UPDATE storage_usage_hidden SET logical_bytes = 1 WHERE owner_kind = 'repo' AND owner_id = $1",
        &[&vault_id],
    )
    .unwrap();
    let lie = |db: &mut postgres::Client| -> i64 {
        db.query_one(
            "SELECT logical_bytes FROM storage_usage_hidden WHERE owner_kind = 'repo' AND owner_id = $1",
            &[&vault_id],
        )
        .unwrap()
        .get(0)
    };
    // Two ticks and more: the lie stands, because every sweep failed.
    std::thread::sleep(Duration::from_millis(2_500));
    assert_eq!(
        lie(&mut db),
        1,
        "a sweep ran against a table it cannot reach"
    );

    // The table is back, and the worker is still ticking: the next
    // sweep corrects the lie. A worker that died on the first error
    // would leave it at 1 forever.
    db.execute(
        "ALTER TABLE storage_usage_hidden RENAME TO storage_usage",
        &[],
    )
    .unwrap();
    wait_until(
        "the sweep after the failures to correct the row",
        SOON,
        || row(&mut db, &vault_id).map(|r| r.0) == Some(bytes as i64),
    );
    assert!(server.healthy());
    gitcli::clone_and_fsck(
        &server.authed_url(&tok, "acme", "vault"),
        &scratch.path().join("vault-again"),
    );
}

/// The cap, at every HTTP door: a push that would cross it is refused
/// in-band with nothing stored, a commit over REST is a 402 with the
/// same sentence, a smaller push lands, making the repository public
/// lets the big one land, making it private again is refused while it
/// holds more than the room, and an organization with no room is told
/// so in the advert before it builds a pack.
#[test]
fn a_push_past_the_cap_is_refused_at_every_http_door_and_nothing_is_stored() {
    let minio = Minio::shared();
    let bucket = minio.bucket("storage-cap");
    let scratch = Scratch::new("storage-cap");
    let (server, _stripe) = stack(
        &bucket.base_url,
        "storage-cap",
        &[("STRATUM_PAID_STORAGE_BYTES_PER_SEAT", ALLOWANCE)],
    );
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let admin = server.bootstrap_org("acme");
    server
        .admin(&["admin", "set-plan", "--org", "acme", "--plan", "paid"])
        .unwrap();
    for name in ["vault", "second"] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name, "public": false })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let mut db = pg(&server);
    let acme = org_id(&mut db, "acme");
    let vault_id = repo_id(&mut db, "acme", "vault");
    let second_id = repo_id(&mut db, "acme", "second");
    let tok = push_token(&server, &admin, "acme", "vault");
    let vault = seeded(&server, &scratch, &tok, "acme", "vault");
    let seed_bytes = row(&mut db, &vault_id).unwrap().0;
    assert!(seed_bytes > 0 && seed_bytes < 4096, "{seed_bytes}");
    let before = keys(&store, &acme, &vault_id);

    // Twenty kilobytes into a four-kilobyte pool: refused in the report,
    // with the sentence, and the prefix holds exactly what it held.
    let big = incompressible(20_000);
    commit_bytes(&vault, "big.bin", &big);
    let err = gitcli::git_expect_err(&vault, &["push", "-q", "origin", "main"]).unwrap();
    assert!(err.contains(REFUSAL), "{err}");
    assert!(err.contains("make the repository public"), "{err}");
    assert_eq!(
        keys(&store, &acme, &vault_id),
        before,
        "the refused push stored something"
    );
    assert_eq!(row(&mut db, &vault_id).unwrap().0, seed_bytes);
    assert!(server.healthy());

    // The same over REST: a 402 beginning `quota:`, nothing stored.
    let big_text = "x".repeat(20_000);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/vault/commits",
        &admin,
        Some(serde_json::json!({
            "branch": "main", "message": "big",
            "operations": [{ "op": "put", "path": "big.txt", "content": big_text }],
        })),
    );
    assert_eq!(st, 402, "{out}");
    assert!(error_of(&out).starts_with(REFUSAL), "{out}");
    assert_eq!(keys(&store, &acme, &vault_id), before);
    assert!(server.healthy());
    // A small one fits, and the row follows it.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/vault/commits",
        &admin,
        Some(serde_json::json!({
            "branch": "main", "message": "small",
            "operations": [{ "op": "put", "path": "small.txt", "content": "small" }],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let after_rest = row(&mut db, &vault_id).unwrap().0;
    assert!(after_rest > seed_bytes);

    // A smaller push lands too. The clone drops its refused commit and
    // builds on what the REST commit left.
    gitcli::git(&vault, &["fetch", "-q", "origin"]);
    gitcli::git(&vault, &["reset", "-q", "--hard", "origin/main"]);
    commit_bytes(&vault, "little.txt", b"little\n");
    gitcli::git(&vault, &["push", "-q", "origin", "main"]);
    let after_small = row(&mut db, &vault_id).unwrap().0;
    assert!(after_small > after_rest);
    assert!(after_small < 4096, "{after_small}");

    // The repository view says the door is open while there is room.
    let (_, view) = server.req("GET", "/v1/orgs/acme/repos/vault", &admin, None);
    assert!(view["write_blocked"].is_null(), "{view}");
    assert_eq!(view["viewer_write"], true, "{view}");

    // Making it public is the way out the sentence names: the big push
    // lands, and its bytes are recorded in the free pool.
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/vault",
        &admin,
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(st, 200, "{out}");
    commit_bytes(&vault, "big.bin", &big);
    gitcli::git(&vault, &["push", "-q", "origin", "main"]);
    let held = row(&mut db, &vault_id).unwrap().0;
    assert!(held > 20_000, "{held}");
    assert_eq!(org_private(&mut db, &acme), 0);
    // And back to private is refused while it holds more than the room
    // — the same sentence, and the row stays public.
    let (st, out) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/vault",
        &admin,
        Some(serde_json::json!({ "public": false })),
    );
    assert_eq!(st, 402, "{out}");
    assert!(error_of(&out).starts_with(REFUSAL), "{out}");
    let (_, view) = server.req("GET", "/v1/orgs/acme/repos/vault", &admin, None);
    assert_eq!(view["public"], true, "{view}");
    assert_eq!(org_private(&mut db, &acme), 0);
    assert!(server.healthy());

    // No room at all: the pool is full before a pack is built, so the
    // advert says so — git shows it as a remote error — and the second
    // repository's view carries the sentence for its readers.
    set_row(&mut db, &acme, &vault_id, true, 4096);
    let (_, view) = server.req("GET", "/v1/orgs/acme/repos/second", &admin, None);
    assert_eq!(view["viewer_write"], false, "{view}");
    assert!(
        view["write_blocked"]
            .as_str()
            .unwrap_or_default()
            .starts_with(REFUSAL),
        "{view}"
    );
    let second_tok = push_token(&server, &admin, "acme", "second");
    let second = scratch.path().join("second");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            &server.authed_url(&second_tok, "acme", "second"),
            second.to_str().unwrap(),
        ],
    );
    gitcli::git(&second, &["checkout", "-q", "-b", "main"]);
    commit_bytes(&second, "seed.txt", b"seed\n");
    let err = gitcli::git_expect_err(&second, &["push", "-q", "origin", "main"]).unwrap();
    assert!(err.contains(REFUSAL), "{err}");
    assert!(keys(&store, &acme, &second_id)
        .iter()
        .all(|k| k.ends_with("/manifest.json")));
    assert!(server.healthy());
    // The pool clears; the push lands.
    set_row(&mut db, &acme, &vault_id, false, held);
    gitcli::git(&second, &["push", "-q", "origin", "main"]);
    assert!(row(&mut db, &second_id).unwrap().0 > 0);

    // Everything served through all of that is still a sound repository.
    let again = scratch.path().join("vault-again");
    gitcli::clone_and_fsck(&server.authed_url(&tok, "acme", "vault"), &again);
    assert!(gitcli::git(&again, &["log", "--oneline"]).contains("big.bin"));
    gitcli::clone_and_fsck(
        &server.authed_url(&second_tok, "acme", "second"),
        &scratch.path().join("second-again"),
    );
    assert!(server.healthy());
}

/// A server with an SSH front door, the provider configured and the
/// allowance shrunk. The SSH port is chosen inside the retry closure,
/// for the reason `ssh_e2e` gives at length.
struct SshServer {
    child: std::process::Child,
    base: String,
    db_url: String,
    ssh_port: u16,
}

impl Drop for SshServer {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn server_env(cmd: &mut Command, store_url: &str, db: &str) {
    cmd.env_clear()
        .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STRATUM_STORE_URL", store_url)
        .env("STRATUM_DB_URL", db)
        .env("STRATUM_MIRROR_POLL_SECS", "0")
        .env("STRATUM_STORAGE_SWEEP_SECS", "0")
        .env("AWS_ACCESS_KEY_ID", ROOT_USER)
        .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
        .env("AWS_REGION", "us-east-1");
}

fn ssh_stack(scratch: &Scratch, store_url: &str, hint: &str, stripe: &FakeStripe) -> SshServer {
    let (host_path, _) = keygen(scratch.path(), "host-key");
    let host_pem = std::fs::read_to_string(&host_path).unwrap();
    let db_url = stratum_testkit::pg::test_db_url(hint);
    let chosen = std::cell::Cell::new(0u16);
    let (child, bind) = stratum_testkit::server::spawn_on_free_port(|bind| {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ssh_port = l.local_addr().unwrap().port();
        drop(l);
        chosen.set(ssh_port);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        server_env(&mut cmd, store_url, &db_url);
        cmd.env("STRATUM_BIND", bind)
            .env("STRATUM_SSH_BIND", format!("127.0.0.1:{ssh_port}"))
            .env("STRATUM_SSH_HOST_KEY", &host_pem)
            .env(
                "STRATUM_SSH_PUBLIC_URL",
                format!("ssh://git@127.0.0.1:{ssh_port}"),
            )
            .env("STRATUM_DATA_DIR", scratch.path().join("data"))
            .env("STRATUM_PAID_STORAGE_BYTES_PER_SEAT", ALLOWANCE);
        for (k, v) in stripe.env() {
            cmd.env(k, v);
        }
        cmd
    });
    SshServer {
        child,
        base: format!("http://{bind}"),
        db_url,
        ssh_port: chosen.get(),
    }
}

impl SshServer {
    fn admin(&self, store_url: &str, args: &[&str]) -> serde_json::Value {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        server_env(&mut cmd, store_url, &self.db_url);
        let out = cmd.args(args).output().expect("run admin command");
        assert!(
            out.status.success(),
            "admin {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
    }

    fn post(&self, path: &str, token: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let resp = ureq::post(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let (st, r) = match resp {
            Ok(r) => (r.status(), r),
            Err(ureq::Error::Status(code, r)) => (code, r),
            Err(e) => panic!("transport: {e}"),
        };
        let text = r.into_string().unwrap_or_default();
        (
            st,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
        )
    }

    fn ssh_url(&self, org: &str, repo: &str) -> String {
        format!("ssh://git@127.0.0.1:{}/{org}/{repo}.git", self.ssh_port)
    }

    fn healthy(&self) -> bool {
        ureq::get(&format!("{}/healthz", self.base))
            .timeout(Duration::from_secs(2))
            .call()
            .is_ok()
    }
}

fn keygen(dir: &Path, name: &str) -> (PathBuf, String) {
    let priv_path = dir.join(name);
    let out = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&priv_path)
        .stdin(Stdio::null())
        .output()
        .expect("run ssh-keygen");
    assert!(
        out.status.success(),
        "ssh-keygen: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pub_line = std::fs::read_to_string(priv_path.with_extension("pub")).unwrap();
    (priv_path, pub_line.trim().to_string())
}

fn git_ssh(cwd: &Path, ssh_cmd: &str, args: &[&str]) -> Result<String, String> {
    let cfg = format!("core.sshCommand={ssh_cmd}");
    let mut full = vec!["-c", cfg.as_str()];
    full.extend_from_slice(args);
    gitcli::git_expect_err(cwd, &full).map_or_else(Ok, Err)
}

/// The same cap over SSH: the big push is refused in-band with the
/// sentence and nothing stored; with no room at all the refusal comes
/// before the advert; a fresh clone is sound throughout.
#[test]
fn an_ssh_push_past_the_cap_is_refused_with_the_same_sentence() {
    let minio = Minio::shared();
    let bucket = minio.bucket("storage-ssh");
    let scratch = Scratch::new("storage-ssh");
    let stripe = FakeStripe::start("whsec_test");
    let server = ssh_stack(&scratch, &bucket.base_url, "storage-ssh", &stripe);
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let boot = server.admin(&bucket.base_url, &["admin", "bootstrap", "--org", "acme"]);
    let admin = boot["admin_token"].as_str().unwrap().to_string();
    let admin_id = admin.split('_').nth(1).unwrap().to_string();
    server.admin(
        &bucket.base_url,
        &["admin", "set-plan", "--org", "acme", "--plan", "paid"],
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "vault", "public": false }),
    );
    assert_eq!(st, 201, "{out}");
    let (key, pub_line) = keygen(scratch.path(), "dev-key");
    let (st, out) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": pub_line, "token_id": admin_id, "label": "laptop" }),
    );
    assert_eq!(st, 201, "{out}");
    let known_hosts = scratch.path().join("known_hosts");
    let ssh = format!(
        "ssh -F none -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none \
         -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={} -i {}",
        known_hosts.display(),
        key.display()
    );
    let mut db = pg_at(&server.db_url);
    let acme = org_id(&mut db, "acme");
    let vault_id = repo_id(&mut db, "acme", "vault");

    let vault = scratch.path().join("vault");
    git_ssh(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "vault"),
            vault.to_str().unwrap(),
        ],
    )
    .unwrap();
    gitcli::git(&vault, &["checkout", "-q", "-b", "main"]);
    commit_bytes(&vault, "seed.txt", b"seed\n");
    git_ssh(&vault, &ssh, &["push", "-q", "origin", "main"]).unwrap();
    let seed_bytes = row(&mut db, &vault_id).unwrap().0;
    assert!(seed_bytes > 0 && seed_bytes < 4096, "{seed_bytes}");
    let before = keys(&store, &acme, &vault_id);

    commit_bytes(&vault, "big.bin", &incompressible(20_000));
    let err = git_ssh(&vault, &ssh, &["push", "-q", "origin", "main"]).unwrap_err();
    assert!(err.contains(REFUSAL), "{err}");
    assert_eq!(keys(&store, &acme, &vault_id), before);
    assert_eq!(row(&mut db, &vault_id).unwrap().0, seed_bytes);
    assert!(server.healthy());

    // No room at all: refused before the advert, same words.
    gitcli::git(&vault, &["reset", "-q", "--hard", "HEAD~1"]);
    commit_bytes(&vault, "little.txt", b"little\n");
    set_row(&mut db, &acme, &vault_id, true, 4096);
    let err = git_ssh(&vault, &ssh, &["push", "-q", "origin", "main"]).unwrap_err();
    assert!(err.contains(REFUSAL), "{err}");
    assert_eq!(keys(&store, &acme, &vault_id), before);
    assert!(server.healthy());
    // Room again: it lands, and the row is the manifest's number again,
    // not the one written by hand.
    set_row(&mut db, &acme, &vault_id, true, seed_bytes);
    git_ssh(&vault, &ssh, &["push", "-q", "origin", "main"]).unwrap();
    let (_, _, bytes) = manifest_bytes(&store, &acme, &vault_id);
    assert_eq!(row(&mut db, &vault_id).unwrap().0, bytes as i64);

    let again = scratch.path().join("vault-again");
    git_ssh(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "vault"),
            again.to_str().unwrap(),
        ],
    )
    .unwrap();
    gitcli::fsck(&again);
    assert!(server.healthy());
}

/// A bare origin under a file:// root with `commits` commits on `main`.
fn make_origin(root: &Path, name: &str, commits: usize) -> PathBuf {
    let work = root.join("work").join(name);
    gitcli::fixture_repo(&work, commits);
    gitcli::git(&work, &["checkout", "-q", "main"]);
    let bare = root.join(format!("acme/{name}.git"));
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    gitcli::git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    gitcli::git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    bare
}

/// A private mirror counts like a private repository, and a sync that
/// would cross the cap is refused like a push: recorded as the sync
/// error, nothing written, retried on the next poll — and it goes
/// through once there is room.
#[test]
fn a_private_mirror_that_would_cross_the_cap_records_the_refusal_and_writes_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("storage-mirror");
    let scratch = Scratch::new("storage-mirror");
    let (server, _stripe) = stack(
        &bucket.base_url,
        "storage-mirror",
        &[("STRATUM_PAID_STORAGE_BYTES_PER_SEAT", ALLOWANCE)],
    );
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let admin = server.bootstrap_org("acme");
    server
        .admin(&["admin", "set-plan", "--org", "acme", "--plan", "paid"])
        .unwrap();
    let origins = scratch.path().join("origins");
    let bare = make_origin(&origins, "widget", 2);
    let origin_url = format!("file://{}", bare.display());
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({
            "name": "widget", "provider": "generic", "origin": origin_url, "public": false,
        })),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "filler", "public": false })),
    );
    assert_eq!(st, 201, "{out}");
    let mut db = pg(&server);
    let acme = org_id(&mut db, "acme");
    let widget_id = repo_id(&mut db, "acme", "widget");
    let filler_id = repo_id(&mut db, "acme", "filler");
    let sync = || server.post("/v1/orgs/acme/mirrors/widget/sync", &admin, None);

    // The first sync fits and is counted as private.
    let (st, out) = sync();
    assert_eq!(st, 200, "{out}");
    assert!(out["sync_error"].is_null(), "{out}");
    let held = row(&mut db, &widget_id).unwrap().0;
    assert!(held > 0 && held < 4096, "{held}");
    assert_eq!(org_private(&mut db, &acme), held);
    let synced = keys(&store, &acme, &widget_id);

    // Another private repository fills the pool to fifty bytes of room.
    // The origin grows by twenty kilobytes; the incremental sync packs
    // them, sees they do not fit, and writes nothing.
    set_row(&mut db, &acme, &filler_id, true, 4096 - held - 50);
    let work = origins.join("work").join("widget");
    commit_bytes(&work, "big.bin", &incompressible(20_000));
    gitcli::git(&work, &["push", "-q", bare.to_str().unwrap(), "main:main"]);
    // A sync that fails is a 502 naming why, and the reason is recorded
    // on the repository for the next poll and the next reader.
    let refused = |st: u16, out: &serde_json::Value| {
        assert_eq!(st, 502, "{out}");
        assert!(error_of(out).starts_with(REFUSAL), "{out}");
        let (_, view) = server.req("GET", "/v1/orgs/acme/repos/widget", &admin, None);
        assert!(
            view["sync_error"]
                .as_str()
                .unwrap_or_default()
                .starts_with(REFUSAL),
            "{view}"
        );
    };
    let (st, out) = sync();
    refused(st, &out);
    assert_eq!(
        keys(&store, &acme, &widget_id),
        synced,
        "the refused sync wrote something"
    );
    assert_eq!(row(&mut db, &widget_id).unwrap().0, held);
    // A push through the mirror is measured the same way, and refused
    // before the origin hears of it: the promise is that the origin
    // never takes a push the mirror then cannot store. Fifty bytes of
    // room, a twenty kilobyte pack, and the origin's tip is untouched.
    let origin_tip = gitcli::git(&bare, &["rev-parse", "refs/heads/main"]);
    let through = scratch.path().join("widget-through");
    gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "widget"), &through);
    commit_bytes(&through, "more.bin", &incompressible(20_000));
    let err = gitcli::git_expect_err(&through, &["push", "-q", "origin", "main"]).unwrap();
    assert!(err.contains(REFUSAL), "{err}");
    assert_eq!(
        gitcli::git(&bare, &["rev-parse", "refs/heads/main"]),
        origin_tip,
        "the origin heard of a push the mirror could not store"
    );
    assert_eq!(keys(&store, &acme, &widget_id), synced);
    // No room at all: refused before the origin is even fetched.
    set_row(&mut db, &acme, &filler_id, true, 4096);
    let (st, out) = sync();
    refused(st, &out);
    assert_eq!(keys(&store, &acme, &widget_id), synced);
    // The mirror's view carries the sentence, like any private repository.
    let (_, view) = server.req("GET", "/v1/orgs/acme/repos/widget", &admin, None);
    assert!(
        view["write_blocked"]
            .as_str()
            .unwrap_or_default()
            .starts_with(REFUSAL),
        "{view}"
    );
    assert!(server.healthy());

    // Raising the limit is the way out the sentence names first: ten
    // cents buys a gigabyte-month, and the same sync goes through. (The
    // filler is deleted too — that frees its bytes, but a twenty
    // kilobyte pack never fitted a four kilobyte pool on its own.)
    let (st, out) = server.delete("/v1/orgs/acme/repos/filler", &admin);
    assert_eq!(st, 204, "{out}");
    assert_eq!(org_private(&mut db, &acme), held);
    db.execute(
        "UPDATE orgs SET spend_limit_cents = 10 WHERE id = $1",
        &[&acme],
    )
    .unwrap();
    let (st, out) = sync();
    assert_eq!(st, 200, "{out}");
    assert!(out["sync_error"].is_null(), "{out}");
    assert!(row(&mut db, &widget_id).unwrap().0 > 20_000);
    let again = scratch.path().join("widget-again");
    gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "widget"), &again);
    assert!(gitcli::git(&again, &["log", "--oneline"]).contains("big.bin"));
    assert!(server.healthy());
}
