//! The check-run reads, end to end against a real server.
//!
//! The interesting claims here are not about SQL — `stratum_control::checks`
//! tests its own filtering and paging against Postgres, and repeating
//! that over HTTP would only prove the same thing more slowly. What only
//! an end-to-end test can show is what happens on the *route*:
//!
//! **A private repository's runs are indistinguishable from a repository
//! that was never created.** Every new read endpoint is a new way to ask
//! "does this exist", and a check history is a particularly good oracle:
//! it names branches, workflows and people. The test asserts the two
//! answers are equal *to each other* rather than against a literal
//! status code, so a deliberate change to the masking answer stays green
//! while an accidental divergence — the actual bug — goes red.
//!
//! **A filter nobody could have meant is refused by name.** The
//! alternative is a page of runs that do not match what was asked for,
//! which reads as the filter being broken to somebody with no way to
//! find out otherwise. Each of those refusals is followed by `/healthz`,
//! because a server that wedges and refuses everything would satisfy
//! every negative assertion in this file.
//!
//! **Empty is a fact.** A repository nobody has reported a run for is a
//! real repository with an empty tab, not a 404.
//!
//! Runs are seeded through `stratum_control::checks::upsert` against the
//! same database the spawned server is using — the pattern `chaos_e2e`
//! already uses for a second session on `server.db_url`. There is
//! deliberately no HTTP route in this file that *creates* a run: the
//! intake is somebody else's contract with its own signature check, and
//! inventing a write endpoint to make a read test easier would be
//! inventing a way to fabricate verdicts.

use std::time::Duration;
use stratum_control::checks::{self, NewCheckRun, RunState};
use stratum_control::registry;
use stratum_control::ControlDb;
use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::{Minio, Server};

fn spawn(store_url: &str, scratch: &Scratch, hint: &str) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .start()
}

fn make_repo(b: &mut Browser, org: &str, name: &str) {
    let (st, body) = b.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        Some(serde_json::json!({ "name": name })),
    );
    assert_eq!(st, 201, "create {org}/{name}: {body}");
}

/// A personal `repo:read` token minted from `b`'s session in `org` —
/// the credential a reader of the repository scripts with. Every
/// repository is private to its organisation, so a read test needs a
/// reader, and one holding no more than read is the honest one.
fn reader_token(b: &mut Browser, org: &str) -> String {
    let (st, body) = b.req(
        "POST",
        &format!("/v1/orgs/{org}/tokens"),
        Some(serde_json::json!({ "scopes": ["repo:read"], "label": "reader" })),
    );
    assert_eq!(st, 201, "mint a reader token in {org}: {body}");
    body["token"].as_str().expect("a token").to_string()
}

/// A second session on the server's own control database — the seam
/// `chaos_e2e` already opens for the same reason. There is no HTTP route
/// on this server that writes a check run without a provider signature,
/// and the alternative to this is a read test that can only ever read
/// nothing.
fn ctl(server: &Server) -> ControlDb {
    ControlDb::open(&server.db_url).expect("a second session on the server's database")
}

fn repo_id(db: &ControlDb, org: &str, repo: &str) -> String {
    let o = registry::org_by_name(db, org)
        .unwrap()
        .unwrap_or_else(|| panic!("no org {org}"));
    registry::repo_by_name(db, &o.id, repo)
        .unwrap()
        .unwrap_or_else(|| panic!("no repo {org}/{repo}"))
        .id
}

/// One run to seed, named the way the filters name things.
struct Seed<'a> {
    /// The provider's own id. Always given, and always distinct, so that
    /// every seed is unambiguously its own row: the id-less upsert path
    /// keys on `(repo, provider, commit, name)` and would fold two seeds
    /// sharing a workflow name onto one row, which would quietly delete
    /// half of a paging fixture.
    external_id: &'a str,
    workflow: &'a str,
    branch: &'a str,
    event: &'a str,
    actor: &'a str,
    state: RunState,
}

/// Seed a run and hand back its id.
///
/// The sleep is not padding. `created_at` is a millisecond and it is the
/// paging key, so a fixture written in a tight loop lands several runs in
/// one millisecond — which `checks::list` handles correctly by returning
/// the whole group, and which would therefore make a cursor test assert
/// nothing at all. Two milliseconds apart puts every seed in its own
/// group, so the page boundaries in the paging test are the ones a real
/// history would have.
fn seed(db: &ControlDb, repo: &str, s: Seed) -> String {
    // One commit per run, derived from the run's own id. The history
    // tests do not care which commit a run belongs to, and giving each
    // its own keeps them from accidentally depending on the grouping
    // that only the commit view cares about.
    let commit = format!("{:0>40}", s.external_id);
    seed_on(db, repo, &commit, s)
}

/// As [`seed`], but pinning the commit.
///
/// The commit view groups several runs under one sha, which is exactly
/// what `seed`'s commit-per-run convention cannot express — and the
/// fixture that matters most for that route is two runs of *one*
/// workflow on *one* commit.
fn seed_on(db: &ControlDb, repo: &str, commit: &str, s: Seed) -> String {
    std::thread::sleep(Duration::from_millis(2));
    checks::upsert(
        db,
        repo,
        &NewCheckRun {
            commit_sha: commit,
            ref_name: Some(s.branch),
            provider: "github",
            external_id: Some(s.external_id),
            name: s.workflow,
            run_number: None,
            event: Some(s.event),
            state: s.state,
            detail_url: Some("https://example.invalid/run"),
            actor: Some(s.actor),
            started_at: None,
            completed_at: None,
        },
    )
    .unwrap_or_else(|e| panic!("seed {}: {e}", s.external_id))
    .id
}

/// The ids on a page, in the order they were served.
fn ids(body: &serde_json::Value) -> Vec<String> {
    body["runs"]
        .as_array()
        .unwrap_or_else(|| panic!("no runs array in {body}"))
        .iter()
        .map(|r| r["id"].as_str().expect("run id").to_string())
        .collect()
}

/// A member of the organisation — a viewer, the weakest role there is —
/// reads a repository's history newest first, with the rail alongside
/// it; a repository nobody has reported a run for is an empty page
/// rather than a 404. Somebody outside the organisation cannot learn
/// that the repository or any of its runs exist: signed in, they get
/// exactly what a missing repository gets, and anonymous gets the same
/// 401 for both.
///
/// This used to be "a stranger reads a public repository's runs". There
/// are no public repositories now, so the reader is a viewer who is not
/// the owner, and the stranger gained the private-only negative.
#[test]
fn a_member_reads_the_runs_newest_first_and_a_stranger_learns_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("checks-e2e-read");
    let scratch = Scratch::new("checks-read");
    let server = spawn(&bucket.base_url, &scratch, "checks_read");

    let mut ada = Browser::stranger(&server, "ada", "ada@example.com");
    let mut bob = Browser::stranger(&server, "bob", "bob@example.com");
    let mut cam = Browser::stranger(&server, "cam", "cam@example.com");
    // A personal namespace has no members, so a repository somebody
    // else can read lives in an organisation.
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs",
        Some(serde_json::json!({ "name": "acme" })),
    );
    assert_eq!(st, 201, "{body}");
    make_repo(&mut ada, "acme", "widget");
    make_repo(&mut ada, "acme", "quiet");
    ada.invite_and_accept("acme", "bob@example.com", "viewer");

    let db = ctl(&server);
    let repo = repo_id(&db, "acme", "widget");

    // A repository that exists and has never been reported on. Empty is
    // a fact about it; a 404 would be the false claim that it is not
    // there, and a client would draw that as a broken page.
    let (st, body) = bob.req("GET", "/v1/orgs/acme/repos/quiet/checks/runs", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["runs"], serde_json::json!([]), "{body}");
    assert_eq!(body["workflows"], serde_json::json!([]), "{body}");
    assert!(body["next_before"].is_null(), "{body}");

    let first = seed(
        &db,
        &repo,
        Seed {
            external_id: "1",
            workflow: "ci",
            branch: "main",
            event: "push",
            actor: "ada",
            state: RunState::Passing,
        },
    );
    let second = seed(
        &db,
        &repo,
        Seed {
            external_id: "2",
            workflow: "deploy",
            branch: "main",
            event: "push",
            actor: "ada",
            state: RunState::Failing,
        },
    );

    let (st, body) = bob.req("GET", "/v1/orgs/acme/repos/widget/checks/runs", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        ids(&body),
        vec![second.clone(), first.clone()],
        "runs are newest first: {body}"
    );
    // The row carries what a table draws, not just an id — a page that
    // had to fetch each run to render a line would make one request per
    // row.
    assert_eq!(body["runs"][0]["name"], "deploy", "{body}");
    assert_eq!(body["runs"][0]["state"], "failing", "{body}");
    assert_eq!(body["runs"][0]["ref_name"], "main", "{body}");
    assert_eq!(body["runs"][0]["actor"], "ada", "{body}");

    // The rail rides along with the page, alphabetically, so the client
    // draws its filter list without a second round trip.
    assert_eq!(
        body["workflows"],
        serde_json::json!(["ci", "deploy"]),
        "{body}"
    );
    // Two runs is not a full page, so there is nothing after this one.
    assert!(
        body["next_before"].is_null(),
        "a short page handed back a cursor: {body}"
    );

    // And one of them by id.
    let one = format!("/v1/orgs/acme/repos/widget/checks/runs/{first}");
    let (st, body) = bob.req("GET", &one, None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["id"], first, "{body}");
    assert_eq!(body["name"], "ci", "{body}");
    assert_eq!(body["state"], "passing", "{body}");

    // Cam is signed in and belongs to nothing here; his session and a
    // token minted in his own namespace are each answered exactly as a
    // repository that does not exist, on the list and on the run.
    // Anonymous is one 401 for all of it. Nothing about a run reaches
    // the wire under any name.
    let cam_token = reader_token(&mut cam, "cam");
    for (real, absent) in [
        (
            "/v1/orgs/acme/repos/widget/checks/runs".to_string(),
            "/v1/orgs/acme/repos/no-such-repo/checks/runs".to_string(),
        ),
        (
            one.clone(),
            format!("/v1/orgs/acme/repos/no-such-repo/checks/runs/{first}"),
        ),
        (
            "/v1/orgs/acme/repos/quiet/checks/runs".to_string(),
            "/v1/orgs/acme/repos/no-such-repo/checks/runs".to_string(),
        ),
    ] {
        let answers = [
            (
                "anonymous",
                server.req("GET", &real, "", None),
                server.req("GET", &absent, "", None),
                401,
            ),
            (
                "cam's token",
                server.req("GET", &real, &cam_token, None),
                server.req("GET", &absent, &cam_token, None),
                404,
            ),
            (
                "cam's session",
                cam.req("GET", &real, None),
                cam.req("GET", &absent, None),
                404,
            ),
        ];
        for (who, got, missing, expect) in answers {
            assert_eq!(got.0, expect, "{who} reading {real}: {}", got.1);
            assert_eq!(
                got, missing,
                "{who} can tell {real} from a missing repository"
            );
            let text = got.1.to_string();
            for leak in ["deploy", "\"ci\"", first.as_str(), second.as_str()] {
                assert!(!text.contains(leak), "{who} was told {leak:?}: {text}");
            }
        }
    }

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A stranger cannot tell a private repository from one that was never
/// created, and a member reads their own.
#[test]
fn a_private_repositorys_runs_answer_a_stranger_as_a_missing_one_does() {
    let minio = Minio::shared();
    let bucket = minio.bucket("checks-e2e-mask");
    let scratch = Scratch::new("checks-mask");
    let server = spawn(&bucket.base_url, &scratch, "checks_mask");

    let mut ada = Browser::stranger(&server, "ada", "ada@example.com");
    make_repo(&mut ada, "ada", "ledger");

    let db = ctl(&server);
    let repo = repo_id(&db, "ada", "ledger");
    let run = seed(
        &db,
        &repo,
        Seed {
            external_id: "1",
            workflow: "secret-build",
            branch: "release",
            event: "push",
            actor: "ada",
            state: RunState::Passing,
        },
    );

    // The property is not a particular status code — it is that the two
    // answers are the same one. Pinning the comparison rather than the
    // number keeps a deliberate change to the masking answer green while
    // an accidental divergence goes red.
    // Two strangers: anonymous, and Bob — signed in, with a token of his
    // own, and no role in Ada's namespace. Anonymous is refused before
    // anything is looked up, so it is Bob who actually tests the mask.
    let mut bob = Browser::stranger(&server, "bob", "bob@example.com");
    let bob_token = reader_token(&mut bob, "bob");
    for (path, missing) in [
        (
            "/v1/orgs/ada/repos/ledger/checks/runs".to_string(),
            "/v1/orgs/ada/repos/no-such-repo/checks/runs".to_string(),
        ),
        (
            format!("/v1/orgs/ada/repos/ledger/checks/runs/{run}"),
            format!("/v1/orgs/ada/repos/no-such-repo/checks/runs/{run}"),
        ),
    ] {
        for (who, token, expect) in [("anonymous", "", 401), ("bob", bob_token.as_str(), 404)] {
            let private = server.req("GET", &path, token, None);
            let absent = server.req("GET", &missing, token, None);
            assert_eq!(
                private, absent,
                "{who} can tell a private repository from a missing one at {path}"
            );
            // The other half: two matching 200s would satisfy the
            // comparison above and leak the entire history.
            assert_eq!(
                private.0, expect,
                "a private repository answered {who} {}: {}",
                private.0, private.1
            );
            // Nothing about the run reached the wire under any name. A
            // leak is the field nobody thought to parse, so this asserts
            // on the raw text.
            let text = private.1.to_string();
            for leak in ["secret-build", "release", run.as_str()] {
                assert!(
                    !text.contains(leak),
                    "{who} was told {leak:?}: {}",
                    private.1
                );
            }
        }
        let private = bob.req("GET", &path, None);
        let absent = bob.req("GET", &missing, None);
        assert_eq!(private.0, 404, "bob's session read {path}: {}", private.1);
        assert_eq!(private, absent, "bob's session can tell {path} apart");
    }

    // The member who owns it reads it perfectly well — otherwise the
    // masking above would be indistinguishable from the endpoint simply
    // not working.
    let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/ledger/checks/runs", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(ids(&body), vec![run.clone()], "{body}");
    assert_eq!(
        body["workflows"],
        serde_json::json!(["secret-build"]),
        "{body}"
    );
    let (st, body) = ada.req(
        "GET",
        &format!("/v1/orgs/ada/repos/ledger/checks/runs/{run}"),
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["id"], run, "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Every filter narrows, and the cursor walks the history without
/// skipping a run or serving one twice.
#[test]
fn every_filter_narrows_and_the_cursor_pages_without_skipping_or_repeating() {
    let minio = Minio::shared();
    let bucket = minio.bucket("checks-e2e-filter");
    let scratch = Scratch::new("checks-filter");
    let server = spawn(&bucket.base_url, &scratch, "checks_filter");

    let mut ada = Browser::stranger(&server, "ada", "ada@example.com");
    make_repo(&mut ada, "ada", "widget");
    let reader = reader_token(&mut ada, "ada");

    let db = ctl(&server);
    let repo = repo_id(&db, "ada", "widget");

    let fixture = [
        Seed {
            external_id: "1",
            workflow: "ci",
            branch: "main",
            event: "push",
            actor: "ada",
            state: RunState::Passing,
        },
        Seed {
            external_id: "2",
            workflow: "ci",
            branch: "topic",
            event: "pull_request",
            actor: "bob",
            state: RunState::Failing,
        },
        Seed {
            external_id: "3",
            workflow: "deploy",
            branch: "main",
            event: "push",
            actor: "bob",
            state: RunState::Queued,
        },
        Seed {
            external_id: "4",
            workflow: "lint",
            branch: "main",
            event: "schedule",
            actor: "ada",
            state: RunState::Skipped,
        },
        Seed {
            external_id: "5",
            workflow: "ci",
            branch: "main",
            event: "push",
            actor: "ada",
            state: RunState::Cancelled,
        },
    ];
    let seeded: Vec<String> = fixture.into_iter().map(|s| seed(&db, &repo, s)).collect();
    // Newest first is the order the route serves, so the expectation is
    // the seeding order reversed.
    let newest_first: Vec<String> = seeded.iter().rev().cloned().collect();

    let page = |q: &str| {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/ada/repos/widget/checks/runs{q}"),
            &reader,
            None,
        );
        assert_eq!(st, 200, "GET {q}: {body}");
        body
    };

    assert_eq!(ids(&page("")), newest_first, "the unfiltered history");

    // Each filter, one at a time, against the run it should leave.
    let one = |i: usize| vec![seeded[i].clone()];
    assert_eq!(
        ids(&page("?branch=topic")),
        one(1),
        "branch matches ref_name"
    );
    assert_eq!(ids(&page("?state=queued")), one(2), "state");
    assert_eq!(ids(&page("?event=schedule")), one(3), "event");
    assert_eq!(ids(&page("?workflow=deploy")), one(2), "workflow");
    assert_eq!(
        ids(&page("?actor=bob")),
        vec![seeded[2].clone(), seeded[1].clone()],
        "actor"
    );
    // Filters compose rather than replacing each other.
    assert_eq!(
        ids(&page("?workflow=ci&branch=main")),
        vec![seeded[4].clone(), seeded[0].clone()],
        "two filters both applied"
    );
    // A filter that matches nothing is an empty page, not an error — the
    // reader picked a combination nobody has run yet, which is a normal
    // thing to do and a fact worth showing them.
    let body = page("?actor=nobody");
    assert_eq!(body["runs"], serde_json::json!([]), "{body}");
    // The rail is not narrowed by the filter. A left rail that only
    // lists the workflow already picked is one you cannot use to pick
    // another.
    assert_eq!(
        body["workflows"],
        serde_json::json!(["ci", "deploy", "lint"]),
        "{body}"
    );

    // A `limit` above the ceiling is answered with the biggest page we
    // have rather than refused — `checks::MAX_LIMIT` argues that, and
    // this pins that the route does not add a refusal of its own.
    assert_eq!(
        ids(&page("?limit=100000")),
        newest_first,
        "an over-large limit was refused instead of clamped"
    );

    // Walk the whole history two at a time and prove the pages join
    // exactly: every run once, in the same order the unpaged read gave.
    let mut walked: Vec<String> = Vec::new();
    let mut cursor: Option<i64> = None;
    for _ in 0..10 {
        let q = match cursor {
            None => "?limit=2".to_string(),
            Some(c) => format!("?limit=2&before={c}"),
        };
        let body = page(&q);
        walked.extend(ids(&body));
        match body["next_before"].as_i64() {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(
        walked, newest_first,
        "paging skipped, repeated or reordered a run"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A value nobody could have meant is refused with a sentence that names
/// it, and the server is still serving afterwards.
#[test]
fn a_filter_value_nobody_meant_is_refused_by_name() {
    let minio = Minio::shared();
    let bucket = minio.bucket("checks-e2e-bad");
    let scratch = Scratch::new("checks-bad");
    let server = spawn(&bucket.base_url, &scratch, "checks_bad");

    let mut ada = Browser::stranger(&server, "ada", "ada@example.com");
    make_repo(&mut ada, "ada", "widget");
    let reader = reader_token(&mut ada, "ada");

    // `success` is the specific mistake worth pinning: it is what a
    // provider's own vocabulary calls `passing`, so it is the wrong
    // value somebody is most likely to send in good faith. Answering a
    // page of unfiltered runs would look exactly like the filter being
    // broken.
    for (q, must_name) in [
        ("?state=success", "success"),
        ("?limit=lots", "lots"),
        ("?before=yesterday", "yesterday"),
    ] {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/ada/repos/widget/checks/runs{q}"),
            &reader,
            None,
        );
        assert_eq!(st, 400, "GET {q}: {body}");
        let msg = body["error"].as_str().unwrap_or_else(|| panic!("{body}"));
        assert!(
            msg.contains(must_name),
            "the refusal for {q} does not name what arrived: {msg}"
        );
        // A server that wedged and refused everything would satisfy
        // every assertion above.
        assert_eq!(
            server.req("GET", "/healthz", "", None).0,
            200,
            "the server stopped serving after {q}"
        );
    }

    // The named-but-valid states are all accepted, so the refusal above
    // is about the value and not about the parameter existing.
    for good in [
        "queued",
        "running",
        "passing",
        "failing",
        "cancelled",
        "skipped",
    ] {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/ada/repos/widget/checks/runs?state={good}"),
            &reader,
            None,
        );
        assert_eq!(st, 200, "state={good}: {body}");
    }

    // A cleared filter is not a filter for the empty string. This is
    // what a `<select>` set back to "All" submits, and matching it
    // literally would empty the page the moment somebody stopped
    // filtering.
    let (st, body) = server.req(
        "GET",
        "/v1/orgs/ada/repos/widget/checks/runs?branch=&state=&workflow=&limit=&before=",
        &reader,
        None,
    );
    assert_eq!(st, 200, "a cleared filter was read as a value: {body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A run belonging to another repository is not readable through this
/// one's path, and is refused in a way that does not admit it exists.
#[test]
fn a_run_in_another_repository_is_not_readable_through_this_ones_path() {
    let minio = Minio::shared();
    let bucket = minio.bucket("checks-e2e-cross");
    let scratch = Scratch::new("checks-cross");
    let server = spawn(&bucket.base_url, &scratch, "checks_cross");

    let mut ada = Browser::stranger(&server, "ada", "ada@example.com");
    make_repo(&mut ada, "ada", "widget");
    make_repo(&mut ada, "ada", "other");
    let reader = reader_token(&mut ada, "ada");

    let db = ctl(&server);
    let other = repo_id(&db, "ada", "other");
    let elsewhere = seed(
        &db,
        &other,
        Seed {
            external_id: "1",
            workflow: "ci",
            branch: "main",
            event: "push",
            actor: "ada",
            state: RunState::Passing,
        },
    );

    // Readable where it lives...
    let (st, body) = server.req(
        "GET",
        &format!("/v1/orgs/ada/repos/other/checks/runs/{elsewhere}"),
        &reader,
        None,
    );
    assert_eq!(st, 200, "{body}");

    // ...and through the neighbouring repository it is answered exactly
    // as an id that was never issued. The reader may read both
    // repositories deliberately: with nothing else masking the request,
    // this asserts the *scoping of the row lookup* rather than the
    // visibility check that would have hidden it anyway.
    let cross = server.req(
        "GET",
        &format!("/v1/orgs/ada/repos/widget/checks/runs/{elsewhere}"),
        &reader,
        None,
    );
    let never = server.req(
        "GET",
        "/v1/orgs/ada/repos/widget/checks/runs/01ZZZZZZZZZZZZZZZZZZZZZZZZ",
        &reader,
        None,
    );
    assert_eq!(
        cross, never,
        "a run in another repository is distinguishable from one that does not exist"
    );
    assert_eq!(cross.0, 404, "{}", cross.1);

    // ...and it is not in the neighbour's list either, which is the
    // other place a repo_id scope can be forgotten.
    let (st, body) = server.req(
        "GET",
        "/v1/orgs/ada/repos/widget/checks/runs",
        &reader,
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["runs"], serde_json::json!([]), "{body}");
    assert_eq!(body["workflows"], serde_json::json!([]), "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The commit view shows the newest verdict per workflow, not every run.
///
/// This is the assertion the route exists for, and it is written so that
/// it cannot pass against a plain list: the fixture puts **two runs of
/// one workflow on one commit** — an older `failing` and a newer
/// `passing` — alongside a second workflow. Three rows are stored and
/// two must come back. A fixture with one run per name would be
/// satisfied by any handler that returned everything, which is precisely
/// the bug this endpoint exists to prevent: "build: failing" from twenty
/// minutes ago rendered above "build: passing" from two.
#[test]
fn the_commit_view_shows_the_newest_verdict_per_workflow_not_every_run() {
    let minio = Minio::shared();
    let bucket = minio.bucket("checks-e2e-commit");
    let scratch = Scratch::new("checks-commit");
    let server = spawn(&bucket.base_url, &scratch, "checks_commit");

    let mut ada = Browser::stranger(&server, "ada", "ada@example.com");
    make_repo(&mut ada, "ada", "widget");
    make_repo(&mut ada, "ada", "other");
    let reader = reader_token(&mut ada, "ada");

    let db = ctl(&server);
    let repo = repo_id(&db, "ada", "widget");
    let sha = "c0ffee".repeat(6) + "abcd";

    // The provider polled: one build reported twice, and the second
    // report is the one a reader must see.
    let stale = seed_on(
        &db,
        &repo,
        &sha,
        Seed {
            external_id: "build-old",
            workflow: "build",
            branch: "main",
            event: "push",
            actor: "ada",
            state: RunState::Failing,
        },
    );
    let current = seed_on(
        &db,
        &repo,
        &sha,
        Seed {
            external_id: "build-new",
            workflow: "build",
            branch: "main",
            event: "push",
            actor: "ada",
            state: RunState::Passing,
        },
    );
    let lint = seed_on(
        &db,
        &repo,
        &sha,
        Seed {
            external_id: "lint-1",
            workflow: "lint",
            branch: "main",
            event: "push",
            actor: "ada",
            state: RunState::Failing,
        },
    );

    let (st, body) = server.req(
        "GET",
        &format!("/v1/orgs/ada/repos/widget/commits/{sha}/checks"),
        &reader,
        None,
    );
    assert_eq!(st, 200, "{body}");
    let got = ids(&body);
    assert_eq!(
        got.len(),
        2,
        "three runs are stored on this commit and two workflows ran; \
         the commit view must collapse to one row per workflow: {body}"
    );
    assert!(
        got.contains(&current) && got.contains(&lint),
        "the newest run of each workflow is missing: {body}"
    );
    assert!(
        !got.contains(&stale),
        "the superseded 'failing' run is still being shown beside the \
         commit, above the 'passing' that replaced it: {body}"
    );
    // Named by workflow, so a reader sees one unambiguous line each.
    let verdict = |name: &str| {
        body["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no {name} row in {body}"))["state"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(verdict("build"), "passing", "{body}");
    assert_eq!(verdict("lint"), "failing", "{body}");

    // All three rows really are stored — otherwise the collapse above
    // would be indistinguishable from the seeding having silently
    // folded two reports into one row, and this test would be proving
    // nothing about the handler at all.
    let (st, all) = server.req(
        "GET",
        "/v1/orgs/ada/repos/widget/checks/runs",
        &reader,
        None,
    );
    assert_eq!(st, 200, "{all}");
    assert_eq!(
        ids(&all).len(),
        3,
        "the history should hold all three: {all}"
    );

    // A commit nobody has built is an empty list and a 200. The 404 on
    // this route belongs to the repository; spending it here would make
    // "no such project" and "nothing has built this yet" the same answer.
    let (st, body) = server.req(
        "GET",
        &format!(
            "/v1/orgs/ada/repos/widget/commits/{}/checks",
            "d".repeat(40)
        ),
        &reader,
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["runs"], serde_json::json!([]), "{body}");

    // A sha that could never have been stored is answered, not 500'd.
    // `%00` is the one that matters: a control byte reaching the query
    // is a database error, and the control plane's guard exists to turn
    // it into "nothing matched" first.
    for bad in ["not-a-sha", "%00", "%01", &"z".repeat(300)] {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/ada/repos/widget/commits/{bad}/checks"),
            &reader,
            None,
        );
        assert!(
            st < 500,
            "a malformed sha {bad:?} was answered {st}, which is ours to \
             fix rather than the caller's: {body}"
        );
        assert_eq!(
            server.req("GET", "/healthz", "", None).0,
            200,
            "the server stopped serving after a malformed sha {bad:?}"
        );
    }

    // The same sha in a neighbouring repository shows nothing. The
    // reader may read both repositories on purpose, so this pins the
    // repo_id scoping of the lookup rather than a visibility check that
    // would have hidden the rows anyway.
    let (st, body) = server.req(
        "GET",
        &format!("/v1/orgs/ada/repos/other/commits/{sha}/checks"),
        &reader,
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        body["runs"],
        serde_json::json!([]),
        "another repository's runs are readable through this commit path: {body}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A private repository's commit checks answer a stranger exactly as a
/// repository that was never created.
#[test]
fn a_private_repositorys_commit_checks_answer_a_stranger_as_a_missing_one_does() {
    let minio = Minio::shared();
    let bucket = minio.bucket("checks-e2e-commit-mask");
    let scratch = Scratch::new("checks-commit-mask");
    let server = spawn(&bucket.base_url, &scratch, "checks_commit_mask");

    let mut ada = Browser::stranger(&server, "ada", "ada@example.com");
    make_repo(&mut ada, "ada", "ledger");

    let db = ctl(&server);
    let repo = repo_id(&db, "ada", "ledger");
    let sha = "a".repeat(40);
    let run = seed_on(
        &db,
        &repo,
        &sha,
        Seed {
            external_id: "1",
            workflow: "secret-build",
            branch: "release",
            event: "push",
            actor: "ada",
            state: RunState::Passing,
        },
    );

    // Anonymous, and Bob — signed in, with a token of his own, and no
    // role in Ada's namespace.
    let mut bob = Browser::stranger(&server, "bob", "bob@example.com");
    let bob_token = reader_token(&mut bob, "bob");
    let path = format!("/v1/orgs/ada/repos/ledger/commits/{sha}/checks");
    let missing = format!("/v1/orgs/ada/repos/no-such-repo/commits/{sha}/checks");
    for (who, token, expect) in [("anonymous", "", 401), ("bob", bob_token.as_str(), 404)] {
        let private = server.req("GET", &path, token, None);
        let absent = server.req("GET", &missing, token, None);
        assert_eq!(
            private, absent,
            "{who} can tell a private repository from a missing one"
        );
        assert_eq!(
            private.0, expect,
            "a private repository answered {who} {}: {}",
            private.0, private.1
        );
        // A leak is the field nobody thought to parse, so this reads the
        // raw text rather than named fields.
        let text = private.1.to_string();
        for leak in ["secret-build", "release", run.as_str()] {
            assert!(
                !text.contains(leak),
                "{who} was told {leak:?}: {}",
                private.1
            );
        }
    }
    let private = bob.req("GET", &path, None);
    assert_eq!(
        private.0, 404,
        "bob's session read the checks: {}",
        private.1
    );
    assert_eq!(private, bob.req("GET", &missing, None));

    // The owner reads it perfectly well. This assertion is what stops
    // the comparison above from being satisfiable by two 404s from a
    // route that does not exist at all — the failure mode that made an
    // earlier version of this suite pass against an unregistered
    // handler.
    let (st, body) = ada.req(
        "GET",
        &format!("/v1/orgs/ada/repos/ledger/commits/{sha}/checks"),
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(ids(&body), vec![run], "{body}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
