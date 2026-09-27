//! Static site hosting end to end: commit a `.weft/site.yml` and a
//! directory, and fetch the result over HTTP on the site's own hostname.
//!
//! Driven with **raw HTTP on a socket** rather than through a client,
//! because the entire feature turns on the `Host` header and most HTTP
//! clients reserve the right to set that themselves. What is being
//! proven here cannot be proven by a request whose `Host` we did not
//! choose.
//!
//! The property that matters most is the negative one. The router ends
//! in a fallback that serves the marketing site and then the dashboard's
//! single-page app, so a dispatch bug does not produce a 404 — it
//! produces *our product's UI under a customer's domain*. There is a
//! test for exactly that below, and it asserts on the bytes rather than
//! on the status code.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

const SITES_DOMAIN: &str = "weft.test";

fn spawn(store_url: &str, scratch: &Scratch) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .db_hint("site-serve-e2e")
        .env("STRATUM_SITES_DOMAIN", SITES_DOMAIN)
        // The publish worker polls; a test should not wait five seconds
        // for each push.
        .env("STRATUM_SITEPUBLISH_POLL_SECS", "1")
        .start()
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One HTTP/1.1 request with a `Host` and a method of our choosing.
fn raw_method(base: &str, method: &str, host: &str, path: &str, extra: &[(&str, &str)]) -> Reply {
    let addr = base.trim_start_matches("http://");
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: 0\r\n"
    );
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).expect("write");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).expect("read");
    parse_reply(&String::from_utf8_lossy(&buf))
}

fn parse_reply(text: &str) -> Reply {
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text, ""));
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status in reply: {head}"));
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Reply {
        status,
        headers,
        body: body.to_string(),
    }
}

/// A `GET` with a `Host` of our choosing — what almost every assertion
/// below wants.
fn raw(base: &str, host: &str, path: &str, extra: &[(&str, &str)]) -> Reply {
    raw_method(base, "GET", host, path, extra)
}

fn commit(server: &Server, token: &str, branch: &str, files: &[(&str, &str)]) -> String {
    let ops: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, c)| serde_json::json!({"op": "put", "path": p, "content": c}))
        .collect();
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/docs/commits",
        token,
        Some(serde_json::json!({
            "branch": branch,
            "message": "site",
            "operations": ops,
        })),
    );
    assert_eq!(st, 201, "{out}");
    out["commit"].as_str().expect("a commit oid").to_string()
}

/// Publishing is a queued job, so the test waits for it rather than
/// assuming it has happened.
fn wait_for(base: &str, host: &str, path: &str, want: &str) -> Reply {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = raw(base, host, path, &[]);
    while Instant::now() < deadline {
        if last.status == 200 && last.body.contains(want) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(250));
        last = raw(base, host, path, &[]);
    }
    panic!(
        "timed out waiting for {want:?} at {host}{path}; last was {} {:?}",
        last.status, last.body
    );
}

const CONFIG: &str = "publish: dist\n";

fn setup(server: &Server) -> String {
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "docs"})),
    );
    assert_eq!(st, 201, "{out}");
    admin
}

/// The whole slice: config plus a directory, published by a push, served
/// on the site's hostname.
#[test]
fn a_pushed_directory_is_served_on_its_own_hostname() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-serve-e2e");
    let scratch = Scratch::new("site-serve-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            ("dist/index.html", "<h1>hello from weft</h1>"),
            ("dist/about.html", "<h1>about</h1>"),
            ("dist/styles.css", "body{color:red}"),
            ("dist/blog/index.html", "<h1>blog</h1>"),
            ("dist/data.json", "{\"a\":1}"),
            // Outside the published directory, and so never reachable.
            ("secrets.txt", "TOP SECRET"),
            ("README.md", "not published"),
        ],
    );

    let r = wait_for(&server.base, &host, "/", "hello from weft");
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert!(r.header("etag").is_some(), "content-addressed etag");

    // `/about` finds `about.html`, the way GitHub Pages does.
    let r = raw(&server.base, &host, "/about", &[]);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("about"), "{}", r.body);

    // A directory without a trailing slash redirects, so relative links
    // inside the page resolve against it.
    let r = raw(&server.base, &host, "/blog", &[]);
    assert_eq!(r.status, 301, "{}", r.body);
    assert_eq!(r.header("location"), Some("/blog/"));

    let r = raw(&server.base, &host, "/blog/", &[]);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("blog"));

    // Content types come from the committed name.
    let r = raw(&server.base, &host, "/styles.css", &[]);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("text/css; charset=utf-8"));
    let r = raw(&server.base, &host, "/data.json", &[]);
    assert_eq!(
        r.header("content-type"),
        Some("application/json; charset=utf-8")
    );

    // Nothing above the published directory is reachable, by name or by
    // climbing.
    for path in [
        "/secrets.txt",
        "/README.md",
        "/../secrets.txt",
        "/..%2fsecrets.txt",
        "/%2e%2e/secrets.txt",
        "/dist/index.html",
    ] {
        let r = raw(&server.base, &host, path, &[]);
        assert!(
            !r.body.contains("TOP SECRET") && !r.body.contains("not published"),
            "{path} leaked: {}",
            r.body
        );
    }
}

/// The failure this feature could actually ship: the router's fallback
/// serves the marketing site and then the dashboard SPA, so a dispatch
/// bug answers a customer's domain with our product.
#[test]
fn a_site_host_never_falls_through_to_the_product() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-fallthrough-e2e");
    let scratch = Scratch::new("site-fallthrough-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            ("dist/index.html", "<h1>customer site</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "customer site");

    // Paths the product owns. On the sites domain none of them may
    // reach it.
    for path in [
        "/dashboard",
        "/dashboard/",
        "/dashboard/settings",
        "/v1/orgs/acme/repos",
        "/healthz",
        "/openapi.json",
        "/explore",
        "/login",
        "/acme/docs",
    ] {
        let r = raw(&server.base, &host, path, &[]);
        let b = r.body.to_lowercase();
        assert!(
            !b.contains("<!doctype html><html") || !(b.contains("dashboard") || b.contains("weft")),
            "{path} answered with product UI: {}",
            &r.body[..r.body.len().min(300)]
        );
        assert_ne!(r.status, 200, "{path} should not succeed on a site host");
    }

    // And an unknown label on the sites domain is our own plain page,
    // never the dashboard.
    let r = raw(&server.base, &format!("nobody.{SITES_DOMAIN}"), "/", &[]);
    assert_eq!(r.status, 404);
    assert!(r.body.contains("No site here"), "{}", r.body);
}

/// The product must be unaffected. Same server, ordinary hostnames.
#[test]
fn the_product_still_answers_on_its_own_hostnames() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-product-e2e");
    let scratch = Scratch::new("site-product-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);

    // Through the ordinary client, which sets its own Host.
    let (st, out) = server.get("/v1/orgs/acme/repos", &admin);
    assert_eq!(st, 200, "{out}");

    // And explicitly, with hosts that resemble the sites domain without
    // being under it.
    for host in ["localhost", "weft.sh", "notweft.test", "a.b.weft.test"] {
        let r = raw(&server.base, host, "/healthz", &[]);
        assert_eq!(r.status, 200, "{host} should reach the product");
    }
}

#[test]
fn the_apex_of_the_sites_domain_redirects_and_never_renders_the_product() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-apex-e2e");
    let scratch = Scratch::new("site-apex-e2e");
    let server = spawn(&bucket.base_url, &scratch);

    let r = raw(&server.base, SITES_DOMAIN, "/", &[]);
    assert_eq!(r.status, 302, "{}", r.body);
    assert!(r.header("location").is_some());
    assert!(!r.body.to_lowercase().contains("dashboard"), "{}", r.body);
}

/// A conditional request is answered from the object id, so a repeat
/// visit costs headers rather than a file.
#[test]
fn a_second_request_with_the_etag_is_answered_304() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-etag-e2e");
    let scratch = Scratch::new("site-etag-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            ("dist/index.html", "<h1>etag</h1>"),
        ],
    );
    let first = wait_for(&server.base, &host, "/", "etag");
    let tag = first.header("etag").expect("etag").to_string();

    let again = raw(&server.base, &host, "/", &[("If-None-Match", &tag)]);
    assert_eq!(again.status, 304, "{}", again.body);
    assert!(again.body.is_empty(), "304 carries no body: {}", again.body);
}

/// A second push republishes, and the new content is what is served.
#[test]
fn a_later_push_replaces_what_is_served() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-republish-e2e");
    let scratch = Scratch::new("site-republish-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            ("dist/index.html", "<h1>version one</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "version one");

    commit(
        &server,
        &admin,
        "main",
        &[("dist/index.html", "<h1>version two</h1>")],
    );
    wait_for(&server.base, &host, "/", "version two");
}

/// A branch moved by the API publishes like a pushed one.
///
/// `POST …/reset` and `POST …/branches` moved the ref and armed nothing:
/// only the commit route and the two push doors called `on_push`. A
/// deploy that stages its chunks on another branch and then moves the
/// published branch in one step — which is how `weftsh/deploy-site`
/// keeps a half-written tree from ever being served — left the site on
/// the old tree until somebody pushed. Both routes arm the publish job
/// now, and the site follows the ref.
#[test]
fn a_branch_created_or_reset_by_the_api_is_published() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-refops-e2e");
    let scratch = Scratch::new("site-refops-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    // Every commit arms a publish job that reads the tip when it runs.
    // The staging commits below arm one too, and if it ran after the ref
    // move it would publish the moved tree and hide a ref-op that armed
    // nothing — which is precisely what let the first version of this
    // test pass against the bug. So each staging commit's job is allowed
    // to finish before the ref moves; then only the ref-op can publish.
    let (st, row) = server.get("/v1/orgs/acme/repos/docs", &admin);
    assert_eq!(st, 200, "{row}");
    let org_id = row["org_id"].as_str().unwrap().to_string();
    let repo_id = row["id"].as_str().unwrap().to_string();
    let db = stratum_control::ControlDb::open(&server.db_url).expect("control plane");
    let settled = |after: Instant| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let latest =
                stratum_control::jobs::latest_for_repo(&db, &org_id, &repo_id, "sitepublish")
                    .expect("read jobs");
            match latest {
                Some(j) if j.state == "done" || j.state == "failed" => return,
                None if after.elapsed() > Duration::from_secs(3) => return,
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "publish job never settled: {latest:?}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };

    // The config names a branch that does not exist yet.
    commit(
        &server,
        &admin,
        "main",
        &[(".weft/site.yml", "publish: dist\nbranch: weft-site\n")],
    );
    let staged = commit(
        &server,
        &admin,
        "staging",
        &[("dist/index.html", "<h1>from staging</h1>")],
    );
    settled(Instant::now());
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/docs/branches",
        &admin,
        Some(serde_json::json!({ "name": "weft-site", "from": staged })),
    );
    assert_eq!(st, 201, "{out}");
    wait_for(&server.base, &host, "/", "from staging");

    let again = commit(
        &server,
        &admin,
        "staging",
        &[("dist/index.html", "<h1>moved by reset</h1>")],
    );
    settled(Instant::now());
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/docs/reset",
        &admin,
        Some(serde_json::json!({ "branch": "weft-site", "to": again, "expected_head": staged })),
    );
    assert_eq!(st, 200, "{out}");
    wait_for(&server.base, &host, "/", "moved by reset");
}

/// `spa: true` answers an unmatched path with the index and a 200, which
/// is what a client-side router needs.
#[test]
fn a_single_page_app_falls_back_to_its_index() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-spa-e2e");
    let scratch = Scratch::new("site-spa-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: dist\nspa: true\n"),
            ("dist/index.html", "<h1>app shell</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "app shell");

    let r = raw(&server.base, &host, "/deep/route/the/client/owns", &[]);
    assert_eq!(r.status, 200, "an SPA route is a 200, not a 404");
    assert!(r.body.contains("app shell"), "{}", r.body);
}

/// Without `spa`, a committed 404 page is served — with a 404 status,
/// because a missing page that reports success is a page search engines
/// will index.
#[test]
fn a_custom_not_found_page_is_served_with_a_404_status() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-404-e2e");
    let scratch = Scratch::new("site-404-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: dist\nnot-found: 404.html\n"),
            ("dist/index.html", "<h1>home</h1>"),
            ("dist/404.html", "<h1>my own 404</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "home");

    let r = raw(&server.base, &host, "/nowhere", &[]);
    assert_eq!(r.status, 404);
    assert!(r.body.contains("my own 404"), "{}", r.body);
}

/// A repository with no site config publishes nothing and answers
/// nothing, rather than serving its whole tree.
#[test]
fn a_repository_with_no_config_publishes_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-noconfig-e2e");
    let scratch = Scratch::new("site-noconfig-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(&server, &admin, "main", &[("index.html", "<h1>tree</h1>")]);
    std::thread::sleep(Duration::from_secs(3));

    let r = raw(&server.base, &host, "/", &[]);
    assert_eq!(r.status, 404);
    assert!(!r.body.contains("tree"), "{}", r.body);
}

/// `GET …/site` is what makes the feature discoverable: publishing is
/// driven entirely by a committed file, so without this route nobody
/// ever learns the address their site is at.
///
/// All three config states in one repository, in the order a person
/// meets them, because the interesting one is the middle: a config that
/// stops parsing must report the refusal *and* leave the last good
/// deploy serving. A route that reported "no site" there would send
/// somebody looking for a deleted site instead of a typo.
#[test]
fn the_site_route_reports_absent_then_live_then_a_refusal() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-route-e2e");
    let scratch = Scratch::new("site-route-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");
    let path = "/v1/orgs/acme/repos/docs/site";

    // Nothing committed at all: not a 404, because "this repository
    // publishes no site" is a fact about it rather than a missing page.
    commit(&server, &admin, "main", &[("README.md", "hi")]);
    let (st, out) = server.get(path, &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["enabled"], serde_json::json!(false));
    assert_eq!(out["config_state"], serde_json::json!("absent"));
    assert_eq!(out["url"], serde_json::Value::Null);
    assert_eq!(out["deploys"].as_array().unwrap().len(), 0);

    // Published.
    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: dist\nspa: true\n"),
            ("dist/index.html", "<h1>live</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "live");

    let (st, out) = server.get(path, &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["enabled"], serde_json::json!(true));
    assert_eq!(out["config_state"], serde_json::json!("ok"));
    assert_eq!(out["config"]["publish"], serde_json::json!("dist"));
    assert_eq!(out["config"]["spa"], serde_json::json!(true));
    assert_eq!(
        out["host"],
        serde_json::json!(host.split('.').next().unwrap())
    );
    // The address a person copies. It names the sites domain, not the
    // product's, which is the whole separation.
    let url = out["url"].as_str().expect("an address");
    assert!(url.starts_with("https://"), "{url}");
    assert!(url.ends_with(SITES_DOMAIN), "{url}");
    let deploys = out["deploys"].as_array().expect("a list");
    assert_eq!(deploys.len(), 1, "{out}");
    assert_eq!(out["current"], deploys[0]["id"]);
    assert_eq!(deploys[0]["publish"], serde_json::json!("dist"));
    assert!(deploys[0]["commit"].as_str().is_some_and(|c| !c.is_empty()));

    // Now break the config. The site must keep serving, and the route
    // must say why nothing new has appeared — with the line number,
    // which is most of the value of the sentence.
    commit(
        &server,
        &admin,
        "main",
        &[(".weft/site.yml", "publish: dist\npubish: typo\n")],
    );
    let (st, out) = server.get(path, &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["config_state"], serde_json::json!("refused"), "{out}");
    let err = out["config_error"].as_str().expect("a refusal");
    assert!(
        err.contains(".weft/site.yml:2"),
        "the line, not just the file: {err}"
    );
    assert!(err.contains("pubish"), "{err}");
    assert_eq!(out["config"], serde_json::Value::Null);
    // Still live, still serving what last parsed.
    assert_eq!(out["enabled"], serde_json::json!(true), "{out}");
    let r = raw(&server.base, &host, "/", &[]);
    assert_eq!(r.status, 200, "a broken config must not take the site down");
    assert!(r.body.contains("live"), "{}", r.body);
}

/// What the publish worker decided, read off the job it completed.
///
/// The three cases below have no observable effect on the site — that is
/// the point of them, nothing is published — so the job's own outcome is
/// the only honest thing to wait on. Polling the row is deterministic
/// where a sleep would be a guess.
fn wait_for_outcome(server: &Server, want: &str) -> String {
    let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls).expect("connect");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = String::new();
    while Instant::now() < deadline {
        let rows = db
            .query(
                "SELECT COALESCE(result, '') FROM jobs \
                 WHERE kind = 'sitepublish' AND state = 'done' \
                 ORDER BY updated_at DESC LIMIT 5",
                &[],
            )
            .expect("read jobs");
        for r in &rows {
            let got: String = r.get(0);
            if got.contains(want) {
                return got;
            }
            last = got;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("no sitepublish job reported {want:?}; last was {last:?}");
}

/// A config that names a directory nobody committed publishes nothing,
/// rather than publishing the repository root.
#[test]
fn a_publish_directory_that_is_not_there_publishes_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-nodir-e2e");
    let scratch = Scratch::new("site-nodir-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: nowhere\n"),
            ("index.html", "<h1>the root</h1>"),
        ],
    );
    let out = wait_for_outcome(&server, "NoSuchDirectory");
    assert!(out.contains("nowhere"), "it names the directory: {out}");

    let r = raw(&server.base, &host, "/", &[]);
    assert_eq!(r.status, 404, "nothing is published");
    assert!(!r.body.contains("the root"), "{}", r.body);
}

/// A `publish:` that names a *file* is the same answer as one that names
/// nothing: there is no directory to serve.
#[test]
fn a_publish_path_that_is_a_file_publishes_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-file-e2e");
    let scratch = Scratch::new("site-file-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: README.md\n"),
            ("README.md", "not a directory"),
        ],
    );
    let out = wait_for_outcome(&server, "NoSuchDirectory");
    assert!(out.contains("README.md"), "{out}");
}

/// A config naming a branch that does not exist publishes nothing, and
/// does not fall back to the branch the config was read from.
#[test]
fn a_branch_that_does_not_exist_publishes_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-nobranch-e2e");
    let scratch = Scratch::new("site-nobranch-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: dist\nbranch: release\n"),
            ("dist/index.html", "<h1>on main</h1>"),
        ],
    );
    wait_for_outcome(&server, "NotNeeded");

    let r = raw(&server.base, &host, "/", &[]);
    assert_eq!(
        r.status, 404,
        "main must not publish when release was asked for"
    );
    assert!(!r.body.contains("on main"), "{}", r.body);
}

/// The worker records the refusal, with its line, and publishes nothing.
#[test]
fn a_config_that_does_not_parse_is_refused_by_the_worker() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-refused-e2e");
    let scratch = Scratch::new("site-refused-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: dist\nspa: yes\n"),
            ("dist/index.html", "<h1>never served</h1>"),
        ],
    );
    let out = wait_for_outcome(&server, "Refused");
    assert!(out.contains(".weft/site.yml:2"), "the line: {out}");
    assert!(out.contains("spa"), "{out}");
}

fn commit_to(server: &Server, token: &str, repo: &str, files: &[(&str, &str)]) -> String {
    let ops: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, c)| serde_json::json!({"op": "put", "path": p, "content": c}))
        .collect();
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        token,
        Some(serde_json::json!({
            "branch": "main",
            "message": "site",
            "operations": ops,
        })),
    );
    assert_eq!(st, 201, "{out}");
    out["commit"].as_str().expect("a commit oid").to_string()
}

fn make_repo(server: &Server, token: &str, name: &str) {
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        token,
        Some(serde_json::json!({"name": name})),
    );
    assert_eq!(st, 201, "{out}");
}

fn site_status(server: &Server, token: &str, repo: &str) -> serde_json::Value {
    let (st, out) = server.get(&format!("/v1/orgs/acme/repos/{repo}/site"), token);
    assert_eq!(st, 200, "{out}");
    out
}

/// Two repositories whose names differ only where DNS cannot carry the
/// difference want one hostname, and the second must still get one.
///
/// This is the lossy half of the derivation meeting the database. The
/// walk itself is unit-tested with no database at all; what is proved
/// here is that the collision really happens against real rows and that
/// the counted-up label is what the product then serves.
#[test]
fn two_repositories_that_want_one_hostname_both_get_one() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-collide-e2e");
    let scratch = Scratch::new("site-collide-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");

    // `my_site` and `my-site` both slug to `my-site`, because `_` is not
    // a character DNS will carry.
    make_repo(&server, &admin, "my_site");
    make_repo(&server, &admin, "my-site");
    for repo in ["my_site", "my-site"] {
        commit_to(
            &server,
            &admin,
            repo,
            &[
                (".weft/site.yml", CONFIG),
                ("dist/index.html", &format!("<h1>{repo}</h1>")),
            ],
        );
    }

    let mut hosts = Vec::new();
    for repo in ["my_site", "my-site"] {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let out = site_status(&server, &admin, repo);
            if let Some(h) = out["host"].as_str() {
                hosts.push(h.to_string());
                break;
            }
            assert!(Instant::now() < deadline, "{repo} never got a host");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    hosts.sort();
    assert_eq!(
        hosts,
        ["my-site--acme", "my-site--acme-2"],
        "the second must be counted up, not refused"
    );

    // And both really serve their own content on their own hostname.
    for (repo, host) in [("my_site", &hosts[0]), ("my-site", &hosts[1])] {
        let served = wait_for(&server.base, &format!("{host}.{SITES_DOMAIN}"), "/", "<h1>");
        assert!(
            served.body.contains("my_site") || served.body.contains("my-site"),
            "{repo} at {host}: {}",
            served.body
        );
    }
}

/// A config that starts naming a branch updates what the settings panel
/// reports, so the panel never shows a ref that stopped publishing.
#[test]
fn naming_a_branch_later_updates_what_is_reported() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-branch-e2e");
    let scratch = Scratch::new("site-branch-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            ("dist/index.html", "<h1>a</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "<h1>a</h1>");
    assert_eq!(
        site_status(&server, &admin, "docs")["branch"],
        serde_json::Value::Null,
        "no branch named yet, so the repository's default is implied"
    );

    // Name it explicitly. Same branch, but now it is the config's word.
    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: dist\nbranch: main\n"),
            ("dist/index.html", "<h1>b</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "<h1>b</h1>");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if site_status(&server, &admin, "docs")["branch"] == serde_json::json!("main") {
            break;
        }
        assert!(Instant::now() < deadline, "the branch was never recorded");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A push that does not change the published directory republishes
/// nothing.
///
/// Without this the history would grow a row for every push anywhere in
/// the repository, and "what changed on the site" would stop meaning
/// anything.
#[test]
fn a_push_that_leaves_the_directory_alone_adds_no_deploy() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-nochange-e2e");
    let scratch = Scratch::new("site-nochange-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            ("dist/index.html", "<h1>one</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "one");
    let first = site_status(&server, &admin, "docs");
    let current = first["current"].as_str().expect("a deploy").to_string();
    assert_eq!(first["deploys"].as_array().unwrap().len(), 1);

    // A push that touches nothing under `dist`.
    commit(&server, &admin, "main", &[("NOTES.md", "unrelated\n")]);
    wait_for_outcome(&server, "Published");
    // Give the worker room to have done the wrong thing, then check it
    // did not: the deploy being served is the same row as before.
    std::thread::sleep(Duration::from_secs(2));
    let after = site_status(&server, &admin, "docs");
    assert_eq!(after["current"].as_str(), Some(current.as_str()));
    assert_eq!(
        after["deploys"].as_array().unwrap().len(),
        1,
        "an unchanged tree must not add a history row: {after}"
    );
}

/// A repository with no commits at all answers the route rather than
/// failing on it.
///
/// The default branch does not resolve, which is a different thing from
/// "there is no config" and reaches a different arm of the reader. It is
/// also the very first thing a person does: create a repository, then
/// look at its settings before pushing anything.
#[test]
fn the_site_route_answers_for_a_repository_with_no_commits() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-empty-e2e");
    let scratch = Scratch::new("site-empty-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);

    let (st, out) = server.get("/v1/orgs/acme/repos/docs/site", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["enabled"], serde_json::json!(false));
    assert_eq!(out["config_state"], serde_json::json!("absent"));
    assert_eq!(out["url"], serde_json::Value::Null);
    assert_eq!(out["deploys"].as_array().unwrap().len(), 0);
}

/// A directory named like a workflow is not one, and must not stop the
/// walk finding the workflows beside it.
///
/// `is_workflow` looks at the name; whether the entry is a tree is a
/// separate question, and a repository really can hold `.weft/ci.yml/`
/// as a directory.
#[test]
fn a_directory_named_like_a_workflow_is_skipped() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-wfdir-e2e");
    let scratch = Scratch::new("site-wfdir-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            // A *directory* whose name ends `.yml`.
            (".weft/ci.yml/notes.txt", "not a workflow"),
            ("dist/index.html", "<h1>still published</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "still published");

    // The workflow listing sees no workflow, and does not fail.
    let (st, out) = server.get("/v1/orgs/acme/repos/docs/workflows", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["workflows"].as_array().unwrap().len(),
        0,
        "a directory is not a workflow: {out}"
    );
}

/// A reader who may not see the repository may not see its site either.
#[test]
fn the_site_route_refuses_a_caller_who_cannot_read_the_repository() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-route-authz-e2e");
    let scratch = Scratch::new("site-route-authz-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "docs", "public": false})),
    );
    assert_eq!(st, 201, "{out}");

    let (st, _) = server.get("/v1/orgs/acme/repos/docs/site", "");
    assert!(
        st == 401 || st == 404,
        "a stranger must not learn a private repository has a site: got {st}"
    );
    let (st, _) = server.get("/v1/orgs/acme/repos/nope/site", &admin);
    assert_eq!(st, 404, "a repository that does not exist");
}

/// A published directory is readable and nothing else.
///
/// The dispatch layer pre-empts routing for *every* method, not just the
/// ones a static host answers, so this has to be refused deliberately.
/// Without it a `POST` would be handed the file — no state changes, but
/// it is not what a static host does, and the next person would build on
/// it. `HEAD` is the other half: it must answer exactly what `GET`
/// answers, headers and all, with no body.
#[test]
fn a_site_answers_get_and_head_and_refuses_everything_else() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-methods-e2e");
    let scratch = Scratch::new("site-methods-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            ("dist/index.html", "<h1>read only</h1>"),
        ],
    );
    let got = wait_for(&server.base, &host, "/", "read only");

    // HEAD: same status, same headers, no body.
    let head = raw_method(&server.base, "HEAD", &host, "/", &[]);
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-type"), got.header("content-type"));
    assert_eq!(head.header("etag"), got.header("etag"));
    assert_eq!(head.header("cache-control"), got.header("cache-control"));
    assert!(head.body.is_empty(), "HEAD carried a body: {:?}", head.body);

    for method in ["POST", "PUT", "DELETE", "PATCH"] {
        let r = raw_method(&server.base, method, &host, "/", &[]);
        assert_eq!(r.status, 405, "{method} was not refused");
        assert_eq!(r.header("allow"), Some("GET, HEAD"), "{method}");
        assert!(
            !r.body.contains("read only"),
            "{method} was served the file: {}",
            r.body
        );
    }

    // The refusal is the site's, not the product's, and says nothing
    // about the dashboard.
    let r = raw_method(&server.base, "POST", &host, "/", &[]);
    assert!(r.body.contains("A published site answers"), "{}", r.body);
}

/// A fallback that is configured but not committed falls back to ours.
///
/// Both halves matter. `spa: true` with no `index.html` is a site whose
/// build did not produce one, and `not-found:` naming a file nobody
/// committed is a typo — in each case the reader must get a plain 404
/// rather than an empty 200, which is what a fallback that silently
/// served nothing would give them.
#[test]
fn a_fallback_that_is_not_committed_falls_back_to_our_own_page() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-nofallback-e2e");
    let scratch = Scratch::new("site-nofallback-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    // `spa: true`, and the only page is not the index it would serve.
    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: dist\nspa: true\n"),
            ("dist/other.html", "<h1>not the index</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/other", "not the index");

    let r = raw(&server.base, &host, "/anything", &[]);
    assert_eq!(r.status, 404, "an SPA with no index must not answer 200");
    assert!(r.body.contains("Page not found"), "{}", r.body);

    // `not-found:` naming a file that is not there.
    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", "publish: dist\nnot-found: missing.html\n"),
            ("dist/index.html", "<h1>home</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "home");

    let r = raw(&server.base, &host, "/nowhere", &[]);
    assert_eq!(r.status, 404);
    assert!(r.body.contains("Page not found"), "{}", r.body);
}

/// The tree walk's refusals, driven through real requests.
///
/// Each of these is a path that *looks* like it could resolve and must
/// not: a file used as a directory, a directory asked for as a file, and
/// a name under something that is not a tree. They are one test because
/// they are one property — the walk stops at the first thing that is not
/// what the path says it is.
#[test]
fn a_path_that_walks_through_a_file_finds_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-walk-e2e");
    let scratch = Scratch::new("site-walk-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    commit(
        &server,
        &admin,
        "main",
        &[
            (".weft/site.yml", CONFIG),
            ("dist/index.html", "<h1>home</h1>"),
            ("dist/notes.txt", "a file, not a directory"),
            ("dist/deep/a/b/page.html", "<h1>deep</h1>"),
        ],
    );
    wait_for(&server.base, &host, "/", "home");

    // The deep path really does resolve, so the walk is being exercised
    // rather than short-circuited.
    let r = raw(&server.base, &host, "/deep/a/b/page.html", &[]);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("deep"), "{}", r.body);

    // A file used as a directory: `notes.txt` is a blob, so nothing
    // under it can resolve.
    for path in ["/notes.txt/anything", "/notes.txt/a/b"] {
        let r = raw(&server.base, &host, path, &[]);
        assert_eq!(r.status, 404, "{path} resolved through a file");
        assert!(!r.body.contains("a file, not"), "{path}: {}", r.body);
    }

    // A directory asked for with a name that cannot be one.
    let r = raw(&server.base, &host, "/deep/a/b/page.html/more", &[]);
    assert_eq!(r.status, 404);

    // An intermediate directory that exists resolves to a redirect, not
    // to its contents.
    let r = raw(&server.base, &host, "/deep/a", &[]);
    assert_eq!(r.status, 301);
    assert_eq!(r.header("location"), Some("/deep/a/"));

    // …and a directory with no index of its own is a miss, not a listing.
    let r = raw(&server.base, &host, "/deep/a/", &[]);
    assert_eq!(r.status, 404, "a directory must never be listed");
    assert!(!r.body.contains("page.html"), "no listing: {}", r.body);
}

/// A symlink in the published directory is never followed.
///
/// This one is pushed with the **real git CLI**, because it is the only
/// way to get mode `120000` into a tree — the commits API writes regular
/// blobs and cannot express a link at all. A test that could not create
/// the input would be a test of nothing, which is the failure this
/// project has already paid for once.
///
/// The link points at a file outside the published directory. Following
/// it is exactly how a published directory stops bounding what can be
/// read, so the guard has to hold against a link that really exists in
/// a really pushed tree.
#[test]
fn a_committed_symlink_is_not_followed() {
    let minio = Minio::shared();
    let bucket = minio.bucket("site-symlink-e2e");
    let scratch = Scratch::new("site-symlink-e2e");
    let server = spawn(&bucket.base_url, &scratch);
    let admin = setup(&server);
    let host = format!("docs--acme.{SITES_DOMAIN}");

    let work = scratch.path().join("work");
    std::fs::create_dir_all(work.join("dist")).unwrap();
    std::fs::create_dir_all(work.join(".weft")).unwrap();
    std::fs::write(work.join(".weft/site.yml"), CONFIG).unwrap();
    std::fs::write(work.join("dist/index.html"), "<h1>home</h1>").unwrap();
    std::fs::write(work.join("secrets.txt"), "TOP SECRET").unwrap();
    // Inside the published directory, pointing out of it.
    std::os::unix::fs::symlink("../secrets.txt", work.join("dist/leak.txt")).unwrap();
    // And one pointing at a sibling that is published, so the test
    // distinguishes "links are not followed" from "this path missed".
    std::os::unix::fs::symlink("index.html", work.join("dist/alias.html")).unwrap();

    gitcli::git(&work, &["init", "-q", "-b", "main"]);
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-q",
            "-m",
            "a site with links in it",
        ],
    );
    let url = format!(
        "{}/acme/docs.git",
        server
            .base
            .replace("http://", &format!("http://x:{admin}@"))
    );
    gitcli::git(&work, &["remote", "add", "origin", &url]);
    gitcli::git(&work, &["push", "-q", "origin", "main"]);

    // The link really is a link in the tree that was pushed.
    let modes = gitcli::git(&work, &["ls-tree", "HEAD", "dist/"]);
    assert!(
        modes.contains("120000") && modes.contains("leak.txt"),
        "the fixture must actually contain a symlink: {modes}"
    );

    wait_for(&server.base, &host, "/", "home");

    for path in ["/leak.txt", "/alias.html"] {
        let r = raw(&server.base, &host, path, &[]);
        assert_eq!(r.status, 404, "{path} was served: {}", r.body);
        assert!(!r.body.contains("TOP SECRET"), "{path} leaked: {}", r.body);
        assert!(!r.body.contains("<h1>home</h1>"), "{path}: {}", r.body);
    }
}
