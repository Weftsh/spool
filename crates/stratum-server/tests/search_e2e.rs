//! Finding a repository, and the two ways that goes wrong.
//!
//! Search is the first route that crosses namespaces, so it is the first
//! place where a visibility bug leaks the *existence* of private work
//! rather than its contents. Half of this file is therefore negative:
//! every way of asking "is there something called X in that namespace?"
//! has to come back the same whether or not there is.
//!
//! The other half is the existence oracle this increment closes.
//! Anonymous REST used to answer 404 for a repository that does not
//! exist and 401 for one that is private, which is the same enumeration
//! by another name — the git wire has always answered 401 to both.

use stratum_testkit::adversarial::INJECTIONS;
use stratum_testkit::{browser::Browser, gitcli::Scratch, Minio, Server};

fn spawn(store_url: &str, scratch: &Scratch, hint: &str) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .start()
}

fn make_repo(server: &Server, org: &str, token: &str, name: &str, public: bool, desc: &str) {
    let (st, out) = server.req(
        "POST",
        &format!("/v1/orgs/{org}/repos"),
        token,
        Some(serde_json::json!({ "name": name, "description": desc })),
    );
    assert_eq!(st, 201, "create {org}/{name}: {out}");
}

/// The names in a search response, in the order it returned them.
fn names(body: &serde_json::Value) -> Vec<String> {
    body["repos"]
        .as_array()
        .expect("repos array")
        .iter()
        .map(|r| {
            format!(
                "{}/{}",
                r["org"].as_str().unwrap(),
                r["name"].as_str().unwrap()
            )
        })
        .collect()
}

/// The happy path, end to end: make repos, find them by three different
/// words, page through, and follow a result to the repo it names.
#[test]
fn search_finds_public_repos_by_name_namespace_and_description() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-happy");
    let scratch = Scratch::new("search-happy");
    let server = spawn(&bucket.base_url, &scratch, "search-happy");
    let admin = server.bootstrap_org("acme");

    make_repo(&server, "acme", &admin, "widget", true, "the fast one");
    make_repo(&server, "acme", &admin, "ledger", true, "money, counted");
    make_repo(&server, "acme", &admin, "secret", false, "not for you");

    // By name.
    let (st, out) = server.req("GET", "/v1/search/repos?q=widg", "", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), ["acme/widget"]);
    // By description.
    let (st, out) = server.req("GET", "/v1/search/repos?q=counted", "", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), ["acme/ledger"]);
    // By namespace.
    let (st, out) = server.req("GET", "/v1/search/repos?q=acme", "", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), ["acme/ledger", "acme/widget"]);
    // An empty query is the discovery page browsing everything public.
    let (st, out) = server.req("GET", "/v1/search/repos", "", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), ["acme/ledger", "acme/widget"]);
    assert!(out["next"].is_null(), "one page, so no cursor: {out}");
    // The private repo is absent from every one of those.
    let (_, out) = server.req("GET", "/v1/search/repos?q=secret", "", None);
    assert_eq!(names(&out), [] as [String; 0]);

    // Paging: one at a time, following `next`, visits each repo once.
    let mut seen: Vec<String> = Vec::new();
    let mut url = "/v1/search/repos?limit=1".to_string();
    for _ in 0..10 {
        let (st, out) = server.req("GET", &url, "", None);
        assert_eq!(st, 200, "{out}");
        seen.extend(names(&out));
        let Some(next) = out["next"].as_str() else {
            break;
        };
        url = format!("/v1/search/repos?limit=1&after={}", urlencode(next));
    }
    assert_eq!(seen, ["acme/ledger", "acme/widget"]);

    // A hit is enough to go and read the repo with.
    let hit = server.req("GET", "/v1/search/repos?q=widget", "", None).1;
    let org = hit["repos"][0]["org"].as_str().unwrap().to_string();
    let name = hit["repos"][0]["name"].as_str().unwrap().to_string();
    let (st, out) = server.req("GET", &format!("/v1/orgs/{org}/repos/{name}"), "", None);
    assert_eq!(st, 200, "a public hit is readable anonymously: {out}");
    assert_eq!(out["description"], "the fast one");

    assert!(server.healthy());
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Who sees what. The assertion that matters is that a member sees the
/// private repo — without it, a filter that returned nothing at all
/// would pass every "it is hidden" check in this file.
#[test]
fn a_private_repo_is_invisible_to_everyone_outside_its_namespace() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-visibility");
    let scratch = Scratch::new("search-visibility");
    let server = spawn(&bucket.base_url, &scratch, "search-visibility");
    let acme = server.bootstrap_org("acme");
    let rival = server.bootstrap_org("rival");

    make_repo(&server, "acme", &acme, "payments", false, "the private one");
    make_repo(&server, "rival", &rival, "public-thing", true, "theirs");

    server
        .admin(&[
            "admin",
            "user-create",
            "--email",
            "ada@acme.test",
            "--name",
            "Ada",
            "--password",
            "a long enough password",
            "--org",
            "acme",
            "--role",
            "owner",
        ])
        .unwrap();
    server
        .admin(&[
            "admin",
            "user-create",
            "--email",
            "eve@rival.test",
            "--name",
            "Eve",
            "--password",
            "a long enough password",
            "--org",
            "rival",
            "--role",
            "owner",
        ])
        .unwrap();

    // Anonymous.
    let (_, out) = server.req("GET", "/v1/search/repos?q=payments", "", None);
    assert_eq!(names(&out), [] as [String; 0], "anonymous: {out}");
    // A member of another org — signed in, and no better off.
    let mut eve = Browser::signed_in(&server, "eve@rival.test", "a long enough password");
    let (st, out) = eve.req("GET", "/v1/search/repos?q=payments", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), [] as [String; 0], "other-org member: {out}");
    // Their own org's token: same answer.
    let (_, out) = server.req("GET", "/v1/search/repos?q=payments", &rival, None);
    assert_eq!(names(&out), [] as [String; 0], "foreign token: {out}");

    // A member of acme does see it — the filter is a filter, not a wall.
    let mut ada = Browser::signed_in(&server, "ada@acme.test", "a long enough password");
    let (st, out) = ada.req("GET", "/v1/search/repos?q=payments", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), ["acme/payments"]);

    // Removed from the org, and it is gone on the very next request —
    // there is no cached authority to outlive the membership.
    let (st, out) = server.req("GET", "/v1/orgs/acme/members", &acme, None);
    assert_eq!(st, 200, "{out}");
    let ada_id = out["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == "ada@acme.test")
        .expect("ada is a member")["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    // An owner cannot be removed while they are the last one, so add a
    // second owner first.
    server
        .admin(&[
            "admin",
            "user-create",
            "--email",
            "bo@acme.test",
            "--name",
            "Bo",
            "--password",
            "a long enough password",
            "--org",
            "acme",
            "--role",
            "owner",
        ])
        .unwrap();
    let (st, out) = server.req(
        "DELETE",
        &format!("/v1/orgs/acme/members/{ada_id}"),
        &acme,
        None,
    );
    assert_eq!(st, 204, "remove ada: {out}");
    let (st, out) = ada.req("GET", "/v1/search/repos?q=payments", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        names(&out),
        [] as [String; 0],
        "an ex-member sees what a stranger sees: {out}"
    );

    // A disabled account's session stops counting too.
    let mut bo = Browser::signed_in(&server, "bo@acme.test", "a long enough password");
    assert_eq!(
        names(&bo.req("GET", "/v1/search/repos?q=payments", None).1),
        ["acme/payments"]
    );
    server
        .admin(&["admin", "user-disable", "--email", "bo@acme.test"])
        .unwrap();
    let (st, out) = bo.req("GET", "/v1/search/repos?q=payments", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), [] as [String; 0], "disabled: {out}");

    assert!(server.healthy());
}

/// A cursor is a window into what you could already see, and tampering
/// with it moves the window, never widens it.
#[test]
fn a_forged_cursor_cannot_page_into_someone_elses_repos() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-cursor");
    let scratch = Scratch::new("search-cursor");
    let server = spawn(&bucket.base_url, &scratch, "search-cursor");
    let acme = server.bootstrap_org("acme");
    make_repo(&server, "acme", &acme, "aaa-public", true, "first");
    make_repo(&server, "acme", &acme, "bbb-private", false, "second");
    make_repo(&server, "acme", &acme, "ccc-public", true, "third");

    // Hand-built cursors that land either side of the private repo.
    for forged in [
        "acme/bbb-private/0",
        "acme/bbb-privatd/zzzzzzzzzzzzzzzzzzzzzzzzzz",
        "acme/aaa-public/zzzzzzzzzzzzzzzzzzzzzzzzzz",
        "%00/%00/%00",
        "../../etc/passwd",
        "a/b/c'; DROP TABLE repos;--",
    ] {
        let (st, out) = server.req(
            "GET",
            &format!("/v1/search/repos?after={}", urlencode(forged)),
            "",
            None,
        );
        assert_eq!(st, 200, "cursor {forged:?}: {out}");
        for name in names(&out) {
            assert_ne!(name, "acme/bbb-private", "cursor {forged:?} leaked: {out}");
        }
    }
    assert!(server.healthy(), "still serving after forged cursors");
}

/// The query string is data. Every entry in the shared corpus goes in,
/// and the server answers a search — never a 500 with a fragment of the
/// database layer in it.
#[test]
fn injections_in_the_query_are_matched_as_text() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-inject");
    let scratch = Scratch::new("search-inject");
    let server = spawn(&bucket.base_url, &scratch, "search-inject");
    let acme = server.bootstrap_org("acme");
    make_repo(&server, "acme", &acme, "widget", true, "ordinary");

    for probe in INJECTIONS {
        let (st, out) = server.req(
            "GET",
            &format!("/v1/search/repos?q={}", urlencode(probe)),
            "",
            None,
        );
        assert!(st == 200 || st == 400, "q={probe:?} answered {st}: {out}");
        if st == 200 {
            // Nothing is named after an injection, so nothing matches —
            // and in particular the whole table does not come back.
            assert_eq!(names(&out), [] as [String; 0], "q={probe:?}: {out}");
        }
    }

    // Overlong and unicode queries are refused or answered, never a 500.
    for q in [
        "a".repeat(129),
        "é".repeat(129),
        "🙂".repeat(200),
        "\u{202e}reversed".to_string(),
    ] {
        let (st, out) = server.req(
            "GET",
            &format!("/v1/search/repos?q={}", urlencode(&q)),
            "",
            None,
        );
        assert!(
            st == 200 || st == 400,
            "len {}: {st} {out}",
            q.chars().count()
        );
    }
    // The repo is still findable afterwards: the server did not wedge.
    let (_, out) = server.req("GET", "/v1/search/repos?q=widget", "", None);
    assert_eq!(names(&out), ["acme/widget"]);
    assert!(server.healthy());
}

/// Which credential is asking changes only how much comes back — and
/// two kinds of credential are deliberately worth no more than none.
#[test]
fn a_credential_widens_the_answer_only_as_far_as_it_reaches() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-creds");
    let scratch = Scratch::new("search-creds");
    let server = spawn(&bucket.base_url, &scratch, "search-creds");
    let admin = server.bootstrap_org("acme");
    make_repo(&server, "acme", &admin, "open", true, "public");
    make_repo(&server, "acme", &admin, "closed", false, "private");

    // A token presented and *wrong* is a 401, not a quiet downgrade to
    // anonymous: somebody with a typo'd credential has to be told, or
    // they see a shorter list they cannot explain.
    let (st, out) = server.req("GET", "/v1/search/repos", "weft_nope_nope", None);
    assert_eq!(st, 401, "{out}");

    // An org-wide service token stands in for its org.
    let (_, out) = server.req("GET", "/v1/search/repos", &admin, None);
    assert_eq!(names(&out), ["acme/closed", "acme/open"]);

    // A token bound to one repository is worth no more than none here.
    // It was minted to reach that repository, and a search is not that
    // repository — so it sees exactly what a stranger sees.
    let bound = server.admin_json(&[
        "admin",
        "mint",
        "--org",
        "acme",
        "--scopes",
        "repo:read",
        "--repo",
        "closed",
        "--label",
        "one-repo",
    ])["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, out) = server.req("GET", "/v1/search/repos", &bound, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), ["acme/open"], "repo-bound token: {out}");

    // A personal token carries the person, so it sees what they see.
    server
        .admin(&[
            "admin",
            "user-create",
            "--email",
            "ada@acme.test",
            "--name",
            "Ada",
            "--password",
            "a long enough password",
            "--org",
            "acme",
            "--role",
            "owner",
        ])
        .unwrap();
    let mut ada = Browser::signed_in(&server, "ada@acme.test", "a long enough password");
    let (st, out) = ada.req(
        "POST",
        "/v1/orgs/acme/tokens",
        Some(serde_json::json!({ "scopes": ["org:read", "repo:read"], "label": "ada" })),
    );
    assert_eq!(st, 201, "{out}");
    let personal = out["token"].as_str().unwrap().to_string();
    let (st, out) = server.req("GET", "/v1/search/repos", &personal, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(names(&out), ["acme/closed", "acme/open"], "personal: {out}");

    assert!(server.healthy());
}

/// The oracle this increment closes.
///
/// Anonymously, "there is no such repository" and "that one is private"
/// must be the same answer. They were not: 404 and 401 respectively, so
/// a stranger could confirm a private repository's *name* by reading a
/// status code — the exact enumeration the git wire has always refused.
#[test]
fn anonymous_reads_cannot_tell_a_private_repo_from_an_absent_one() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-oracle");
    let scratch = Scratch::new("search-oracle");
    let server = spawn(&bucket.base_url, &scratch, "search-oracle");
    let admin = server.bootstrap_org("acme");
    make_repo(&server, "acme", &admin, "payments", false, "private");

    // Every repo-scoped read route, asked for a repo that exists but is
    // private, and for one that does not exist at all.
    for route in [
        "",
        "/tree",
        "/tree/src",
        "/files/README.md",
        "/log",
        "/refs",
        "/branches",
        "/tags",
        "/metrics",
        "/sync-status",
    ] {
        let private = server.status_get(&format!("/v1/orgs/acme/repos/payments{route}"), None);
        let absent = server.status_get(&format!("/v1/orgs/acme/repos/no-such-repo{route}"), None);
        assert_eq!(
            private, absent,
            "route {route:?}: private answered {private}, absent answered {absent}"
        );
        assert_eq!(
            private, 401,
            "route {route:?}: anonymous refusals are 401, as on the git wire"
        );
    }

    // The git wire agreed all along; assert it still does, so the two
    // front doors cannot drift apart again.
    let wire_private =
        server.status_get("/acme/payments.git/info/refs?service=git-upload-pack", None);
    let wire_absent = server.status_get("/acme/nope.git/info/refs?service=git-upload-pack", None);
    assert_eq!(wire_private, 401);
    assert_eq!(wire_absent, 401);

    // A *credentialed* caller who may not have it gets 404 for both —
    // the masking rule, unchanged: having authenticated tells you
    // nothing about repos you cannot reach.
    let rival = server.bootstrap_org("rival");
    for path in ["payments", "no-such-repo"] {
        let (st, out) = server.req("GET", &format!("/v1/orgs/acme/repos/{path}"), &rival, None);
        assert_eq!(st, 404, "foreign token on {path}: {out}");
    }

    // And a public repo is still readable by anyone — the fix must not
    // have closed the door it was holding open.
    make_repo(&server, "acme", &admin, "widget", true, "public");
    assert_eq!(server.status_get("/v1/orgs/acme/repos/widget", None), 200);
    assert_eq!(
        server.status_get("/v1/orgs/acme/repos/widget/refs", None),
        200
    );

    assert!(server.healthy());
}

/// Editing what a repo says about itself, and who may publish one.
#[test]
fn a_description_can_be_set_cleared_and_published_but_only_by_the_right_role() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-patch");
    let scratch = Scratch::new("search-patch");
    let server = spawn(&bucket.base_url, &scratch, "search-patch");
    let admin = server.bootstrap_org("acme");
    make_repo(&server, "acme", &admin, "widget", false, "first words");

    let url = "/v1/orgs/acme/repos/widget";
    // Set.
    let (st, out) = server.req(
        "PATCH",
        url,
        &admin,
        Some(serde_json::json!({ "description": "second words" })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["description"], "second words");
    assert_eq!(out["public"], false, "a description edit must not publish");

    // Clear, explicitly.
    let (st, out) = server.req(
        "PATCH",
        url,
        &admin,
        Some(serde_json::json!({ "description": null })),
    );
    assert_eq!(st, 200, "{out}");
    assert!(out["description"].is_null(), "{out}");
    assert_eq!(out["public"], false);

    // Publish, and it appears in anonymous search.
    assert_eq!(
        names(&server.req("GET", "/v1/search/repos?q=widget", "", None).1),
        [] as [String; 0]
    );
    let (st, out) = server.req(
        "PATCH",
        url,
        &admin,
        Some(serde_json::json!({ "public": true, "description": "now public" })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["public"], true);
    assert_eq!(
        names(&server.req("GET", "/v1/search/repos?q=widget", "", None).1),
        ["acme/widget"]
    );
    // Publishing is in the trail. It is the question asked after a leak.
    let (st, out) = server.req(
        "GET",
        "/v1/orgs/acme/audit?action=repo.visibility",
        &admin,
        None,
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["entries"].as_array().map(Vec::len), Some(1), "{out}");

    // Bad descriptions are refused, not stored.
    for bad in [
        serde_json::json!({ "description": "two\nlines" }),
        serde_json::json!({ "description": "x".repeat(513) }),
    ] {
        let (st, out) = server.req("PATCH", url, &admin, Some(bad.clone()));
        assert_eq!(st, 400, "{bad}: {out}");
    }
    // ...and the injection corpus is stored as text or refused, never a
    // 500, and never as SQL.
    for probe in INJECTIONS {
        let one_line = probe.replace(['\n', '\r', '\t', '\0'], " ");
        let (st, out) = server.req(
            "PATCH",
            url,
            &admin,
            Some(serde_json::json!({ "description": one_line })),
        );
        assert!(st == 200 || st == 400, "{probe:?} answered {st}: {out}");
    }
    let (st, _) = server.req(
        "PATCH",
        url,
        &admin,
        Some(serde_json::json!({ "description": "settled" })),
    );
    assert_eq!(st, 200, "the table survived the corpus");

    // Authority: a repo-write token may describe, but not publish.
    let writer = server.admin_json(&[
        "admin",
        "mint",
        "--org",
        "acme",
        "--scopes",
        "repo:read,repo:write",
        "--label",
        "writer",
    ])["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, out) = server.req(
        "PATCH",
        url,
        &writer,
        Some(serde_json::json!({ "description": "by the writer" })),
    );
    assert_eq!(st, 200, "{out}");
    let (st, out) = server.req(
        "PATCH",
        url,
        &writer,
        Some(serde_json::json!({ "public": false })),
    );
    assert_eq!(st, 404, "publishing needs org admin: {out}");
    assert_eq!(
        server.req("GET", url, &admin, None).1["public"],
        true,
        "and it did not happen"
    );

    // Anonymous and foreign callers cannot patch, and cannot learn from
    // trying whether the repo is there.
    let rival = server.bootstrap_org("rival");
    make_repo(&server, "acme", &admin, "hidden", false, "shh");
    for name in ["hidden", "no-such-repo"] {
        let anon = server.status_post(
            &format!("/v1/orgs/acme/repos/{name}"),
            "",
            serde_json::json!({ "description": "mine now" }),
        );
        let foreign = server.req(
            "PATCH",
            &format!("/v1/orgs/acme/repos/{name}"),
            &rival,
            Some(serde_json::json!({ "description": "mine now" })),
        );
        assert_eq!(foreign.0, 404, "{name}: {}", foreign.1);
        assert!(anon == 401 || anon == 405, "{name}: {anon}");
    }

    assert!(server.healthy());
}

/// A description is checked the same way wherever it arrives — and on
/// the mirror path it is checked *before* the origin probe, so a field
/// we are going to refuse does not first cost an outbound fetch.
#[test]
fn a_mirror_is_refused_for_a_bad_description_without_touching_the_origin() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-mirrordesc");
    let scratch = Scratch::new("search-mirrordesc");
    let server = spawn(&bucket.base_url, &scratch, "search-mirrordesc");
    let admin = server.bootstrap_org("acme");

    // An origin that could never be probed: if the description were
    // checked second, this would answer 422 about the origin instead.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({
            "name": "widget",
            "provider": "generic",
            "origin": "https://origin.invalid/acme/widget.git",
            "description": "two\nlines",
        })),
    );
    assert_eq!(st, 400, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("control"),
        "the message names the field, not the origin: {out}"
    );
    assert!(server.healthy());
}

/// The topics people are actually using — and only the ones this caller
/// may know about.
///
/// The discovery page's chips were a fixed list of ten common words, so
/// they matched whatever a repository happened to *say* rather than what
/// maintainers had filed things under: most returned nothing on any real
/// instance, while the topics in genuine use appeared nowhere.
///
/// The scoping assertion is the one that matters, and it is the same
/// hazard as repo search: a topic carried only by private repositories
/// must be invisible to a stranger, because a list of topic names is an
/// existence oracle for the work behind them. "we have a `project-atlas`
/// topic" is a sentence about a private repository.
#[test]
fn topics_in_use_are_listed_by_popularity_and_scoped_to_the_viewer() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-topics");
    let scratch = Scratch::new("search-topics");
    let server = spawn(&bucket.base_url, &scratch, "search-topics");
    let acme = server.bootstrap_org("acme");
    let rival = server.bootstrap_org("rival");

    make_repo(&server, "acme", &acme, "alpha", true, "one");
    make_repo(&server, "acme", &acme, "beta", true, "two");
    make_repo(&server, "acme", &acme, "hush", false, "private one");
    make_repo(&server, "rival", &rival, "gamma", true, "theirs");

    let tag = |repo: &str, token: &str, topics: serde_json::Value| {
        let (st, out) = server.req(
            "PUT",
            &format!(
                "/v1/orgs/{}/repos/{repo}/topics",
                if repo == "gamma" { "rival" } else { "acme" }
            ),
            token,
            Some(serde_json::json!({ "topics": topics })),
        );
        assert_eq!(st, 200, "tag {repo}: {out}");
    };
    tag("alpha", &acme, serde_json::json!(["rust", "cli"]));
    tag("beta", &acme, serde_json::json!(["rust"]));
    tag("gamma", &rival, serde_json::json!(["rust"]));
    // Only on the private one, so its very name is a leak.
    tag("hush", &acme, serde_json::json!(["project-atlas"]));

    let listed = |body: &serde_json::Value| -> Vec<(String, i64)> {
        body["topics"]
            .as_array()
            .expect("topics array")
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap().to_string(),
                    t["repos"].as_i64().unwrap(),
                )
            })
            .collect()
    };

    // Anonymous: the three public repositories only. `rust` leads on
    // count; `cli` follows. `project-atlas` is not here at all.
    let (st, out) = server.req("GET", "/v1/search/topics", "", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        listed(&out),
        vec![("rust".to_string(), 3), ("cli".to_string(), 1)],
        "anonymous topic list: {out}"
    );

    // A member of acme sees their private repository's topic, and the
    // count for `rust` is unchanged — `hush` does not carry it.
    server
        .admin(&[
            "admin",
            "user-create",
            "--email",
            "ada@acme.test",
            "--name",
            "Ada",
            "--password",
            "a long enough password",
            "--org",
            "acme",
            "--role",
            "owner",
        ])
        .unwrap();
    let mut ada = Browser::signed_in(&server, "ada@acme.test", "a long enough password");
    let (st, out) = ada.req("GET", "/v1/search/topics", None);
    assert_eq!(st, 200, "{out}");
    let mine = listed(&out);
    assert!(
        mine.contains(&("project-atlas".to_string(), 1)),
        "a member cannot see their own private repository's topic: {out}"
    );
    assert!(
        mine.contains(&("rust".to_string(), 3)),
        "the public count moved for a signed-in member: {out}"
    );

    // Another org's token is no better off than a stranger.
    let (st, out) = server.req("GET", "/v1/search/topics", &rival, None);
    assert_eq!(st, 200, "{out}");
    assert!(
        !listed(&out).iter().any(|(n, _)| n == "project-atlas"),
        "a foreign token saw a private repository's topic: {out}"
    );

    // A typo'd credential is told so, not quietly downgraded to
    // anonymous and shown a shorter list it cannot explain — the same
    // rule the repo search states, and now shared with it through one
    // `resolve_viewer`.
    let (st, out) = server.req("GET", "/v1/search/topics", "weft_nope_nope", None);
    assert_eq!(st, 401, "a bad token was treated as anonymous: {out}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
