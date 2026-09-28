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
//!
//! Every repository is private to its organization, so search is for
//! people who belong somewhere: no credential is a 401, a person sees
//! the organizations they belong to, and a token sees the one it was
//! minted in.

use stratum_testkit::adversarial::INJECTIONS;
use stratum_testkit::{browser::Browser, gitcli::Scratch, Minio, Server};

fn spawn(store_url: &str, scratch: &Scratch, hint: &str) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .start()
}

fn make_repo(server: &Server, org: &str, token: &str, name: &str, desc: &str) {
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
/// words, page through, and follow a result to the repo it names — as a
/// member, because nobody else may search here at all.
#[test]
fn search_finds_a_members_repos_by_name_namespace_and_description() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-happy");
    let scratch = Scratch::new("search-happy");
    let server = spawn(&bucket.base_url, &scratch, "search-happy");
    let admin = server.bootstrap_org("acme");
    let rival = server.bootstrap_org("rival");

    make_repo(&server, "acme", &admin, "widget", "the fast one");
    make_repo(&server, "acme", &admin, "ledger", "money, counted");
    make_repo(&server, "acme", &admin, "secret", "not for you");
    make_repo(&server, "rival", &rival, "other", "theirs");

    // Nobody searches anonymously — not the whole table, not one word.
    for q in ["", "?q=widg", "?q=secret", "?limit=1"] {
        let (st, out) = server.req("GET", &format!("/v1/search/repos{q}"), "", None);
        assert_eq!(st, 401, "anonymous search {q:?}: {out}");
    }

    let search = |q: &str| {
        let (st, out) = server.req("GET", &format!("/v1/search/repos{q}"), &admin, None);
        assert_eq!(st, 200, "{q}: {out}");
        out
    };
    // By name.
    assert_eq!(names(&search("?q=widg")), ["acme/widget"]);
    // By description.
    assert_eq!(names(&search("?q=counted")), ["acme/ledger"]);
    // By namespace.
    assert_eq!(
        names(&search("?q=acme")),
        ["acme/ledger", "acme/secret", "acme/widget"]
    );
    // An empty query browses everything this caller belongs to — and
    // nothing of rival's.
    let out = search("");
    assert_eq!(names(&out), ["acme/ledger", "acme/secret", "acme/widget"]);
    assert!(out["next"].is_null(), "one page, so no cursor: {out}");

    // Another organisation's token finds none of it, by any word.
    for q in ["?q=secret", "?q=acme", "?q=widget", ""] {
        let (st, out) = server.req("GET", &format!("/v1/search/repos{q}"), &rival, None);
        assert_eq!(st, 200, "{out}");
        for name in names(&out) {
            assert!(!name.starts_with("acme/"), "rival found {name} with {q:?}");
        }
    }

    // Paging: one at a time, following `next`, visits each repo once.
    let mut seen: Vec<String> = Vec::new();
    let mut url = "?limit=1".to_string();
    for _ in 0..10 {
        let out = search(&url);
        seen.extend(names(&out));
        let Some(next) = out["next"].as_str() else {
            break;
        };
        url = format!("?limit=1&after={}", urlencode(next));
    }
    assert_eq!(seen, ["acme/ledger", "acme/secret", "acme/widget"]);

    // A hit is enough to go and read the repo with, on the same
    // credential that found it.
    let hit = search("?q=widget");
    let org = hit["repos"][0]["org"].as_str().unwrap().to_string();
    let name = hit["repos"][0]["name"].as_str().unwrap().to_string();
    let (st, out) = server.req("GET", &format!("/v1/orgs/{org}/repos/{name}"), &admin, None);
    assert_eq!(st, 200, "a hit is readable by whoever found it: {out}");
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

    make_repo(&server, "acme", &acme, "payments", "the private one");
    make_repo(&server, "rival", &rival, "their-thing", "theirs");

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

    // Anonymous: not an empty list, a refusal.
    let (st, out) = server.req("GET", "/v1/search/repos?q=payments", "", None);
    assert_eq!(st, 401, "anonymous: {out}");
    assert!(!out.to_string().contains("payments"), "{out}");
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

    // A disabled account's session stops counting too — it is no
    // session at all, so the search is refused as an anonymous one is.
    let mut bo = Browser::signed_in(&server, "bo@acme.test", "a long enough password");
    assert_eq!(
        names(&bo.req("GET", "/v1/search/repos?q=payments", None).1),
        ["acme/payments"]
    );
    server
        .admin(&["admin", "user-disable", "--email", "bo@acme.test"])
        .unwrap();
    let (st, out) = bo.req("GET", "/v1/search/repos?q=payments", None);
    assert_eq!(st, 401, "disabled: {out}");
    assert!(!out.to_string().contains("payments"), "disabled: {out}");

    assert!(server.healthy());
}

/// A cursor is a window into what you could already see, and tampering
/// with it moves the window, never widens it.
///
/// The searcher is another organisation, whose token sees its own
/// repositories and none of acme's; the forged cursors are aimed at
/// acme's names, from either side.
#[test]
fn a_forged_cursor_cannot_page_into_someone_elses_repos() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-cursor");
    let scratch = Scratch::new("search-cursor");
    let server = spawn(&bucket.base_url, &scratch, "search-cursor");
    let acme = server.bootstrap_org("acme");
    let rival = server.bootstrap_org("rival");
    make_repo(&server, "acme", &acme, "aaa-first", "first");
    make_repo(&server, "acme", &acme, "bbb-private", "second");
    make_repo(&server, "acme", &acme, "ccc-third", "third");
    make_repo(&server, "rival", &rival, "mine", "theirs");

    // Hand-built cursors that land either side of acme's repositories.
    for forged in [
        "acme/bbb-private/0",
        "acme/bbb-privatd/zzzzzzzzzzzzzzzzzzzzzzzzzz",
        "acme/aaa-first/zzzzzzzzzzzzzzzzzzzzzzzzzz",
        "aaaa/a/0",
        "%00/%00/%00",
        "../../etc/passwd",
        "a/b/c'; DROP TABLE repos;--",
    ] {
        let path = format!("/v1/search/repos?after={}", urlencode(forged));
        let (st, out) = server.req("GET", &path, &rival, None);
        assert_eq!(st, 200, "cursor {forged:?}: {out}");
        for name in names(&out) {
            assert!(
                !name.starts_with("acme/"),
                "cursor {forged:?} leaked {name}: {out}"
            );
        }
        // Without a credential a cursor is no way in either.
        let (st, _) = server.req("GET", &path, "", None);
        assert_eq!(st, 401, "anonymous cursor {forged:?}");
    }
    // The window still works for its owner: rival pages to its own.
    let (_, out) = server.req("GET", "/v1/search/repos?after=aaaa%2Fa%2F0", &rival, None);
    assert_eq!(names(&out), ["rival/mine"]);
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
    make_repo(&server, "acme", &acme, "widget", "ordinary");

    for probe in INJECTIONS {
        let (st, out) = server.req(
            "GET",
            &format!("/v1/search/repos?q={}", urlencode(probe)),
            &acme,
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
            &acme,
            None,
        );
        assert!(
            st == 200 || st == 400,
            "len {}: {st} {out}",
            q.chars().count()
        );
    }
    // The repo is still findable afterwards: the server did not wedge.
    let (_, out) = server.req("GET", "/v1/search/repos?q=widget", &acme, None);
    assert_eq!(names(&out), ["acme/widget"]);
    assert!(server.healthy());
}

/// Which credential is asking changes only how much comes back — and
/// no credential, or one bound to a single repository, gets nothing.
///
/// A token is bound to the organisation it was minted in; a person's
/// session spans every organisation they belong to. So a person in two
/// organisations finds both with their session, and with a token minted
/// in one of them finds that one.
#[test]
fn a_credential_widens_the_answer_only_as_far_as_it_reaches() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-creds");
    let scratch = Scratch::new("search-creds");
    let server = spawn(&bucket.base_url, &scratch, "search-creds");
    let admin = server.bootstrap_org("acme");
    let bravo = server.bootstrap_org("bravo");
    make_repo(&server, "acme", &admin, "open", "one");
    make_repo(&server, "acme", &admin, "closed", "two");
    make_repo(&server, "bravo", &bravo, "elsewhere", "three");

    // A token presented and *wrong* is a 401, not a quiet downgrade to
    // anonymous: somebody with a typo'd credential has to be told. And
    // no credential at all is the same 401 — there is nothing here an
    // anonymous caller may find.
    let (st, out) = server.req("GET", "/v1/search/repos", "weft_nope_nope", None);
    assert_eq!(st, 401, "{out}");
    let (st, out) = server.req("GET", "/v1/search/repos", "", None);
    assert_eq!(st, 401, "{out}");

    // An org-wide service token stands in for its org, and only its org.
    let (_, out) = server.req("GET", "/v1/search/repos", &admin, None);
    assert_eq!(names(&out), ["acme/closed", "acme/open"]);
    let (_, out) = server.req("GET", "/v1/search/repos", &bravo, None);
    assert_eq!(names(&out), ["bravo/elsewhere"]);

    // A token bound to one repository is refused, by name. It was minted
    // to reach that repository, and a search is not that repository;
    // answering it as its org would widen it, and answering it with an
    // empty list would read as "nothing here".
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
    for path in ["/v1/search/repos", "/v1/search/topics"] {
        let (st, out) = server.req("GET", path, &bound, None);
        assert_eq!(st, 403, "{path}: {out}");
        assert!(
            out.to_string().contains(
                "a token bound to one repository cannot search; use a personal or \
                 organization token"
            ),
            "{path}: {out}"
        );
    }

    // Ada belongs to both organisations.
    for org in ["acme", "bravo"] {
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
                org,
                "--role",
                "owner",
            ])
            .unwrap();
    }
    let mut ada = Browser::signed_in(&server, "ada@acme.test", "a long enough password");
    // Her session spans both.
    let (st, out) = ada.req("GET", "/v1/search/repos", None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        names(&out),
        ["acme/closed", "acme/open", "bravo/elsewhere"],
        "a session: {out}"
    );
    // A personal token she mints in acme is acme's: it carries her, and
    // it is bound to the organisation it was minted in, so a script
    // holding it cannot enumerate bravo.
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
    let (st, out) = server.req("GET", "/v1/search/repos?q=elsewhere", &personal, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        names(&out),
        [] as [String; 0],
        "a token minted in acme found a repository in bravo: {out}"
    );

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
    make_repo(&server, "acme", &admin, "payments", "private");

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

    // And a member still reads it — the fix must not have closed the
    // door it was holding open.
    assert_eq!(
        server.status_get("/v1/orgs/acme/repos/payments", Some(&admin)),
        200
    );
    assert_eq!(
        server.status_get("/v1/orgs/acme/repos/payments/refs", Some(&admin)),
        200
    );

    assert!(server.healthy());
}

/// Editing what a repo says about itself, and who may. Publishing it is
/// not a thing anybody may do: there are no public repositories, and a
/// patch that asks for one is refused whole — and refused only to
/// somebody who could have made the patch at all, so the refusal is not
/// a way to learn that a repository exists.
#[test]
fn a_description_can_be_set_and_cleared_by_the_right_role_and_never_published() {
    let minio = Minio::shared();
    let bucket = minio.bucket("search-patch");
    let scratch = Scratch::new("search-patch");
    let server = spawn(&bucket.base_url, &scratch, "search-patch");
    let admin = server.bootstrap_org("acme");
    make_repo(&server, "acme", &admin, "widget", "first words");

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
    assert!(out.get("public").is_none(), "{out}");

    // Clear, explicitly.
    let (st, out) = server.req(
        "PATCH",
        url,
        &admin,
        Some(serde_json::json!({ "description": null })),
    );
    assert_eq!(st, 200, "{out}");
    assert!(out["description"].is_null(), "{out}");

    // Publishing is refused in words, and the patch it came in is
    // refused whole: the description beside it did not land either.
    let (st, out) = server.req(
        "PATCH",
        url,
        &admin,
        Some(serde_json::json!({ "public": true, "description": "now public" })),
    );
    assert_eq!(st, 400, "{out}");
    assert_eq!(
        out["error"],
        "this server has no public repositories: every repository is private to its \
         organization — omit \"public\" or set it to false"
    );
    let (_, out) = server.req("GET", url, &admin, None);
    assert!(
        out["description"].is_null(),
        "half a refused patch landed: {out}"
    );
    // Anonymous search still finds nothing, because it is still refused.
    assert_eq!(
        server.req("GET", "/v1/search/repos?q=widget", "", None).0,
        401
    );
    // `"public": false` is what already happens, and is accepted.
    let (st, out) = server.req(
        "PATCH",
        url,
        &admin,
        Some(serde_json::json!({ "public": false, "description": "private words" })),
    );
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["description"], "private words");

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

    // Authority: a repo-write token may describe.
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
    // A reader may not, and is not told more than a stranger would be.
    let reader = server.admin_json(&[
        "admin",
        "mint",
        "--org",
        "acme",
        "--scopes",
        "repo:read",
        "--label",
        "reader",
    ])["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, out) = server.req(
        "PATCH",
        url,
        &reader,
        Some(serde_json::json!({ "description": "by the reader" })),
    );
    assert_eq!(st, 404, "{out}");
    assert_eq!(
        server.req("GET", url, &admin, None).1["description"],
        "by the writer",
        "and it did not happen"
    );

    // Anonymous and foreign callers cannot patch, and cannot learn from
    // trying whether the repo is there — whatever the patch asks for.
    // A body that would be refused *on its merits* (`"public": true`) is
    // the sharp case: judged before the caller's authority, it would be
    // a 400 for a repository that exists and a 404 for one that does
    // not, which is an existence oracle for every private name.
    let rival = server.bootstrap_org("rival");
    make_repo(&server, "acme", &admin, "hidden", "shh");
    for body in [
        serde_json::json!({ "description": "mine now" }),
        serde_json::json!({ "public": true }),
        serde_json::json!({ "public": true, "description": "mine now" }),
    ] {
        for (who, token, expect) in [
            ("anonymous", "", 401),
            ("a rival org", rival.as_str(), 404),
            ("a reader", reader.as_str(), 404),
        ] {
            let hidden = server.req(
                "PATCH",
                "/v1/orgs/acme/repos/hidden",
                token,
                Some(body.clone()),
            );
            let absent = server.req(
                "PATCH",
                "/v1/orgs/acme/repos/no-such-repo",
                token,
                Some(body.clone()),
            );
            assert_eq!(
                hidden.0, expect,
                "{who} patching hidden with {body}: {}",
                hidden.1
            );
            assert_eq!(
                hidden, absent,
                "{who} told a real repo from an absent one with {body}"
            );
        }
    }
    let (_, out) = server.req("GET", "/v1/orgs/acme/repos/hidden", &admin, None);
    assert_eq!(out["description"], "shh", "{out}");

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

    make_repo(&server, "acme", &acme, "alpha", "one");
    make_repo(&server, "acme", &acme, "beta", "two");
    make_repo(&server, "acme", &acme, "hush", "private one");
    make_repo(&server, "rival", &rival, "gamma", "theirs");

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

    // Anonymous: not a shorter list, a refusal — a list of topic names
    // is a list of what the work is about.
    let (st, out) = server.req("GET", "/v1/search/topics", "", None);
    assert_eq!(st, 401, "{out}");
    assert!(!out.to_string().contains("project-atlas"), "{out}");

    // A member of acme sees acme's topics, their counts, and the
    // private project's — and nothing of rival's: `rust` counts alpha
    // and beta, not gamma. Most used first.
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
    assert_eq!(
        mine.first(),
        Some(&("rust".to_string(), 2)),
        "the most-used topic leads, counted over acme only: {out}"
    );
    let mut rest = mine[1..].to_vec();
    rest.sort();
    assert_eq!(
        rest,
        vec![("cli".to_string(), 1), ("project-atlas".to_string(), 1)],
        "a member's topic list: {out}"
    );
    // The org's own token sees the same.
    let (st, out) = server.req("GET", "/v1/search/topics", &acme, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(listed(&out).len(), 3, "{out}");

    // Another org's token sees its own, and not one of acme's.
    let (st, out) = server.req("GET", "/v1/search/topics", &rival, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        listed(&out),
        vec![("rust".to_string(), 1)],
        "a foreign token saw acme's topics or counts: {out}"
    );

    // A typo'd credential is told so — the same rule the repo search
    // states, and shared with it through one `resolve_viewer`.
    let (st, out) = server.req("GET", "/v1/search/topics", "weft_nope_nope", None);
    assert_eq!(st, 401, "a bad token was treated as anonymous: {out}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
