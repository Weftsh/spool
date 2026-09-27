//! Importing a project's issues from GitHub, against the fake.
//!
//! The claim under test is the migration promise in one sentence: a
//! project's issues arrive here **with the numbers they came with**, so
//! that `#4721` in a commit message, a changelog or somebody else's
//! documentation still means what it says.
//!
//! Everything else in this file is a refusal, because the refusals are
//! where an import goes quietly wrong. An import that produces nothing
//! looks exactly like a project that never had issues; an import that
//! renumbers looks like a success until somebody follows a link.

use std::path::Path;
use std::time::{Duration, Instant};
use stratum_testkit::fake_github;
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::{wait_for, wait_until, Minio, Server};

/// A server importing from the fake, with the per-run page budget under
/// the test's control.
///
/// `None` leaves `STRATUM_IMPORT_PAGES_PER_RUN` unset, so a test that
/// does not care inherits **the product's own default** rather than a
/// copy of it — a restated "20" here would go on passing on the day the
/// worker's default changed, which is the same class of quiet
/// disagreement the local CI script exists to prevent.
///
/// That default is larger than every fixture in this file, which is why
/// the resume path went unwalked for so long — see
/// `a_large_tracker_resumes_after_spending_its_page_budget`, which is
/// the reason this parameter exists at all.
fn spawn_server(
    store_url: &str,
    scratch: &Scratch,
    api_base: &str,
    key: &Path,
    pages_per_run: Option<&str>,
) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url).db_hint("import");
    if let Some(pages) = pages_per_run {
        b = b.env("STRATUM_IMPORT_PAGES_PER_RUN", pages);
    }
    b.data_dir(scratch.path().join("data"))
        .env("STRATUM_GITHUB_APP_ID", "12345")
        .env("STRATUM_GITHUB_APP_KEY_PEM", key.display().to_string())
        .env("STRATUM_GITHUB_API_BASE", api_base)
        .env(
            "STRATUM_GITHUB_INSTALL_URL",
            "https://github.com/apps/stratum/installations/new",
        )
        .env("STRATUM_MIRROR_POLL_SECS", "0")
        // A tenth of a second, not a whole one. The importer sleeps a
        // poll only when the queue is empty, so this is the latency
        // between enqueueing an import and the run starting — and a
        // resume pays it again for every page budget it spends. The
        // knob takes a decimal now; `0` still means "off".
        .env("STRATUM_IMPORT_POLL_SECS", "0.1")
        .start()
}

/// Wait for a job row to finish, and hand it back.
///
/// Four tests here drive the worker by creating the job row directly and
/// then reading its outcome, because the outcome recorded on a row is a
/// fact about the run that wrote it and cannot be overtaken by the next
/// one. What they were all waiting on is this: the row leaving
/// `queued`/`running`. The wait ends when it does.
fn await_job(
    db: &stratum_control::ControlDb,
    org_id: &str,
    id: &str,
) -> stratum_control::jobs::Job {
    wait_for("the job's run to finish", Duration::from_secs(60), || {
        let j = stratum_control::jobs::get(db, org_id, id)
            .expect("read job")
            .expect("the job exists");
        (j.state == "done" || j.state == "failed").then_some(j)
    })
}

/// A mirror row pointing at `full_name`, which is what an import reads.
///
/// Created directly rather than through a sync, because this file is
/// about issues and a git fetch would only add a way to fail that has
/// nothing to do with them.
fn mirror(server: &Server, admin: &str, name: &str, full_name: &str) {
    let (st, out) = server.post(
        "/v1/orgs/acme/mirrors",
        admin,
        Some(serde_json::json!({
            "name": name,
            "provider": "github",
            "origin": full_name,
            "installation_id": "777",
        })),
    );
    assert!(st == 202 || st == 201, "create mirror: {st} {out}");
}

/// Wait for the import to report `issues: "done"`, or give up.
fn await_import(server: &Server, admin: &str, name: &str) -> serde_json::Value {
    let path = format!("/v1/orgs/acme/repos/{name}/import");
    wait_for("the import to finish", Duration::from_secs(60), || {
        let (st, out) = server.get(&path, admin);
        assert_eq!(st, 200, "{out}");
        (out["issues"] == serde_json::json!("done")).then_some(out)
    })
}

fn setup(hint: &'static str) -> (Scratch, fake_github::FakeGithub, Server, String) {
    setup_with(hint, None)
}

fn setup_with(
    hint: &'static str,
    pages_per_run: Option<&str>,
) -> (Scratch, fake_github::FakeGithub, Server, String) {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let gh = fake_github::spawn();
    let key = scratch.path().join("app-key.pem");
    std::fs::write(&key, fake_github::TEST_APP_KEY_PEM).unwrap();
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &gh.base_url,
        &key,
        pages_per_run,
    );
    let admin = server.bootstrap_org("acme");
    // Every mirror in here goes through installation 777, and creation
    // refuses an installation the org has not connected.
    server.connect_installation("acme", &admin, "777");
    (scratch, gh, server, admin)
}

#[test]
fn an_import_brings_the_issues_over_with_the_numbers_they_came_with() {
    let (_scratch, _gh, server, admin) = setup("import-numbers");
    mirror(&server, &admin, "widget", "acme/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/widget/import", &admin, None);
    assert_eq!(st, 202, "{out}");
    await_import(&server, &admin, "widget");

    let (st, listed) = server.get(
        "/v1/orgs/acme/repos/widget/issues?state=all&limit=100",
        &admin,
    );
    assert_eq!(st, 200, "{listed}");
    let numbers: Vec<i64> = listed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .map(|i| i["number"].as_i64().expect("number"))
        .collect();
    // The fake's `acme/*` fixture is seven issues numbered 1..=7. What
    // matters is that they are *those* numbers and not 1..=n allocated
    // fresh — the fixture's numbers happen to start at 1, so the closed
    // ones and the gaps are what prove nothing was renumbered.
    assert_eq!(numbers.len(), 7, "{listed}");
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, (1..=7).collect::<Vec<i64>>(), "{listed}");

    // Closed issues came too. GitHub's default is `state=open`, and an
    // import that takes it loses most of a project's memory.
    assert_eq!(listed["counts"]["closed"], 1, "{listed}");

    // An upstream author with no account here is named, never claimed.
    let one = listed["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["number"] == 1)
        .unwrap();
    assert_eq!(one["author"], serde_json::Value::Null, "{one}");
    assert_eq!(one["author_label"], "octocat-1 (github)", "{one}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_large_tracker_is_paged_to_the_end_without_guessing() {
    let (_scratch, _gh, server, admin) = setup("import-paged");
    // `many/*` is 250 issues: three pages at a hundred, with a short
    // last one. An importer that counted pages instead of following
    // `Link` would stop early on a full page and there would be no
    // error anywhere.
    mirror(&server, &admin, "big", "many/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/big/import", &admin, None);
    assert_eq!(st, 202, "{out}");
    await_import(&server, &admin, "big");

    let (st, listed) = server.get("/v1/orgs/acme/repos/big/issues?state=all&limit=1", &admin);
    assert_eq!(st, 200, "{listed}");
    let total = listed["counts"]["open"].as_i64().unwrap_or(0)
        + listed["counts"]["closed"].as_i64().unwrap_or(0);
    assert_eq!(
        total, 250,
        "the import stopped short of the last page: {listed}"
    );

    // The highest number is present, which is the one a page-counting
    // importer loses.
    let (st, last) = server.get("/v1/orgs/acme/repos/big/issues/250", &admin);
    assert_eq!(st, 200, "issue 250 is missing: {last}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn an_installation_without_the_permission_is_refused_and_not_reported_empty() {
    let (_scratch, _gh, server, admin) = setup("import-noperm");
    mirror(&server, &admin, "locked", "noperm/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/locked/import", &admin, None);
    assert_eq!(st, 202, "{out}");

    // The job fails, and the tracker stays empty rather than being
    // declared imported. "No issues" and "we were not allowed to look"
    // must never be the same answer.
    //
    // **And the API has to be able to tell them apart**, which for a
    // long time it could not: this route answered three phase cursors
    // and nothing else, so a refused import and an import still walking
    // its first page were byte-identical replies. The sentence naming
    // the permission to grant was written by `refusal()`, recorded on
    // the job, and read by nobody. This test was named for that claim
    // and could only assert the weaker half of it.
    let refused = wait_for(
        "the refused import to report itself failed",
        Duration::from_secs(30),
        || {
            let (_, s) = server.get("/v1/orgs/acme/repos/locked/import", &admin);
            assert_ne!(
                s["issues"],
                serde_json::json!("done"),
                "a refused import reported itself finished: {s}"
            );
            (s["state"] == serde_json::json!("failed")).then_some(s)
        },
    );
    let why = refused["error"].as_str().unwrap_or_default();
    assert!(
        why.contains("issues: read"),
        "the refusal does not name the permission to grant, which is the \
         one thing the reader can act on: {refused}"
    );
    let (st, listed) = server.get("/v1/orgs/acme/repos/locked/issues?state=all", &admin);
    assert_eq!(st, 200, "{listed}");
    assert_eq!(listed["counts"]["open"], 0, "{listed}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_tracker_that_already_has_issues_refuses_the_import_and_says_why() {
    let (_scratch, _gh, server, admin) = setup("import-occupied");
    mirror(&server, &admin, "widget", "acme/widget");

    // A native issue filed before anybody thought to import.
    //
    // Filed by a **person**, because the org token cannot: an issue has
    // an author and a service token is not one. That refusal is the
    // right one and it is tested in `issues_e2e`; here it is just the
    // reason this fixture needs an account.
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "ada@acme.test",
            "--name",
            "Ada",
            "--password",
            "a long enough password",
            "--role",
            "owner",
        ])
        .expect("create the person who files it");
    let mut ada = stratum_testkit::browser::Browser::new(&server);
    assert_eq!(ada.login("ada@acme.test", "a long enough password"), 200);
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "filed here first" })),
    );
    assert_eq!(st, 201, "{out}");

    let (st, refused) = server.post("/v1/orgs/acme/repos/widget/import", &admin, None);
    assert_eq!(st, 409, "{refused}");
    let msg = refused["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("numbers"),
        "the refusal does not say why, so it reads as arbitrary: {msg}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_repository_with_no_github_origin_cannot_be_imported_into() {
    let (_scratch, _gh, server, admin) = setup("import-noorigin");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "native", "public": true })),
    );
    assert_eq!(st, 201, "{out}");

    let (st, refused) = server.post("/v1/orgs/acme/repos/native/import", &admin, None);
    assert_eq!(st, 400, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("mirror"),
        "the refusal does not name the way through: {refused}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// An import that spends its page budget comes back and finishes.
///
/// This is the path every fixture above walks straight past. The budget
/// defaults to twenty pages and the largest tracker here is three, so
/// until this test existed the import suite was 5/5 green against a
/// resume that did not work at all: `jobs_active_per_repo` grew to cover
/// `import`, the re-enqueue from inside `run_one` began conflicting with
/// the still-running row it was called from, `ON CONFLICT DO NOTHING`
/// swallowed it, and every large import stopped dead at page twenty with
/// no error anywhere. Nothing failed. Nothing logged. The tracker was
/// simply short, and looked finished.
///
/// One page per run against `many/*` — 250 issues over three pages —
/// makes the resume load-bearing: the second and third pages arrive only
/// if the worker re-enqueues itself.
///
/// The job row is created here rather than through `POST /import` so
/// that its id is in hand, and the first run's **result** is what is
/// asserted rather than a row count. Counting rows races the run the
/// re-enqueue has already started; the outcome recorded on a row is a
/// fact about the run that wrote it and cannot be overtaken. (The route
/// itself is covered by the tests above.)
#[test]
fn a_large_tracker_resumes_after_spending_its_page_budget() {
    let (_scratch, _gh, server, admin) = setup_with("import-resume", Some("1"));
    mirror(&server, &admin, "big", "many/widget");

    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    let repo_id = stratum_control::registry::repo_by_name(&db, &org_id, "big")
        .unwrap()
        .expect("the mirror")
        .id;
    let job = stratum_control::jobs::create(&db, &org_id, Some(&repo_id), "import", None)
        .expect("enqueue an import")
        .id;

    let first = await_job(&db, &org_id, &job);
    assert_eq!(first.state, "done", "{first:?}");
    assert!(
        first
            .result
            .as_deref()
            .unwrap_or_default()
            .starts_with("More"),
        "a run that spent its budget must say the work is not finished, or \
         the import has quietly declared a short tracker complete: {first:?}"
    );

    // And it comes back, unasked, until it is actually finished.
    await_import(&server, &admin, "big");
    let (st, listed) = server.get("/v1/orgs/acme/repos/big/issues?state=all&limit=1", &admin);
    assert_eq!(st, 200, "{listed}");
    let total = listed["counts"]["open"].as_i64().unwrap_or(0)
        + listed["counts"]["closed"].as_i64().unwrap_or(0);
    assert_eq!(
        total, 250,
        "the import stopped at its first budget and never resumed: {listed}"
    );
    let (st, last) = server.get("/v1/orgs/acme/repos/big/issues/250", &admin);
    assert_eq!(st, 200, "issue 250 is missing: {last}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A rate limit on the **labels** phase, which is the first call an
/// import makes.
///
/// Rate limiting is the ordinary state of a large import, not an edge
/// case, and the importer answers it the same way in three places:
/// record what was done and come back. Only one of those three arms was
/// reachable from this suite, because the fake's every-third-call rule
/// always landed on the issues page — so the two that run *before*
/// issues had never executed. A budget does not care which endpoint
/// spends it.
///
/// The claim is that a refusal here is a **delay, not a failure**: the
/// job does not fail, no phase is marked done on a page that never
/// arrived, and the import finishes once the limit clears.
#[test]
fn a_rate_limit_on_the_first_phase_is_a_delay_and_not_a_failure() {
    let (_scratch, _gh, server, admin) = setup("import-rl-labels");
    mirror(&server, &admin, "slow", "ratelimited-labels/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/slow/import", &admin, None);
    assert_eq!(st, 202, "{out}");

    // Labels never completes, because that endpoint always refuses here.
    // What must never happen is the phase being marked done anyway, or
    // the job being failed: both turn "come back later" into a migration
    // that has quietly lost its labels.
    //
    // **Counted in runs, not in seconds.** A rate-limited run completes
    // with `More` and re-enqueues, so each pass over the arm under test
    // leaves a *new* job row — and that row is the observable saying the
    // importer came back and refused again. Ten of them is ten passes,
    // whatever the machine's speed; the ten-second window this replaces
    // was a guess at the same number that cost ten seconds even when the
    // worker had already made its point, and made none at all on a
    // machine slow enough to have run twice.
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    let repo_id = stratum_control::registry::repo_by_name(&db, &org_id, "slow")
        .unwrap()
        .expect("the mirror")
        .id;
    let mut runs = std::collections::HashSet::new();
    wait_until(
        "ten rate-limited import runs to come and go",
        Duration::from_secs(30),
        || {
            let (st, s) = server.get("/v1/orgs/acme/repos/slow/import", &admin);
            assert_eq!(st, 200, "{s}");
            assert_ne!(
                s["labels"],
                serde_json::json!("done"),
                "a phase whose page was refused was marked finished: {s}"
            );
            assert_eq!(
                s["issues"],
                serde_json::Value::Null,
                "the import ran past a phase it never completed: {s}"
            );
            assert_ne!(
                s["state"],
                serde_json::json!("failed"),
                "a rate limit failed the job instead of delaying it: {s}"
            );
            if let Some(j) =
                stratum_control::jobs::latest_for_repo(&db, &org_id, &repo_id, "import")
                    .expect("read the latest import job")
            {
                runs.insert(j.id);
            }
            runs.len() >= 10
        },
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The same, one phase later: milestones.
///
/// Kept as its own test rather than folded into the one above because
/// the two arms are separate code with separate cursors, and a single
/// test that walked both would stop at the first.
#[test]
fn a_rate_limit_on_the_milestones_phase_leaves_the_labels_it_already_took() {
    let (_scratch, _gh, server, admin) = setup("import-rl-miles");
    mirror(&server, &admin, "slow", "ratelimited-milestones/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/slow/import", &admin, None);
    assert_eq!(st, 202, "{out}");

    // Labels went through before the refusal, and the work already done
    // is kept — that is the whole reason a phase has its own cursor.
    wait_until(
        "the labels phase to survive the refusal of the one after it",
        Duration::from_secs(20),
        || {
            let (_, s) = server.get("/v1/orgs/acme/repos/slow/import", &admin);
            assert_ne!(
                s["milestones"],
                serde_json::json!("done"),
                "a refused milestones page was marked finished: {s}"
            );
            s["labels"] == serde_json::json!("done")
        },
    );

    let (st, labels) = server.get("/v1/orgs/acme/repos/slow/labels", &admin);
    assert_eq!(st, 200, "{labels}");
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A refusal that is **not** the missing-permission one.
///
/// `refusal()` translates GitHub's "Resource not accessible by
/// integration" into a sentence about `issues: read`, and everything
/// else keeps its status and body. Only the first half was tested, so
/// the fallback — the arm that carries a 500 through to an operator —
/// had never run. Telling somebody to grant a permission they already
/// hold, over an upstream outage, is the exact failure this repository
/// has already paid for once on the CI poller.
#[test]
fn an_upstream_error_is_reported_as_itself_and_not_as_a_permission_problem() {
    let (_scratch, _gh, server, admin) = setup("import-500");
    mirror(&server, &admin, "broken", "brokenupstream/widget");

    // The job is created here rather than through `POST /import` so its
    // id is in hand: the reason a failed import gives lives on the job
    // row and nowhere the API can reach, which is a real gap and is
    // reported as one. Inventing a route to assert against would be
    // testing a product that does not exist.
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    let repo_id = stratum_control::registry::repo_by_name(&db, &org_id, "broken")
        .unwrap()
        .expect("the mirror")
        .id;
    let job = stratum_control::jobs::create(&db, &org_id, Some(&repo_id), "import", None)
        .expect("enqueue an import")
        .id;

    let done = await_job(&db, &org_id, &job);
    assert_eq!(
        done.state, "failed",
        "an upstream 500 was not an error: {done:?}"
    );
    let err = done.error.unwrap_or_default();
    assert!(
        err.contains("500"),
        "an upstream error lost its status: {err}"
    );
    assert!(
        !err.contains("issues: read"),
        "an upstream outage was reported as a missing permission, \
         which sends a maintainer to re-grant something they already \
         hold: {err}"
    );
    // And no phase was marked done on a page that never arrived.
    let (_, phases) = server.get("/v1/orgs/acme/repos/broken/import", &admin);
    assert_ne!(phases["labels"], serde_json::json!("done"), "{phases}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A real export's rough edges: entries the importer is written to skip.
///
/// A label with no name, the same label name twice, a milestone with no
/// title, a *closed* milestone, and an issue with no number. Every one
/// of those is a line in the importer, and not one of them could be
/// produced by a fixture until now — so the skips were untested and the
/// closed-milestone arm meant every imported milestone was open whatever
/// the origin said.
///
/// The claim is that none of them stops the migration, and that the
/// things that *can* be imported still are.
#[test]
fn a_messy_export_is_imported_around_rather_than_refused() {
    let (_scratch, _gh, server, admin) = setup("import-messy");
    mirror(&server, &admin, "messy", "messy/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/messy/import", &admin, None);
    assert_eq!(st, 202, "{out}");
    await_import(&server, &admin, "messy");

    // The nameless label is skipped and the duplicate does not become a
    // second row — one `bug`, not two and not a failed import.
    let (st, labels) = server.get("/v1/orgs/acme/repos/messy/labels", &admin);
    assert_eq!(st, 200, "{labels}");
    let names: Vec<String> = labels["labels"]
        .as_array()
        .expect("labels")
        .iter()
        .filter_map(|l| l["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        names.iter().filter(|n| n.as_str() == "bug").count(),
        1,
        "a repeated label name was imported twice: {labels}"
    );
    assert!(
        names.iter().all(|n| !n.is_empty()),
        "a label with no name got a row: {labels}"
    );

    // The milestones went through the importer — a titleless entry
    // skipped, a closed one taking the arm that stores `closed` — and
    // that is asserted here only by the import having *finished*:
    // `await_import` above cannot return unless the milestones phase
    // completed without error.
    //
    // It cannot be asserted any harder than that, because **milestones
    // have no read path at all**. `put_milestone` writes a table no
    // control-plane function lists, no route serves and no view renders,
    // and `issues.milestone_id` is never set by the import either — so
    // every milestone arrives as an orphan row nobody can reach. The
    // import status page says "Milestones: done" and there is nowhere to
    // go and see one. Reported as a finding rather than papered over
    // with a route invented for this test, which would prove nothing
    // about the product.

    // And the numberless issue is skipped while its neighbours arrive
    // with the numbers they came with.
    let (st, listed) = server.get(
        "/v1/orgs/acme/repos/messy/issues?state=all&limit=100",
        &admin,
    );
    assert_eq!(st, 200, "{listed}");
    let numbers: Vec<i64> = listed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter_map(|i| i["number"].as_i64())
        .collect();
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        (1..=7).collect::<Vec<i64>>(),
        "the numberless entry was renumbered in, or a real issue was \
         lost with it: {listed}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Milestones arrive, are readable, and the issues that belong to them
/// point at them.
///
/// Every part of that sentence was untrue. `put_milestone` wrote a table
/// with no list function, no route and no view; `import_issues` never
/// set `issues.milestone_id`; and the import status page said
/// "Milestones: done" over rows nobody could reach. The coverage gate
/// found it the same way it found the checks merge — an arm that decides
/// a milestone's stored state could not be executed, because nothing
/// downstream of it existed.
#[test]
fn milestones_are_imported_readable_and_linked_to_their_issues() {
    let (_scratch, _gh, server, admin) = setup("import-milestones");
    mirror(&server, &admin, "widget", "acme/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/widget/import", &admin, None);
    assert_eq!(st, 202, "{out}");
    await_import(&server, &admin, "widget");

    let (st, out) = server.get("/v1/orgs/acme/repos/widget/milestones", &admin);
    assert_eq!(st, 200, "{out}");
    let miles = out["milestones"].as_array().expect("milestones");
    assert_eq!(miles.len(), 1, "{out}");
    let m = &miles[0];
    // The number it came with, for the same reason issues keep theirs.
    assert_eq!(m["number"], serde_json::json!(1), "{out}");
    assert_eq!(m["title"], serde_json::json!("v1.0"), "{out}");
    assert_eq!(m["state"], serde_json::json!("open"), "{out}");

    // The fixture puts every even-numbered issue on `v1.0` — 2, 4 and 6
    // of seven — and issue 4 is the closed one. Both counts, because a
    // single progress fraction cannot tell an empty milestone from a
    // finished one.
    assert_eq!(
        m["open_issues"],
        serde_json::json!(2),
        "the issues on this milestone are not linked to it: {out}"
    );
    assert_eq!(m["closed_issues"], serde_json::json!(1), "{out}");

    // Issue 7 names a milestone that is not in the milestones page — one
    // deleted upstream. It has to survive with no milestone rather than
    // being lost to keep a dangling pointer.
    let (st, seven) = server.get("/v1/orgs/acme/repos/widget/issues/7", &admin);
    assert_eq!(
        st, 200,
        "an issue was lost over a dangling milestone: {seven}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A conversation longer than one page.
///
/// The importer follows `Link` on comments for the same reason it does
/// on issues, and the doc comment says why: a thread of more than a
/// hundred is exactly the one a migrating project most wants to keep,
/// and an importer that reads only the first page truncates it in
/// silence. Nothing could send it a second page until the fake learned
/// to produce one, so the loop's second lap had never run.
#[test]
fn a_conversation_longer_than_a_page_is_imported_whole() {
    let (_scratch, _gh, server, admin) = setup("import-chatty");
    mirror(&server, &admin, "chatty", "chatty/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/chatty/import", &admin, None);
    assert_eq!(st, 202, "{out}");
    await_import(&server, &admin, "chatty");

    // 150 comments at 100 a page: the second page only arrives if the
    // `Link` header was followed.
    let (st, thread) = server.get(
        "/v1/orgs/acme/repos/chatty/issues/1/comments?limit=200",
        &admin,
    );
    assert_eq!(st, 200, "{thread}");
    let comments = thread["comments"].as_array().expect("comments");
    assert_eq!(
        comments.len(),
        150,
        "a conversation was truncated at the first page: {}",
        comments.len()
    );
    // The last comment of the second page is present, which is the one a
    // single-page importer loses without saying so.
    assert!(
        comments.iter().any(|c| c["body"]
            .as_str()
            .unwrap_or_default()
            .contains("comment 149")),
        "the tail of the conversation is missing"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The four refusals `run_one` opens with, none of which the route can
/// produce.
///
/// `POST /import` refuses a native repository at the door with a 400, so
/// the worker's own guards sit behind a check that has already passed —
/// which is exactly why they were never executed and why they matter. A
/// job row outlives the request that made it: the repository can be
/// deleted, the App can be removed from the deployment, and a job can be
/// enqueued by something other than that route. A guard that has never
/// run is a guess about what happens when it does.
///
/// One test, because each case is two lines of setup against the same
/// world and four near-identical tests would be four times the fixture
/// for the same claim: the worker refuses in words, and the server is
/// still serving afterwards.
#[test]
fn the_worker_refuses_a_job_it_cannot_run_and_says_which_way() {
    let (_scratch, _gh, server, admin) = setup("import-guards");
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;

    let run = |repo_id: Option<&str>| -> stratum_control::jobs::Job {
        let id = stratum_control::jobs::create(&db, &org_id, repo_id, "import", None)
            .expect("enqueue")
            .id;
        await_job(&db, &org_id, &id)
    };

    // 1. A job with no repository at all. Nothing enqueues one today;
    //    the guard is here so that something which does gets an error
    //    rather than a worker that unwraps.
    let j = run(None);
    assert_eq!(j.state, "failed", "{j:?}");
    assert!(
        j.error.unwrap_or_default().contains("without repo"),
        "the refusal does not say what was missing"
    );

    // 2. A repository deleted between enqueue and claim. `jobs.repo_id`
    //    has no foreign key, so the row outlives the repository — this
    //    is not a hypothetical.
    mirror(&server, &admin, "doomed", "acme/widget");
    let doomed = stratum_control::registry::repo_by_name(&db, &org_id, "doomed")
        .unwrap()
        .expect("the mirror")
        .id;
    let (st, out) = server.delete("/v1/orgs/acme/repos/doomed", &admin);
    assert!(st == 204 || st == 202, "{st} {out}");
    let j = run(Some(&doomed));
    assert_eq!(
        j.state, "done",
        "a vanished repository was an error rather than nothing to do: {j:?}"
    );

    // 3. A native repository. The route refuses this with a 400, so the
    //    worker's sentence had never been produced — and it is the more
    //    useful of the two, because it names the way through.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "native", "public": true })),
    );
    assert_eq!(st, 201, "{out}");
    let native = stratum_control::registry::repo_by_name(&db, &org_id, "native")
        .unwrap()
        .expect("the repo")
        .id;
    let j = run(Some(&native));
    assert_eq!(j.state, "failed", "{j:?}");
    let why = j.error.unwrap_or_default();
    assert!(
        why.contains("mirror"),
        "the refusal does not name the way through: {why}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A deployment with no GitHub App, holding a mirror.
///
/// Reachable in production the moment an App is removed from a
/// deployment that already has mirrors, and not reachable through any
/// API here — a mirror cannot be *created* without one. So the row is
/// made directly, which is the honest way to set up a state the product
/// arrives at by a route this test is not about.
#[test]
fn an_import_on_a_deployment_with_no_github_app_says_so_rather_than_hanging() {
    let minio = Minio::shared();
    let bucket = minio.bucket("import-noapp");
    let scratch = Scratch::new("import-noapp");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("import-noapp")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_MIRROR_POLL_SECS", "0")
        .env("STRATUM_IMPORT_POLL_SECS", "0.1")
        .start();
    let admin = server.bootstrap_org("acme");
    let _ = &admin;

    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    let repo = stratum_control::registry::create_repo(
        &db,
        &org_id,
        &stratum_control::registry::NewRepo {
            name: "orphaned",
            description: None,
            kind: stratum_control::registry::RepoKind::Mirror,
            public: true,
            default_branch: "main",
            origin_url: Some("acme/widget"),
            origin_provider: Some("github"),
            origin_installation: Some("777"),
        },
    )
    .expect("a mirror row from before the App was removed");

    let id = stratum_control::jobs::create(&db, &org_id, Some(&repo.id), "import", None)
        .expect("enqueue")
        .id;
    let j = await_job(&db, &org_id, &id);
    assert_eq!(j.state, "failed", "{j:?}");
    assert!(
        j.error.unwrap_or_default().contains("GitHub App"),
        "the refusal does not name what the deployment is missing"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A rate limit met **mid-walk**, on the issues page and again inside a
/// conversation.
///
/// `ratelimited/*` refuses every third call, and an import's calls run
/// labels, milestones, issues, comments — so the refusals land on the
/// issue list and then partway through the comments. Both arms answer
/// the same way and neither had ever run: record what was done, come
/// back. The first two phases had tests; the two that do the actual work
/// did not.
///
/// The claim is that a large import **finishes anyway**. A rate limit is
/// the expected state of one, not a failure, and an importer that fails
/// the job here would make every real migration look broken.
#[test]
fn a_rate_limit_partway_through_the_walk_still_finishes_the_import() {
    let (_scratch, _gh, server, admin) = setup("import-rl-walk");
    mirror(&server, &admin, "slow", "ratelimited/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/slow/import", &admin, None);
    assert_eq!(st, 202, "{out}");
    await_import(&server, &admin, "slow");

    // Seven issues, all of them, despite the refusals along the way.
    let (st, listed) = server.get(
        "/v1/orgs/acme/repos/slow/issues?state=all&limit=100",
        &admin,
    );
    assert_eq!(st, 200, "{listed}");
    let mut numbers: Vec<i64> = listed["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter_map(|i| i["number"].as_i64())
        .collect();
    numbers.sort_unstable();
    assert_eq!(
        numbers,
        (1..=7).collect::<Vec<i64>>(),
        "a rate limit lost issues instead of delaying them: {listed}"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A permission lost **after** the phases that already succeeded.
///
/// `noperm/*` refuses the first call an import makes, so the refusal
/// arms on the issue list and inside a conversation had never run —
/// they sat behind a phase that had already stopped. A permission can be
/// revoked between two calls of one run, and a provider can refuse one
/// endpoint and not another.
///
/// Both cases stop the import with the sentence naming `issues: read`,
/// and neither marks a phase done on a page that never arrived.
#[test]
fn a_refusal_after_the_first_phase_stops_the_import_with_the_reason() {
    let (_scratch, _gh, server, admin) = setup("import-noperm-late");
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;

    for (repo, origin) in [
        ("listrefused", "noperm-issues/widget"),
        ("talkrefused", "noperm-comments/widget"),
    ] {
        mirror(&server, &admin, repo, origin);
        let (st, out) = server.post(&format!("/v1/orgs/acme/repos/{repo}/import"), &admin, None);
        assert_eq!(st, 202, "{out}");

        let refused = wait_for(
            "the import to stop with the reason it was refused",
            Duration::from_secs(60),
            || {
                let (_, s) = server.get(&format!("/v1/orgs/acme/repos/{repo}/import"), &admin);
                assert_ne!(
                    s["issues"],
                    serde_json::json!("done"),
                    "a refused import reported itself finished: {s}"
                );
                (s["state"] == serde_json::json!("failed")).then_some(s)
            },
        );
        let why = refused["error"].as_str().unwrap_or_default();
        assert!(
            why.contains("issues: read"),
            "{repo}: the refusal does not name the permission to grant: {refused}"
        );
        let _ = &org_id;
    }
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The workers a deployment can switch off.
///
/// `STRATUM_*_POLL_SECS=0` means "do not run this worker here", which is
/// how a fleet gives one node the queues and leaves the others serving.
/// Every one of those early returns was unexecuted: the suites that pass
/// `0` do it to stop a worker interfering with what they are actually
/// testing, and none of them then asserts that it *stayed* off. A switch
/// nobody checks is a switch that quietly stops working.
///
/// The claim is both halves: the server comes up and serves, and a job
/// enqueued for a disabled worker is still sitting there afterwards.
#[test]
fn a_worker_switched_off_does_not_run_and_the_server_still_serves() {
    let minio = Minio::shared();
    let bucket = minio.bucket("import-workers-off");
    let scratch = Scratch::new("import-workers-off");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("import-workers-off")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_IMPORT_POLL_SECS", "0")
        .env("STRATUM_CHECKS_POLL_SECS", "0")
        .env("STRATUM_SIGNALS_ROLLUP_SECS", "0")
        .env("STRATUM_MIRROR_POLL_SECS", "0")
        // One worker left **on**, and turned right up. It is the witness:
        // see below.
        .env("STRATUM_FORK_POLL_SECS", "0.1")
        .start();
    let admin = server.bootstrap_org("acme");

    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "idle", "public": true })),
    );
    assert_eq!(st, 201, "{out}");
    let repo_id = stratum_control::registry::repo_by_name(&db, &org_id, "idle")
        .unwrap()
        .expect("the repo")
        .id;

    let mut ids = Vec::new();
    // `checkspoll`, which is the kind the worker actually claims
    // (`checks_poll::JOB_KIND` — the crate is a binary, so the constant
    // cannot be reached from here). This said `"checks_poll"` first,
    // and a kind no worker claims stays queued whatever the switch
    // does: the assertion would have passed against a switch that had
    // stopped working, which is the one thing it exists to catch.
    for kind in ["import", "checkspoll"] {
        ids.push(
            stratum_control::jobs::create(&db, &org_id, Some(&repo_id), kind, None)
                .expect("enqueue")
                .id,
        );
    }

    // **A witness, not a wall clock.**
    //
    // The question this test asks is "has enough happened that a worker
    // which was still running would have claimed?", and five seconds was
    // a guess at the answer. The honest answer is a *live* worker on this
    // same server claiming *its* job: the forker is left on at a tenth of
    // a second and given a `fork` job on a repository that is not a fork,
    // which it claims and completes as nothing to do. When that row has
    // left the queue, the queue has demonstrably been serviced since
    // these two were enqueued, and the two below have still not been
    // touched.
    //
    // That is also strictly more than the sleep proved. A sleep passes
    // just as happily against a server on which `spawn_all` ran no
    // workers at all — every job stays queued, every assertion holds,
    // and the switch under test is never exercised. The witness cannot
    // pass that way, because it has to be claimed by something.
    let witness = stratum_control::jobs::create(&db, &org_id, Some(&repo_id), "fork", None)
        .expect("enqueue the witness")
        .id;
    let w = await_job(&db, &org_id, &witness);
    assert_eq!(
        w.state, "done",
        "the witness worker never ran, so this proves nothing about the \
         ones that were switched off: {w:?}"
    );

    for id in &ids {
        let j = stratum_control::jobs::get(&db, &org_id, id)
            .expect("read job")
            .expect("the job exists");
        assert_eq!(
            j.state, "queued",
            "a worker that was switched off claimed a job anyway: {j:?}"
        );
        assert_eq!(j.attempts, 0, "{j:?}");
    }

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// Who may ask, and who may look.
///
/// Three routes, three different authorities, and none of their refusal
/// arms had ever run: starting an import is `OrgAdmin`, reading its
/// progress is `RepoRead`, and the milestones it wrote are `RepoRead`
/// too. An import is the one operation here that writes rows nobody can
/// undo by hand, so "who may start one" is not a detail.
///
/// The masking claim is the same one every read on a private repository
/// makes: the answer must be identical for "not yours" and "not there",
/// or it is an oracle telling a stranger which repositories exist.
#[test]
fn the_import_routes_refuse_the_people_they_should_and_mask_the_rest() {
    let (_scratch, _gh, server, admin) = setup("import-authz");
    mirror(&server, &admin, "widget", "acme/widget");

    // A plain member is not an org admin: they may read the repository
    // and may not start an import in it.
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "mem@acme.test",
            "--name",
            "Mem",
            "--password",
            "a long enough password",
            "--role",
            "member",
        ])
        .expect("create a member");
    let resp = ureq::post(&format!("{}/v1/auth/login", server.base))
        .set("Content-Type", "application/json")
        .send_string(
            &serde_json::json!({
                "email": "mem@acme.test",
                "password": "a long enough password",
            })
            .to_string(),
        )
        .expect("sign in");
    let cookie = resp
        .header("set-cookie")
        .and_then(|c| c.split(';').next())
        .expect("a session cookie")
        .to_string();

    let as_member = |method: &str, path: &str| -> u16 {
        match ureq::request(method, &format!("{}{path}", server.base))
            .set("Cookie", &cookie)
            .call()
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(s, _)) => s,
            Err(e) => panic!("transport {method} {path}: {e}"),
        }
    };

    // 404, not 403: the org-admin gate masks rather than announcing
    // itself, the same way every other authority here does. A member who
    // may read the repository still learns nothing about whether an
    // import surface exists on it.
    assert_eq!(
        as_member("POST", "/v1/orgs/acme/repos/widget/import"),
        404,
        "a plain member started an import, which writes rows nobody can \
         undo by hand"
    );
    // …but reading how far one has got is a repository read, and they
    // may do that.
    assert_eq!(as_member("GET", "/v1/orgs/acme/repos/widget/import"), 200);
    assert_eq!(
        as_member("GET", "/v1/orgs/acme/repos/widget/milestones"),
        200
    );

    // A stranger with no credential is refused all three, and told
    // nothing about which repositories are here.
    let anon = |path: &str| -> (u16, serde_json::Value) { server.req("GET", path, "", None) };
    let (st, denied) = anon("/v1/orgs/acme/repos/widget/milestones");
    assert_eq!(st, 401, "{denied}");
    let (st, absent) = anon("/v1/orgs/acme/repos/no-such-repo/milestones");
    assert_eq!(st, 401);
    assert_eq!(
        denied, absent,
        "the refusal tells a stranger which repositories exist"
    );
    let (st, _) = anon("/v1/orgs/acme/repos/widget/import");
    assert_eq!(st, 401);
    assert_eq!(
        server
            .req("POST", "/v1/orgs/acme/repos/widget/import", "", None)
            .0,
        401
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// A provider that says "come back in a second" is obeyed, and the wait
/// costs one repository rather than the fleet.
///
/// `fake_github.rs` has said what this checks since it was written —
/// "`ratelimited/*` refuses every third call with GitHub's 403 +
/// `Retry-After`, so an importer that ignores the header spins and one
/// that honours it finishes" — and until this landed the importer
/// ignored it. `requeue` took `retry_after_secs` and dropped it, returned
/// `More`, and the worker completed the row, re-enqueued, and claimed the
/// replacement straight back: it only sleeps its poll interval on an
/// *empty* claim. So an import GitHub had asked to slow down asked again
/// as fast as the network allowed, for as long as the limit lasted. On
/// real GitHub that is how a primary limit earns a secondary one on top,
/// which is the same family as the `Retry-After` misclassification
/// CLAUDE.md records against the CI poller.
///
/// Counted in job rows rather than in seconds. Each pass over a refused
/// phase completes one row and enqueues its replacement, so the rows are
/// the spin, and with `Retry-After: 1` an honest importer makes about one
/// per second. The old behaviour made them as fast as the fake could
/// answer — tens in the same window — so the margin here is an order of
/// magnitude, not a hair.
#[test]
fn a_rate_limited_import_waits_the_second_it_was_asked_to_wait() {
    let (_scratch, _gh, server, admin) = setup("import-rl-backoff");
    mirror(&server, &admin, "slow", "ratelimited-labels/widget");

    let (st, out) = server.post("/v1/orgs/acme/repos/slow/import", &admin, None);
    assert_eq!(st, 202, "{out}");

    let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls)
        .expect("connect to the server's database");

    let rows = |db: &mut postgres::Client| -> i64 {
        db.query_one(
            "SELECT COUNT(*) FROM jobs WHERE kind = 'import' AND org_id IN \
             (SELECT id FROM orgs WHERE name = 'acme')",
            &[],
        )
        .expect("count import jobs")
        .get(0)
    };

    // A real wall-clock window, and one of the few that should be.
    //
    // The claim here is a *rate* — how many times the importer asks a
    // provider that told it to wait, per unit of time — so there is no
    // observable to wait on instead: the whole point is that nothing
    // should happen for a while. This is the case the rule about waiting
    // on the observable explicitly leaves room for, not an exception to
    // it. Three seconds of a one-second `Retry-After`.
    let started = Instant::now();
    let window = Duration::from_secs(3);
    while started.elapsed() < window {
        std::thread::sleep(Duration::from_millis(100));
    }
    let made = rows(&mut db);

    // Generous: three windows plus the first run plus slack. The bug
    // produced rows as fast as an HTTP round trip to a local fake, so it
    // fails this by a wide margin rather than by a rounding error.
    assert!(
        made <= 8,
        "the importer made {made} import jobs in {:?} against a Retry-After of 1s — \
         it is asking a provider that told it to wait, as fast as it can",
        started.elapsed()
    );

    // And it really is still working, not wedged: the job is not failed
    // and the phase it could not read is not marked done.
    let (st, s) = server.get("/v1/orgs/acme/repos/slow/import", &admin);
    assert_eq!(st, 200, "{s}");
    assert_ne!(
        s["labels"],
        serde_json::json!("done"),
        "a phase whose page was refused was marked finished: {s}"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
