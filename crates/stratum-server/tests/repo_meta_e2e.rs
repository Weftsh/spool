//! The repository's own description of itself — languages, licence,
//! community files, topics — end to end against a real server.
//!
//! Three things are being proved here, and only the first is the
//! feature:
//!
//! 1. Every fact except the topics is derived from the tree we already
//!    serve, so a repository that has just been pushed to answers
//!    correctly with nothing else having run.
//! 2. **The panel is masked exactly as the code is.** This is a new way
//!    to leak that a private repository exists, and a language bar is a
//!    small thing to leak next to "acme has a private repository called
//!    `payments`". Both the anonymous and the signed-in-but-foreign
//!    caller must get the same answer they get for a repository that was
//!    never created, and the server must still be serving afterwards.
//! 3. Topics are lowercased at the write. The column carries
//!    `CHECK (topic = lower(topic))`, so the failure mode of forgetting
//!    is a 500 about our schema — which is why there is a test that
//!    sends `Rust` and reads back `rust` rather than one that sends
//!    `rust` and is satisfied.

use stratum_testkit::{gitcli::Scratch, CountingProxy, Minio, Server};

fn spawn(store_url: &str, scratch: &Scratch, hint: &str) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .start()
}

const MIT: &str = "MIT License\n\nCopyright (c) 2026 Ada Lovelace\n\n\
Permission is hereby granted, free of charge, to any person obtaining a copy \
of this software and associated documentation files (the \"Software\"), to deal \
in the Software without restriction, including without limitation the rights \
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell \
copies of the Software.\n\n\
THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR \
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, \
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.\n";

const APACHE: &str = "                                 Apache License\n\
                           Version 2.0, January 2004\n\
                        http://www.apache.org/licenses/\n\n\
   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION\n";

fn create(server: &Server, token: &str, org: &str, repo: &str) {
    let (st, out) = server.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        token,
        Some(serde_json::json!({ "name": repo })),
    );
    assert_eq!(st, 201, "{out}");
}

fn commit(server: &Server, token: &str, org: &str, repo: &str, ops: serde_json::Value) {
    let (st, out) = server.req(
        "POST",
        &format!("/v1/orgs/{org}/repos/{repo}/commits"),
        token,
        Some(serde_json::json!({ "message": "seed", "operations": ops })),
    );
    assert_eq!(st, 201, "{out}");
}

/// A project shaped like an open-source project: three languages in
/// different quantities, a licence, a contributing guide at the root and
/// a security policy in `.github/`.
fn seed_project(server: &Server, token: &str, org: &str, repo: &str) {
    create(server, token, org, repo);
    commit(
        server,
        token,
        org,
        repo,
        serde_json::json!([
            { "op": "put", "path": "LICENSE", "content": MIT },
            { "op": "put", "path": "README.md", "content": "# widget\n" },
            { "op": "put", "path": "CONTRIBUTING.md", "content": "send patches\n" },
            { "op": "put", "path": ".github/SECURITY.md", "content": "mail security@\n" },
            // 300 bytes of Rust, 120 of TypeScript, 40 of Python — three
            // different sizes so the ranking has something to get wrong.
            { "op": "put", "path": "src/main.rs", "content": "r".repeat(200) },
            { "op": "put", "path": "src/lib.rs", "content": "r".repeat(100) },
            { "op": "put", "path": "web/app.ts", "content": "t".repeat(120) },
            { "op": "put", "path": "tools/gen.py", "content": "p".repeat(40) },
            // Neither of these is a language, and counting them is the
            // mistake that turns every documented project into a
            // "Markdown project".
            { "op": "put", "path": "Cargo.toml", "content": "x".repeat(5000) },
            { "op": "put", "path": "docs/guide.md", "content": "d".repeat(9000) },
        ]),
    );
}

#[test]
fn a_repository_describes_itself_from_its_own_tree() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-e2e");
    let scratch = Scratch::new("meta");
    let server = spawn(&bucket.base_url, &scratch, "meta");
    let token = server.bootstrap_org("acme");
    seed_project(&server, &token, "acme", "widget");

    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/widget/meta", &token, None);
    assert_eq!(st, 200, "{meta}");

    // Languages, by bytes, biggest first — and *only* languages. The
    // 9 KB of Markdown and 5 KB of TOML outweigh every line of source
    // here, so a bar that counted them would put "Markdown" first and
    // this assertion is what stops that shipping.
    let langs: Vec<(&str, u64)> = meta["languages"]
        .as_array()
        .expect("languages")
        .iter()
        .map(|l| (l["name"].as_str().unwrap(), l["bytes"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        langs,
        vec![("Rust", 300), ("TypeScript", 120), ("Python", 40)],
        "{meta}"
    );
    assert_eq!(meta["languages_truncated"], false);

    // The licence is named, and the path is there so a reader can go
    // and check our answer rather than take it.
    assert_eq!(meta["license"]["spdx"], "MIT", "{meta}");
    assert_eq!(meta["license"]["name"], "MIT License");
    assert_eq!(meta["license"]["path"], "LICENSE");
    assert_eq!(meta["license"]["recognised"], true);

    // Community files, from the root and from `.github/`.
    let community: Vec<(&str, &str)> = meta["community"]
        .as_array()
        .expect("community")
        .iter()
        .map(|c| (c["kind"].as_str().unwrap(), c["path"].as_str().unwrap()))
        .collect();
    assert_eq!(
        community,
        vec![
            ("contributing", "CONTRIBUTING.md"),
            ("security", ".github/SECURITY.md"),
        ],
        "{meta}"
    );
    // Not found, and absent rather than a null placeholder: the rail
    // renders what is there, and a row for a file nobody wrote is
    // furniture.
    assert!(
        !community.iter().any(|(k, _)| *k == "code_of_conduct"),
        "{meta}"
    );

    // The README, by path. The client renders the file, so it needs
    // somewhere to fetch it from; before this it fetched the literal
    // name `README.md`, which is a naming rule living on the wrong side
    // of the wire.
    assert_eq!(meta["readme"], "README.md", "{meta}");

    // No topics until somebody sets one, and an empty list rather than
    // a null: "this project has chosen no topics" is a fact.
    assert_eq!(meta["topics"], serde_json::json!([]));
}

/// An empty repository is not an error, and neither is one with nothing
/// this module recognises. Both are the state a repository is in one
/// minute after it is created, and answering 500 there would make the
/// page look broken on the most common day of a project's life.
#[test]
fn an_empty_repository_answers_emptily() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-empty");
    let scratch = Scratch::new("meta-empty");
    let server = spawn(&bucket.base_url, &scratch, "meta-empty");
    let token = server.bootstrap_org("acme");
    create(&server, &token, "acme", "fresh");

    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/fresh/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(meta["languages"], serde_json::json!([]));
    assert!(meta["license"].is_null(), "{meta}");
    assert_eq!(meta["community"], serde_json::json!([]));
    assert!(meta["readme"].is_null(), "{meta}");
    assert_eq!(meta["topics"], serde_json::json!([]));

    // And a repository with content but nothing recognisable in it.
    create(&server, &token, "acme", "prose");
    commit(
        &server,
        &token,
        "acme",
        "prose",
        serde_json::json!([{ "op": "put", "path": "notes.txt", "content": "hello\n" }]),
    );
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/prose/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(meta["languages"], serde_json::json!([]), "{meta}");
    assert!(meta["license"].is_null(), "{meta}");
    assert!(meta["readme"].is_null(), "{meta}");
}

/// The README is reported by path, and its absence is reported as
/// `null` — never as an empty string.
///
/// The About rail renders all six health rows present-or-absent, so
/// "there is no README" is a fact the client has to be able to *read*,
/// not something it infers from a fetch of a guessed filename coming
/// back 404. That is the difference between a row that says "None" and
/// a row that says nothing while the page waits.
///
/// The empty-string case has a test of its own because it is the shape
/// this gets wrong by accident: a `String::new()` default serialises to
/// `""`, the client's truthiness check reads it as absent by luck, and
/// the first client that links to `meta.readme` links to a file named
/// nothing.
#[test]
fn a_readme_is_reported_by_path_and_its_absence_is_null() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-readme");
    let scratch = Scratch::new("meta-readme");
    let server = spawn(&bucket.base_url, &scratch, "meta-readme");
    let token = server.bootstrap_org("acme");

    let readme = |repo: &str| -> serde_json::Value {
        let (st, meta) = server.req(
            "GET",
            &format!("/v1/orgs/acme/repos/{repo}/meta"),
            &token,
            None,
        );
        assert_eq!(st, 200, "{meta}");
        meta["readme"].clone()
    };

    // A project with everything *but* a README. This is the case that
    // must not be indistinguishable from one that has one.
    create(&server, &token, "acme", "bare");
    commit(
        &server,
        &token,
        "acme",
        "bare",
        serde_json::json!([
            { "op": "put", "path": "LICENSE", "content": MIT },
            { "op": "put", "path": "CONTRIBUTING.md", "content": "send patches\n" },
            { "op": "put", "path": "src/main.rs", "content": "fn main() {}\n" },
        ]),
    );
    let none = readme("bare");
    assert!(none.is_null(), "a missing README must be null: {none}");
    assert!(
        !none.is_string(),
        "absent and \"a file named nothing\" must not be the same value: {none}"
    );

    // Not a Markdown rule. A `.rst` README is a README, because the
    // naming rule is the server's business — the client is handed a
    // path and never has to know which extensions count.
    create(&server, &token, "acme", "rst");
    commit(
        &server,
        &token,
        "acme",
        "rst",
        serde_json::json!([{ "op": "put", "path": "README.rst", "content": "widget\n======\n" }]),
    );
    assert_eq!(readme("rst"), "README.rst");

    // And an extensionless one, which is what a project that predates
    // Markdown looks like.
    create(&server, &token, "acme", "plain");
    commit(
        &server,
        &token,
        "acme",
        "plain",
        serde_json::json!([{ "op": "put", "path": "README", "content": "widget\n" }]),
    );
    assert_eq!(readme("plain"), "README");

    // `.github/` is where a project puts the files it does not want at
    // the root, and a README there is still this project's README.
    create(&server, &token, "acme", "hidden");
    commit(
        &server,
        &token,
        "acme",
        "hidden",
        serde_json::json!([{ "op": "put", "path": ".github/README.md", "content": "# widget\n" }]),
    );
    assert_eq!(readme("hidden"), ".github/README.md");

    // Precedence: with both, the root wins. The same rule the community
    // files follow, and it falls out of the walk order rather than out
    // of a tie-break — but a rule nothing exercises is a rule nobody
    // knows still holds.
    create(&server, &token, "acme", "both");
    commit(
        &server,
        &token,
        "acme",
        "both",
        serde_json::json!([
            { "op": "put", "path": ".github/README.md", "content": "# meta\n" },
            { "op": "put", "path": "README.md", "content": "# widget\n" },
        ]),
    );
    assert_eq!(readme("both"), "README.md");

    // A README nine directories down belongs to that directory. The
    // same rule that keeps `vendor/foo/LICENSE` from being read as this
    // project's licence: hoisting it would attribute somebody else's
    // front page to this repository.
    create(&server, &token, "acme", "nested");
    commit(
        &server,
        &token,
        "acme",
        "nested",
        serde_json::json!([
            { "op": "put", "path": "docs/README.md", "content": "# docs\n" },
            { "op": "put", "path": "vendor/foo/README.md", "content": "# foo\n" },
        ]),
    );
    let nested = readme("nested");
    assert!(
        nested.is_null(),
        "a nested README must not be hoisted onto the front page: {nested}"
    );

    // A README is not a community `kind`. The client derives the other
    // rows from that closed set and reads this one from its own field;
    // reporting it in both places would give it two sources of truth
    // that could disagree.
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/hidden/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(meta["community"], serde_json::json!([]), "{meta}");
}

/// **A wrong licence is worse than no licence.** Three shapes that a
/// naive detector gets wrong, against a real server.
#[test]
fn an_unrecognised_licence_is_said_to_be_unrecognised() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-lic");
    let scratch = Scratch::new("meta-lic");
    let server = spawn(&bucket.base_url, &scratch, "meta-lic");
    let token = server.bootstrap_org("acme");

    // A LICENSE holding only the licence's *name*. This is common —
    // plenty of repositories ship exactly this — and it is the case a
    // substring match on "MIT" reports as MIT.
    create(&server, &token, "acme", "named");
    commit(
        &server,
        &token,
        "acme",
        "named",
        serde_json::json!([{ "op": "put", "path": "LICENSE", "content": "MIT\n" }]),
    );
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/named/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(meta["license"]["recognised"], false, "{meta}");
    assert!(meta["license"]["spdx"].is_null(), "{meta}");
    // The path is still there: "we could not identify this, here it is"
    // is a useful answer and a blank is not.
    assert_eq!(meta["license"]["path"], "LICENSE");

    // Dual-licensed, in the shape the whole Rust ecosystem uses.
    //
    // Two things must both be true, and the first version of this got
    // the second one wrong: it must not name one of the two, *and* it
    // must not go silent. `rust-lang/rust` rendered no licence row at
    // all until a browser pass looked at the real page — a
    // dual-licensed project reading as unlicensed, which is exactly the
    // misreading the feature exists to prevent.
    create(&server, &token, "acme", "dual");
    commit(
        &server,
        &token,
        "acme",
        "dual",
        serde_json::json!([
            { "op": "put", "path": "LICENSE-APACHE", "content": APACHE },
            { "op": "put", "path": "LICENSE-MIT", "content": MIT },
        ]),
    );
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/dual/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert!(
        !meta["license"].is_null(),
        "a dual-licensed project must not read as unlicensed: {meta}"
    );
    assert!(meta["license"]["spdx"].is_null(), "{meta}");
    assert_eq!(meta["license"]["recognised"], false);
    // No single path, because there is no single file to link to — the
    // list is what a reader needs.
    assert!(meta["license"]["path"].is_null(), "{meta}");
    let mut files: Vec<&str> = meta["license"]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    files.sort();
    assert_eq!(files, ["LICENSE-APACHE", "LICENSE-MIT"], "{meta}");

    // And one licence file still carries a list of one, so the client
    // has one shape to render rather than two.
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/named/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(
        meta["license"]["files"],
        serde_json::json!(["LICENSE"]),
        "{meta}"
    );

    // A licence somewhere other than the root belongs to that
    // directory, not to the project — the same rule that keeps a
    // `docs/README.md` from being hoisted onto the front page.
    create(&server, &token, "acme", "vendored");
    commit(
        &server,
        &token,
        "acme",
        "vendored",
        serde_json::json!([{ "op": "put", "path": "vendor/foo/LICENSE", "content": MIT }]),
    );
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/vendored/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert!(meta["license"].is_null(), "{meta}");
}

#[test]
fn topics_are_lowercased_at_the_write_and_replaced_whole() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-topics");
    let scratch = Scratch::new("meta-topics");
    let server = spawn(&bucket.base_url, &scratch, "meta-topics");
    let token = server.bootstrap_org("acme");
    create(&server, &token, "acme", "widget");
    let path = "/v1/orgs/acme/repos/widget/topics";

    // Sent with capitals. The column's CHECK refuses a non-lowercase
    // write, so a handler that passed this through would 500 — which is
    // why this test sends `Rust` rather than `rust`.
    let (st, out) = server.req(
        "PUT",
        path,
        &token,
        Some(serde_json::json!({ "topics": ["Rust", "Object-Storage", "GIT"] })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["topics"],
        serde_json::json!(["git", "object-storage", "rust"]),
        "{out}"
    );

    // And the read side agrees, which is the only thing the page will
    // ever look at.
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/widget/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(
        meta["topics"],
        serde_json::json!(["git", "object-storage", "rust"])
    );

    // Replaced whole, not merged.
    let (st, out) = server.req(
        "PUT",
        path,
        &token,
        Some(serde_json::json!({ "topics": ["rust"] })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["topics"], serde_json::json!(["rust"]));

    // An empty list clears them — how a form removes the last pill.
    let (st, out) = server.req(
        "PUT",
        path,
        &token,
        Some(serde_json::json!({ "topics": [] })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["topics"], serde_json::json!([]));
}

/// Bounded input, refused rather than truncated (I13).
///
/// Each of these is a 400 the caller can act on, not a 500 about our
/// schema and not a silently shortened list. The last assertion is the
/// one that matters most: after every refusal the stored set is
/// untouched, so a bad edit cannot cost somebody the topics they had.
#[test]
fn a_hostile_topic_is_refused_and_the_stored_set_survives_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-bounds");
    let scratch = Scratch::new("meta-bounds");
    let server = spawn(&bucket.base_url, &scratch, "meta-bounds");
    let token = server.bootstrap_org("acme");
    create(&server, &token, "acme", "widget");
    let path = "/v1/orgs/acme/repos/widget/topics";

    let (st, _) = server.req(
        "PUT",
        path,
        &token,
        Some(serde_json::json!({ "topics": ["rust", "storage"] })),
    );
    assert_eq!(st, 200);

    let long = "a".repeat(36);
    let many: Vec<String> = (0..21).map(|i| format!("t{i}")).collect();
    let refused: Vec<serde_json::Value> = vec![
        serde_json::json!(["two words"]),
        serde_json::json!(["rust/lang"]),
        serde_json::json!(["-flag"]),
        serde_json::json!(["trailing-"]),
        serde_json::json!([""]),
        serde_json::json!(["日本語"]),
        serde_json::json!(["rust\u{0}lang"]),
        serde_json::json!(["'; DROP TABLE repo_topics --"]),
        serde_json::json!([long]),
        serde_json::json!(many),
    ];
    for topics in refused {
        let (st, body) = server.req(
            "PUT",
            path,
            &token,
            Some(serde_json::json!({ "topics": topics })),
        );
        assert_eq!(st, 400, "{topics} was not refused: {body}");
        assert!(body["error"].is_string(), "{body}");
    }

    // Nothing a refusal did reached the table.
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/widget/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(meta["topics"], serde_json::json!(["rust", "storage"]));

    // And the server is still serving.
    assert_eq!(server.status_get("/healthz", None), 200);
}

/// **The bound is honest when it is hit.**
///
/// A walk that stopped early and reported its partial proportions as if
/// they were the whole tree would be a bar that quietly lies about big
/// repositories — exactly the failure mode that never shows up in
/// testing, because nobody builds a twenty-thousand-file fixture. So
/// the budget is configurable, this spawns a server with a tiny one,
/// and the answer has to say `languages_truncated`.
///
/// Both bounds get their own server: the entry budget and the depth
/// cap reach the same field by different paths, and a test that only
/// exercised one would leave the other's branch unexecuted.
#[test]
fn a_walk_that_hits_its_bound_says_so() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-bound");
    let scratch = Scratch::new("meta-bound");
    let server = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("meta-bound")
        .data_dir(scratch.path().join("data"))
        .envs(&[
            ("STRATUM_META_WALK_BUDGET", "2".to_string()),
            // A zero is floored to one rather than reporting every
            // repository as empty-and-truncated.
            ("STRATUM_META_WALK_DEPTH", "0".to_string()),
        ])
        .start();
    let token = server.bootstrap_org("acme");
    create(&server, &token, "acme", "big");
    commit(
        &server,
        &token,
        "acme",
        "big",
        serde_json::json!([
            { "op": "put", "path": "a.rs", "content": "a" },
            { "op": "put", "path": "b.rs", "content": "b" },
            { "op": "put", "path": "c.rs", "content": "c" },
            { "op": "put", "path": "d.rs", "content": "d" },
        ]),
    );
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/big/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(
        meta["languages_truncated"], true,
        "the walk stopped early and did not say so: {meta}"
    );

    // A repository small enough to finish under the same bound is *not*
    // reported as truncated — otherwise this would pass against a
    // server that flagged everything.
    create(&server, &token, "acme", "small");
    commit(
        &server,
        &token,
        "acme",
        "small",
        serde_json::json!([{ "op": "put", "path": "a.rs", "content": "a" }]),
    );
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/small/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(meta["languages_truncated"], false, "{meta}");

    // And the depth cap, reached through a subdirectory. With a cap of
    // one, the root is walked and nothing below it is — so the nested
    // file is uncounted and the answer says the bar is partial rather
    // than reporting a repository with no Rust in it.
    create(&server, &token, "acme", "deep");
    commit(
        &server,
        &token,
        "acme",
        "deep",
        serde_json::json!([{ "op": "put", "path": "src/main.rs", "content": "fn main() {}\n" }]),
    );
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/deep/meta", &token, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(meta["languages"], serde_json::json!([]), "{meta}");
    assert_eq!(
        meta["languages_truncated"], true,
        "a subtree the walk refused to enter must be admitted to: {meta}"
    );
}

/// **The walk is not cheap, so a revisit does not pay for it.**
///
/// The panel is a pure function of the commit at HEAD and the topic
/// rows, so the ETag is those two hashed — exact, not heuristic. This
/// matters because the walk reads every blob it counts (a blob's length
/// is only knowable by inflating it) and the repository page is the
/// most visited page in the product.
///
/// What is asserted is that the ETag *moves when the answer moves*, in
/// both directions. An ETag that never changed would also produce 304s
/// and would serve a stale licence forever, which is the failure this
/// test exists to make impossible — so both halves of the tag get their
/// own push: a commit, and a topic edit that touches no commit at all.
#[test]
fn an_unchanged_panel_is_answered_without_walking_the_tree_again() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-etag");
    let scratch = Scratch::new("meta-etag");
    let server = spawn(&bucket.base_url, &scratch, "meta-etag");
    let token = server.bootstrap_org("acme");
    seed_project(&server, &token, "acme", "widget");
    let path = "/v1/orgs/acme/repos/widget/meta";

    /// GET with an `If-None-Match`, without following anything.
    fn conditional(server: &Server, path: &str, token: &str, etag: &str) -> u16 {
        let resp = ureq::get(&format!("{}{path}", server.base))
            .set("Authorization", &format!("Bearer {token}"))
            .set("If-None-Match", etag)
            .call();
        match resp {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(s, _)) => s,
            Err(e) => panic!("{e}"),
        }
    }

    fn tag(server: &Server, path: &str, token: &str) -> String {
        let (st, body, headers) = server.req_full("GET", path, token, None);
        assert_eq!(st, 200, "{body}");
        // Revalidate every time rather than cache for a while: a stale
        // language bar is harmless and a stale *licence* is not.
        assert_eq!(
            headers.get("cache-control").map(String::as_str),
            Some("private, no-cache"),
            "{headers:?}"
        );
        headers
            .get("etag")
            .cloned()
            .unwrap_or_else(|| panic!("no ETag: {headers:?}"))
    }

    let first = tag(&server, path, &token);
    assert!(first.starts_with('"'), "{first}");

    // Unchanged: 304, and the walk never ran.
    assert_eq!(conditional(&server, path, &token, &first), 304);
    // A tag from somewhere else is not honoured.
    assert_eq!(
        conditional(
            &server,
            path,
            &token,
            "\"0000000000000000000000000000000000000000\""
        ),
        200
    );

    // A push changes the tree, so it must change the tag.
    commit(
        &server,
        &token,
        "acme",
        "widget",
        serde_json::json!([{ "op": "put", "path": "extra.go", "content": "package main\n" }]),
    );
    assert_eq!(
        conditional(&server, path, &token, &first),
        200,
        "a push must not be served from a stale tag"
    );
    let after_push = tag(&server, path, &token);
    assert_ne!(after_push, first);

    // And so must a topic edit, which touches no commit at all — the
    // half a commit-only tag would get wrong, and would leave somebody
    // looking at the pills they just deleted.
    let (st, out) = server.req(
        "PUT",
        "/v1/orgs/acme/repos/widget/topics",
        &token,
        Some(serde_json::json!({ "topics": ["rust"] })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        conditional(&server, path, &token, &after_push),
        200,
        "a topic edit changes the panel and must change the tag"
    );

    assert!(server.healthy());
}

/// Setting a topic is a write, and a read credential is not one.
#[test]
fn topics_need_write_access() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-write");
    let scratch = Scratch::new("meta-write");
    let server = spawn(&bucket.base_url, &scratch, "meta-write");
    let admin = server.bootstrap_org("acme");
    create(&server, &admin, "acme", "widget");

    let (st, minted) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:read"], "repo": "widget" })),
    );
    assert_eq!(st, 201, "{minted}");
    let reader = minted["token"].as_str().unwrap().to_string();

    // It may read the panel — that is what repo:read is.
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/widget/meta", &reader, None);
    assert_eq!(st, 200, "{meta}");

    // It may not write one. 404 rather than 403: a credential without
    // the right learns nothing about what it was refused.
    let (st, body) = server.req(
        "PUT",
        "/v1/orgs/acme/repos/widget/topics",
        &reader,
        Some(serde_json::json!({ "topics": ["rust"] })),
    );
    assert_eq!(st, 404, "{body}");

    // Nothing was written.
    let (st, meta) = server.req("GET", "/v1/orgs/acme/repos/widget/meta", &admin, None);
    assert_eq!(st, 200, "{meta}");
    assert_eq!(meta["topics"], serde_json::json!([]));
}

/// **A private repository's topics, languages and licence are masked
/// exactly as its code is.**
///
/// The two callers that matter are the anonymous one and the one with a
/// perfectly good credential for somewhere else. Both must get the same
/// answer they would get for a repository that was never created, or
/// the URL bar becomes an oracle for which private repositories a
/// namespace holds. Nothing here reimplements that rule — the routes go
/// through `app::rest_repo_auth`, and this is what proves they do.
#[test]
fn a_private_repositorys_panel_is_masked_from_everyone_who_may_not_read_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-mask");
    let scratch = Scratch::new("meta-mask");
    let server = spawn(&bucket.base_url, &scratch, "meta-mask");
    let acme = server.bootstrap_org("acme");
    let other = server.bootstrap_org("other");

    create(&server, &acme, "acme", "secret");
    commit(
        &server,
        &acme,
        "acme",
        "secret",
        serde_json::json!([
            { "op": "put", "path": "LICENSE", "content": MIT },
            { "op": "put", "path": "payments.rs", "content": "fn charge() {}\n" },
        ]),
    );
    let (st, out) = server.req(
        "PUT",
        "/v1/orgs/acme/repos/secret/topics",
        &acme,
        Some(serde_json::json!({ "topics": ["payments"] })),
    );
    assert_eq!(st, 200, "{out}");

    let meta = "/v1/orgs/acme/repos/secret/meta";
    let topics = "/v1/orgs/acme/repos/secret/topics";
    // A repository that does not exist, for comparison. The two answers
    // must be indistinguishable — that is the whole property.
    let ghost = "/v1/orgs/acme/repos/no-such-repo/meta";

    // Anonymous. 401, the same as for the ghost: no credential at all
    // is answered with "send one", never with a status that separates
    // "private" from "absent".
    let (anon_st, anon_body) = server.req("GET", meta, "", None);
    let (ghost_st, _) = server.req("GET", ghost, "", None);
    assert_eq!(anon_st, 401, "anonymous: {anon_body}");
    assert_eq!(anon_st, ghost_st, "anonymous: {anon_body}");
    assert!(
        !anon_body.to_string().contains("payments"),
        "the panel leaked to a stranger: {anon_body}"
    );
    assert!(!anon_body.to_string().contains("MIT"), "{anon_body}");

    // A real credential, for the wrong namespace. 404 — masked, not
    // 403, which would confirm the repository exists.
    let (st, body) = server.req("GET", meta, &other, None);
    assert_eq!(st, 404, "{body}");
    let (ghost_st, _) = server.req("GET", ghost, &other, None);
    assert_eq!(
        st, ghost_st,
        "a private repo answered differently from an absent one"
    );
    assert!(!body.to_string().contains("payments"), "{body}");
    assert!(!body.to_string().contains("Rust"), "{body}");

    // And the write door, from both.
    for (who, tok, expect) in [("anonymous", "", 401), ("foreign", other.as_str(), 404)] {
        let (st, body) = server.req(
            "PUT",
            topics,
            tok,
            Some(serde_json::json!({ "topics": ["stolen"] })),
        );
        assert_eq!(st, expect, "{who} write: {body}");
    }
    // The owner's topics are untouched by any of that.
    let (st, mine) = server.req("GET", meta, &acme, None);
    assert_eq!(st, 200, "{mine}");
    assert_eq!(mine["topics"], serde_json::json!(["payments"]));

    // Somebody the organisation *has* let read it gets the whole panel,
    // on the weakest credential there is — a read-only token — so this
    // test would not pass against a server that had simply stopped
    // serving the endpoint, or served it to its owner alone.
    let (st, minted) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &acme,
        Some(serde_json::json!({ "scopes": ["repo:read"], "label": "reader" })),
    );
    assert_eq!(st, 201, "{minted}");
    let reader = minted["token"].as_str().unwrap().to_string();
    let (st, read) = server.req("GET", meta, &reader, None);
    assert_eq!(st, 200, "a reader of the org was refused the panel: {read}");
    assert_eq!(read["languages"][0]["name"], "Rust", "{read}");
    assert_eq!(read["topics"], serde_json::json!(["payments"]), "{read}");
    // Reading is not writing, even inside the org.
    let (st, body) = server.req(
        "PUT",
        topics,
        &reader,
        Some(serde_json::json!({ "topics": ["stolen"] })),
    );
    assert_eq!(st, 404, "a reader wrote topics: {body}");

    // The server survived every refusal and is still serving.
    assert_eq!(server.status_get("/healthz", None), 200);
    assert_eq!(server.status_get("/readyz", None), 200);
}

/// **What the panel actually costs, as store round trips.**
///
/// The claim the ETag rests on is that the walk costs something and a
/// revalidation costs less. That is worth a number rather than a
/// paragraph, and measuring it corrected the assumption it was written
/// on: the panel costs **three** store round trips for this fixture,
/// not one per file, because `LayoutReader` fetches segments rather
/// than objects and the blobs come out of packs already in hand. The
/// walk's real expense is decompression, which is CPU and is what
/// `WALK_BUDGET` bounds.
///
/// So the assertion with teeth is the strict inequality, not the
/// absolute budget: a 304 must cost *fewer* round trips than a full
/// answer, which is false the moment the ETag is computed after the
/// walk instead of before it. The absolute budget is a loose regression
/// gate on the shape — a second reader, or a history walk creeping in,
/// would break it; MinIO's chunking will not.
#[test]
fn a_revalidated_panel_costs_strictly_fewer_store_round_trips() {
    let minio = Minio::shared();
    let bucket = minio.bucket("meta-rtt");
    let scratch = Scratch::new("meta-rtt");
    // `host:port`, with no scheme and no bucket — the shape
    // `CountingProxy::start` expects.
    let upstream = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let proxy = CountingProxy::start(&upstream);
    let bucket_name = bucket.base_url.rsplit('/').next().unwrap();
    // Background workers quiesced, so a bracketed count sees only the
    // request under test.
    let server = Server::builder(
        env!("CARGO_BIN_EXE_stratum-server"),
        &format!("{}/{bucket_name}", proxy.url),
    )
    .db_hint("meta-rtt")
    .data_dir(scratch.path().join("data"))
    .envs(&[
        ("STRATUM_COMPACT_POLL_SECS", "86400".to_string()),
        ("STRATUM_AUDIT_SHIP_SECS", "86400".to_string()),
        ("STRATUM_USAGE_ROLLUP_SECS", "86400".to_string()),
        ("STRATUM_STORAGE_SWEEP_SECS", "0".to_string()),
        // The count below is what *one request* costs the store. The
        // process read cache makes that depend on who read the objects
        // first — the contribution walker runs behind the seed's commits
        // and, when it wins the race, the full answer is served from
        // memory at the 304's price, and the strict inequality this test
        // is about reads as broken when it is not. A cache that remembers
        // nothing makes the request's cost the request's again.
        ("STRATUM_READ_CACHE_MB", "0".to_string()),
    ])
    .start();
    let token = server.bootstrap_org("acme");
    seed_project(&server, &token, "acme", "widget");
    let path = "/v1/orgs/acme/repos/widget/meta";

    let before = proxy.count();
    let (st, _, headers) = server.req_full("GET", path, &token, None);
    assert_eq!(st, 200);
    let full = proxy.since(before);
    let etag = headers.get("etag").cloned().expect("etag");

    let before = proxy.count();
    let resp = ureq::get(&format!("{}{path}", server.base))
        .set("Authorization", &format!("Bearer {token}"))
        .set("If-None-Match", &etag)
        .call();
    let status = match resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(s, _)) => s,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(status, 304);
    let revalidated = proxy.since(before);

    assert!(
        revalidated < full,
        "a 304 cost {revalidated} store ops and the full answer cost {full}; \
         the ETag is being computed after the walk rather than before it"
    );
    // The shape, loosely: a full answer reads the manifest, the layout
    // and one object per counted blob. Ten files is comfortably inside
    // this; a regression that started reading the whole history, or
    // opening a second reader, would not be.
    assert!(full <= 60, "the panel cost {full} store ops (budget 60)");
    assert!(server.healthy());
}
