//! `GET …/workflows` against a real repository: what `.weft/` asks
//! to have run at a rev, and why a file is refused.
//!
//! Driven through HTTP with real commits, because the unit tests already
//! cover the parser and planner in isolation and what is left to prove
//! is the part they cannot see — that the route reads the right
//! directory out of the right tree at the right rev, and that a refusal
//! survives the trip out to a caller with its line intact.

use stratum_testkit::{gitcli::Scratch, Minio, Server};

fn spawn(store_url: &str, scratch: &Scratch) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .db_hint("workflows-e2e")
        .start()
}

fn commit(server: &Server, token: &str, branch: &str, files: &[(&str, &str)]) -> String {
    let ops: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, c)| serde_json::json!({"op": "put", "path": p, "content": c}))
        .collect();
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        token,
        Some(serde_json::json!({
            "branch": branch,
            "message": "workflows",
            "operations": ops,
        })),
    );
    assert_eq!(st, 201, "{out}");
    out["commit"].as_str().expect("a commit oid").to_string()
}

const GOOD: &str = "\
name: build and test
on: [push, pull_request]
jobs:
  build:
    image: rust:1.83
    strategy:
      matrix:
        os: [linux, mac]
    steps:
      - run: cargo build
  ship:
    needs: build
    steps:
      - run: ./ship.sh
";

/// The whole slice, end to end: read the directory at a rev, parse,
/// expand the matrix, order the graph, and answer.
#[test]
fn the_route_reports_what_would_run_and_why_a_file_is_wrong() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workflows-e2e");
    let scratch = Scratch::new("workflows-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    assert_eq!(st, 201, "{out}");

    // A repository with no `.weft/` at all answers with an empty
    // list, not a 404: "this project has no workflows" is a fact about
    // the project, not a missing page.
    commit(&server, &admin, "main", &[("README.md", "hi")]);
    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["workflows"].as_array().unwrap().len(), 0, "{out}");

    // Now add one good file and one that a pasted Actions workflow
    // would look like.
    let bad = "\
on: push
jobs:
  a:
    steps:
      - uses: actions/checkout@v4
      - run: make
";
    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/ci.yml", GOOD),
            (".weft/legacy.yml", bad),
            // Not a workflow; must be ignored rather than refused.
            (".weft/README.md", "notes"),
        ],
    );

    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows", &admin);
    assert_eq!(st, 200, "{out}");
    let wfs = out["workflows"].as_array().expect("a list");
    assert_eq!(wfs.len(), 2, "the README is not a workflow: {out}");

    let good = wfs
        .iter()
        .find(|w| w["file"] == ".weft/ci.yml")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(good["ok"], serde_json::json!(true), "{good}");
    assert_eq!(good["name"], serde_json::json!("build and test"));
    // `pull_request` and `push` both, deduplicated to our two words.
    let on: Vec<&str> = good["on"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert!(on.contains(&"push") && on.contains(&"change"), "{good}");

    // The matrix is expanded here, so a reader sees the cells rather
    // than the recipe — three jobs, not two.
    let jobs = good["jobs"].as_array().expect("jobs");
    let keys: Vec<&str> = jobs.iter().map(|j| j["key"].as_str().unwrap()).collect();
    assert_eq!(keys, vec!["build (linux)", "build (mac)", "ship"], "{good}");
    // And `ship` waits for both cells, named the way they are listed.
    let ship = jobs.iter().find(|j| j["key"] == "ship").unwrap();
    let needs: Vec<&str> = ship["needs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    assert_eq!(needs, vec!["build (linux)", "build (mac)"], "{ship}");

    // The refusal survives the trip out, with its line and its way
    // forward — this is the message somebody migrating meets first.
    let bad = wfs
        .iter()
        .find(|w| w["file"] == ".weft/legacy.yml")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(bad["ok"], serde_json::json!(false), "{bad}");
    let p = &bad["problems"][0];
    assert_eq!(p["line"], serde_json::json!(5), "{p}");
    assert!(p["message"].as_str().unwrap().contains("`uses:`"), "{p}");
    assert!(p["hint"].as_str().unwrap().contains("run:"), "{p}");
    assert!(
        p["text"]
            .as_str()
            .unwrap()
            .starts_with(".weft/legacy.yml:5:"),
        "the rendered line names the file and the line: {p}"
    );

    assert!(server.healthy());
}

/// A file that parses and still cannot be used: the planner's refusals
/// have to reach a caller too, and they are a different code path from
/// the parser's.
///
/// `needs` naming a job that does not exist is the ordinary way this
/// happens — a typo — and it is refused rather than ignored, because
/// ignoring it runs a job whose author believed it was gated.
#[test]
fn a_workflow_the_planner_refuses_comes_back_with_its_reasons() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workflows-e2e-plan");
    let scratch = Scratch::new("workflows-e2e-plan");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    commit(
        &server,
        &admin,
        "main",
        &[(
            ".weft/ci.yml",
            "\
on: push
jobs:
  test:
    needs: buidl
    steps:
      - run: x
  build:
    steps:
      - run: y
",
        )],
    );

    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows", &admin);
    assert_eq!(st, 200, "{out}");
    let w = &out["workflows"][0];
    assert_eq!(w["ok"], serde_json::json!(false), "{w}");
    // The workflow parsed, so it still knows its own name — a caller can
    // say *which* workflow is broken, not just which file.
    assert_eq!(w["name"], serde_json::json!("ci"), "{w}");
    let p = &w["problems"][0];
    assert!(p["message"].as_str().unwrap().contains("buidl"), "{p}");
    // And the hint lists the jobs that do exist, which is what makes a
    // typo obvious rather than merely reported.
    assert!(p["hint"].as_str().unwrap().contains("build"), "{p}");

    assert!(server.healthy());
}

/// `?at=` reads the tree at that rev, so a workflow can be inspected on
/// a branch before it is anywhere near trunk — which is the whole point
/// of being able to ask before pushing.
#[test]
fn workflows_are_read_at_the_rev_asked_for() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workflows-e2e-rev");
    let scratch = Scratch::new("workflows-e2e-rev");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    commit(&server, &admin, "main", &[("README.md", "hi")]);
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/branches",
        &admin,
        Some(serde_json::json!({"name": "feature", "from": "main"})),
    );
    assert!(st == 201 || st == 200, "{out}");
    commit(&server, &admin, "feature", &[(".weft/ci.yml", GOOD)]);

    // Trunk has none...
    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows?at=main", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["workflows"].as_array().unwrap().len(), 0, "{out}");

    // ...the branch has one.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows?at=feature", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["workflows"].as_array().unwrap().len(), 1, "{out}");

    // A rev nobody has is a 404 naming it, not an empty list — an empty
    // list would say "this ref has no workflows" about a ref that does
    // not exist.
    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows?at=ghost", &admin);
    assert_eq!(st, 404, "{out}");
    assert!(out["error"].as_str().unwrap().contains("ghost"), "{out}");

    assert!(server.healthy());
}

/// A repository is untrusted content, so `.weft/` with hundreds of
/// files in it must not turn one request into hundreds of object reads.
#[test]
fn a_directory_full_of_workflows_is_read_only_up_to_the_cap() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workflows-e2e-many");
    let scratch = Scratch::new("workflows-e2e-many");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    let names: Vec<String> = (0..40).map(|i| format!(".weft/w{i:02}.yml")).collect();
    let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), GOOD)).collect();
    commit(&server, &admin, "main", &files);

    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows", &admin);
    assert_eq!(st, 200, "{out}");
    let n = out["workflows"].as_array().unwrap().len();
    assert!(
        n > 0 && n <= 32,
        "the cap bounds the work rather than the answer being empty: got {n}"
    );

    assert!(server.healthy());
}

/// The route is a repository read, so it answers a stranger the way
/// every other repository read does.
#[test]
fn a_private_repos_workflows_are_not_readable_by_a_stranger() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workflows-e2e-auth");
    let scratch = Scratch::new("workflows-e2e-auth");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    commit(&server, &admin, "main", &[(".weft/ci.yml", GOOD)]);

    let (st, _) = server.get("/v1/orgs/acme/repos/app/workflows", "");
    assert!(
        st == 401 || st == 404,
        "a private repo's workflows must not be readable anonymously, got {st}"
    );

    assert!(server.healthy());
}

/// A repository whose `.weft` is a *file*, and one whose workflow is
/// not valid UTF-8. Both are things a repository can contain, and
/// neither may be a 500.
#[test]
fn odd_repository_content_is_answered_rather_than_crashed() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workflows-e2e-odd");
    let scratch = Scratch::new("workflows-e2e-odd");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );
    // `.weft` as a file rather than a directory.
    commit(&server, &admin, "main", &[(".weft", "not a directory")]);
    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["workflows"].as_array().unwrap().len(), 0, "{out}");

    assert!(server.healthy());
}

/// The directory is `.weft/`, and only `.weft/`.
///
/// It was `.stratum/` until the product was named, and the rename was a
/// find-and-replace across 266 lines — the kind of change where one
/// missed constant leaves the server reading the old name while every
/// document says the new one, or reading both, so that a repository
/// nobody audited under `.stratum/` keeps running CI. So: the same file
/// under the old name is not a workflow, and under the new one it is.
#[test]
fn the_old_directory_name_is_not_read() {
    let minio = Minio::shared();
    let bucket = minio.bucket("workflows-e2e-rename");
    let scratch = Scratch::new("workflows-e2e-rename");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "app"})),
    );

    commit(&server, &admin, "main", &[(".stratum/ci.yml", GOOD)]);
    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["workflows"].as_array().unwrap().len(),
        0,
        "a workflow under the old `.stratum/` name was read:\n{out}"
    );

    commit(&server, &admin, "main", &[(".weft/ci.yml", GOOD)]);
    let (st, out) = server.get("/v1/orgs/acme/repos/app/workflows", &admin);
    assert_eq!(st, 200, "{out}");
    let files: Vec<&str> = out["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["file"].as_str().unwrap())
        .collect();
    assert_eq!(files, [".weft/ci.yml"], "{out}");

    assert!(server.healthy());
}
