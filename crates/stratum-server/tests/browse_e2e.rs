//! Reading code through the API: what a file browser needs, and every
//! way of asking for something you should not get.
//!
//! The read endpoints existed before this and were exercised by clients
//! that already knew what they wanted. A browser asks differently — for
//! sizes, for one kind of ref, for a path a user typed — so this covers
//! the shapes a person's clicking produces, and the shapes an attacker's
//! does.

use stratum_testkit::{
    gitcli::{self, Scratch},
    Minio, Server,
};

fn spawn(store_url: &str, scratch: &Scratch, hint: &str) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .start()
}

/// A repo with a nested tree, a binary file and two branches.
fn seed(server: &Server, token: &str, repo: &str, public: bool) {
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        token,
        Some(serde_json::json!({ "name": repo, "public": public })),
    );
    assert_eq!(st, 201, "{out}");
    let base = format!("/v1/orgs/acme/repos/{repo}");
    let (st, out) = server.req(
        "POST",
        &format!("{base}/commits"),
        token,
        Some(serde_json::json!({
            "message": "first",
            "operations": [
                { "op": "put", "path": "README.md", "content": "# hello\nsecond line\n" },
                { "op": "put", "path": "src/main.rs", "content": "fn main() {}\n" },
                { "op": "put", "path": "src/deep/nested.txt", "content": "down here\n" },
                // A NUL byte makes this binary by the same rule git uses.
                { "op": "put", "path": "logo.png", "content": "\u{0}PNG binary-ish" },
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let commit = out["commit"].as_str().unwrap().to_string();
    let (st, out) = server.req(
        "POST",
        &format!("{base}/branches"),
        token,
        Some(serde_json::json!({ "name": "side", "from": commit })),
    );
    assert!(st == 200 || st == 201, "create branch: {st} {out}");
}

/// `?history=1` puts a last-commit column beside every entry, the way
/// GitHub's file list does.
///
/// The opt-in is the reason this needed a test and did not have one.
/// The columns were built, verified by eye in a browser, and shipped —
/// and because every existing tree test asks for a plain `/tree`, not
/// one line of `last_commit_per_entry` was executed by the suite. The
/// coverage gate reported the whole function uncovered, which is
/// exactly what it was.
///
/// What the walk has to get right is **attribution to a directory**: a
/// change to `src/deep/nested.txt` is the last change to `src/`, seen
/// from the root. Reporting only files, or attributing a nested change
/// to nothing, both look plausible on a flat repository and are wrong
/// on every real one.
/// The last-commit column has a deadline, and the listing does not wait
/// for it.
///
/// On a mirror of a real project the walk behind `?history=1` ran past
/// the edge's sixty-second timeout: thousands of commits, every tree
/// read from object storage, and the code browser sat on "Loading…"
/// for a listing the server had read in the first second. The walk now
/// stops on a wall-clock budget and says so; a budget of zero — walk
/// nothing — is how a two-commit repository reaches the branch.
#[test]
fn history_stops_on_its_budget_and_says_so() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-history-budget");
    let scratch = Scratch::new("browse-history-budget");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("browse-history-budget")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_TREE_HISTORY_BUDGET_MS", "0")
        // The budget charges store reads, not cache hits, and this
        // process's workers share the read cache with its requests: the
        // contribution walker reads every commit of the seed behind the
        // POST that wrote it, and a history walk that finds all of them
        // in memory legitimately finishes inside a zero budget. A cache
        // that remembers nothing is what makes "the first store read is
        // refused" the deterministic case it is meant to be.
        .env("STRATUM_READ_CACHE_MB", "0")
        .start();
    let token = server.bootstrap_org("acme");
    seed(&server, &token, "widget", true);
    let base = "/v1/orgs/acme/repos/widget";
    let (st, out) = server.req(
        "POST",
        &format!("{base}/commits"),
        &token,
        Some(serde_json::json!({
            "message": "dig deeper",
            "operations": [
                { "op": "put", "path": "src/deep/nested.txt", "content": "further down\n" },
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");

    // The budget is spent before the first commit is examined, so no
    // entry is attributed and the response says the walk was cut short
    // — distinguishable from "nothing touched it", which is `null` with
    // `history_truncated: false`.
    let started = std::time::Instant::now();
    let (st, tree) = server.req("GET", &format!("{base}/tree?history=1"), &token, None);
    assert_eq!(st, 200, "{tree}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "a budgeted walk took {:?}",
        started.elapsed()
    );
    assert_eq!(tree["history_truncated"], true, "{tree}");
    assert!(
        tree["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["last_commit"].is_null()),
        "an entry was attributed inside a zero budget: {tree}"
    );
    // The listing itself is whole: the budget bounds the column, not
    // the directory.
    assert!(
        tree["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "src"),
        "{tree}"
    );
    // And a plain listing carries no such flag: nothing was walked.
    let (st, plain) = server.req("GET", &format!("{base}/tree"), &token, None);
    assert_eq!(st, 200, "{plain}");
    assert!(plain.get("history_truncated").is_none(), "{plain}");

    // The same walk over a folded layout. Above, the head commit sat in a
    // WAL pack the listing had already opened, so the first store read
    // the budget refused was the parent's tree diff. After a fold every
    // object is in the plane, each read is a locator lookup, and the
    // very first read of the walk — the head commit itself — is the one
    // refused. Both exits have to say "truncated", and only one of them
    // is reachable from either shape of layout. The fold has thresholds
    // (eight WAL entries), so the repository is pushed past them first.
    for i in 0..8 {
        let (st, out) = server.req(
            "POST",
            &format!("{base}/commits"),
            &token,
            Some(serde_json::json!({
                "message": format!("pad {i}"),
                "operations": [
                    { "op": "put", "path": format!("pad/{i}.txt"), "content": "x\n" },
                ],
            })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let (st, out) = server.req("POST", &format!("{base}/compact"), &token, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");
    let (st, tree) = server.req("GET", &format!("{base}/tree?history=1"), &token, None);
    assert_eq!(st, 200, "{tree}");
    assert_eq!(tree["history_truncated"], true, "{tree}");
    assert!(
        tree["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["last_commit"].is_null()),
        "{tree}"
    );
}

#[test]
fn a_tree_can_carry_the_last_commit_that_touched_each_entry() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-history");
    let scratch = Scratch::new("browse-history");
    let server = spawn(&bucket.base_url, &scratch, "browse-history");
    let token = server.bootstrap_org("acme");
    seed(&server, &token, "widget", true);
    let base = "/v1/orgs/acme/repos/widget";

    // A second commit touching only the nested file, so the two entries
    // at the root must report *different* commits. One commit for
    // everything would pass against a walk that never moved.
    let (st, out) = server.req(
        "POST",
        &format!("{base}/commits"),
        &token,
        Some(serde_json::json!({
            "message": "dig deeper",
            "operations": [
                { "op": "put", "path": "src/deep/nested.txt", "content": "further down\n" },
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let second = out["commit"].as_str().expect("commit").to_string();

    // Without the flag: no columns at all. The walk costs a commit
    // traversal, so it is opt-in and must stay opt-in.
    let (st, plain) = server.req("GET", &format!("{base}/tree"), &token, None);
    assert_eq!(st, 200, "{plain}");
    let entry = |t: &serde_json::Value, name: &str| -> serde_json::Value {
        t["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|e| e["name"] == name)
            .unwrap_or_else(|| panic!("no entry {name} in {t}"))
            .clone()
    };
    assert!(
        entry(&plain, "src")["last_commit"].is_null(),
        "a plain tree carried history nobody asked for: {plain}"
    );

    let (st, tree) = server.req("GET", &format!("{base}/tree?history=1"), &token, None);
    assert_eq!(st, 200, "{tree}");

    // `src/` is attributed the nested change, because that is the last
    // commit that altered anything under it.
    let src = entry(&tree, "src");
    assert_eq!(src["last_commit"]["commit"], second, "{tree}");
    assert_eq!(src["last_commit"]["message"], "dig deeper", "{tree}");
    assert!(
        !src["last_commit"]["author"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "{tree}"
    );

    // README.md was not touched by the second commit, so it must still
    // report the first — different from `src/`, which is the assertion
    // that a walk stuck on HEAD would fail.
    let readme = entry(&tree, "README.md");
    assert_ne!(
        readme["last_commit"]["commit"], second,
        "an untouched file was attributed the newest commit: {tree}"
    );
    assert_eq!(readme["last_commit"]["message"], "first", "{tree}");

    // Only the first line of the message: a subject, not a body.
    assert!(
        !readme["last_commit"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains('\n'),
        "{tree}"
    );

    // A third commit touching the two root files, so that walking back
    // from it attributes everything within two steps and the loop exits
    // early rather than running to its cap. That early exit is the
    // whole point of the bounded walk — without it a repository with a
    // long history pays 500 commit reads to describe four entries — and
    // it is only reachable when every entry is attributed *before* the
    // history runs out, which the two-commit case above never does.
    let (st, out) = server.req(
        "POST",
        &format!("{base}/commits"),
        &token,
        Some(serde_json::json!({
            "message": "touch the top",
            "operations": [
                { "op": "put", "path": "README.md", "content": "# hello\nchanged\n" },
                { "op": "put", "path": "logo.png", "content": "\u{0}PNG different" },
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let third = out["commit"].as_str().expect("commit").to_string();

    let (st, tree) = server.req("GET", &format!("{base}/tree?history=1"), &token, None);
    assert_eq!(st, 200, "{tree}");
    assert_eq!(
        entry(&tree, "README.md")["last_commit"]["commit"],
        third,
        "{tree}"
    );
    assert_eq!(
        entry(&tree, "logo.png")["last_commit"]["commit"],
        third,
        "{tree}"
    );
    // And `src` still reports the commit that actually touched it, two
    // steps back, rather than the newest one.
    assert_eq!(
        entry(&tree, "src")["last_commit"]["commit"],
        second,
        "{tree}"
    );

    // Inside a subdirectory the prefix moves with it.
    let (st, sub) = server.req("GET", &format!("{base}/tree/src?history=1"), &token, None);
    assert_eq!(st, 200, "{sub}");
    assert_eq!(
        entry(&sub, "deep")["last_commit"]["commit"],
        second,
        "{sub}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// `?recursive=1` names every path under a directory in one answer,
/// which is what a tree component needs to draw a project whole instead
/// of one request per fold.
///
/// The shape is the whole contract: paths relative to the directory
/// asked for, a directory ending in `/`, and pre-order in git's own
/// tree order — `src/` before `src/deep/` before `src/deep/nested.txt`
/// — so the client can hand the list over as it is. The expectation is
/// a literal list rather than something derived, because the order is
/// the thing under test.
#[test]
fn a_recursive_listing_names_every_path_and_marks_directories() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-recursive");
    let scratch = Scratch::new("browse-recursive");
    let server = spawn(&bucket.base_url, &scratch, "browse-recursive");
    let token = server.bootstrap_org("acme");
    seed(&server, &token, "widget", true);
    let base = "/v1/orgs/acme/repos/widget";

    let (st, root) = server.req("GET", &format!("{base}/tree?recursive=1"), &token, None);
    assert_eq!(st, 200, "{root}");
    assert_eq!(
        root["paths"],
        serde_json::json!([
            "README.md",
            "logo.png",
            "src/",
            "src/deep/",
            "src/deep/nested.txt",
            "src/main.rs"
        ]),
        "{root}"
    );
    assert_eq!(root["truncated"], false, "{root}");
    assert_eq!(root["commit"].as_str().map(str::len), Some(40), "{root}");
    // The flat shape replaces the entries shape rather than sitting
    // beside it: no sizes, no modes, nothing a client would have to
    // ignore.
    assert!(root.get("entries").is_none(), "{root}");

    // Under a subdirectory the paths are relative to it.
    let (st, sub) = server.req("GET", &format!("{base}/tree/src?recursive=1"), &token, None);
    assert_eq!(st, 200, "{sub}");
    assert_eq!(
        sub["paths"],
        serde_json::json!(["deep/", "deep/nested.txt", "main.rs"]),
        "{sub}"
    );
    assert_eq!(sub["truncated"], false, "{sub}");

    // `at` still picks the revision.
    let (st, side) = server.req(
        "GET",
        &format!("{base}/tree?recursive=1&at=side"),
        &token,
        None,
    );
    assert_eq!(st, 200, "{side}");
    assert_eq!(side["paths"], root["paths"], "{side}");
    assert_eq!(side["commit"], root["commit"], "{side}");

    // `history=1` is ignored in the recursive form: there is no row to
    // put a last-commit column on, so there is no flag saying whether
    // that column was cut short.
    let (st, hist) = server.req(
        "GET",
        &format!("{base}/tree?recursive=1&history=1"),
        &token,
        None,
    );
    assert_eq!(st, 200, "{hist}");
    assert_eq!(hist["paths"], root["paths"], "{hist}");
    assert!(hist.get("history_truncated").is_none(), "{hist}");
    assert!(server.healthy());
}

/// The recursive walk is bounded, and says so.
///
/// A hostile push can make a tree arbitrarily wide, so the walk stops at
/// `STRATUM_TREE_RECURSIVE_CAP` paths. Reaching the default needs fifty
/// thousand files, so the servers under test are given caps of two and
/// four: two stops the walk between two root entries, four stops it
/// inside a nested directory, which is the case where a child's
/// truncation has to travel back up the stack rather than let the
/// parent carry on naming the rest of its own directory.
#[test]
fn a_recursive_listing_stops_at_its_cap_and_says_so() {
    let minio = Minio::shared();
    for (cap, expect) in [
        (2, vec!["README.md", "logo.png"]),
        (4, vec!["README.md", "logo.png", "src/", "src/deep/"]),
    ] {
        let name = format!("browse-recursive-cap-{cap}");
        let bucket = minio.bucket(&name);
        let scratch = Scratch::new(&name);
        let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
            .db_hint(&name)
            .data_dir(scratch.path().join("data"))
            .env("STRATUM_TREE_RECURSIVE_CAP", cap.to_string())
            .start();
        let token = server.bootstrap_org("acme");
        seed(&server, &token, "widget", true);
        let base = "/v1/orgs/acme/repos/widget";

        let (st, out) = server.req("GET", &format!("{base}/tree?recursive=1"), &token, None);
        assert_eq!(st, 200, "cap {cap}: {out}");
        assert_eq!(out["paths"], serde_json::json!(expect), "cap {cap}: {out}");
        assert_eq!(
            out["paths"].as_array().map(Vec::len),
            Some(cap),
            "cap {cap}: {out}"
        );
        assert_eq!(out["truncated"], true, "cap {cap}: {out}");
        // The cap bounds the recursive form only: the ordinary listing
        // is a single tree read and is whole.
        let (st, plain) = server.req("GET", &format!("{base}/tree"), &token, None);
        assert_eq!(st, 200, "{plain}");
        assert_eq!(
            plain["entries"].as_array().map(Vec::len),
            Some(3),
            "{plain}"
        );
        assert!(server.healthy());
    }
}

/// A file page needs its own history: who last touched *this* file, what
/// they did to it, and which versions there are to switch between.
///
/// The filter is the point. Asking for the whole log and discarding rows
/// in the browser gives the same answer for a small repository and falls
/// over on a real one, where a file touched once at the start means
/// downloading everything to find it.
#[test]
fn a_files_history_holds_only_the_commits_that_changed_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-history");
    let scratch = Scratch::new("browse-history");
    let server = spawn(&bucket.base_url, &scratch, "browse-history");
    let token = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &token,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let base = "/v1/orgs/acme/repos/app";

    let commits = [
        serde_json::json!({ "message": "first", "operations": [
            { "op": "put", "path": "README.md", "content": "v1\n" },
            { "op": "put", "path": "src/main.rs", "content": "fn main() {}\n" },
            { "op": "put", "path": "doomed.txt", "content": "here for now\n" }]}),
        serde_json::json!({ "message": "readme v2", "operations": [
            { "op": "put", "path": "README.md", "content": "v2\n" }]}),
        serde_json::json!({ "message": "main only", "operations": [
            { "op": "put", "path": "src/main.rs", "content": "fn main() { }\n" }]}),
        serde_json::json!({ "message": "readme v3, and drop doomed", "operations": [
            { "op": "put", "path": "README.md", "content": "v3\n" },
            { "op": "delete", "path": "doomed.txt" }]}),
    ];
    for c in commits {
        let (st, out) = server.req("POST", &format!("{base}/commits"), &token, Some(c));
        assert_eq!(st, 201, "{out}");
    }

    let history = |path: &str| -> Vec<(String, String)> {
        let (st, out) = server.get(&format!("{base}/log?limit=50&path={path}"), &token);
        assert_eq!(st, 200, "{out}");
        out["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .map(|e| {
                (
                    e["message"].as_str().unwrap_or_default().trim().to_string(),
                    e["change"].as_str().unwrap_or("none").to_string(),
                )
            })
            .collect()
    };

    // The unfiltered log still sees all four.
    let (_, all) = server.get(&format!("{base}/log?limit=50"), &token);
    assert_eq!(all["entries"].as_array().map(Vec::len), Some(4), "{all}");
    // ...and says nothing about any one path, because it cannot.
    assert!(
        all["entries"][0].get("change").is_none(),
        "an unfiltered log must not claim a per-path change: {all}"
    );

    assert_eq!(
        history("README.md"),
        [
            (
                "readme v3, and drop doomed".to_string(),
                "modified".to_string()
            ),
            ("readme v2".to_string(), "modified".to_string()),
            ("first".to_string(), "added".to_string()),
        ]
    );
    assert_eq!(
        history("src/main.rs"),
        [
            ("main only".to_string(), "modified".to_string()),
            ("first".to_string(), "added".to_string()),
        ]
    );
    // A deletion is a change to that path, and the last thing that
    // happened to it.
    assert_eq!(
        history("doomed.txt"),
        [
            (
                "readme v3, and drop doomed".to_string(),
                "deleted".to_string()
            ),
            ("first".to_string(), "added".to_string()),
        ]
    );
    // A path nothing ever touched has an empty history, not an error.
    assert_eq!(history("never/existed.txt"), [] as [(String, String); 0]);
    // Leading and trailing slashes are the same path.
    assert_eq!(history("/README.md").len(), 3);

    // Every commit the filter returned can read the file back at that
    // version — which is exactly what the version switcher does.
    let (st, out) = server.get(&format!("{base}/log?limit=50&path=README.md"), &token);
    assert_eq!(st, 200, "{out}");
    let oldest = out["entries"][2]["commit"].as_str().unwrap().to_string();
    let (st, body, _) = server.req_full(
        "GET",
        &format!("{base}/files/README.md?at={oldest}"),
        &token,
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert!(
        body.as_str().unwrap_or_default().contains("v1"),
        "the oldest version should still read v1: {body}"
    );

    assert!(server.healthy());
}

/// A filtered walk is bounded, and the cursor it hands back is usable.
///
/// The budget exists so a file changed once at the start of a long
/// history cannot hold a connection open for an unbounded scan. Reaching
/// it normally needs five hundred commits, so the server under test is
/// given a budget of one — a limit nothing ever exercises is a limit
/// nobody knows still works.
#[test]
fn a_filtered_log_stops_at_its_budget_and_hands_back_a_working_cursor() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-budget");
    let scratch = Scratch::new("browse-budget");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("browse-budget")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_LOG_SCAN_BUDGET", "1")
        .start();
    let token = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &token,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let base = "/v1/orgs/acme/repos/app";

    // Four commits, only the first and last touching `rare.txt`. With a
    // budget of one, finding the older of them takes several requests.
    for (i, ops) in [
        serde_json::json!([{ "op": "put", "path": "rare.txt", "content": "one\n" }]),
        serde_json::json!([{ "op": "put", "path": "noise.txt", "content": "a\n" }]),
        serde_json::json!([{ "op": "put", "path": "noise.txt", "content": "b\n" }]),
        serde_json::json!([{ "op": "put", "path": "rare.txt", "content": "two\n" }]),
    ]
    .into_iter()
    .enumerate()
    {
        let (st, out) = server.req(
            "POST",
            &format!("{base}/commits"),
            &token,
            Some(serde_json::json!({ "message": format!("c{i}"), "operations": ops })),
        );
        assert_eq!(st, 201, "{out}");
    }

    // Page through, following the cursor, and collect what turns up.
    let mut seen: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    let mut requests = 0;
    loop {
        requests += 1;
        assert!(requests < 20, "the cursor is not advancing");
        let url = match &after {
            Some(a) => format!("{base}/log?path=rare.txt&limit=50&after={a}"),
            None => format!("{base}/log?path=rare.txt&limit=50"),
        };
        let (st, out) = server.get(&url, &token);
        assert_eq!(st, 200, "{out}");
        for e in out["entries"].as_array().expect("entries") {
            seen.push(e["message"].as_str().unwrap_or_default().trim().to_string());
        }
        match out["next_after"].as_str() {
            Some(n) => after = Some(n.to_string()),
            None => break,
        }
    }

    // Both commits that touched the file, and neither that did not —
    // the budget bounds the work per request, never the answer.
    assert_eq!(seen, ["c3", "c0"], "paged result");
    // And it really took more than one request to get there, which is
    // what proves the budget was reached rather than ignored.
    assert!(
        requests > 2,
        "a budget of one should have needed several pages, took {requests}"
    );
    assert!(server.healthy());
}

/// Everything a file browser asks for on the way to showing a file.
#[test]
fn browsing_gives_a_tree_with_sizes_a_file_with_a_type_and_the_refs() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-happy");
    let scratch = Scratch::new("browse-happy");
    let server = spawn(&bucket.base_url, &scratch, "browse-happy");
    let token = server.bootstrap_org("acme");
    seed(&server, &token, "app", false);
    let base = "/v1/orgs/acme/repos/app";

    // The root listing: directories first-class, blobs measured when
    // asked. A plain listing reads no blob at all — GitHub's file table
    // has no size column and neither does ours, and on a real mirror the
    // thirty reads that column cost were most of the page's wait — so
    // `size` is null there and a number only under `?sizes=1`.
    let (st, plain) = server.req("GET", &format!("{base}/tree"), &token, None);
    assert_eq!(st, 200, "{plain}");
    assert!(
        plain["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["size"].is_null()),
        "a plain listing measured a blob: {plain}"
    );
    let (st, tree) = server.req("GET", &format!("{base}/tree?sizes=1"), &token, None);
    assert_eq!(st, 200, "{tree}");
    let entries = tree["entries"].as_array().unwrap();
    let by = |n: &str| {
        entries
            .iter()
            .find(|e| e["name"] == n)
            .unwrap_or_else(|| panic!("no {n} in {tree}"))
            .clone()
    };
    assert_eq!(by("src")["kind"], "tree");
    // A tree has no size: the size of its listing is not what anybody
    // reading that column would assume.
    assert!(by("src")["size"].is_null(), "{tree}");
    assert_eq!(by("README.md")["kind"], "blob");
    assert_eq!(by("README.md")["size"], "# hello\nsecond line\n".len());

    // A nested listing, which is what a breadcrumb click produces.
    let (st, sub) = server.req("GET", &format!("{base}/tree/src"), &token, None);
    assert_eq!(st, 200, "{sub}");
    let names: Vec<&str> = sub["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"main.rs"), "{sub}");
    assert!(names.contains(&"deep"), "{sub}");

    // A text file: served as text, flagged as not binary, with the
    // commit it came from.
    let (st, body, headers) =
        server.req_full("GET", &format!("{base}/files/README.md"), &token, None);
    assert_eq!(st, 200, "{body}");
    let header = |n: &str| headers.get(n).cloned().unwrap_or_default();
    assert_eq!(header("content-type"), "text/plain; charset=utf-8");
    assert_eq!(header("x-weft-binary"), "false");
    assert_eq!(header("x-weft-commit").len(), 40, "{headers:?}");
    assert!(!header("etag").is_empty());

    // A binary file: flagged, and typed by name rather than by guessing
    // at its content.
    let (st, _, headers) = server.req_full("GET", &format!("{base}/files/logo.png"), &token, None);
    assert_eq!(st, 200);
    assert_eq!(
        headers.get("x-weft-binary").cloned().unwrap_or_default(),
        "true"
    );
    assert_eq!(
        headers.get("content-type").cloned().unwrap_or_default(),
        "image/png"
    );

    // Branches and tags separately, sorted, with the default marked —
    // both paths were write-only until now.
    let (st, br) = server.req("GET", &format!("{base}/branches"), &token, None);
    assert_eq!(st, 200, "{br}");
    let names: Vec<&str> = br["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["main", "side"], "sorted, and both present: {br}");
    let default_count = br["branches"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["default"] == true)
        .count();
    assert_eq!(default_count, 1, "exactly one default: {br}");
    let (st, tags) = server.req("GET", &format!("{base}/tags"), &token, None);
    assert_eq!(st, 200, "{tags}");
    assert!(tags["tags"].as_array().unwrap().is_empty(), "{tags}");

    // Reading at a branch that is not the default resolves there.
    let (st, at_side) = server.req("GET", &format!("{base}/tree?at=side"), &token, None);
    assert_eq!(st, 200, "{at_side}");
    assert!(server.healthy());
}

/// The conditional read, which is what makes revisiting a file free —
/// and which used to cost a second authorization and a second reader
/// even when it answered 304.
#[test]
fn an_unchanged_file_answers_304_and_the_read_is_not_done_twice() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-etag");
    let scratch = Scratch::new("browse-etag");
    let server = spawn(&bucket.base_url, &scratch, "browse-etag");
    let token = server.bootstrap_org("acme");
    seed(&server, &token, "app", false);
    let path = "/v1/orgs/acme/repos/app/files/README.md";

    let (st, body, headers) = server.req_full("GET", path, &token, None);
    assert_eq!(st, 200, "{body}");
    let etag = headers.get("etag").cloned().unwrap();

    let resp = ureq::get(&format!("{}{path}", server.base))
        .set("Authorization", &format!("Bearer {token}"))
        .set("If-None-Match", &etag)
        .call();
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(resp.status(), 304);
    assert_eq!(resp.header("etag"), Some(etag.as_str()));
    // 304 means no body, which is the whole point of asking.
    assert_eq!(resp.into_string().unwrap_or_default(), "");

    // A stale ETag gets the content, not another 304.
    let resp = ureq::get(&format!("{}{path}", server.base))
        .set("Authorization", &format!("Bearer {token}"))
        .set(
            "If-None-Match",
            "\"0000000000000000000000000000000000000000\"",
        )
        .call()
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.into_string().unwrap().contains("hello"));
    assert!(server.healthy());
}

/// Everything a browser can be pointed at that it should not reach.
#[test]
fn reading_refuses_traversal_strangers_and_nonsense_revisions() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-neg");
    let scratch = Scratch::new("browse-neg");
    let server = spawn(&bucket.base_url, &scratch, "browse-neg");
    let token = server.bootstrap_org("acme");
    let other = server.bootstrap_org("bravo");
    seed(&server, &token, "private-app", false);
    seed(&server, &token, "open-app", true);
    let base = "/v1/orgs/acme/repos/private-app";

    // Path traversal, in the shapes a URL can carry it. Nothing may
    // escape the repository, and nothing may 500.
    for path in [
        "../../../etc/passwd",
        "..%2f..%2fetc%2fpasswd",
        "src/../../../etc/passwd",
        "src/./../../etc/passwd",
        "/etc/passwd",
        "src%00/main.rs",
        ".git/config",
    ] {
        for kind in ["files", "tree"] {
            let (st, out) = server.req("GET", &format!("{base}/{kind}/{path}"), &token, None);
            assert!(st == 400 || st == 404, "{kind}/{path} answered {st}: {out}");
        }
    }

    // A private repo: invisible without credentials and to another org,
    // and answered the same way either time so neither can be used to
    // discover the other.
    for tok in ["", other.as_str()] {
        for path in ["/tree", "/files/README.md", "/branches", "/tags", "/log"] {
            let st = server.status_get(&format!("{base}{path}"), Some(tok));
            assert!(st == 401 || st == 404, "{path} leaked to a stranger: {st}");
        }
    }
    // …while the public one is readable by anybody.
    let st = server.status_get("/v1/orgs/acme/repos/open-app/tree", None);
    assert_eq!(st, 200, "a public repo was not public");

    // Revisions that are not revisions. A 40-hex string that is not an
    // object used to pass `resolve_rev` and fail later somewhere less
    // helpful.
    for rev in [
        "no-such-branch",
        "0000000000000000000000000000000000000000",
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "refs/heads/../../etc",
        "HEAD~99999",
        "",
    ] {
        let (st, out) = server.req(
            "GET",
            &format!("{base}/tree?at={}", urlencode(rev)),
            &token,
            None,
        );
        assert!(st == 400 || st == 404, "rev {rev:?} answered {st}: {out}");
    }

    // A path that exists but is the other kind. A deep link has no
    // listing behind it, so the client has to guess whether a path is a
    // directory or a file, and the wrong guess is the ordinary case, not
    // a broken server. Both answered 500 until a manual pass opened a
    // file link in a fresh tab.
    for (kind, path) in [("tree", "src/main.rs"), ("files", "src")] {
        let (st, out) = server.req("GET", &format!("{base}/{kind}/{path}"), &token, None);
        assert_eq!(st, 404, "{kind} of {path:?} answered {st}: {out}");
        let said = out["error"].as_str().unwrap_or_default().to_string();
        assert!(said.contains("is not a"), "{kind} of {path:?} said: {out}");
    }
    // The recursive form checks the kind before it walks anything: a
    // blob asked for recursively is the same 404, not a walk of nothing
    // answering an empty list.
    let (st, out) = server.req(
        "GET",
        &format!("{base}/tree/src/main.rs?recursive=1"),
        &token,
        None,
    );
    assert_eq!(st, 404, "recursive tree of a blob answered {st}: {out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("is not a tree"),
        "{out}"
    );
    // And it is behind the same door as the plain listing: a stranger
    // cannot enumerate a private repository's paths.
    for tok in ["", other.as_str()] {
        let st = server.status_get(&format!("{base}/tree?recursive=1"), Some(tok));
        assert!(
            st == 401 || st == 404,
            "recursive tree leaked to a stranger: {st}"
        );
    }

    // The injection corpus through the path segment a user types.
    for bad in stratum_testkit::adversarial::INJECTIONS {
        let (st, _) = server.req(
            "GET",
            &format!("{base}/files/{}", urlencode(bad)),
            &token,
            None,
        );
        assert!(st == 400 || st == 404, "{bad:?} answered {st}");
    }
    assert!(server.healthy(), "still serving after all that");
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The commit count is GitHub's number — every commit reachable from the
/// default branch, through every parent — taken by the job that follows
/// a write and served from the repository row.
///
/// The dashboard used to fetch it itself: the log with a limit of a
/// thousand, which the server clamped to five hundred and walked one
/// object at a time, first-parent only. Nine seconds on the production
/// mirror, for "119 commits" on a repository GitHub says has 483. The
/// merge below is the case the first-parent walk gets wrong, and the cap
/// is the case a walk with no bound gets wrong on a repository that is
/// large enough.
#[test]
fn the_commit_count_is_every_reachable_commit_taken_after_a_write() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-commit-count");
    let scratch = Scratch::new("browse-commit-count");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("browse-commit-count")
        .data_dir(scratch.path().join("data"))
        // Manual only: the count rides on the compaction job, and the
        // test drives that job through `POST …/compact` so there is one
        // moment at which the number is expected to have changed.
        .env("STRATUM_COMPACT_POLL_SECS", "0")
        .start();
    let token = server.bootstrap_org("acme");
    seed(&server, &token, "app", false);
    let base = "/v1/orgs/acme/repos/app";

    // Before any fold: no number, and the flag has one honest value.
    let (st, row) = server.req("GET", base, &token, None);
    assert_eq!(st, 200, "{row}");
    assert!(row["commits"].is_null(), "{row}");
    assert!(row["commits_tip"].is_null(), "{row}");
    assert_eq!(row["commits_exact"], true, "{row}");

    // The job that follows a write takes the count.
    let (st, out) = server.req("POST", &format!("{base}/compact"), &token, None);
    assert_eq!(st, 200, "{out}");
    let (_, row) = server.req("GET", base, &token, None);
    let (_, log) = server.req("GET", &format!("{base}/log?limit=1"), &token, None);
    let head = log["entries"][0]["commit"].as_str().unwrap();
    assert_eq!(row["commits"], 1, "{row}");
    assert_eq!(row["commits_tip"], head, "{row}");
    assert_eq!(row["commits_exact"], true, "{row}");

    // A merge, pushed with the real client: main gains a merge commit
    // and a side commit. Four commits reach the tip; a first-parent walk
    // would say three.
    let url = server.authed_url(&token, "acme", "app");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);
    gitcli::git(&clone, &["checkout", "-q", "-b", "side"]);
    std::fs::write(clone.join("side.txt"), "on the side\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(
        &clone,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-q",
            "-m",
            "side",
        ],
    );
    gitcli::git(&clone, &["checkout", "-q", "main"]);
    std::fs::write(clone.join("main.txt"), "on main\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(
        &clone,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-q",
            "-m",
            "main",
        ],
    );
    gitcli::git(
        &clone,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "merge side",
            "side",
        ],
    );
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    let new_head = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Until the job runs the row still carries the old tip, and says
    // so — which is how a client knows not to print the old number as
    // current.
    let (_, row) = server.req("GET", base, &token, None);
    assert_eq!(row["commits"], 1, "{row}");
    assert_eq!(row["commits_tip"], head, "{row}");
    assert_ne!(row["commits_tip"], new_head);

    let (st, out) = server.req("POST", &format!("{base}/compact"), &token, None);
    assert_eq!(st, 200, "{out}");
    let (_, row) = server.req("GET", base, &token, None);
    assert_eq!(row["commits"], 4, "every parent is followed: {row}");
    assert_eq!(row["commits_tip"], new_head, "{row}");
    assert_eq!(row["commits_exact"], true, "{row}");

    // A second fold with nothing pushed changes nothing and walks
    // nothing: the row is already for this tip.
    let (st, _) = server.req("POST", &format!("{base}/compact"), &token, None);
    assert_eq!(st, 200);
    let (_, again) = server.req("GET", base, &token, None);
    assert_eq!(again["commits"], 4);
    assert_eq!(again["commits_tip"], new_head);

    // A repository with no commits has no tip to count from, and the
    // fold that runs over it leaves the count absent rather than zero:
    // "nothing has been pushed" and "we counted none" are different
    // sentences and only the first is true.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &token,
        Some(serde_json::json!({ "name": "blank", "public": false })),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.req("POST", "/v1/orgs/acme/repos/blank/compact", &token, None);
    assert_eq!(st, 200, "{out}");
    let (_, blank) = server.req("GET", "/v1/orgs/acme/repos/blank", &token, None);
    assert!(blank["commits"].is_null(), "{blank}");
    assert!(server.healthy());
}

/// A walk that reaches the cap stores what it counted and says the
/// number is a floor, so a page prints `2+` rather than `2`.
#[test]
fn a_commit_count_past_the_cap_is_stored_as_a_floor() {
    let minio = Minio::shared();
    let bucket = minio.bucket("browse-commit-cap");
    let scratch = Scratch::new("browse-commit-cap");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("browse-commit-cap")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_COMPACT_POLL_SECS", "0")
        .env("STRATUM_COMMIT_COUNT_CAP", "2")
        .start();
    let token = server.bootstrap_org("acme");
    seed(&server, &token, "app", false);
    let base = "/v1/orgs/acme/repos/app";
    for i in 0..3 {
        let (st, out) = server.req(
            "POST",
            &format!("{base}/commits"),
            &token,
            Some(serde_json::json!({
                "message": format!("more {i}"),
                "operations": [
                    { "op": "put", "path": format!("f{i}.txt"), "content": "x\n" },
                ],
            })),
        );
        assert_eq!(st, 201, "{out}");
    }
    let (st, out) = server.req("POST", &format!("{base}/compact"), &token, None);
    assert_eq!(st, 200, "{out}");
    let (_, row) = server.req("GET", base, &token, None);
    assert_eq!(row["commits"], 2, "the cap, not the four that exist: {row}");
    assert_eq!(row["commits_exact"], false, "{row}");
    assert!(server.healthy());
}
