//! Stored bytes, end to end: the number the usage page shows for a
//! repository follows every write, and the sweep corrects anything that
//! lied.
//!
//! Every case ends by proving the server is still serving and that a
//! fresh clone still passes `git fsck --full --strict`.

use std::path::{Path, PathBuf};
use std::time::Duration;
use stratum_store::{LatencyModel, Manifest, ObjectStore};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::wait::wait_until;
use stratum_testkit::{Minio, Server};

const SOON: Duration = Duration::from_secs(15);

/// A server with the storage sweep off, so every number a test reads
/// is one a write or an explicit sweep put there.
fn stack(bucket_url: &str, hint: &str, extra: &[(&str, &str)]) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), bucket_url)
        .db_hint(hint)
        .env("STRATUM_STORAGE_SWEEP_SECS", "0");
    for (k, v) in extra {
        b = b.env(k, *v);
    }
    b.start()
}

fn pg(server: &Server) -> postgres::Client {
    postgres::Client::connect(&server.db_url, postgres::NoTls).expect("the control plane")
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

fn set_row(db: &mut postgres::Client, org_id: &str, repo_id: &str, bytes: i64) {
    db.execute(
        "INSERT INTO storage_usage \
             (owner_kind, owner_id, org_id, private, logical_bytes, sampled_at) \
         VALUES ('repo', $1, $2, TRUE, $3, 0) \
         ON CONFLICT (owner_kind, owner_id) DO UPDATE SET logical_bytes = $3",
        &[&repo_id, &org_id, &bytes],
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

/// The row follows every write and the sweep corrects a lie.
///
/// A push sets the row to exactly what the manifest names — the WAL
/// entry's payload plus its oid list — and a second push moves it by
/// the second entry. A value written by hand is replaced by the next
/// sweep, which also inventories the bucket. Each repository is its own
/// row, and deleting one stops counting it at once.
#[test]
fn stored_bytes_follow_every_write_and_the_sweep_corrects_a_lie() {
    let minio = Minio::shared();
    let bucket = minio.bucket("storage-follow");
    let scratch = Scratch::new("storage-follow");
    let server = stack(
        &bucket.base_url,
        "storage-follow",
        &[
            ("STRATUM_STORAGE_SWEEP_SECS", "1"),
            ("STRATUM_STORAGE_INVENTORY_SECS", "1"),
        ],
    );
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let admin = server.bootstrap_org("acme");
    for name in ["vault", "open"] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name })),
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
    set_row(&mut db, &acme, &vault_id, 1);
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
    // A second repository's bytes are its own row, beside the first.
    let open_tok = push_token(&server, &admin, "acme", "open");
    seeded(&server, &scratch, &open_tok, "acme", "open");
    let (_, _, open_bytes) = manifest_bytes(&store, &acme, &open_id);
    assert_eq!(row(&mut db, &open_id).map(|r| r.0), Some(open_bytes as i64));
    assert_eq!(row(&mut db, &vault_id).map(|r| r.0), Some(bytes2 as i64));
    let total: i64 = db
        .query_one(
            "SELECT COALESCE(SUM(logical_bytes), 0)::BIGINT FROM storage_usage WHERE org_id = $1",
            &[&acme],
        )
        .unwrap()
        .get(0);
    assert_eq!(total, (bytes2 + open_bytes) as i64);

    // Deleting stops counting now, not when the sweeper reaches the
    // prefix.
    let (st, out) = server.delete("/v1/orgs/acme/repos/vault", &admin);
    assert_eq!(st, 204, "{out}");
    assert_eq!(row(&mut db, &vault_id), None);
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
    let server = stack(
        &bucket.base_url,
        "storage-sweep-fails",
        &[("STRATUM_STORAGE_SWEEP_SECS", "1")],
    );
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "vault" })),
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
