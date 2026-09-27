//! Vendor CI intake and the README badge, end to end against a real
//! server.
//!
//! Two new attack surfaces arrive together here, so this file is half
//! happy path and half negative suite:
//!
//! * a route whose **only** credential is an HMAC over the body, with no
//!   Authorization header for the usual masking to key off; and
//! * a route that is *meant* to be fetched anonymously by every reader
//!   of a README, which makes it the most inviting existence oracle in
//!   the API.
//!
//! Every attack case ends by asserting the server is still healthy and
//! still serving the thing it was serving before — a server that survives
//! by refusing everything has failed differently, not passed.
//!
//! The documentation is executed rather than described: the shell in
//! `docs/guide/ci-integration.md` is read out of the
//! committed markdown and run verbatim against this server, and the four
//! provider snippets are asserted to contain that same shell line for
//! line. A snippet that 404s is worse than no snippet.

use std::process::Command;
use std::time::Duration;
use stratum_testkit::{gitcli::Scratch, Minio, Server};

const PASSWORD: &str = "a long enough password";
const DOC: &str = include_str!("../../../docs/guide/ci-integration.md");

fn spawn_server(store_url: &str, scratch: &Scratch, hint: &str) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        // The lander polls every second so the badge's "what landed"
        // question has an answer within the test's patience.
        .env("STRATUM_LAND_POLL_SECS", "1")
        .start()
}

// ---------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------

/// The signature, computed by **the exact command the documentation
/// tells a reader to run**.
///
/// Using `hmac` here instead would test our own arithmetic against
/// itself and leave the documented one-liner unverified — and the
/// one-liner is the part a reader actually pastes. `openssl dgst`
/// changed its default output prefix between 1.1 and 3.x
/// (`HMAC-SHA256(stdin)=` vs `SHA2-256(stdin)=`), which is exactly the
/// sort of thing a docs page gets wrong and nobody notices; the `sed`
/// strips either.
fn doc_signature(secret: &str, body: &str) -> String {
    let out = Command::new("bash")
        .arg("-c")
        .arg(r#"printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$SECRET" | sed 's/^.*= //'"#)
        .env("BODY", body)
        .env("SECRET", secret)
        .output()
        .expect("run openssl (the docs require it of readers, so tests require it too)");
    assert!(
        out.status.success(),
        "openssl failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(hex.len(), 64, "not a sha256 hex digest: {hex:?}");
    format!("sha256={hex}")
}

/// POST with a raw body and an optional signature, and no Authorization
/// header at all — the posture a CI runner is actually in.
fn post_signed(
    server: &Server,
    path: &str,
    signature: Option<&str>,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut r =
        ureq::post(&format!("{}{path}", server.base)).set("Content-Type", "application/json");
    if let Some(s) = signature {
        r = r.set("X-Weft-Signature-256", s);
    }
    let resp = match r.send_string(body) {
        Ok(x) => x,
        Err(ureq::Error::Status(_, x)) => x,
        Err(e) => panic!("transport POST {path}: {e}"),
    };
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

/// A GET whose body is not JSON — the badge. Returns status, body and
/// headers, because for this route the headers *are* half the contract.
fn get_raw(
    server: &Server,
    path: &str,
    token: Option<&str>,
) -> (u16, String, std::collections::HashMap<String, String>) {
    let mut r = ureq::get(&format!("{}{path}", server.base));
    if let Some(t) = token {
        r = r.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = match r.call() {
        Ok(x) => x,
        Err(ureq::Error::Status(_, x)) => x,
        Err(e) => panic!("transport GET {path}: {e}"),
    };
    let status = resp.status();
    let headers: std::collections::HashMap<String, String> = resp
        .headers_names()
        .into_iter()
        .filter_map(|n| resp.header(&n).map(|v| (n.to_lowercase(), v.to_string())))
        .collect();
    (status, resp.into_string().unwrap_or_default(), headers)
}

fn as_person(server: &Server, cookie: &str, method: &str, path: &str) -> (u16, serde_json::Value) {
    let resp = match ureq::request(method, &format!("{}{path}", server.base))
        .set("Cookie", cookie)
        .call()
    {
        Ok(x) => x,
        Err(ureq::Error::Status(_, x)) => x,
        Err(e) => panic!("transport {method} {path}: {e}"),
    };
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

// ---------------------------------------------------------------------
// World
// ---------------------------------------------------------------------

struct World {
    server: Server,
    admin: String,
    /// Alice's session cookie: an owner, so she can approve.
    alice: String,
}

fn commit(
    server: &Server,
    token: &str,
    repo: &str,
    branch: &str,
    message: &str,
    files: &[(&str, &str)],
) -> String {
    let ops: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, c)| serde_json::json!({"op": "put", "path": p, "content": c}))
        .collect();
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        token,
        Some(serde_json::json!({
            "branch": branch,
            "message": message,
            "operations": ops,
        })),
    );
    assert_eq!(st, 201, "commit to {repo}/{branch}: {out}");
    out["commit"].as_str().unwrap().to_string()
}

fn branch(server: &Server, token: &str, repo: &str, name: &str, from: &str) {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        token,
        Some(serde_json::json!({"name": name, "from": from})),
    );
    assert_eq!(st, 201, "branch {name} from {from}: {out}");
}

/// One org, one public repo `app` with a root OWNERS naming Alice, and
/// one private repo `vault`. Public because a badge that nobody may
/// fetch anonymously is not a badge.
fn world(server: Server) -> World {
    let admin = server.bootstrap_org("acme");
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "alice@acme.test",
            "--name",
            "Alice",
            "--password",
            PASSWORD,
            "--role",
            "member",
        ])
        .expect("create alice");
    let mut b = stratum_testkit::browser::Browser::new(&server);
    assert_eq!(b.login("alice@acme.test", PASSWORD), 200);
    let alice = b.cookie.clone().expect("a session cookie");

    for (name, public) in [("app", true), ("vault", false)] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name, "public": public })),
        );
        assert_eq!(st, 201, "{out}");
    }
    commit(
        &server,
        &admin,
        "app",
        "main",
        "root owners",
        &[("OWNERS", "alice@acme.test\n"), ("README.md", "# app\n")],
    );
    World {
        server,
        admin,
        alice,
    }
}

/// Mint an intake secret for a repo through the API, the way the docs do.
fn mint_secret(server: &Server, admin: &str, repo: &str) -> String {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/ci/secret"),
        admin,
        None,
    );
    assert_eq!(st, 201, "{out}");
    out["secret"].as_str().expect("a secret").to_string()
}

/// Open a change on `repo` from a fresh feature branch. Returns
/// (change key, tip commit).
fn open_change(
    server: &Server,
    admin: &str,
    repo: &str,
    change_id: &str,
    feature: &str,
    file: (&str, &str),
) -> (String, String) {
    let (st, refs) = server.get(&format!("/v1/orgs/acme/repos/{repo}/refs"), admin);
    assert_eq!(st, 200, "{refs}");
    let main = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .expect("main exists")["oid"]
        .as_str()
        .unwrap()
        .to_string();
    branch(server, admin, repo, feature, &main);
    let tip = commit(
        server,
        admin,
        repo,
        feature,
        &format!("work on {feature}\n\nChange-Id: {change_id}\n"),
        &[file],
    );
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        admin,
        Some(serde_json::json!({ "from": feature })),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["change"]["key"], serde_json::json!(change_id));
    (change_id.to_string(), tip)
}

fn intake_body(change: &str, commit: &str, name: &str, state: &str, sent_at: i64) -> String {
    serde_json::json!({
        "change": change,
        "commit": commit,
        "name": name,
        "state": state,
        "summary": "42 passed, 0 failed",
        "sent_at": sent_at,
    })
    .to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Approve as Alice, land as admin, and wait for the lander.
fn land(w: &World, repo: &str, key: &str) {
    let (st, out) = as_person(
        &w.server,
        &w.alice,
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/approve"),
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = w.server.post(
        &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/land"),
        &w.admin,
        None,
    );
    assert_eq!(st, 202, "{out}");
    for _ in 0..150 {
        let (st, out) = w.server.get(
            &format!("/v1/orgs/acme/repos/{repo}/changes/{key}"),
            &w.admin,
        );
        assert_eq!(st, 200, "{out}");
        if out["change"]["state"] == serde_json::json!("landed") {
            return;
        }
        assert_ne!(
            out["change"]["state"],
            serde_json::json!("open"),
            "the land was rejected: {out}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{key} never landed");
}

// ---------------------------------------------------------------------
// Markdown extraction — the docs runner
// ---------------------------------------------------------------------

/// Fenced blocks of one language, in document order.
fn blocks(markdown: &str, lang: &str) -> Vec<String> {
    let open = format!("```{lang}\n");
    let mut out = Vec::new();
    let mut rest = markdown;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let end = after.find("\n```").expect("unterminated fence");
        out.push(after[..end].to_string());
        rest = &after[end..];
    }
    out
}

/// The canonical request: the one bash block that signs and posts.
fn canonical_shell() -> String {
    let mut found: Vec<String> = blocks(DOC, "bash")
        .into_iter()
        .filter(|b| b.contains("X-Weft-Signature-256"))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "ci-integration.md must have exactly one canonical signing shell block, found {}",
        found.len()
    );
    found.pop().unwrap()
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

/// The documented shell is run, verbatim, against a live server — first
/// the block that mints a secret, then the block that posts a verdict —
/// and the check it claims to create is then read back through the API.
#[test]
fn the_documented_shell_runs_verbatim_against_a_live_server() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-docs");
    let scratch = Scratch::new("ci-intake-docs");
    let w = world(spawn_server(&bucket.base_url, &scratch, "ci-intake-docs"));
    let (server, admin) = (&w.server, &w.admin);
    let (key, tip) = open_change(
        server,
        admin,
        "app",
        "Idcc00001",
        "docs-feature",
        ("src/a.rs", "fn a() {}"),
    );

    // Block one of the page: mint the secret.
    let mint = blocks(DOC, "bash")
        .into_iter()
        .find(|b| b.contains("/ci/secret"))
        .expect("the page documents how to mint a secret");
    let out = Command::new("bash")
        .arg("-c")
        .arg(&mint)
        .env("WEFT_URL", &server.base)
        .env("ORG", "acme")
        .env("REPO", "app")
        .env("TOKEN", admin)
        .output()
        .expect("run the documented mint");
    assert!(
        out.status.success(),
        "documented mint failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let minted: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "the documented mint did not print JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    let secret = minted["secret"].as_str().expect("a secret").to_string();

    // Block two: the signed post. Run exactly as written.
    let shell = canonical_shell();
    let out = Command::new("bash")
        .arg("-c")
        .arg(&shell)
        .env("WEFT_URL", &server.base)
        .env("ORG", "acme")
        .env("REPO", "app")
        .env("WEFT_CI_SECRET", &secret)
        // **No CHANGE.** The documented shell is commit-scoped, and this
        // test used to hand it a change key read out of the API — a
        // value no CI system can compute, because a change key is `I`
        // plus the hex of the commit's own Change-Id trailer and is
        // oid-derived when there is no trailer. So the snippet passed
        // here for years while every provider snippet on the page sent
        // a branch name or a pull request title in that slot and was
        // answered `404 no such change` on every event a real project
        // ever had. The fixture was supplying the one thing the product
        // could not, which is the only way a green test hides a feature
        // that has never worked.
        .env("COMMIT", &tip)
        .env("STATE", "passing")
        .env("REF", "docs-feature")
        .env("EVENT", "push")
        .env("ACTOR", "ada")
        .env("RUN_ID", "run-42")
        .env("RUN_NUMBER", "42")
        .env("SUMMARY", "42 passed, 0 failed")
        .env("RUN_URL", "https://ci.example.test/runs/42")
        .output()
        .expect("run the documented post");
    assert!(
        out.status.success(),
        "the documented snippet failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // It reaches the Checks tab, which is the surface the page promises
    // and the one a commit-scoped report is *for*. The old change-scoped
    // snippet wrote `change_checks` only, so following this page
    // verbatim left that tab empty forever.
    let (st, runs) = server.get("/v1/orgs/acme/repos/app/checks/runs", admin);
    assert_eq!(st, 200, "{runs}");
    let tab = runs["runs"].as_array().expect("runs array");
    assert_eq!(
        tab.len(),
        1,
        "the documented snippet filled no Checks tab: {runs}"
    );
    assert_eq!(tab[0]["name"], serde_json::json!("ci/tests"));
    assert_eq!(tab[0]["state"], serde_json::json!("passing"));
    // Every optional field the page tells people to send is a column
    // that page also says the tab filters on. A snippet that omits them
    // lands a row with blanks where a reader expects a branch.
    assert_eq!(
        tab[0]["ref_name"],
        serde_json::json!("docs-feature"),
        "{runs}"
    );
    assert_eq!(tab[0]["event"], serde_json::json!("push"), "{runs}");
    assert_eq!(tab[0]["actor"], serde_json::json!("ada"), "{runs}");
    assert_eq!(tab[0]["external_id"], serde_json::json!("run-42"), "{runs}");
    assert_eq!(tab[0]["run_number"], serde_json::json!(42), "{runs}");

    // And, without naming a change anywhere, it still reaches the review
    // page — the merged read unions the runs reported against the
    // patchset's commit. This is the claim that lets the page tell
    // people one commit-scoped request is enough.
    let (st, checks) = server.get(
        &format!("/v1/orgs/acme/repos/app/changes/{key}/checks"),
        admin,
    );
    assert_eq!(st, 200, "{checks}");
    let rows = checks["checks"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{checks}");
    assert_eq!(rows[0]["name"], serde_json::json!("ci/tests"));
    assert_eq!(rows[0]["state"], serde_json::json!("passing"));
    assert_eq!(
        rows[0]["source"],
        serde_json::json!("commit"),
        "a commit-scoped report must reach the change as a commit-scoped row: {checks}"
    );
    // Honest about what wrote it: the provider, for a commit-scoped row.
    assert_eq!(rows[0]["posted_by"], serde_json::json!("intake"));
    // The page tells a reader that `url` is the whole escape hatch from
    // a red check back to the thing that broke, so the snippet has to
    // actually send one — advice the documented command does not follow
    // is worse than no advice.
    assert_eq!(
        rows[0]["url"],
        serde_json::json!("https://ci.example.test/runs/42"),
        "the documented snippet did not carry a run URL: {checks}"
    );

    // The summary has nowhere to live on the check row, so the page's
    // claim that it is kept has to be true somewhere. It is the trail.
    let (st, audit) = server.get("/v1/orgs/acme/audit?limit=50", admin);
    assert_eq!(st, 200, "{audit}");
    let entry = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "ci.check")
        .unwrap_or_else(|| panic!("no ci.check entry: {audit}"));
    assert_eq!(
        entry["context"]["summary"],
        serde_json::json!("42 passed, 0 failed"),
        "{entry}"
    );

    assert!(server.healthy());
}

/// Each provider snippet is the verified shell, line for line.
///
/// One block is executed above; four are pasted into YAML that no test
/// can run. Asserting they are the *same* lines is what makes the
/// executed one cover all five — and it is what catches the ordinary
/// failure, which is somebody fixing the bash block and leaving the four
/// copies below it saying the old thing.
#[test]
fn every_provider_snippet_is_the_verified_shell_line_for_line() {
    let shell = canonical_shell();
    let yaml = blocks(DOC, "yaml");
    assert_eq!(
        yaml.len(),
        4,
        "the page must carry one snippet per documented provider"
    );
    for vendor in ["GitHub Actions", "CircleCI", "Buildkite", "GitLab CI"] {
        assert!(DOC.contains(vendor), "{vendor} is not documented");
    }
    for (i, snippet) in yaml.iter().enumerate() {
        let flat: String = snippet
            .lines()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join("\n");
        for line in shell.lines().map(str::trim).filter(|l| !l.is_empty()) {
            assert!(
                flat.contains(line),
                "provider snippet {i} has drifted from the verified shell.\n\
                 missing line: {line}\n\nsnippet:\n{snippet}"
            );
        }
    }
}

/// The badge: what it reports, what it caches, and what it refuses to
/// admit about a private repository.
#[test]
fn the_badge_reports_what_landed_and_masks_a_private_repo() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-badge");
    let scratch = Scratch::new("ci-intake-badge");
    let w = world(spawn_server(&bucket.base_url, &scratch, "ci-intake-badge"));
    let (server, admin) = (&w.server, &w.admin);
    let secret = mint_secret(server, admin, "app");
    let path = "/v1/orgs/acme/repos/app/badge.svg";

    // Nothing has landed. Grey, and never green: an empty check list is
    // byte-identical to "everything passed", and reading it as passing
    // is how a badge comes to certify a repository nobody has built.
    let (st, svg, headers) = get_raw(server, path, None);
    assert_eq!(st, 200, "{svg}");
    assert!(svg.contains(">no status</text>"), "{svg}");
    assert!(!svg.contains(">passing</text>"), "{svg}");
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("image/svg+xml; charset=utf-8"),
        "{headers:?}"
    );
    // A badge that caches for a day is a badge that lies.
    let cache = headers.get("cache-control").expect("a cache header");
    assert!(cache.contains("max-age=60"), "{cache}");
    assert_eq!(
        headers.get("x-content-type-options").map(String::as_str),
        Some("nosniff")
    );

    // An open change with a green check still does not colour the
    // branch: it is not on the branch yet.
    let (key, tip) = open_change(
        server,
        admin,
        "app",
        "Ibadd0001",
        "badge-feature",
        ("src/b.rs", "fn b() {}"),
    );
    let body = intake_body(&key, &tip, "ci/tests", "passing", now_ms());
    let sig = doc_signature(&secret, &body);
    let (st, out) = post_signed(
        server,
        "/v1/orgs/acme/repos/app/ci/checks",
        Some(&sig),
        &body,
    );
    assert_eq!(st, 201, "{out}");
    let (_, svg, _) = get_raw(server, path, None);
    assert!(
        svg.contains(">no status</text>"),
        "an open change coloured trunk's badge: {svg}"
    );

    // Landing puts it on the branch, and the badge follows.
    land(&w, "app", &key);
    let (st, svg, _) = get_raw(server, path, None);
    assert_eq!(st, 200);
    assert!(svg.contains(">passing</text>"), "{svg}");
    assert!(svg.contains("<svg xmlns="), "{svg}");

    // A post-merge run finding trunk broken turns it red. This is the
    // only way red is reachable at all — landing already refuses a
    // change whose checks are failing — and it is the case a README
    // badge exists for.
    let body = intake_body(&key, &tip, "ci/nightly", "failing", now_ms());
    let sig = doc_signature(&secret, &body);
    let (st, out) = post_signed(
        server,
        "/v1/orgs/acme/repos/app/ci/checks",
        Some(&sig),
        &body,
    );
    assert_eq!(st, 201, "{out}");
    let (_, svg, _) = get_raw(server, path, None);
    assert!(svg.contains(">failing</text>"), "{svg}");

    // The same check reported again — a re-run, which is the most
    // ordinary thing CI does. **200, not 201**, and it must *update*
    // rather than accumulate: a second row under one name would leave
    // the badge picking between two answers, and nothing says which is
    // current. This is the only path that reaches the `OK` arm; every
    // other test here posts a name for the first time.
    let body = intake_body(&key, &tip, "ci/nightly", "passing", now_ms());
    let sig = doc_signature(&secret, &body);
    let (st, out) = post_signed(
        server,
        "/v1/orgs/acme/repos/app/ci/checks",
        Some(&sig),
        &body,
    );
    assert_eq!(
        st, 200,
        "a re-run of an existing check answered {st} rather than updating it: {out}"
    );
    let (_, svg, _) = get_raw(server, path, None);
    assert!(
        svg.contains(">passing</text>"),
        "a green re-run left the badge red: {svg}"
    );

    // A branch nothing landed on is grey, not the default branch's
    // answer under another name.
    let (st, svg, _) = get_raw(server, &format!("{path}?branch=release-9"), None);
    assert_eq!(st, 200);
    assert!(svg.contains(">no status</text>"), "{svg}");

    // A change that landed with **no CI at all** is grey, not green.
    //
    // This case was missing until a mutation found it: with the
    // assertions above, turning the empty-check verdict into `passing`
    // left the whole suite green, because the only grey answer being
    // exercised was "nothing has landed here" — which returns before the
    // aggregation is ever reached. The dangerous read is the other one:
    // a project with no CI wired up would have been certified passing by
    // its own README, which is the single worst thing a badge can do.
    let (unchecked, _) = open_change(
        server,
        admin,
        "app",
        "Ibadd0002",
        "unchecked-feature",
        ("src/e.rs", "fn e() {}"),
    );
    land(&w, "app", &unchecked);
    let (st, svg, _) = get_raw(server, path, None);
    assert_eq!(st, 200);
    assert!(
        svg.contains(">no status</text>"),
        "a change that landed with no checks reported green: {svg}"
    );

    // The branch is compared, never drawn. Git ref names may legally
    // contain `<`, `>` and `&`, so a badge that echoed one would be
    // stored XSS reachable from any README on the internet.
    let hostile = "%3Cscript%3Ealert(1)%3C/script%3E";
    let (st, svg, _) = get_raw(server, &format!("{path}?branch={hostile}"), None);
    assert_eq!(st, 200);
    assert!(!svg.contains("<script"), "{svg}");
    assert!(!svg.contains("alert"), "{svg}");
    assert!(svg.contains(">no status</text>"), "{svg}");

    // Masking. A stranger asking about the private repo must not be able
    // to tell it apart from one that does not exist, and must never get
    // a badge that says "private" — that badge is the confirmation.
    let private = "/v1/orgs/acme/repos/vault/badge.svg";
    let absent = "/v1/orgs/acme/repos/no-such-repo/badge.svg";
    let (st_private, body_private, _) = get_raw(server, private, None);
    let (st_absent, body_absent, _) = get_raw(server, absent, None);
    assert_eq!(st_private, st_absent, "the two answers differ by status");
    assert_eq!(body_private, body_absent, "the two answers differ by body");
    assert!(!body_private.contains("<svg"), "{body_private}");
    assert!(!body_private.to_lowercase().contains("private"));

    // Same again for somebody holding a credential that is not for this
    // org: masked, and masked identically.
    let other = server.bootstrap_org("rival");
    let (st_private, body_private, _) = get_raw(server, private, Some(&other));
    let (st_absent, body_absent, _) = get_raw(server, absent, Some(&other));
    assert_eq!(st_private, 404);
    assert_eq!(st_private, st_absent);
    assert_eq!(body_private, body_absent);

    // A member may of course see their own repo's badge.
    let (st, svg, _) = get_raw(server, private, Some(admin));
    assert_eq!(st, 200, "{svg}");
    assert!(svg.contains(">no status</text>"), "{svg}");

    assert!(server.healthy());
}

/// The negative suite. Every refusal the intake owes, and proof that
/// none of them moved the check that was already there.
#[test]
fn the_intake_refuses_every_attack_and_keeps_serving() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-attacks");
    let scratch = Scratch::new("ci-intake-attacks");
    let w = world(spawn_server(
        &bucket.base_url,
        &scratch,
        "ci-intake-attacks",
    ));
    let (server, admin) = (&w.server, &w.admin);
    let secret = mint_secret(server, admin, "app");
    let vault_secret = mint_secret(server, admin, "vault");
    let route = "/v1/orgs/acme/repos/app/ci/checks";
    let (key, tip) = open_change(
        server,
        admin,
        "app",
        "Iaaac0001",
        "attack-feature",
        ("src/c.rs", "fn c() {}"),
    );

    // A good delivery first, so every refusal below has something it
    // could have corrupted.
    let good = intake_body(&key, &tip, "ci/tests", "passing", now_ms());
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &good)), &good);
    assert_eq!(st, 201, "{out}");

    let fresh = |state: &str| intake_body(&key, &tip, "ci/tests", state, now_ms());
    let sign = |b: &str| doc_signature(&secret, b);

    // --- the credential ------------------------------------------------

    // Unsigned.
    let b = fresh("failing");
    let (st, unsigned) = post_signed(server, route, None, &b);
    assert_eq!(st, 404, "{unsigned}");

    // Wrong signature: one flipped hex digit.
    let b = fresh("failing");
    let mut bad = sign(&b);
    let last = bad.len() - 1;
    let c = if bad.as_bytes()[last] == b'a' {
        'b'
    } else {
        'a'
    };
    bad.replace_range(last.., &c.to_string());
    let (st, out) = post_signed(server, route, Some(&bad), &b);
    assert_eq!(st, 404, "{out}");

    // A signature for a *different repository*. The secret is real, the
    // body is real, and it is being pointed at the wrong door — which is
    // exactly what a leaked-and-reused secret looks like.
    let b = fresh("failing");
    let (st, out) = post_signed(server, route, Some(&doc_signature(&vault_secret, &b)), &b);
    assert_eq!(st, 404, "{out}");
    // And the converse: this repo's secret does not open the other door.
    let (st, out) = post_signed(
        server,
        "/v1/orgs/acme/repos/vault/ci/checks",
        Some(&sign(&b)),
        &b,
    );
    assert_eq!(st, 404, "{out}");

    // A signature of a different algorithm, and a header that is not a
    // signature at all.
    for header in [
        sign(&b).replace("sha256=", "sha1="),
        sign(&b).replace("sha256=", ""),
        "sha256=".to_string(),
        "sha256=zz".to_string(),
        String::new(),
    ] {
        let (st, out) = post_signed(server, route, Some(&header), &b);
        assert_eq!(st, 404, "{header:?} was not refused: {out}");
    }

    // Every one of those answers is the *same* answer a nonexistent
    // repository gives, and the same a repository with no secret gives.
    // That is the whole masking argument, so it is asserted rather than
    // asserted-about.
    let (st_absent, absent) = post_signed(
        server,
        "/v1/orgs/acme/repos/no-such-repo/ci/checks",
        Some(&sign(&b)),
        &b,
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({"name": "unconfigured", "public": true})),
    );
    assert_eq!(st, 201, "{out}");
    let (st_nosecret, nosecret) = post_signed(
        server,
        "/v1/orgs/acme/repos/unconfigured/ci/checks",
        Some(&sign(&b)),
        &b,
    );
    assert_eq!((st_absent, st_nosecret), (404, 404));
    assert_eq!(unsigned, absent, "bad signature vs. absent repo differ");
    assert_eq!(unsigned, nosecret, "bad signature vs. no secret differ");

    // --- replay --------------------------------------------------------

    // The exact bytes, sent twice. The first is fine and the second is
    // not: a captured request is a credential otherwise.
    let replay = intake_body(&key, &tip, "ci/replay", "passing", now_ms());
    let sig = sign(&replay);
    let (st, out) = post_signed(server, route, Some(&sig), &replay);
    assert_eq!(st, 201, "{out}");
    let (st, out) = post_signed(server, route, Some(&sig), &replay);
    assert_eq!(st, 409, "a replayed body was applied: {out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("already been applied"),
        "{out}"
    );

    // A body captured earlier and sent back later fails on its own
    // clock, whatever the ring remembers.
    let stale = intake_body(&key, &tip, "ci/tests", "failing", now_ms() - 10 * 60 * 1000);
    let (st, out) = post_signed(server, route, Some(&sign(&stale)), &stale);
    assert_eq!(st, 409, "{out}");
    // A clock far in the future is refused too — otherwise a sender
    // could mint a body that stays valid for as long as it liked.
    let ahead = intake_body(&key, &tip, "ci/tests", "failing", now_ms() + 10 * 60 * 1000);
    let (st, out) = post_signed(server, route, Some(&sign(&ahead)), &ahead);
    assert_eq!(st, 409, "{out}");

    // --- bounds (I13): refused, never truncated ------------------------

    let oversize_field = serde_json::json!({
        "change": key, "commit": tip, "name": "ci/tests", "state": "failing",
        "summary": "x".repeat(2001), "sent_at": now_ms(),
    })
    .to_string();
    let (st, out) = post_signed(server, route, Some(&sign(&oversize_field)), &oversize_field);
    assert_eq!(st, 400, "{out}");
    assert!(out["error"].as_str().unwrap().contains("summary"), "{out}");

    // Exactly at the bound is accepted — a limit nobody may reach is a
    // different limit than the one documented.
    let at_bound = serde_json::json!({
        "change": key, "commit": tip, "name": "ci/atbound", "state": "passing",
        "summary": "x".repeat(2000), "sent_at": now_ms(),
    })
    .to_string();
    let (st, out) = post_signed(server, route, Some(&sign(&at_bound)), &at_bound);
    assert_eq!(st, 201, "{out}");

    // An oversized *body* is a different refusal from an oversized
    // field, and it comes after the signature — so 413 is only ever
    // reachable by somebody who holds the secret.
    let huge = serde_json::json!({
        "change": key, "commit": tip, "name": "ci/tests", "state": "failing",
        "summary": "ok", "sent_at": now_ms(), "padding": "p".repeat(20_000),
    })
    .to_string();
    let (st, out) = post_signed(server, route, Some(&sign(&huge)), &huge);
    assert_eq!(st, 413, "{out}");
    // …and the same oversized body unsigned is still just a 404: the
    // size refusal must not become an oracle of its own.
    let (st, out) = post_signed(server, route, None, &huge);
    assert_eq!(st, 404, "{out}");

    for (bad_body, why) in [
        (
            serde_json::json!({"change": key, "commit": "nothex", "name": "ci/tests",
                               "state": "passing", "sent_at": now_ms()}),
            "commit",
        ),
        (
            serde_json::json!({"change": key, "commit": tip, "name": "ci/tests",
                               "state": "green", "sent_at": now_ms()}),
            "state",
        ),
        (
            serde_json::json!({"change": key, "commit": tip, "name": "ci/../../etc",
                               "state": "passing", "sent_at": now_ms()}),
            "name",
        ),
        (
            serde_json::json!({"change": key, "commit": tip, "name": "x".repeat(101),
                               "state": "passing", "sent_at": now_ms()}),
            "name",
        ),
        (
            serde_json::json!({"change": key, "commit": tip, "name": "ci/tests",
                               "state": "passing", "url": "javascript:alert(1)",
                               "sent_at": now_ms()}),
            "url",
        ),
        (
            serde_json::json!({"change": key, "commit": tip, "name": "ci/tests",
                               "state": "passing", "url": format!("https://x/{}", "y".repeat(1000)),
                               "sent_at": now_ms()}),
            "url",
        ),
        (
            serde_json::json!({"change": key, "commit": tip, "name": "ci/tests",
                               "state": "passing", "summary": "green\u{0}",
                               "sent_at": now_ms()}),
            "summary",
        ),
        (
            serde_json::json!({"change": key, "commit": tip, "name": "ci/tests",
                               "state": "passing"}),
            "missing sent_at",
        ),
    ] {
        let b = bad_body.to_string();
        let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
        assert_eq!(st, 400, "{why} was accepted: {out}");
    }

    // Not JSON at all, having been correctly signed.
    for b in ["", "{", "[]", "null", "\u{feff}{}"] {
        let (st, out) = post_signed(server, route, Some(&sign(b)), b);
        assert_eq!(st, 400, "{b:?}: {out}");
    }

    // --- what the secret may say about ---------------------------------

    // A change that does not exist, and one whose key is not a key.
    for change in [
        "Inosuchchange",
        "../../etc/passwd",
        "'; DROP TABLE change_checks; --",
    ] {
        let b = intake_body(change, &tip, "ci/tests", "passing", now_ms());
        let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
        assert_eq!(st, 404, "{change:?}: {out}");
    }

    // A verdict about a commit that is not the latest patchset. This is
    // the race a retrying runner actually hits, and marking the new
    // patchset green on the strength of a build of the old one is the
    // expensive way to be wrong.
    let stale_commit = "0".repeat(40);
    let b = intake_body(&key, &stale_commit, "ci/tests", "passing", now_ms());
    let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"].as_str().unwrap().contains(&stale_commit),
        "{out}"
    );

    // An abandoned change is a report about nothing.
    let (abandoned, ab_tip) = open_change(
        server,
        admin,
        "app",
        "Iabba0001",
        "abandon-feature",
        ("src/d.rs", "fn d() {}"),
    );
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/app/changes/{abandoned}/abandon"),
        admin,
        None,
    );
    assert_eq!(st, 204, "{out}");
    let b = intake_body(&abandoned, &ab_tip, "ci/tests", "passing", now_ms());
    let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
    assert_eq!(st, 409, "{out}");

    // --- revocation ----------------------------------------------------

    let (st, out) = server.get("/v1/orgs/acme/repos/app/ci/secret", admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["configured"], serde_json::json!(true));
    assert!(out.get("secret").is_none(), "the secret is readable: {out}");

    let (st, out) = server.delete("/v1/orgs/acme/repos/app/ci/secret", admin);
    assert_eq!(st, 204, "{out}");
    let b = intake_body(&key, &tip, "ci/tests", "failing", now_ms());
    let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
    assert_eq!(st, 404, "a revoked secret still worked: {out}");
    let (st, out) = server.get("/v1/orgs/acme/repos/app/ci/secret", admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["configured"], serde_json::json!(false));

    // Rotation invalidates the previous holder, which is the only story
    // a "shown once" secret can offer for a leak.
    let rotated = mint_secret(server, admin, "app");
    assert_ne!(rotated, secret);
    let b = intake_body(&key, &tip, "ci/rotated", "passing", now_ms());
    let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
    assert_eq!(st, 404, "the old secret still worked after rotation: {out}");
    let (st, out) = post_signed(server, route, Some(&doc_signature(&rotated, &b)), &b);
    assert_eq!(st, 201, "{out}");

    // --- nothing moved -------------------------------------------------

    // Every refusal above was aimed at `ci/tests`, and most of them tried
    // to turn it red. A server that survived by wedging, or that let one
    // through, both fail here.
    let (st, checks) = server.get(
        &format!("/v1/orgs/acme/repos/app/changes/{key}/checks"),
        admin,
    );
    assert_eq!(st, 200, "{checks}");
    let tests = checks["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "ci/tests")
        .unwrap_or_else(|| panic!("{checks}"));
    assert_eq!(tests["state"], serde_json::json!("passing"), "{checks}");

    // Still serving, and still serving *this*.
    assert!(server.healthy());
    let (st, svg, _) = get_raw(server, "/v1/orgs/acme/repos/app/badge.svg", None);
    assert_eq!(st, 200, "{svg}");
}

/// The intake secret is an authorization fact about the repository, so
/// only somebody who may change the repository may mint or revoke it,
/// and the trail says who did.
#[test]
fn minting_an_intake_secret_needs_write_and_is_recorded() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-authz");
    let scratch = Scratch::new("ci-intake-authz");
    let w = world(spawn_server(&bucket.base_url, &scratch, "ci-intake-authz"));
    let (server, admin) = (&w.server, &w.admin);

    // Anonymous, and a credential from another org: both refused, and
    // the private repo stays masked.
    let rival = server.bootstrap_org("rival");
    assert_eq!(
        server.status_post(
            "/v1/orgs/acme/repos/app/ci/secret",
            "",
            serde_json::json!({})
        ),
        401
    );
    assert_eq!(
        server.status_post(
            "/v1/orgs/acme/repos/vault/ci/secret",
            &rival,
            serde_json::json!({})
        ),
        404
    );

    // A read-only credential may ask whether one is configured but not
    // mint one.
    let (st, out) = server.post(
        "/v1/orgs/acme/tokens",
        admin,
        Some(serde_json::json!({"name": "reader", "scopes": ["repo:read"]})),
    );
    assert_eq!(st, 201, "{out}");
    let reader = out["token"].as_str().unwrap().to_string();
    assert_eq!(
        server.status_post(
            "/v1/orgs/acme/repos/app/ci/secret",
            &reader,
            serde_json::json!({})
        ),
        404
    );
    let (st, out) = server.get("/v1/orgs/acme/repos/app/ci/secret", &reader);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["configured"], serde_json::json!(false));

    let first = mint_secret(server, admin, "app");
    let second = mint_secret(server, admin, "app");
    assert_ne!(first, second, "rotation must actually rotate");

    let (st, audit) = server.get("/v1/orgs/acme/audit?limit=50", admin);
    assert_eq!(st, 200, "{audit}");
    let rotations = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "ci.secret.rotate")
        .count();
    assert_eq!(rotations, 2, "{audit}");

    assert!(server.healthy());
}

// =====================================================================
// The other two writers into `check_runs`
// =====================================================================
//
// One table, two writers, and the whole point of the table is that a
// Checks tab never learns which of them put a row there. So both are
// exercised here, against the same reader: `stratum_control::checks`,
// read out of the server's own control database. Reading through the
// control plane rather than through an HTTP route is deliberate — it is
// the seam the two writers actually meet at, and a test that went
// through the read API would be testing the read API.
//
// The refusals are the half that matters. A poller that cannot page is
// a poller that is quietly a hundred runs short; a poller that reports
// "no CI" when it was refused sends a maintainer to look for a problem
// in their workflow files that is really a permission on an App; and an
// intake that accepted a body naming nothing would write a verdict about
// no commit at all.

use std::time::Instant;
use stratum_control::checks::{self, RunQuery};
use stratum_control::{imports, jobs, registry, ControlDb};

/// A server wired to the fake GitHub, with the poller running.
///
/// `STRATUM_MIRROR_POLL_SECS=0` because this file is about verdicts: a
/// git fetch against the fake would only add a way to fail that has
/// nothing to do with them.
fn spawn_polling_server(
    store_url: &str,
    scratch: &Scratch,
    hint: &str,
    api_base: &str,
    key: &std::path::Path,
    pages_per_run: &str,
) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_GITHUB_APP_ID", "12345")
        .env("STRATUM_GITHUB_APP_KEY_PEM", key.display().to_string())
        .env("STRATUM_GITHUB_API_BASE", api_base)
        .env(
            "STRATUM_GITHUB_INSTALL_URL",
            "https://github.com/apps/stratum/installations/new",
        )
        .env("STRATUM_MIRROR_POLL_SECS", "0")
        .env("STRATUM_CHECKS_POLL_SECS", "1")
        .env("STRATUM_CHECKS_PAGES_PER_RUN", pages_per_run)
        .start()
}

struct Polling {
    _scratch: Scratch,
    _gh: stratum_testkit::fake_github::FakeGithub,
    server: Server,
    admin: String,
    db: ControlDb,
    org_id: String,
}

fn polling_world(hint: &'static str, pages_per_run: &str) -> Polling {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let gh = stratum_testkit::fake_github::spawn();
    let key = scratch.path().join("app-key.pem");
    std::fs::write(&key, stratum_testkit::fake_github::TEST_APP_KEY_PEM).unwrap();
    let server = spawn_polling_server(
        &bucket.base_url,
        &scratch,
        hint,
        &gh.base_url,
        &key,
        pages_per_run,
    );
    let admin = server.bootstrap_org("acme");
    // Every mirror in here goes through installation 777, and creation
    // refuses an installation the org has not connected.
    server.connect_installation("acme", &admin, "777");
    let db = ControlDb::open(&server.db_url).expect("read the server's own control plane");
    let org_id = registry::org_by_name(&db, "acme")
        .expect("org")
        .expect("acme exists")
        .id;
    Polling {
        _scratch: scratch,
        _gh: gh,
        server,
        admin,
        db,
        org_id,
    }
}

/// A mirror row pointing at `full_name` — the origin a poll reads.
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
    assert!(st == 202 || st == 201, "create mirror {name}: {st} {out}");
}

fn repo_id(w: &Polling, name: &str) -> String {
    registry::repo_by_name(&w.db, &w.org_id, name)
        .expect("repo lookup")
        .unwrap_or_else(|| panic!("no repo {name}"))
        .id
}

/// Enqueue one poll job directly, and hand back its id.
///
/// Directly rather than through `POST /ci/poll` so that every poller
/// test below stands on its own: the route is a convenience for the UI
/// and is tested once, on its own, where a failure names the route
/// rather than looking like a broken poller.
fn enqueue_poll(w: &Polling, repo: &str) -> String {
    poll_job(w, Some(&repo_id(w, repo)))
}

/// The poll row to drive for `repo_id`.
///
/// A mirror's creation arms a poll of its own, from the initial sync
/// that runs in the background — and a second active row for one
/// repository is refused by the unique index. Reading "is there one
/// already" and then creating is a race against that sync, and CI
/// lost it once: the sync's row landed between the read and the
/// create, and `jobs::create` answered "db error". `enqueue_unique`
/// is the same statement the sync uses, so whichever of the two lands
/// first, the other is a no-op and the row read back is the one to
/// drive.
fn poll_job(w: &Polling, repo_id: Option<&str>) -> String {
    let Some(rid) = repo_id else {
        return jobs::create(&w.db, &w.org_id, None, "checkspoll", None)
            .expect("enqueue a poll")
            .id;
    };
    jobs::enqueue_unique(&w.db, &w.org_id, rid, "checkspoll", None).expect("enqueue a poll");
    jobs::latest_for_repo(&w.db, &w.org_id, rid, "checkspoll")
        .expect("read jobs")
        .expect("a poll row exists after enqueue_unique")
        .id
}

/// Every run recorded for a repository, oldest paging key last.
fn all_runs(w: &Polling, repo: &str) -> Vec<checks::CheckRun> {
    let id = repo_id(w, repo);
    let mut out: Vec<checks::CheckRun> = Vec::new();
    let mut before: Option<i64> = None;
    loop {
        let (page, next) = checks::list(
            &w.db,
            &id,
            &RunQuery {
                limit: 100,
                before,
                ..Default::default()
            },
        )
        .expect("list runs");
        let empty = page.is_empty();
        out.extend(page);
        match next {
            Some(c) if !empty => before = Some(c),
            _ => return out,
        }
    }
}

fn phase(w: &Polling, repo: &str, phase: &str) -> Option<String> {
    imports::cursor(&w.db, &repo_id(w, repo), phase)
        .expect("read poller state")
        .filter(|v| !v.is_empty())
}

/// Wait for a poll job to leave the queue, and hand back the row.
fn await_job(w: &Polling, id: &str) -> jobs::Job {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let job = jobs::get(&w.db, &w.org_id, id)
            .expect("read job")
            .expect("the job exists");
        if job.state == "done" || job.state == "failed" {
            return job;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the poll job never finished");
}

/// Wait until a repository has completed at least one walk.
fn await_polled(w: &Polling, repo: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if phase(w, repo, "checks:github:high-water").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the poll never completed a walk for {repo}");
}

/// The fake's run ids are `90_000_000 + n`, and its `head_sha` for run
/// `n` is `n` in forty hex digits. Both are needed to assert that a
/// particular run came across whole.
fn external_id(n: usize) -> String {
    (90_000_000 + n).to_string()
}

/// "Mirrored from GitHub — you do nothing." The docs said so, and the
/// only thing that ever enqueued a poll was `POST …/ci/poll`: the first
/// real mirror on weft.sh sat on "first poll has not finished" for hours,
/// and nothing about a later push would have changed that. Creating a
/// mirror is now enough — its first sync arms the poll, whether or not
/// the git fetch itself succeeded (this world has no git origin at all).
#[test]
fn a_mirror_is_polled_without_anyone_asking() {
    let w = polling_world("checks-poll-unasked", "5");
    mirror(&w.server, &w.admin, "widget", "acme/widget");
    // No `enqueue_poll`, no `POST …/ci/poll`.
    await_polled(&w, "widget");
    let runs = all_runs(&w, "widget");
    assert_eq!(runs.len(), 7, "the fixture is seven runs: {runs:#?}");
    // And a poll is armed again by every later sync, so a run that
    // finishes after this walk is seen by the next one. Deduplicated:
    // one active row, however many syncs land while it is queued.
    for _ in 0..3 {
        let (st, out) = w
            .server
            .post("/v1/orgs/acme/mirrors/widget/sync", &w.admin, None);
        assert!(st == 200 || st == 502, "sync answered {st}: {out}");
    }
    let latest = jobs::latest_for_repo(&w.db, &w.org_id, &repo_id(&w, "widget"), "checkspoll")
        .expect("read jobs")
        .expect("a sync armed a poll");
    assert!(
        matches!(latest.state.as_str(), "queued" | "running" | "done"),
        "the re-armed poll is {latest:?}"
    );
    assert!(w.server.healthy());
}

#[test]
fn a_poll_records_every_run_with_the_fields_a_checks_tab_shows() {
    let w = polling_world("checks-poll-fields", "5");
    mirror(&w.server, &w.admin, "widget", "acme/widget");
    let job = enqueue_poll(&w, "widget");
    let done = await_job(&w, &job);
    assert_eq!(done.state, "done", "{done:?}");
    await_polled(&w, "widget");

    let runs = all_runs(&w, "widget");
    assert_eq!(runs.len(), 7, "the fixture is seven runs: {runs:#?}");
    assert!(
        runs.iter().all(|r| r.provider == "github"),
        "a poller's rows say who reported them: {runs:#?}"
    );

    let by_id = |n: usize| -> checks::CheckRun {
        runs.iter()
            .find(|r| r.external_id.as_deref() == Some(external_id(n).as_str()))
            .unwrap_or_else(|| panic!("run {n} is missing: {runs:#?}"))
            .clone()
    };

    // Run 7 is `completed/success` in the fixture's `n % 7` walk, and
    // it is the one every optional field is present on.
    let seven = by_id(7);
    assert_eq!(seven.state, "passing", "{seven:?}");
    assert_eq!(seven.name, "CI", "{seven:?}");
    assert_eq!(seven.commit_sha, format!("{:040x}", 7), "{seven:?}");
    assert_eq!(seven.ref_name.as_deref(), Some("main"), "{seven:?}");
    assert_eq!(seven.event.as_deref(), Some("push"), "{seven:?}");
    assert_eq!(seven.actor.as_deref(), Some("octocat-7"), "{seven:?}");
    assert_eq!(seven.run_number, Some(7), "{seven:?}");
    assert_eq!(
        seven.detail_url.as_deref(),
        Some("https://github.com/acme/widget/actions/runs/90000007"),
        "every row is one click from the log it summarises: {seven:?}"
    );
    assert!(seven.started_at.is_some(), "{seven:?}");
    assert!(
        seven.completed_at.is_some(),
        "a finished run has a finish time: {seven:?}"
    );

    // The whole (status, conclusion) space the fixture walks, including
    // the conclusion this codebase has never heard of. `action_required`
    // must degrade to "we do not know yet" — a green tick there is the
    // one wrong answer somebody merges on.
    for (n, want) in [
        (1, "running"),
        (2, "failing"),
        (3, "queued"),
        (4, "cancelled"),
        (5, "skipped"),
        (6, "queued"),
        (7, "passing"),
    ] {
        assert_eq!(by_id(n).state, want, "run {n}");
    }

    // The three fields the fixture deliberately leaves out, because they
    // are missing on real runs too. A default in any of them is a
    // confident wrong answer, not a blank.
    assert_eq!(by_id(5).actor, None, "a deleted account is not a name");
    assert_eq!(by_id(6).ref_name, None, "a tag build has no branch");
    assert_eq!(
        by_id(2).started_at,
        None,
        "a run predating `run_started_at` is not dated to 1970"
    );
    // …and an in-flight run has no completion time, whatever its last
    // heartbeat was.
    assert_eq!(by_id(1).completed_at, None, "still running");
    assert_eq!(by_id(3).completed_at, None, "still queued");

    assert!(w.server.healthy());
}

/// The property the whole `external_id` design exists for.
///
/// A provider polls, and polls again, and the second poll is about the
/// *same runs*. A forge that inserted a second row would show a commit
/// with two contradictory verdicts beside it, which is worse than
/// showing none — a reader cannot tell which one is current.
///
/// Proved by moving a row away from what GitHub says and watching a
/// re-poll move it back **in the same row**: same id, same `created_at`,
/// which is what makes it an update rather than a replacement.
#[test]
fn a_re_poll_updates_the_same_rows_rather_than_adding_more() {
    let w = polling_world("checks-poll-repoll", "5");
    mirror(&w.server, &w.admin, "widget", "acme/widget");
    await_job(&w, &enqueue_poll(&w, "widget"));
    await_polled(&w, "widget");
    assert_eq!(all_runs(&w, "widget").len(), 7);

    let id = repo_id(&w, "widget");
    let before = all_runs(&w, "widget")
        .into_iter()
        .find(|r| r.external_id.as_deref() == Some(external_id(7).as_str()))
        .expect("run 7");
    assert_eq!(before.state, "passing");

    // Move it, through the same door the poller writes through, keeping
    // the identity GitHub gave it.
    let moved = checks::upsert(
        &w.db,
        &id,
        &checks::NewCheckRun {
            commit_sha: &before.commit_sha,
            ref_name: None,
            provider: "github",
            external_id: before.external_id.as_deref(),
            name: &before.name,
            run_number: None,
            event: None,
            state: checks::RunState::Failing,
            detail_url: None,
            actor: None,
            started_at: None,
            completed_at: None,
        },
    )
    .expect("move the run");
    assert_eq!(moved.id, before.id, "the upsert took the same row");
    assert_eq!(
        all_runs(&w, "widget").len(),
        7,
        "a report about a known run must not add a row"
    );

    // Now poll again. GitHub still says `passing`, so the row must come
    // back — and it must be the same row.
    await_job(&w, &enqueue_poll(&w, "widget"));
    let deadline = Instant::now() + Duration::from_secs(60);
    let after = loop {
        let got = all_runs(&w, "widget")
            .into_iter()
            .find(|r| r.id == before.id)
            .expect("the row is still there");
        if got.state == "passing" || Instant::now() > deadline {
            break got;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(after.state, "passing", "the re-poll did not restate it");
    assert_eq!(
        all_runs(&w, "widget").len(),
        7,
        "the re-poll added rows instead of updating: {:#?}",
        all_runs(&w, "widget")
    );
    assert_eq!(
        after.created_at, before.created_at,
        "`created_at` is the paging key and must not move, or a busy \
         repository's runs shuffle past a reader all afternoon"
    );
    assert!(after.updated_at >= before.updated_at, "{after:?}");

    assert!(w.server.healthy());
}

/// A history longer than one page, walked to the end.
///
/// `many/*` is 250 runs: three pages at a hundred, with a short last
/// one. A poller that counted pages instead of following `Link` would
/// stop early on a full page, and nothing anywhere would say so.
#[test]
fn a_long_history_is_paged_to_the_end_by_following_link() {
    let w = polling_world("checks-poll-paging", "5");
    mirror(&w.server, &w.admin, "big", "many/widget");
    let done = await_job(&w, &enqueue_poll(&w, "big"));
    assert_eq!(done.state, "done", "{done:?}");
    await_polled(&w, "big");

    let runs = all_runs(&w, "big");
    assert_eq!(runs.len(), 250, "the walk stopped short of the last page");
    // The oldest run is the one a page-counting walk loses, and the
    // newest is the one a walk that never started would.
    for n in [1usize, 100, 101, 250] {
        assert!(
            runs.iter()
                .any(|r| r.external_id.as_deref() == Some(external_id(n).as_str())),
            "run {n} is missing"
        );
    }
    assert!(
        phase(&w, "big", "checks:github:cursor").is_none(),
        "a finished walk left a resume point behind"
    );
    assert!(w.server.healthy());
}

/// One run of the poller is bounded, and the walk resumes.
///
/// With a budget of one page, 250 runs cannot arrive in a single run of
/// the worker — they arrive only if each run writes where it got to and
/// re-enqueues. A poller that did not would sit at a hundred forever.
#[test]
fn a_bounded_run_re_enqueues_and_the_walk_resumes_where_it_stopped() {
    let w = polling_world("checks-poll-resume", "1");
    mirror(&w.server, &w.admin, "big", "many/widget");
    let first = await_job(&w, &enqueue_poll(&w, "big"));
    assert_eq!(first.state, "done", "{first:?}");
    // What *that run* did, which is a fact about it and not a race with
    // the re-enqueued one already running: one page, a hundred runs,
    // and "the work is not finished". Counting rows here instead would
    // be counting whatever the next run had got to by the time the
    // assertion read them.
    assert_eq!(
        first.result.as_deref().unwrap_or_default(),
        "More { runs: 100 }",
        "a one-page budget walked more than one page, or called it done: {first:?}"
    );

    // The worker re-enqueued itself, so the rest arrives without anybody
    // asking again.
    await_polled(&w, "big");
    assert_eq!(all_runs(&w, "big").len(), 250);
    assert!(w.server.healthy());
}

/// The refusal that must never look like an empty repository.
///
/// `noperm/*` answers 403 with GitHub's own wording for an installation
/// without `actions: read`; `empty/*` answers a real, empty list. A
/// Checks tab that rendered both as nothing would tell a maintainer
/// their project has no CI when the truth is that we may not see it.
#[test]
fn a_refused_actions_read_is_recorded_and_is_not_a_repository_with_no_ci() {
    let w = polling_world("checks-poll-noperm", "5");
    mirror(&w.server, &w.admin, "locked", "noperm/widget");
    mirror(&w.server, &w.admin, "quiet", "empty/widget");

    let refused = await_job(&w, &enqueue_poll(&w, "locked"));
    let listed = await_job(&w, &enqueue_poll(&w, "quiet"));

    // Both have no runs. That is the entire trap: the counts are equal
    // and the states must not be.
    assert!(all_runs(&w, "locked").is_empty());
    assert!(all_runs(&w, "quiet").is_empty());

    assert_eq!(refused.state, "failed", "a refusal is not a success");
    let why = refused.error.unwrap_or_default();
    assert!(
        why.contains("actions: read"),
        "the failure does not name the permission, so nobody can act on it: {why}"
    );

    let recorded = phase(&w, "locked", "checks:github:error")
        .expect("the refusal is durable, not only in a worker's log");
    assert_eq!(recorded, why, "the API reads what the job reported");
    assert!(
        phase(&w, "locked", "checks:github:high-water").is_none(),
        "a refused poll declared itself to have looked"
    );

    // The repository with no CI is the opposite of all three.
    assert_eq!(listed.state, "done", "{listed:?}");
    assert!(phase(&w, "quiet", "checks:github:error").is_none());
    assert!(
        phase(&w, "quiet", "checks:github:high-water").is_some(),
        "a repository we looked at and found nothing in must record that \
         we looked — otherwise it is indistinguishable from one we never \
         polled, which is the same lie in a different direction"
    );

    assert!(w.server.healthy());
}

/// Being asked to come back later is a normal state, and it is obeyed.
///
/// The "not before" mark is what a rate limit leaves behind, and a
/// claimed job that dialled GitHub anyway is how a rate limit becomes a
/// ban. Driven by planting the mark rather than by provoking a 429:
/// `fake_github`'s Actions route has no rate-limit path (its every-third
/// -call refusal lives in the issues route, deliberately, so that paging
/// tests do not test the refusal schedule). That the mark is *set* from
/// a 429 is the one line here no test can reach today, and it is
/// reported rather than papered over — `stratum-testkit` belongs to
/// another track.
#[test]
fn a_poll_asked_to_wait_does_not_call_github_until_it_may() {
    let w = polling_world("checks-poll-backoff", "5");
    mirror(&w.server, &w.admin, "widget", "acme/widget");
    let id = repo_id(&w, "widget");
    let later = now_ms() + 10 * 60 * 1000;
    imports::set_cursor(&w.db, &id, "checks:github:after", &later.to_string())
        .expect("plant the not-before mark");

    let job = await_job(&w, &enqueue_poll(&w, "widget"));
    assert_eq!(job.state, "done", "a backoff is not a failure: {job:?}");
    assert!(
        job.result
            .as_deref()
            .unwrap_or_default()
            .contains("Backoff"),
        "{job:?}"
    );
    assert!(
        all_runs(&w, "widget").is_empty(),
        "the poll called GitHub while it had been asked not to"
    );
    assert!(
        phase(&w, "widget", "checks:github:high-water").is_none(),
        "a poll that never looked claimed to have looked"
    );

    // And when the mark passes, the same repository polls normally —
    // the backoff is a delay, not a dead end.
    imports::set_cursor(&w.db, &id, "checks:github:after", "0").expect("clear the mark");
    await_polled(&w, "widget");
    assert_eq!(all_runs(&w, "widget").len(), 7);
    assert!(w.server.healthy());
}

/// A repository with no GitHub origin is nothing to poll, and saying so
/// is a refusal with a sentence rather than a job that fails a minute
/// later in a log.
#[test]
fn the_poll_route_reports_state_and_refuses_a_repository_with_no_origin() {
    let w = polling_world("checks-poll-route", "5");
    mirror(&w.server, &w.admin, "widget", "acme/widget");
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos",
        &w.admin,
        Some(serde_json::json!({"name": "native", "public": true})),
    );
    assert_eq!(st, 201, "{out}");

    let (st, refused) = w
        .server
        .post("/v1/orgs/acme/repos/native/ci/poll", &w.admin, None);
    assert_eq!(st, 400, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("mirror"),
        "the refusal does not name the way through: {refused}"
    );

    // Before anything has been polled, the state says so — `polled:
    // false` is "we have not looked", which the UI must not render as
    // "there is nothing to see".
    let (st, before) = w.server.get("/v1/orgs/acme/repos/widget/ci/poll", &w.admin);
    assert_eq!(st, 200, "{before}");
    assert_eq!(before["connected"], serde_json::json!(true), "{before}");
    assert_eq!(before["polled"], serde_json::json!(false), "{before}");
    assert_eq!(before["denied"], serde_json::json!(false), "{before}");

    let (st, queued) = w
        .server
        .post("/v1/orgs/acme/repos/widget/ci/poll", &w.admin, None);
    assert_eq!(st, 202, "{queued}");
    await_polled(&w, "widget");
    assert_eq!(all_runs(&w, "widget").len(), 7);

    let (st, after) = w.server.get("/v1/orgs/acme/repos/widget/ci/poll", &w.admin);
    assert_eq!(st, 200, "{after}");
    assert_eq!(after["polled"], serde_json::json!(true), "{after}");
    assert_eq!(after["denied"], serde_json::json!(false), "{after}");
    assert_eq!(after["error"], serde_json::Value::Null, "{after}");
    assert!(after["high_water"].as_i64().unwrap_or(0) > 0, "{after}");

    // …and the refusal is reported as a refusal, with the flag a UI
    // needs to offer a re-approve link instead of an empty table.
    mirror(&w.server, &w.admin, "locked", "noperm/widget");
    await_job(&w, &enqueue_poll(&w, "locked"));
    let (st, denied) = w.server.get("/v1/orgs/acme/repos/locked/ci/poll", &w.admin);
    assert_eq!(st, 200, "{denied}");
    assert_eq!(denied["denied"], serde_json::json!(true), "{denied}");
    assert_eq!(denied["polled"], serde_json::json!(false), "{denied}");
    assert!(
        denied["error"]
            .as_str()
            .unwrap_or_default()
            .contains("actions: read"),
        "{denied}"
    );

    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// The intake's second shape: a verdict about a commit
// ---------------------------------------------------------------------

/// A commit-scoped body: no `change`, and every optional field the shape
/// carries, so that a test asserting they arrive is asserting about
/// something that was actually sent.
fn commit_body(commit: &str, name: &str, state: &str, sent_at: i64) -> String {
    serde_json::json!({
        "commit": commit,
        "name": name,
        "state": state,
        "url": "https://ci.example/runs/17",
        "summary": "42 passed, 0 failed",
        "sent_at": sent_at,
        "external_id": "buildkite-17",
        "ref": "main",
        "run_number": 17,
        "event": "push",
        "actor": "ada",
        "started_at": 1_700_000_000_000i64,
        "completed_at": 1_700_000_060_000i64,
    })
    .to_string()
}

/// Both intake shapes, against the one table each is supposed to write.
///
/// The load-bearing assertion is the *separation*. `change_checks` is
/// the per-patchset merge gate — three states, invalidated by a
/// force-push — and `check_runs` is a repository's history of verdicts
/// about commits. They are not two renderings of one fact, and a body
/// that named a change must not quietly also write a run: a Checks tab
/// would then show a row for every merge-gate report ever made, which is
/// not what that table is a history of.
#[test]
fn a_commit_scoped_report_writes_a_run_and_a_patchset_scoped_one_does_not() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-commit");
    let scratch = Scratch::new("ci-intake-commit");
    let w = world(spawn_server(&bucket.base_url, &scratch, "ci-intake-commit"));
    let (server, admin) = (&w.server, &w.admin);
    let secret = mint_secret(server, admin, "app");
    let route = "/v1/orgs/acme/repos/app/ci/checks";
    let db = ControlDb::open(&server.db_url).expect("the server's control plane");
    let org_id = registry::org_by_name(&db, "acme").unwrap().unwrap().id;
    let id = registry::repo_by_name(&db, &org_id, "app")
        .unwrap()
        .unwrap()
        .id;
    let runs = || {
        checks::list(
            &db,
            &id,
            &RunQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .expect("list runs")
        .0
    };

    // --- the patchset-scoped shape, unchanged --------------------------
    let (key, tip) = open_change(
        server,
        admin,
        "app",
        "Icccc0001",
        "commit-scoped-feature",
        ("src/e.rs", "fn e() {}"),
    );
    let b = intake_body(&key, &tip, "ci/tests", "passing", now_ms());
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
    assert_eq!(st, 201, "the merge gate still works: {out}");
    let (st, gate) = server.get(
        &format!("/v1/orgs/acme/repos/app/changes/{key}/checks"),
        admin,
    );
    assert_eq!(st, 200, "{gate}");
    assert_eq!(gate["checks"].as_array().unwrap().len(), 1, "{gate}");
    assert!(
        runs().is_empty(),
        "a patchset-scoped report wrote a check run: {:#?}",
        runs()
    );

    // --- the commit-scoped shape ---------------------------------------
    let b = commit_body(&tip, "nightly", "running", now_ms());
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["provider"], serde_json::json!("intake"), "{out}");
    let all = runs();
    assert_eq!(all.len(), 1, "{all:#?}");
    let run = &all[0];
    assert_eq!(run.commit_sha, tip);
    assert_eq!(run.provider, "intake");
    assert_eq!(run.external_id.as_deref(), Some("buildkite-17"));
    assert_eq!(run.name, "nightly");
    assert_eq!(run.state, "running");
    assert_eq!(run.ref_name.as_deref(), Some("main"));
    assert_eq!(run.event.as_deref(), Some("push"));
    assert_eq!(run.actor.as_deref(), Some("ada"));
    assert_eq!(run.run_number, Some(17));
    assert_eq!(
        run.detail_url.as_deref(),
        Some("https://ci.example/runs/17")
    );
    assert_eq!(run.started_at, Some(1_700_000_000_000));
    assert_eq!(run.completed_at, Some(1_700_000_060_000));
    // …and the two shapes are still distinguishable where it matters.
    //
    // This block used to assert that the commit-scoped run "did not touch
    // the merge gate", and read the change's checks route to prove it.
    // That route answered the intake's rows and nothing else, so the
    // assertion held for a reason that had nothing to do with the gate —
    // and it *pinned the bug*: `land_gate` has always merged a run
    // reported against the patchset's own commit, so this nightly did
    // count, and the only thing not counting it was the page.
    //
    // The real claim is about which table each shape writes, and it is
    // now asserted the way a reader can see it: both rows come back, each
    // saying what it is a statement about.
    let (_, gate) = server.get(
        &format!("/v1/orgs/acme/repos/app/changes/{key}/checks"),
        admin,
    );
    let rows = gate["checks"].as_array().expect("a checks array");
    assert_eq!(rows.len(), 2, "{gate}");
    let by_name = |n: &str| -> serde_json::Value {
        rows.iter()
            .find(|r| r["name"] == serde_json::json!(n))
            .unwrap_or_else(|| panic!("{n} is missing: {gate}"))
            .clone()
    };
    assert_eq!(by_name("ci/tests")["source"], serde_json::json!("patchset"));
    assert_eq!(by_name("nightly")["source"], serde_json::json!("commit"));
    assert_eq!(by_name("nightly")["posted_by"], serde_json::json!("intake"));

    // The same run, reported again as it finishes. One row, moved.
    let b = commit_body(&tip, "nightly", "failing", now_ms());
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
    assert_eq!(st, 200, "{out}");
    let all = runs();
    assert_eq!(all.len(), 1, "a second report added a row: {all:#?}");
    assert_eq!(all[0].state, "failing");
    assert_eq!(all[0].id, run.id, "it is the same row");

    // A report about a commit that is not any change's tip. This is the
    // case the whole shape exists for: a nightly against `main`, which
    // the patchset-scoped route cannot express at all.
    let (st, refs) = server.get("/v1/orgs/acme/repos/app/refs", admin);
    assert_eq!(st, 200, "{refs}");
    let main = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .expect("main")["oid"]
        .as_str()
        .unwrap()
        .to_string();
    //
    // Its own `external_id`, because that is the identity: the same id
    // against a different commit is the *same run*, moved, which is
    // right for a provider that re-reports and wrong for a second
    // build. Nothing is left to infer from the commit.
    let b = serde_json::json!({
        "commit": main, "name": "trunk-nightly", "state": "failing",
        "external_id": "buildkite-18", "sent_at": now_ms(),
    })
    .to_string();
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
    assert_eq!(st, 200, "{out}");
    assert_eq!(runs().len(), 2, "{:#?}", runs());

    // The six, not the three: every state a run can be in is accepted,
    // and each is stored as itself.
    for (n, state) in [
        "queued",
        "running",
        "passing",
        "failing",
        "cancelled",
        "skipped",
    ]
    .into_iter()
    .enumerate()
    {
        let b = serde_json::json!({
            "commit": main, "name": format!("suite-{n}"), "state": state,
            "sent_at": now_ms(),
        })
        .to_string();
        let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
        assert_eq!(st, 200, "{state} was refused: {out}");
        assert_eq!(out["state"], serde_json::json!(state), "{out}");
    }

    assert!(server.healthy());
}

/// An uppercase sha is accepted, **normalised**, and readable at the
/// sha git actually prints.
///
/// The bug this pins: `commit_ok` was `len() == 40 && all
/// is_ascii_hexdigit`, and `is_ascii_hexdigit` accepts `A-F`. So an
/// uppercase sha passed validation, `checks::upsert` stored it verbatim,
/// and every read matches `commit_sha = $2` exactly. A CI script that
/// uppercased its sha anywhere — a `tr` in a pipeline, a Windows
/// toolchain, someone reading "40-hex" the way the rest of the world
/// does — got a 200 and a run id, and the commit page, fetching with the
/// lowercase sha git gave it, got `{"runs": []}`. Neither side errored.
/// Nobody would ever have reported it as anything but "our checks do not
/// show up".
///
/// Three separate failures follow from it and all three are asserted
/// here, because fixing one is easy and fixing one is not the fix:
///
/// 1. the row is unreadable at the real sha;
/// 2. the id-less upsert identity is `(repo_id, provider, commit_sha,
///    name)`, so one run reported once in each case becomes two rows;
/// 3. the patchset path compares `latest.commit_oid != parsed.commit`
///    byte for byte and answered **409 "this reports on ABC123…, but the
///    latest patchset is abc123…"** — two shas that read as identical to
///    the person holding the message.
#[test]
fn an_uppercase_sha_is_stored_and_read_at_the_sha_git_prints() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-sha-case");
    let scratch = Scratch::new("ci-intake-sha-case");
    let w = world(spawn_server(
        &bucket.base_url,
        &scratch,
        "ci-intake-sha-case",
    ));
    let (server, admin) = (&w.server, &w.admin);
    let secret = mint_secret(server, admin, "app");
    let route = "/v1/orgs/acme/repos/app/ci/checks";

    let (st, refs) = server.get("/v1/orgs/acme/repos/app/refs", admin);
    assert_eq!(st, 200, "{refs}");
    let lower = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .expect("main")["oid"]
        .as_str()
        .unwrap()
        .to_string();
    let upper = lower.to_ascii_uppercase();
    assert_ne!(upper, lower, "the fixture sha has no letters in it");

    // --- a commit-scoped report, sent uppercase ------------------------
    let b = commit_body(&upper, "nightly", "passing", now_ms());
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
    assert_eq!(st, 200, "an uppercase sha was refused outright: {out}");
    assert_eq!(
        out["commit"],
        serde_json::json!(lower),
        "the response echoed back a spelling that is not what was stored: {out}"
    );

    // The assertion the whole fix is for: git prints the lowercase sha,
    // the commit page fetches with it, and the run must be there.
    let commit_runs = |sha: &str| {
        let (st, body) = server.get(
            &format!("/v1/orgs/acme/repos/app/commits/{sha}/checks"),
            admin,
        );
        assert_eq!(st, 200, "{body}");
        body["runs"].as_array().expect("a runs array").clone()
    };
    let at_lower = commit_runs(&lower);
    assert_eq!(
        at_lower.len(),
        1,
        "a run reported with an uppercase sha is invisible at the sha \
         git prints — the CI script got a 200 and a run id for a row \
         nothing can ever find"
    );
    assert_eq!(at_lower[0]["name"], serde_json::json!("nightly"));
    assert_eq!(at_lower[0]["state"], serde_json::json!("passing"));

    // And the read side normalises too, so a caller who sends the
    // uppercase sha to the read route gets the same one row. One half of
    // this pair without the other orphans rows in the other direction.
    let at_upper = commit_runs(&upper);
    assert_eq!(at_upper.len(), 1, "{at_upper:?}");
    assert_eq!(at_upper[0]["id"], at_lower[0]["id"], "two different rows");

    // --- the split identity --------------------------------------------
    // The same run, reported again in the other case. `check_runs` keys
    // an id-less upsert on (repo_id, provider, commit_sha, name), so a
    // stored spelling that differs is a second row for one run.
    let b = commit_body(&lower, "nightly", "failing", now_ms());
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
    assert_eq!(st, 200, "{out}");
    let (st, all) = server.get("/v1/orgs/acme/repos/app/checks/runs", admin);
    assert_eq!(st, 200, "{all}");
    let rows = all["runs"].as_array().expect("a runs array");
    assert_eq!(
        rows.len(),
        1,
        "one run reported twice, once in each case, became two rows: {all}"
    );
    assert_eq!(rows[0]["id"], at_lower[0]["id"], "{all}");
    assert_eq!(rows[0]["state"], serde_json::json!("failing"), "{all}");

    // --- the patchset path ----------------------------------------------
    // `latest.commit_oid != parsed.commit` is a byte comparison, so
    // before the fix this answered 409 with a message naming two shas
    // that read as the same one.
    let (key, tip) = open_change(
        server,
        admin,
        "app",
        "Idddd0001",
        "sha-case-feature",
        ("src/f.rs", "fn f() {}"),
    );
    let b = intake_body(
        &key,
        &tip.to_ascii_uppercase(),
        "ci/tests",
        "passing",
        now_ms(),
    );
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
    assert_eq!(
        st, 201,
        "an uppercase sha naming the latest patchset was refused, in a \
         message whose two shas read as identical: {out}"
    );
    let (st, gate) = server.get(
        &format!("/v1/orgs/acme/repos/app/changes/{key}/checks"),
        admin,
    );
    assert_eq!(st, 200, "{gate}");
    assert_eq!(gate["checks"].as_array().unwrap().len(), 1, "{gate}");

    // A sha that is not hex is still refused; normalising is not
    // loosening.
    let b = commit_body(&"g".repeat(40), "nightly", "passing", now_ms());
    let (st, out) = post_signed(server, route, Some(&doc_signature(&secret, &b)), &b);
    assert_eq!(st, 400, "{out}");

    assert!(server.healthy());
}

/// Every way the new shape can be wrong, and the proof that none of the
/// old protections was loosened to let it in.
#[test]
fn the_commit_scoped_intake_refuses_every_bad_body_and_keeps_serving() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-commit-bad");
    let scratch = Scratch::new("ci-intake-commit-bad");
    let w = world(spawn_server(
        &bucket.base_url,
        &scratch,
        "ci-intake-commit-bad",
    ));
    let (server, admin) = (&w.server, &w.admin);
    let secret = mint_secret(server, admin, "app");
    let route = "/v1/orgs/acme/repos/app/ci/checks";
    let sign = |b: &str| doc_signature(&secret, b);
    let db = ControlDb::open(&server.db_url).expect("the server's control plane");
    let org_id = registry::org_by_name(&db, "acme").unwrap().unwrap().id;
    let id = registry::repo_by_name(&db, &org_id, "app")
        .unwrap()
        .unwrap()
        .id;
    let count = || {
        checks::list(
            &db,
            &id,
            &RunQuery {
                limit: 100,
                ..Default::default()
            },
        )
        .expect("list runs")
        .0
        .len()
    };
    let commit = "3f2a1b4c5d6e7f8091a2b3c4d5e6f708192a3b4c";

    // A body that names neither. The refusal has to name *both* doors:
    // "commit must be 40-hex" would send a reader hunting for a typo in
    // a field they never wrote.
    let nothing =
        serde_json::json!({"name": "ci", "state": "passing", "sent_at": now_ms()}).to_string();
    let (st, out) = post_signed(server, route, Some(&sign(&nothing)), &nothing);
    assert_eq!(st, 400, "{out}");
    let why = out["error"].as_str().unwrap_or_default();
    assert!(why.contains("change") && why.contains("commit"), "{why}");
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);

    // Every other refusal, each with the reason it must name.
    for (bad, expect) in [
        (
            serde_json::json!({"commit": "nothex", "name": "ci", "state": "passing",
                               "sent_at": now_ms()}),
            "commit",
        ),
        // The three-state vocabulary is the *other* table's. `pending`
        // is not a run state, and an unknown word is refused by name
        // rather than read as a default — quietly green ships broken
        // code and quietly red blocks good code.
        (
            serde_json::json!({"commit": commit, "name": "ci", "state": "pending",
                               "sent_at": now_ms()}),
            "pending",
        ),
        (
            serde_json::json!({"commit": commit, "name": "ci", "state": "success",
                               "sent_at": now_ms()}),
            "success",
        ),
        (
            serde_json::json!({"commit": commit, "name": "ci/../../etc", "state": "passing",
                               "sent_at": now_ms()}),
            "name",
        ),
        (
            serde_json::json!({"commit": commit, "name": "x".repeat(101), "state": "passing",
                               "sent_at": now_ms()}),
            "name",
        ),
        (
            serde_json::json!({"commit": commit, "name": "ci", "state": "passing",
                               "url": "javascript:alert(1)", "sent_at": now_ms()}),
            "url",
        ),
        (
            serde_json::json!({"commit": commit, "name": "ci", "state": "passing",
                               "ref": "main\u{1b}[31m", "sent_at": now_ms()}),
            "ref",
        ),
        (
            serde_json::json!({"commit": commit, "name": "ci", "state": "passing",
                               "actor": "x".repeat(201), "sent_at": now_ms()}),
            "actor",
        ),
        (
            serde_json::json!({"commit": commit, "name": "ci", "state": "passing",
                               "event": "push\u{0}", "sent_at": now_ms()}),
            "event",
        ),
        (
            serde_json::json!({"commit": commit, "name": "ci", "state": "passing",
                               "external_id": "a\nb", "sent_at": now_ms()}),
            "external_id",
        ),
        (
            serde_json::json!({"commit": commit, "name": "ci", "state": "passing",
                               "summary": "green\u{0}", "sent_at": now_ms()}),
            "summary",
        ),
    ] {
        let b = bad.to_string();
        let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
        assert_eq!(st, 400, "{expect} was accepted: {out}");
        assert!(
            out["error"].as_str().unwrap_or_default().contains(expect),
            "the refusal does not name what was wrong: {out}"
        );
        assert_eq!(count(), 0, "a refused body wrote a run: {out}");
        assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
    }

    // --- the protections that existed before are the same protections --

    // Unsigned, and signed wrong: still the one opaque answer, which is
    // the masking argument. A new body shape must not become a new
    // oracle.
    let good = commit_body(commit, "ci", "passing", now_ms());
    let (st, out) = post_signed(server, route, None, &good);
    assert_eq!(
        st, 404,
        "an unsigned commit-scoped body was accepted: {out}"
    );
    let (st, out) = post_signed(server, route, Some("sha256=zz"), &good);
    assert_eq!(st, 404, "{out}");
    assert_eq!(count(), 0);

    // Stale, and ahead: the freshness window is the same window.
    for skew in [-10 * 60 * 1000i64, 10 * 60 * 1000] {
        let b = commit_body(commit, "ci", "passing", now_ms() + skew);
        let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
        assert_eq!(st, 409, "skew {skew} was accepted: {out}");
        assert_eq!(count(), 0);
    }

    // Replayed: the same bytes twice, and the second is refused by the
    // same ring the patchset-scoped shape uses.
    let sig = sign(&good);
    let (st, out) = post_signed(server, route, Some(&sig), &good);
    assert_eq!(st, 200, "{out}");
    assert_eq!(count(), 1);
    let (st, out) = post_signed(server, route, Some(&sig), &good);
    assert_eq!(st, 409, "a replayed commit-scoped body was applied: {out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("already been applied"),
        "{out}"
    );
    assert_eq!(count(), 1, "the replay wrote a second row");

    // An oversized body is still refused by size, after the signature.
    let huge = serde_json::json!({
        "commit": commit, "name": "ci", "state": "passing", "sent_at": now_ms(),
        "padding": "p".repeat(20_000),
    })
    .to_string();
    let (st, out) = post_signed(server, route, Some(&sign(&huge)), &huge);
    assert_eq!(st, 413, "{out}");

    // Nothing moved, and the server is still serving the thing it was.
    assert_eq!(count(), 1);
    assert!(server.healthy());
    let (st, svg, _) = get_raw(server, "/v1/orgs/acme/repos/app/badge.svg", None);
    assert_eq!(st, 200, "{svg}");
}

/// A spent rate budget is a normal state, and the poll survives it.
///
/// This is the arm that had no fixture until `fake_github`'s Actions
/// route grew one: `ratelimited/*` refuses every other call with a 403
/// carrying `Retry-After`, which is how GitHub spells "come back" and is
/// told from "you may not look" by that header and by nothing else. Both
/// halves of the contract are asserted, because only the pair is the
/// property worth having — a poller that backed off for ever would pass
/// the first half alone, and one that hammered through would pass the
/// second.
///
/// The schedule is per repository in the fake, so this does not have to
/// be the only reader in its world — `fake_github`'s
/// `another_repositorys_reads_do_not_shift_the_refusal_schedule` pins
/// that. It was a shared counter when this test was written, which would
/// have made the first read arrive already recovered if anything else
/// polled first; the trap is gone, and this note stays because "why is
/// this world polling one repository" is otherwise a question with no
/// answer in the file.
#[test]
fn a_rate_limited_poll_backs_off_and_comes_back_rather_than_failing() {
    let w = polling_world("checks-poll-ratelimit", "5");
    mirror(&w.server, &w.admin, "throttled", "ratelimited/widget");

    let first = await_job(&w, &enqueue_poll(&w, "throttled"));
    assert_eq!(
        first.state, "done",
        "a rate limit was reported as a failure, which makes a large \
         repository look broken every time it hits the budget it was \
         always going to hit: {first:?}"
    );
    assert_eq!(
        first.result.as_deref().unwrap_or_default(),
        "More { runs: 0 }",
        "a refused read must not be reported as a completed walk: {first:?}"
    );
    assert!(
        all_runs(&w, "throttled").is_empty(),
        "runs appeared out of a refused read"
    );
    assert!(
        phase(&w, "throttled", "checks:github:high-water").is_none(),
        "a poll that was refused claimed to have looked — which is the \
         same lie an empty list would have been"
    );

    // The wait GitHub asked for is recorded, and in the future: without
    // it the re-enqueued job dials straight back into the limit, which
    // is how a rate limit becomes a ban.
    let after: i64 = phase(&w, "throttled", "checks:github:after")
        .expect("the not-before mark")
        .parse()
        .expect("epoch milliseconds");
    assert!(after > now_ms(), "the mark is already in the past: {after}");

    // And it comes back. The worker re-enqueued itself, so the runs
    // arrive with nobody asking a second time.
    await_polled(&w, "throttled");
    assert_eq!(all_runs(&w, "throttled").len(), 7);
    assert!(
        phase(&w, "throttled", "checks:github:error").is_none(),
        "being asked to wait is not a failure and must leave no error \
         on the screen once the runs are in"
    );
    assert!(w.server.healthy());
}

/// The poller's state has two audiences, and a stranger is one of them.
///
/// `RepoRead` on a public mirror is anybody at all. What they may have
/// is the *shape* of the problem — connected, polled, denied — none of
/// which says anything a visitor could not infer from the empty tab in
/// front of them, and all of which the tab needs in order to say "this
/// installation cannot read Actions" instead of drawing nothing.
///
/// What they may not have is the operator text. `error` is GitHub's raw
/// response body or a `GET {url}: {e}` carrying the full
/// `api.github.com` URL, which names the origin `owner/repo` this
/// mirror pulls from; `resuming_from` is a raw upstream page URL. A
/// repository mirrored from a private upstream would otherwise publish
/// that upstream's name to every anonymous reader.
///
/// Asserted against the **JSON**, not against what a page renders. A
/// client that merely declines to draw the field has still shipped it,
/// and anybody who opens devtools reads it out of the response.
#[test]
fn the_poll_state_gives_a_stranger_the_shape_and_an_operator_the_detail() {
    let w = polling_world("checks-poll-audience", "5");
    mirror(&w.server, &w.admin, "locked", "noperm/widget");
    // Public, which is the posture that makes this reachable: an
    // anonymous GET of a private repo is masked long before it gets
    // here, so a private-only test would pass against the leak.
    let (st, out) = w.server.req(
        "PATCH",
        "/v1/orgs/acme/repos/locked",
        &w.admin,
        Some(serde_json::json!({"public": true})),
    );
    assert_eq!(st, 200, "{out}");
    await_job(&w, &enqueue_poll(&w, "locked"));

    // The operator, who may write, gets everything.
    let (st, mine) = w.server.get("/v1/orgs/acme/repos/locked/ci/poll", &w.admin);
    assert_eq!(st, 200, "{mine}");
    assert_eq!(mine["denied"], serde_json::json!(true), "{mine}");
    let detail = mine["error"].as_str().unwrap_or_default();
    assert!(detail.contains("actions: read"), "{mine}");

    // The stranger, with no credential at all, gets the shape and not
    // the detail — and `error` is *present and null* rather than
    // missing, so a client cannot read "we will not tell you" as "there
    // is no error".
    let (st, theirs) = w.server.get("/v1/orgs/acme/repos/locked/ci/poll", "");
    assert_eq!(st, 200, "{theirs}");
    assert_eq!(theirs["connected"], serde_json::json!(true), "{theirs}");
    assert_eq!(theirs["polled"], serde_json::json!(false), "{theirs}");
    assert_eq!(
        theirs["denied"],
        serde_json::json!(true),
        "a stranger must still be told the checks cannot be read, or the \
         tab is back to rendering a permission problem as an empty list: \
         {theirs}"
    );
    assert!(theirs.get("error").is_some(), "the key is gone: {theirs}");
    assert_eq!(theirs["error"], serde_json::Value::Null, "{theirs}");
    assert_eq!(theirs["resuming_from"], serde_json::Value::Null, "{theirs}");

    // The whole body, not just the fields we thought to name: nothing
    // anywhere in a stranger's response mentions the upstream.
    let text = theirs.to_string();
    assert!(
        !text.contains("noperm"),
        "the origin's name reached an anonymous reader: {text}"
    );
    assert!(
        !text.contains("api.github.com") && !text.contains("actions: read"),
        "operator text reached an anonymous reader: {text}"
    );

    assert!(w.server.healthy());
}

/// A spent hourly budget is a wait, not a permission problem.
///
/// The distinction this pins is the expensive one. GitHub's *primary*
/// rate limit — the hourly budget gone — is a 403 with
/// `x-ratelimit-remaining: 0` and a reset timestamp and **no
/// `Retry-After`**, which is exactly the shape of the 403 that means
/// "this installation may not read Actions". Told apart wrongly, the
/// poller does the worst available thing: it hard-fails the job and
/// records, durably and on the repository's own page, that the
/// maintainer's App installation lacks a permission they in fact
/// granted. They go and re-approve an installation that was never the
/// problem, and the checks come back an hour later for reasons nobody
/// can connect to what they did.
///
/// So the assertion is against the *other two* outcomes at once: this
/// must behave like `ratelimited/*` (back off, resume) and must not
/// behave like `noperm/*` (fail, record a denial). `budgetspent/*` is
/// the only fixture that can produce the ambiguous shape.
#[test]
fn a_spent_hourly_budget_backs_off_and_is_never_recorded_as_a_denial() {
    let w = polling_world("checks-poll-budget", "5");
    mirror(&w.server, &w.admin, "spent", "budgetspent/widget");

    let job = await_job(&w, &enqueue_poll(&w, "spent"));
    assert_eq!(
        job.state, "done",
        "a spent budget was reported as a failed job: {job:?}"
    );
    assert_eq!(
        job.result.as_deref().unwrap_or_default(),
        "More { runs: 0 }",
        "{job:?}"
    );
    assert!(
        phase(&w, "spent", "checks:github:error").is_none(),
        "a spent budget was recorded as a hard failure on the repository \
         — which is how a maintainer gets told to re-approve an \
         installation that was never the problem"
    );

    // Not denied, and the API says so. This is the field a UI hangs a
    // re-approve link on, and offering that link here would send
    // somebody to fix a permission that is already correct.
    let (st, state) = w.server.get("/v1/orgs/acme/repos/spent/ci/poll", &w.admin);
    assert_eq!(st, 200, "{state}");
    assert_eq!(state["denied"], serde_json::json!(false), "{state}");
    assert_eq!(state["error"], serde_json::Value::Null, "{state}");

    // The wait came from the reset timestamp, not from a `Retry-After`
    // that was never sent. The fixture resets two minutes out, so a
    // mark of a second or two would mean the duration had been invented
    // rather than derived.
    let retry_in = state["retry_in_ms"].as_i64().unwrap_or(0);
    assert!(
        (60_000..=3_600_000).contains(&retry_in),
        "the wait was not derived from the reset timestamp: {retry_in}ms"
    );

    // And it is a delay, not a dead end: cleared, the same repository
    // polls and the runs arrive.
    imports::set_cursor(&w.db, &repo_id(&w, "spent"), "checks:github:after", "0")
        .expect("clear the mark");
    await_polled(&w, "spent");
    assert_eq!(all_runs(&w, "spent").len(), 7);
    assert!(w.server.healthy());
}

/// **The list an author reads and the gate that refuses them must be the
/// same list.**
///
/// They were not. `land_gate` has always decided on the merged view —
/// the patchset's own `change_checks` rows unioned with the `check_runs`
/// reported against that patchset's commit — while
/// `GET /changes/{key}/checks` rendered `checks_for(patchset)`, the
/// intake's rows and nothing else. So a commit-scoped verdict blocked
/// the land, named itself in the reason, and appeared nowhere on the
/// page: "required check 'ci/tests' is failing" printed above a table
/// with no `ci/tests` in it, and nothing to click through to.
///
/// The coverage gate is what found it. `CheckSource::as_str` was
/// unreachable — the enum that says which system to go and look at had
/// no caller anywhere in the product, because the only read that would
/// have carried it was reading the wrong side of the merge.
///
/// Both directions are asserted, because covering only the first would
/// pass against a route that listed *everything* and lost the merge:
/// the commit-scoped row appears and blocks, and then the same name
/// reported against the patchset collapses the two to one row — the
/// narrower statement about *this* revision winning, which is the rule
/// the gate has always used and the page now shows.
#[test]
fn a_commit_scoped_verdict_that_blocks_the_land_is_on_the_page_that_blocks() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-merged");
    let scratch = Scratch::new("ci-intake-merged");
    let w = world(spawn_server(&bucket.base_url, &scratch, "ci-intake-merged"));
    let (server, admin) = (&w.server, &w.admin);
    let secret = mint_secret(server, admin, "app");

    let (key, tip) = open_change(
        server,
        admin,
        "app",
        "Ideadbe01",
        "merged-feature",
        ("src/m.rs", "fn m() {}"),
    );
    let cp = format!("/v1/orgs/acme/repos/app/changes/{key}");

    // Required on the target branch, so a missing verdict is a wait and
    // a red one is a refusal — the case the page has to explain. The
    // branch has to be protected first: a required check on a branch
    // anyone may push past is not a gate, and the API says so.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/protections",
        admin,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/app/required-checks/main",
        admin,
        Some(serde_json::json!({"name": "ci/tests"})),
    );
    assert_eq!(st, 201, "{out}");

    // A **commit-scoped** verdict: no `change` field, so the intake
    // writes `check_runs` keyed on the sha. This is what a provider
    // poller and a signing CI both produce.
    let body = serde_json::json!({
        "commit": tip,
        "name": "ci/tests",
        "state": "failing",
        "summary": "3 failed",
        "url": "https://ci.example.com/run/99",
        "sent_at": now_ms(),
    })
    .to_string();
    let sig = doc_signature(&secret, &body);
    let (st, out) = post_signed(
        server,
        "/v1/orgs/acme/repos/app/ci/checks",
        Some(&sig),
        &body,
    );
    // The commit-scoped route answers 200 on a write it treats as an
    // upsert; either is a recorded verdict, and which one is not this
    // test's claim — `the_intake_records_a_commit_run…` owns that.
    assert!(st == 200 || st == 201, "{st} {out}");

    // The page. Before the fix this array was empty.
    let (st, out) = server.get(&format!("{cp}/checks"), admin);
    assert_eq!(st, 200, "{out}");
    let checks = out["checks"].as_array().expect("a checks array");
    assert_eq!(
        checks.len(),
        1,
        "the verdict that blocks the land is not on the page: {out}"
    );
    let row = &checks[0];
    assert_eq!(row["name"], serde_json::json!("ci/tests"), "{out}");
    assert_eq!(row["state"], serde_json::json!("failing"), "{out}");
    assert_eq!(
        row["required"],
        serde_json::json!(true),
        "a required check did not say so: {out}"
    );
    assert_eq!(
        row["source"],
        serde_json::json!("commit"),
        "the row does not say what it is a statement about: {out}"
    );
    assert_eq!(
        row["posted_by"],
        serde_json::json!("intake"),
        "the row does not say who reported it, which is where a reader goes next: {out}"
    );
    assert_eq!(
        row["url"],
        serde_json::json!("https://ci.example.com/run/99"),
        "a red row with nothing to click through to: {out}"
    );

    // And the gate agrees with the page, in the same words.
    let (st, out) = as_person(
        server,
        &w.alice,
        "POST",
        &format!("/v1/orgs/acme/repos/app/changes/{key}/approve"),
    );
    assert_eq!(st, 204, "{out}");
    let (st, refused) = server.post(&format!("{cp}/land"), admin, None);
    assert_eq!(st, 409, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .contains("ci/tests"),
        "the refusal names a check the page does not show: {refused}"
    );

    // The same name, now reported **against this patchset**. Two rows in
    // two tables, one name: the page must show the narrower statement
    // and only it, exactly as the gate resolves it.
    let body = intake_body(&key, &tip, "ci/tests", "passing", now_ms());
    let sig = doc_signature(&secret, &body);
    let (st, out) = post_signed(
        server,
        "/v1/orgs/acme/repos/app/ci/checks",
        Some(&sig),
        &body,
    );
    assert_eq!(st, 201, "{out}");

    let (st, out) = server.get(&format!("{cp}/checks"), admin);
    assert_eq!(st, 200, "{out}");
    let checks = out["checks"].as_array().expect("a checks array");
    assert_eq!(
        checks.len(),
        1,
        "one name reported twice became two rows: {out}"
    );
    assert_eq!(
        checks[0]["source"],
        serde_json::json!("patchset"),
        "the commit-scoped row outranked the statement about this revision: {out}"
    );
    assert_eq!(checks[0]["state"], serde_json::json!("passing"), "{out}");

    // Which is also what the gate now says, and the land goes through.
    let (st, out) = server.post(&format!("{cp}/land"), admin, None);
    assert!(st == 202 || st == 200, "{st} {out}");

    assert!(server.healthy());
}

/// The four refusals `checks_poll::run_one` opens with.
///
/// `POST /ci/poll` refuses a repository with no GitHub origin at the
/// door, so the worker's own guards sit behind a check that has already
/// passed — and none of them had ever run. A job row outlives the
/// request that made it: the repository can be deleted, the App can be
/// removed from the deployment, and a job can be enqueued by something
/// other than that route.
///
/// The distinction the last two draw is the one worth pinning. "Nothing
/// to poll" is **not needed**, and completes; "this deployment has no
/// App" is an **error**, and fails. Collapsing them either way is a real
/// defect in both directions: a native repository whose poll job kept
/// failing would fill an operator's queue with red that means nothing,
/// and a missing App reported as "nothing to do" would silently stop
/// every mirror's checks with no sign anywhere.
#[test]
fn the_poller_refuses_a_job_it_cannot_run_and_says_which_way() {
    let w = polling_world("checks-poll-guards", "5");

    let run = |repo_id: Option<&str>| -> jobs::Job { await_job(&w, &poll_job(&w, repo_id)) };

    // 1. No repository at all.
    let j = run(None);
    assert_eq!(j.state, "failed", "{j:?}");
    assert!(
        j.error.unwrap_or_default().contains("without repo"),
        "the refusal does not say what was missing"
    );

    // 2. A repository deleted between enqueue and claim. `jobs.repo_id`
    //    has no foreign key, so the row outlives the repository.
    mirror(&w.server, &w.admin, "doomed", "acme/widget");
    let doomed = repo_id(&w, "doomed");
    let (st, out) = w.server.delete("/v1/orgs/acme/repos/doomed", &w.admin);
    assert!(st == 204 || st == 202, "{st} {out}");
    let j = run(Some(&doomed));
    assert_eq!(
        j.state, "done",
        "a vanished repository was an error rather than nothing to do: {j:?}"
    );

    // 3. A native repository: no Actions to poll, and asking for them is
    //    not an error. The route refuses this with a 400, so the
    //    worker's own answer had never been produced.
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos",
        &w.admin,
        Some(serde_json::json!({ "name": "native", "public": true })),
    );
    assert_eq!(st, 201, "{out}");
    let j = run(Some(&repo_id(&w, "native")));
    assert_eq!(
        j.state, "done",
        "a repository with no provider was an error rather than nothing \
         to do, which fills a queue with red that means nothing: {j:?}"
    );

    assert!(w.server.healthy());
}

/// A refused state names the **scope it was judged under**, and what the
/// other scope would have accepted.
///
/// One door, two vocabularies: `change_checks` takes three words and
/// `check_runs` takes six, and which set applies turns on whether the
/// body named a `change` — a field a reporter chooses for a different
/// reason entirely. Each downstream validator refuses correctly for its
/// own table and lists its own words, and that is precisely the
/// unhelpful answer.
///
/// The case that made this worth fixing: a push has no change to name,
/// so it reports commit-scoped; `pending` is the natural word for
/// "started", and it is the first word the docs listed. The refusal was
/// six states with `pending` absent from them, no mention of scope, and
/// no hint that `pending` is valid one field away. A reporter reads that
/// as "Weft has no pending state" and stops sending the start of the
/// run at all.
#[test]
fn a_refused_state_names_its_scope_and_the_other_one() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ci-intake-scopes");
    let scratch = Scratch::new("ci-intake-scopes");
    let w = world(spawn_server(&bucket.base_url, &scratch, "ci-intake-scopes"));
    let (server, admin) = (&w.server, &w.admin);
    let secret = mint_secret(server, admin, "app");
    let route = "/v1/orgs/acme/repos/app/ci/checks";
    let sign = |b: &str| doc_signature(&secret, b);
    let now = now_ms();

    let (key, tip) = open_change(
        server,
        admin,
        "app",
        "I5c0b0001",
        "scopes",
        ("s.rs", "fn main() {}"),
    );

    // Commit-scoped, sent `pending`: the real-world case.
    let b = commit_body(&tip, "ci/tests", "pending", now);
    let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
    assert_eq!(st, 400, "{out}");
    let msg = out["error"].as_str().expect("an error sentence");
    assert!(
        msg.contains("a report on a commit may use"),
        "the refusal must say which scope it judged: {msg}"
    );
    assert!(
        msg.contains("Name a `change`") && msg.contains("pending, passing, failing"),
        "and must name the other scope and its states, so `pending` is reachable: {msg}"
    );

    // Patchset-scoped, sent `queued`: the mirror case. A provider that
    // reports queued/running everywhere hits this on its first request.
    let b = intake_body(&key, &tip, "ci/tests", "queued", now);
    let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
    assert_eq!(st, 400, "{out}");
    let msg = out["error"].as_str().expect("an error sentence");
    assert!(
        msg.contains("a change's patchset may use"),
        "the refusal must say which scope it judged: {msg}"
    );
    assert!(
        msg.contains("Drop `change`") && msg.contains("queued"),
        "and must point at the scope where `queued` is legal: {msg}"
    );

    // A word neither scope has is still refused by name rather than read
    // as a default, in both shapes.
    for body in [
        commit_body(&tip, "ci/tests", "success", now + 1),
        intake_body(&key, &tip, "ci/tests", "success", now + 2),
    ] {
        let (st, out) = post_signed(server, route, Some(&sign(&body)), &body);
        assert_eq!(st, 400, "{out}");
        assert!(
            out["error"]
                .as_str()
                .unwrap_or_default()
                .contains("success"),
            "an unknown word is named back, never defaulted: {out}"
        );
    }

    // And the states each scope *does* accept still go through, so the
    // guard added here refuses nothing that used to work.
    let b = commit_body(&tip, "ci/tests", "queued", now + 3);
    let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
    assert!(st == 200 || st == 201, "{st}: {out}");
    let b = intake_body(&key, &tip, "ci/tests", "pending", now + 4);
    let (st, out) = post_signed(server, route, Some(&sign(&b)), &b);
    assert!(st == 200 || st == 201, "{st}: {out}");

    assert!(server.healthy());
}
