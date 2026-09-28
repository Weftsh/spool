//! Landing a changeset, end to end: the pre-flight that refuses before
//! anything is written, the commit point, the apply across several
//! repositories, the unwind when one of them has moved, and the reaper
//! that finishes a landing whose driver died. Every branch of the
//! protocol's steps 4 and 5 (`docs/CHANGESETS.md`) has a deterministic
//! test here; the SIGKILL sibling lives in `chaos_e2e.rs` and is
//! `#[ignore]`d, so nothing in this file is the sole cover for a line.
//!
//! Everything is asserted the way a person finds out — the changeset
//! view, the change view, the trunk's refs and files — never by reading
//! tables.

use std::time::Duration;

use stratum_testkit::adversarial::{percent_encode, INJECTIONS};
use stratum_testkit::faultproxy::{Fault, FaultPlan, FaultRule};
use stratum_testkit::{gitcli::Scratch, FaultProxy, Minio, Server};

const PASSWORD: &str = "a long enough password";
const CS: &str = "/v1/orgs/acme/changesets";

/// A server whose lander polls every second and whose reaper treats a
/// job idle for one second as dead. Both are minutes in production —
/// the right scale, and an impossible one to assert against.
fn spawn(store_url: &str, scratch: &Scratch, poll: &str) -> Server {
    spawn_with(store_url, scratch, poll, &[])
}

/// The same, with the reaper's grace or anything else overridden. A
/// `STRATUM_LAND_RECHECK_SECS` of `600` is how a test holds a failed job's
/// landing still while it moves the world underneath it.
fn spawn_with(store_url: &str, scratch: &Scratch, poll: &str, env: &[(&str, &str)]) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .db_hint("changeset-landing-e2e")
        .env("STRATUM_LAND_POLL_SECS", poll)
        .env("STRATUM_LAND_RECHECK_SECS", "1")
        // Nobody on the wire but the lander. Every test here arms a
        // fault on a repository's manifest and then says something about
        // how many times it was met — and the compactor, the CDN packer,
        // the contribution walk and the notifier's OWNERS read all read
        // that same manifest on their own clocks. On a development
        // machine they had not got there yet; on a two-core runner they
        // had, and `a_store_failure_during_the_pre_flight_begins_nothing`
        // counted 13 faults where it expected 6. None of those workers
        // is what this file is about, so the honest fix is to leave them
        // out rather than to weaken every count to a `>=`, which would
        // pass against an unbounded retry loop.
        .env("STRATUM_COMPACT_POLL_SECS", "0")
        .env("STRATUM_CDNPACK_POLL_SECS", "0")
        .env("STRATUM_CONTRIB_POLL_SECS", "0")
        .env("STRATUM_NOTIFY_POLL_SECS", "0")
        // The changeset notifier is the fifth of these and the newest.
        // Every test in this file composes a changeset, which enqueues a
        // notification; the worker then resolves each member's OWNERS to
        // work out who to tell, and that read goes at the same manifest
        // the fault is armed on. It cost
        // `a_store_failure_during_the_pre_flight_begins_nothing` a fourth
        // fault where it expects three — on CI under coverage, where the
        // worker had time to get there and on a development machine it
        // had not. Same lesson as the four above, one worker later.
        .env("STRATUM_CHANGESET_NOTIFY_POLL_SECS", "0")
        // The sixth: the storage sweep re-reads every repository's
        // manifest on its first tick, at boot, and that read goes at the
        // armed manifest like the notifier's did. It cost the hardening
        // suite a third store op against a budget of two, on CI.
        .env("STRATUM_STORAGE_SWEEP_SECS", "0");
    for (k, v) in env {
        b = b.env(k, *v);
    }
    b.start()
}

/// Poll until `f` holds, with a message for the deadline.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("never happened: {what}");
}

/// The same bucket through a fault proxy.
fn proxied(hint: &str) -> (FaultProxy, Scratch) {
    let (proxy, _, scratch) = proxied_pair(hint);
    (proxy, scratch)
}

/// A bucket through a fault proxy, and the same bucket's direct URL, so
/// a second node can write to the store the first node is being lied to
/// about — the shape of a fleet where one node's view is stale.
fn proxied_pair(hint: &str) -> (FaultProxy, String, Scratch) {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let upstream = bucket
        .base_url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let proxy = FaultProxy::start(&upstream);
    let bucket_name = bucket.base_url.rsplit('/').next().unwrap().to_string();
    (
        FaultProxy {
            url: format!("{}/{bucket_name}", proxy.url),
            handle: proxy.handle,
        },
        bucket.base_url.clone(),
        Scratch::new(hint),
    )
}

/// A second node on the same control plane, reading and writing the
/// store directly. Its lander is off, so the node under test is the only
/// driver; this one is the rest of the fleet, taking pushes.
fn second_node(store_url: &str, scratch: &Scratch, first: &Server) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data-b"))
        .db_url(&first.db_url)
        .env("STRATUM_LAND_POLL_SECS", "0")
        .start()
}

fn make_user(server: &Server, email: &str, name: &str, role: &str) {
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            email,
            "--name",
            name,
            "--password",
            PASSWORD,
            "--role",
            role,
        ])
        .unwrap_or_else(|e| panic!("user-create {email}: {e}"));
}

fn sign_in(server: &Server, email: &str) -> String {
    let resp = ureq::post(&format!("{}/v1/auth/login", server.base))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({"email": email, "password": PASSWORD}).to_string())
        .unwrap_or_else(|e| panic!("login {email}: {e}"));
    resp.header("set-cookie")
        .and_then(|c| c.split(';').next())
        .expect("a session cookie")
        .to_string()
}

fn as_person(server: &Server, cookie: &str, method: &str, path: &str) -> (u16, serde_json::Value) {
    let r = ureq::request(method, &format!("{}{path}", server.base)).set("Cookie", cookie);
    let resp = match r.call() {
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

/// Commit one file to a branch through the commit API; the new commit.
fn commit(
    server: &Server,
    token: &str,
    repo: &str,
    branch: &str,
    message: &str,
    path: &str,
) -> String {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        token,
        Some(serde_json::json!({
            "branch": branch,
            "message": message,
            "operations": [{"op": "put", "path": path, "content": message}],
        })),
    );
    assert_eq!(st, 201, "commit to {repo}/{branch}: {out}");
    out["commit"].as_str().unwrap().to_string()
}

/// A repository whose trunk has one commit, a `feature` branch one
/// commit ahead of it, and an open change `key` from that branch. Every
/// member is owned by `owner`, so approval is one person's word. Returns
/// the store prefix, so a fault can be aimed at this repository alone.
fn repo_with_change(server: &Server, admin: &str, repo: &str, owner: &str, key: &str) -> String {
    repo_with_change_to(server, admin, repo, owner, key, None)
}

/// [`repo_with_change`] with the change aimed at `target` — which may be
/// a branch the repository does not have yet.
fn repo_with_change_to(
    server: &Server,
    admin: &str,
    repo: &str,
    owner: &str,
    key: &str,
    target: Option<&str>,
) -> String {
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        Some(serde_json::json!({"name": repo, "public": false})),
    );
    assert_eq!(st, 201, "create repo {repo}: {out}");
    let prefix = format!(
        "o/{}/r/{}/prod",
        out["org_id"].as_str().unwrap(),
        out["id"].as_str().unwrap()
    );
    commit(server, admin, repo, "main", &format!("{owner}\n"), "OWNERS");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        admin,
        Some(serde_json::json!({"name": "feature", "from": "main"})),
    );
    assert_eq!(st, 201, "branch feature in {repo}: {out}");
    commit(
        server,
        admin,
        repo,
        "feature",
        &format!("change {repo}\n\nChange-Id: {key}\n"),
        "feature.txt",
    );
    let mut body = serde_json::json!({"from": "feature"});
    if let Some(t) = target {
        body["target"] = serde_json::json!(t);
    }
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        admin,
        Some(body),
    );
    assert_eq!(st, 201, "open change in {repo}: {out}");
    prefix
}

fn approve(server: &Server, email: &str, repo: &str, key: &str) {
    let cookie = sign_in(server, email);
    let (st, out) = as_person(
        server,
        &cookie,
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/approve"),
    );
    assert_eq!(st, 204, "{email} approving {repo}/{key}: {out}");
}

fn member(repo: &str, change: &str) -> serde_json::Value {
    serde_json::json!({"repo": repo, "change": change})
}

fn edge(from: (&str, &str), to: (&str, &str)) -> serde_json::Value {
    serde_json::json!({"from": member(from.0, from.1), "to": member(to.0, to.1)})
}

/// Two owned repositories, `api` and `web`, each with an approved change,
/// composed into changeset `key` with api landing first.
fn two_member_changeset(server: &Server, admin: &str, key: &str) -> (String, String) {
    let api = repo_with_change(server, admin, "api", "oa@acme.test", "Iaa000001");
    let web = repo_with_change(server, admin, "web", "ow@acme.test", "Ibb000002");
    approve(server, "oa@acme.test", "api", "Iaa000001");
    approve(server, "ow@acme.test", "web", "Ibb000002");
    let (st, out) = server.post(
        CS,
        admin,
        Some(serde_json::json!({
            "key": key,
            "title": "land",
            "members": [member("web", "Ibb000002"), member("api", "Iaa000001")],
            "edges": [edge(("api", "Iaa000001"), ("web", "Ibb000002"))],
        })),
    );
    assert_eq!(st, 201, "{out}");
    (api, web)
}

fn tip(server: &Server, admin: &str, repo: &str, branch: &str) -> Option<String> {
    let (st, refs) = server.get(&format!("/v1/orgs/acme/repos/{repo}/refs"), admin);
    assert_eq!(st, 200, "{refs}");
    refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == format!("refs/heads/{branch}"))
        .map(|r| r["oid"].as_str().unwrap().to_string())
}

fn change(server: &Server, admin: &str, repo: &str, key: &str) -> serde_json::Value {
    let (st, out) = server.get(&format!("/v1/orgs/acme/repos/{repo}/changes/{key}"), admin);
    assert_eq!(st, 200, "{out}");
    out["change"].clone()
}

fn changeset(server: &Server, admin: &str, key: &str) -> serde_json::Value {
    let (st, out) = server.get(&format!("{CS}/{key}"), admin);
    assert_eq!(st, 200, "{out}");
    out
}

/// Poll the changeset until it leaves `landing`, with a deadline that
/// says what never happened.
fn wait_settled(server: &Server, admin: &str, key: &str) -> serde_json::Value {
    for _ in 0..300 {
        let cs = changeset(server, admin, key);
        if cs["state"] != "landing" {
            return cs;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!(
        "changeset {key} never left landing: {}",
        changeset(server, admin, key)
    );
}

/// Whether `path` exists on `branch` as the API serves it.
fn has_file(server: &Server, admin: &str, repo: &str, branch: &str, path: &str) -> bool {
    let (st, _) = server.get(
        &format!("/v1/orgs/acme/repos/{repo}/files/{path}?at=refs/heads/{branch}"),
        admin,
    );
    assert!(st == 200 || st == 404, "{repo}/{path}: {st}");
    st == 200
}

fn log(server: &Server, admin: &str, repo: &str, branch: &str) -> Vec<serde_json::Value> {
    let (st, out) = server.get(
        &format!("/v1/orgs/acme/repos/{repo}/log?rev=refs/heads/{branch}"),
        admin,
    );
    assert_eq!(st, 200, "{out}");
    out["entries"].as_array().unwrap().clone()
}

/// The states of the latest landing's members, in plan order.
fn landing_states(cs: &serde_json::Value) -> Vec<(String, String)> {
    cs["landing"]["members"]
        .as_array()
        .unwrap_or_else(|| panic!("landing members in {cs}"))
        .iter()
        .map(|m| {
            (
                m["repo"].as_str().unwrap().to_string(),
                m["state"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The pre-flight refuses, before anything is written, everything the
/// doc says it must: a member without approval (named), a member whose
/// required check is still running (a wait for one change, a refusal
/// for a changeset), a failing check, a member that is not a
/// fast-forward of its trunk, a member already landed on its own, and
/// the changeset not being open. Then the ordinary landing: 202 with the
/// plan, the lander walks it api-first, both trunks move to the
/// patchset commits, both changes are `landed` in the changeset's words,
/// and the changeset's view carries the finished plan. Nothing about a
/// landing changeset may be reshaped while it lands, and a member may
/// not land on its own.
#[test]
fn a_changeset_lands_every_member_or_refuses_before_writing_anything() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cs-land-happy");
    let scratch = Scratch::new("cs-land-happy");
    let server = spawn(&bucket.base_url, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    make_user(&server, "vic@acme.test", "Vic", "viewer");

    repo_with_change(&server, &admin, "api", "oa@acme.test", "Iaa000001");
    repo_with_change(&server, &admin, "web", "ow@acme.test", "Ibb000002");
    // web's trunk requires a check; api's does not.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/protections",
        &admin,
        Some(serde_json::json!({"branch": "main"})),
    );
    assert!(st == 201 || st == 200, "protect web/main: {st} {out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/required-checks/main",
        &admin,
        Some(serde_json::json!({"name": "ci/tests"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000001",
            "title": "land",
            "members": [member("web", "Ibb000002"), member("api", "Iaa000001")],
            "edges": [edge(("api", "Iaa000001"), ("web", "Ibb000002"))],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let land = |key: &str| server.post(&format!("{CS}/{key}/land"), &admin, None);
    let api_main = tip(&server, &admin, "api", "main").unwrap();
    let web_main = tip(&server, &admin, "web", "main").unwrap();
    let untouched = || {
        assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_main);
        assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_main);
        let cs = changeset(&server, &admin, "Ic5000001");
        assert_eq!(cs["state"], "open", "{cs}");
        assert!(
            cs["landing"].is_null(),
            "a refused landing left a record: {cs}"
        );
        assert_eq!(change(&server, &admin, "api", "Iaa000001")["state"], "open");
        assert_eq!(change(&server, &admin, "web", "Ibb000002")["state"], "open");
    };

    // Nothing approved: refused at the first member in landing order.
    let (st, out) = land("Ic5000001");
    assert_eq!(st, 409, "{out}");
    let expl = out["error"].as_str().unwrap();
    assert!(expl.starts_with("api/Iaa000001: "), "{out}");
    assert!(expl.contains("oa@acme.test"), "{out}");
    // Found in the app: this body carried `gate: "ready"` — the check
    // gate, which no check was holding — beside an error that said
    // "blocked". A refusal's gate is what stands in the way, and a
    // missing approval is a block. The check still to report is listed
    // all the same: it is the next thing in the way.
    assert_eq!(out["gate"], "blocked", "{out}");
    assert_eq!(
        out["waiting_on"],
        serde_json::json!(["web/Ibb000002: ci/tests"]),
        "{out}"
    );
    untouched();

    // Approved everywhere, but web's check has not reported. A single
    // change would be queued and held; a changeset is refused, and says
    // what it is waiting on.
    approve(&server, "oa@acme.test", "api", "Iaa000001");
    approve(&server, "ow@acme.test", "web", "Ibb000002");
    let (st, out) = land("Ic5000001");
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["gate"], "waiting", "{out}");
    assert_eq!(
        out["waiting_on"],
        serde_json::json!(["web/Ibb000002: ci/tests"]),
        "{out}"
    );
    untouched();

    // A failing check: blocked, in the check's words.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/changes/Ibb000002/checks",
        &admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "failing"})),
    );
    assert!(st == 201 || st == 200, "{out}");
    let (st, out) = land("Ic5000001");
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["gate"], "blocked", "{out}");
    assert_eq!(
        out["error"], "web/Ibb000002: blocked: required check 'ci/tests' is failing",
        "{out}"
    );
    untouched();
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/changes/Ibb000002/checks",
        &admin,
        Some(serde_json::json!({"name": "ci/tests", "state": "passing"})),
    );
    assert!(st == 201 || st == 200, "{out}");

    // Green — but api's trunk has moved on since the feature branched, so
    // the patchset is not a fast-forward of it. Refused, naming the
    // member and the tip; the store is untouched.
    let moved = commit(
        &server,
        &admin,
        "api",
        "main",
        "someone else's work",
        "other.txt",
    );
    let (st, out) = land("Ic5000001");
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        format!("api/Iaa000001: not fast-forward from {}", &moved[..12]),
        "{out}"
    );
    // The same body shape as every other pre-flight refusal: a trunk
    // that moved is a block, not a field a client has to notice is
    // missing.
    assert_eq!(out["gate"], "blocked", "{out}");
    assert_eq!(out["waiting_on"], serde_json::json!([]), "{out}");
    assert_eq!(changeset(&server, &admin, "Ic5000001")["state"], "open");
    // Rebase, by re-branching: a fresh patchset on top of the new trunk.
    let (st, out) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/api/branches/feature",
        &admin,
        None,
    );
    assert!(st == 204 || st == 200, "delete api/feature: {st} {out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/branches",
        &admin,
        Some(serde_json::json!({"name": "feature", "from": "main"})),
    );
    assert_eq!(st, 201, "{out}");
    commit(
        &server,
        &admin,
        "api",
        "feature",
        "change api\n\nChange-Id: Iaa000001\n",
        "feature.txt",
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/changes",
        &admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["number"], 2, "{out}");
    let api_main = moved;
    // A new patchset needs approving again.
    let (st, out) = land("Ic5000001");
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .starts_with("api/Iaa000001: "),
        "{out}"
    );
    approve(&server, "oa@acme.test", "api", "Iaa000001");

    // Who may land: the same rule as composing. A viewer and a stranger
    // are told there is no changeset, and nobody at all is asked who they
    // are before anything is looked up; a hostile key is a 400 or a 404;
    // an unknown org is a 404.
    let vic = sign_in(&server, "vic@acme.test");
    let (st, out) = as_person(&server, &vic, "POST", &format!("{CS}/Ic5000001/land"));
    assert_eq!(st, 404, "a viewer landing: {out}");
    let rival = server.bootstrap_org("rival");
    let (st, out) = server.post(&format!("{CS}/Ic5000001/land"), &rival, None);
    assert_eq!(st, 404, "{out}");
    let (st, out) = server.post(&format!("{CS}/Ic5000001/land"), "", None);
    assert_eq!(st, 401, "{out}");
    let (st, out) = server.post("/v1/orgs/nobody/changesets/Ic5000001/land", &admin, None);
    assert_eq!(st, 404, "{out}");
    let (st, out) = land("Ic5000009");
    assert_eq!(st, 404, "{out}");
    assert_eq!(out["error"], "no changeset \"Ic5000009\"", "{out}");
    for inj in INJECTIONS {
        let (st, out) = server.post(&format!("{CS}/{}/land", percent_encode(inj)), &admin, None);
        assert!(st == 400 || st == 404, "{inj:?}: {st} {out}");
    }
    assert_eq!(changeset(&server, &admin, "Ic5000001")["state"], "open");

    // The landing. 202 with the plan in landing order: each member's ref,
    // the tip it was judged against, and the commit it lands.
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let web_ps = change(&server, &admin, "web", "Ibb000002")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, out) = land("Ic5000001");
    assert_eq!(st, 202, "{out}");
    assert_eq!(out["queued"], true, "{out}");
    assert_eq!(out["changeset"], "Ic5000001");
    assert!(out["job"].as_str().is_some_and(|j| !j.is_empty()), "{out}");
    assert!(
        out["landing"].as_str().is_some_and(|l| !l.is_empty()),
        "{out}"
    );
    assert_eq!(
        out["plan"],
        serde_json::json!([
            {"repo": "api", "change": "Iaa000001", "ref": "refs/heads/main", "old": api_main, "new": api_ps},
            {"repo": "web", "change": "Ibb000002", "ref": "refs/heads/main", "old": web_main, "new": web_ps},
        ]),
        "{out}"
    );
    let landing_id = out["landing"].as_str().unwrap().to_string();

    // While it lands: nothing may reshape it, nothing may land twice, and
    // a member may not land alone. (The lander polls every second, so
    // these may race the landing itself; every answer is one of the two
    // the state machine allows, and none is a 5xx.)
    let (st, out) = land("Ic5000001");
    assert!(st == 409, "a second land: {st} {out}");
    assert!(
        out["error"] == "changeset is landing" || out["error"] == "changeset is landed",
        "{out}"
    );
    let (st, out) = server.post(&format!("{CS}/Ic5000001/abandon"), &admin, None);
    assert_eq!(st, 409, "{out}");
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/changes/Ibb000002/land",
        &admin,
        None,
    );
    assert_eq!(st, 409, "{out}");

    let cs = wait_settled(&server, &admin, "Ic5000001");
    assert_eq!(cs["state"], "landed", "{cs}");
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_ps);
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_ps);
    for (repo, key, commit) in [("api", "Iaa000001", &api_ps), ("web", "Ibb000002", &web_ps)] {
        let c = change(&server, &admin, repo, key);
        assert_eq!(c["state"], "landed", "{c}");
        assert_eq!(c["landed_commit"], serde_json::json!(commit), "{c}");
        assert_eq!(c["land_verdict"], "landed with changeset Ic5000001", "{c}");
    }
    assert_eq!(cs["landing"]["id"], serde_json::json!(landing_id), "{cs}");
    assert_eq!(cs["landing"]["outcome"], "landed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 1, "{cs}");
    assert!(cs["landing"]["finished_at"].as_i64().is_some(), "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![("api".into(), "done".into()), ("web".into(), "done".into())]
    );
    assert_eq!(
        cs["landing"]["members"][0]["new"],
        serde_json::json!(api_ps),
        "{cs}"
    );
    assert!(cs["landing"]["members"][0]["note"].is_null(), "{cs}");
    // The verdict endpoint agrees, and a landed changeset is final.
    let (st, out) = server.get(&format!("{CS}/Ic5000001/verdict"), &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["explanation"], "changeset is landed", "{out}");
    let (st, out) = land("Ic5000001");
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], "changeset is landed", "{out}");
    let (st, out) = server.post(&format!("{CS}/Ic5000001/abandon"), &admin, None);
    assert_eq!(st, 409, "{out}");

    // A change that landed by inclusion cannot be composed again, and a
    // changeset over one cannot exist to be landed.
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000002",
            "title": "again",
            "members": [member("api", "Iaa000001")],
        })),
    );
    assert_eq!(st, 409, "{out}");
    assert!(server.healthy());
}

/// Step 4's failure branch. The plan is made against the trunks as they
/// stood; then web's trunk moves before the lander gets there. api lands
/// (it was first), web's CAS finds a different tip and fails, and the
/// unwind puts api back: a revert commit restoring the pre-landing tree
/// on top of the landed one — never a rewind — so api's trunk is one
/// commit longer and `feature.txt` is gone from it. The changeset is
/// `failed`, api's change is open again and says it was landed then
/// reverted and why, web's change is open again and says what moved, and
/// the landing's record shows `reverted` / `failed` member by member.
/// Both are free to be composed again.
#[test]
fn a_member_whose_trunk_moved_fails_the_landing_and_the_landed_ones_are_reverted() {
    let minio = Minio::shared();
    let bucket = minio.bucket("cs-land-unwind");
    let scratch = Scratch::new("cs-land-unwind");
    // The lander is off so the world can move between the plan and the
    // apply — the window the protocol exists for.
    let mut server = spawn(&bucket.base_url, &scratch, "0");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    two_member_changeset(&server, &admin, "Ic5000003");
    let api_before = tip(&server, &admin, "api", "main").unwrap();
    let web_before = tip(&server, &admin, "web", "main").unwrap();
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();

    let (st, out) = server.post(&format!("{CS}/Ic5000003/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    assert_eq!(
        out["plan"][1]["old"],
        serde_json::json!(web_before),
        "{out}"
    );
    let cs = changeset(&server, &admin, "Ic5000003");
    assert_eq!(cs["state"], "landing", "{cs}");
    assert_eq!(cs["landing"]["outcome"], serde_json::Value::Null, "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "pending".into()),
            ("web".into(), "pending".into())
        ]
    );
    assert_eq!(
        change(&server, &admin, "web", "Ibb000002")["state"],
        "landing"
    );

    // With the lander stopped the window is held open, so what a landing
    // changeset refuses can be asserted without racing it: it cannot be
    // reshaped, abandoned or landed again, and a member cannot land on
    // its own. A third change to try to add:
    repo_with_change(&server, &admin, "cli", "oa@acme.test", "Icc000003");
    for (method, path, body) in [
        ("POST", "members", Some(member("cli", "Icc000003"))),
        ("PUT", "edges", Some(serde_json::json!({"edges": []}))),
        ("DELETE", "members/web/Ibb000002", None),
        ("POST", "abandon", None),
        ("POST", "land", None),
    ] {
        let (st, out) = server.req(method, &format!("{CS}/Ic5000003/{path}"), &admin, body);
        assert_eq!(st, 409, "{method} {path} on a landing changeset: {out}");
        assert_eq!(
            out["error"], "changeset is landing",
            "{method} {path}: {out}"
        );
    }
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/changes/Ibb000002/land",
        &admin,
        None,
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], "change is landing", "{out}");
    // Nor be abandoned: it is bound, and the changeset owns that call.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/changes/Ibb000002/abandon",
        &admin,
        None,
    );
    assert_eq!(st, 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("member of changeset Ic5000003"),
        "{out}"
    );
    // And nothing above moved anything.
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_before);
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_before);

    // Somebody pushes to web's trunk in the window.
    let web_moved = commit(&server, &admin, "web", "main", "hotfix", "hotfix.txt");

    server.restart_with(&[("STRATUM_LAND_POLL_SECS", "1".into())]);
    let cs = wait_settled(&server, &admin, "Ic5000003");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(cs["landing"]["outcome"], "failed", "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "reverted".into()),
            ("web".into(), "failed".into())
        ],
        "{cs}"
    );
    assert_eq!(
        cs["landing"]["members"][1]["note"],
        format!(
            "refs/heads/main moved to {} before web/Ibb000002 could land",
            &web_moved[..12]
        ),
        "{cs}"
    );

    // web's trunk is the hotfix, untouched by us.
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_moved);
    assert!(!has_file(&server, &admin, "web", "main", "feature.txt"));

    // api's trunk: the revert, on top of the landed patchset, on top of
    // the old trunk. The tree is the old trunk's; the file is gone.
    let api_after = tip(&server, &admin, "api", "main").unwrap();
    assert_ne!(api_after, api_before, "a rewind, not a revert");
    assert_ne!(api_after, api_ps, "api was left landed");
    let entries = log(&server, &admin, "api", "main");
    assert_eq!(
        entries[0]["commit"],
        serde_json::json!(api_after),
        "{entries:?}"
    );
    assert_eq!(
        entries[0]["parents"],
        serde_json::json!([api_ps]),
        "{entries:?}"
    );
    assert_eq!(
        entries[1]["commit"],
        serde_json::json!(api_ps),
        "{entries:?}"
    );
    assert_eq!(
        entries[2]["commit"],
        serde_json::json!(api_before),
        "{entries:?}"
    );
    assert_eq!(
        entries[0]["tree"], entries[2]["tree"],
        "the revert restores the old tree"
    );
    let msg = entries[0]["message"].as_str().unwrap();
    assert!(
        msg.starts_with("Revert api/Iaa000001: changeset Ic5000003 did not land\n"),
        "{msg:?}"
    );
    assert!(msg.contains("web/Ibb000002"), "{msg:?}");
    assert!(
        msg.contains(&format!("Reverts commit {api_ps}.")),
        "{msg:?}"
    );
    assert!(!has_file(&server, &admin, "api", "main", "feature.txt"));
    assert!(has_file(&server, &admin, "api", "main", "OWNERS"));
    assert_eq!(
        cs["landing"]["members"][0]["note"],
        serde_json::json!(api_after),
        "the record names the revert: {cs}"
    );

    // Both changes are open again, each with its own sentence.
    let a = change(&server, &admin, "api", "Iaa000001");
    assert_eq!(a["state"], "open", "{a}");
    assert_eq!(
        a["land_verdict"],
        format!(
            "landed, then reverted in {}: web/Ibb000002 — refs/heads/main moved to {} \
             before web/Ibb000002 could land — push a new patchset to land again",
            &api_after[..12],
            &web_moved[..12]
        ),
        "{a}"
    );
    let b = change(&server, &admin, "web", "Ibb000002");
    assert_eq!(b["state"], "open", "{b}");
    assert_eq!(
        b["land_verdict"],
        format!(
            "ejected: refs/heads/main moved to {} before web/Ibb000002 could land",
            &web_moved[..12]
        ),
        "{b}"
    );

    // Released: the members may be composed again, and the failed
    // changeset is final.
    let (st, out) = server.post(&format!("{CS}/Ic5000003/land"), &admin, None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], "changeset is failed", "{out}");
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000004",
            "title": "again",
            "members": [member("api", "Iaa000001"), member("web", "Ibb000002")],
        })),
    );
    assert_eq!(st, 201, "{out}");
    // Its verdict says what is now true: api's patchset no longer
    // fast-forwards the reverted trunk — the pre-flight, not the verdict,
    // says so, in the words a person can act on.
    let (st, out) = server.post(&format!("{CS}/Ic5000004/land"), &admin, None);
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        format!("api/Iaa000001: not fast-forward from {}", &api_after[..12]),
        "{out}"
    );
    assert!(server.healthy());
}

/// Step 5, the answer-lost branch. Every manifest PUT is applied by the
/// store and answered 503 — the fault an outage cannot simulate — so
/// each attempt lands one member and then dies. The reaper hands the
/// landing to a fresh job each time; the new driver reads the trunk, finds
/// it already at the member's commit, trusts the store over the record,
/// and moves on. The changeset lands on the third attempt with nothing
/// reverted and nothing landed twice.
#[test]
fn a_landing_whose_driver_dies_after_each_cas_is_finished_from_the_store() {
    let (proxy, scratch) = proxied("cs-land-resume-after");
    let server = spawn(&proxy.url, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    two_member_changeset(&server, &admin, "Ic5000005");
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let web_ps = change(&server, &admin, "web", "Ibb000002")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();

    // The pre-flight only reads, so the plan can be armed before it.
    proxy.handle.set_plan(
        FaultPlan::new(7).with(
            FaultRule::new(Fault::ErrorAfter, 1.0)
                .only_methods(["PUT"])
                .only_keys(["manifest.json"]),
        ),
    );
    let (st, out) = server.post(&format!("{CS}/Ic5000005/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let cs = wait_settled(&server, &admin, "Ic5000005");
    proxy.handle.clear_plan();
    assert_eq!(cs["state"], "landed", "{cs}");
    assert_eq!(
        cs["landing"]["attempt"], 3,
        "one job per member CAS, plus the one that finished: {cs}"
    );
    assert_eq!(
        proxy.handle.stats().count(Fault::ErrorAfter),
        2,
        "each member's CAS was answered 503 exactly once"
    );
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_ps);
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_ps);
    assert_eq!(
        landing_states(&cs),
        vec![("api".into(), "done".into()), ("web".into(), "done".into())]
    );
    assert_eq!(
        log(&server, &admin, "api", "main").len(),
        2,
        "landed once, not twice"
    );
    for (repo, key) in [("api", "Iaa000001"), ("web", "Ibb000002")] {
        let c = change(&server, &admin, repo, key);
        assert_eq!(c["state"], "landed", "{c}");
        assert_eq!(c["land_verdict"], "landed with changeset Ic5000005", "{c}");
    }
    assert!(server.healthy());
}

/// Step 5, the not-yet-applied branch. The first manifest PUT is refused
/// outright — nothing landed — and the job dies with the plan's first
/// step pending. The reaper's driver reads the trunk, finds it where the
/// plan said, and makes the CAS itself. The changeset lands on the
/// second attempt.
#[test]
fn a_landing_whose_driver_dies_before_its_cas_is_resumed_from_the_plan() {
    let (proxy, scratch) = proxied("cs-land-resume-before");
    let server = spawn(&proxy.url, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, _) = two_member_changeset(&server, &admin, "Ic5000006");
    let api_before = tip(&server, &admin, "api", "main").unwrap();
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();

    proxy
        .handle
        .inject(&format!("PUT {api_prefix}/manifest.json"), 1, 503);
    let (st, out) = server.post(&format!("{CS}/Ic5000006/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    // The first driver dies with api still where it was.
    assert!(
        (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            changeset(&server, &admin, "Ic5000006")["landing"]["attempt"] == 2
        }),
        "the reaper never handed the landing to a second job: {}",
        changeset(&server, &admin, "Ic5000006")
    );
    let cs = wait_settled(&server, &admin, "Ic5000006");
    assert_eq!(cs["state"], "landed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 2, "{cs}");
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_ps);
    assert_ne!(api_before, api_ps);
    assert_eq!(
        landing_states(&cs),
        vec![("api".into(), "done".into()), ("web".into(), "done".into())]
    );
    assert_eq!(
        change(&server, &admin, "api", "Iaa000001")["state"],
        "landed"
    );
    assert!(server.healthy());
}

/// The resumed driver reads the trunk's *history*, not just its tip. The
/// first driver's CAS on api is applied and answered 503, so the record
/// still says `pending` when the job dies; before the reaper gets to it
/// somebody pushes on top of api's new trunk, and somebody else pushes
/// to web's. A driver that only compared tips would call api "moved
/// before it could land" and eject a change whose commit is on the
/// trunk. This one finds the landed commit in the tip's history: api is
/// `done` with a note saying the trunk has moved on, web fails the
/// landing, and api — which cannot be reverted over somebody's push — is
/// left `landed` and says so.
#[test]
fn a_member_landed_by_a_dead_driver_and_pushed_over_is_kept_landed() {
    let (proxy, scratch) = proxied("cs-land-landed-then-pushed");
    let mut server = spawn_with(
        &proxy.url,
        &scratch,
        "1",
        &[("STRATUM_LAND_RECHECK_SECS", "600")],
    );
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, _) = two_member_changeset(&server, &admin, "Ic5000007");
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();

    proxy.handle.set_plan(
        FaultPlan::new(11).with(
            FaultRule::new(Fault::ErrorAfter, 1.0)
                .only_methods(["PUT"])
                .only_keys([format!("{api_prefix}/manifest.json")]),
        ),
    );
    let (st, out) = server.post(&format!("{CS}/Ic5000007/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    wait_for("api's CAS to be applied and its answer lost", || {
        tip(&server, &admin, "api", "main").as_deref() == Some(api_ps.as_str())
            && proxy.handle.stats().count(Fault::ErrorAfter) == 1
    });
    proxy.handle.clear_plan();
    // The record is behind the store, and the reaper is ten minutes away.
    let cs = changeset(&server, &admin, "Ic5000007");
    assert_eq!(cs["state"], "landing", "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "pending".into()),
            ("web".into(), "pending".into())
        ],
        "{cs}"
    );

    // The world moves: on top of api's landed trunk, and on web's.
    let api_over = commit(
        &server,
        &admin,
        "api",
        "main",
        "built on the landing",
        "next.txt",
    );
    let web_moved = commit(&server, &admin, "web", "main", "hotfix", "hotfix.txt");

    server.restart_with(&[("STRATUM_LAND_RECHECK_SECS", "1".into())]);
    let cs = wait_settled(&server, &admin, "Ic5000007");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 2, "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "done".into()),
            ("web".into(), "failed".into())
        ],
        "{cs}"
    );
    let note = format!(
        "refs/heads/main moved to {} after api/Iaa000001 landed",
        &api_over[..12]
    );
    assert_eq!(
        cs["landing"]["members"][0]["note"],
        serde_json::json!(note),
        "{cs}"
    );
    assert_eq!(
        cs["landing"]["members"][1]["note"],
        format!(
            "refs/heads/main moved to {} before web/Ibb000002 could land",
            &web_moved[..12]
        ),
        "{cs}"
    );
    // api is landed — its commit is on the trunk, under somebody's — and
    // nothing was rewound or reverted over that push.
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_over);
    let a = change(&server, &admin, "api", "Iaa000001");
    assert_eq!(a["state"], "landed", "{a}");
    assert_eq!(a["landed_commit"], serde_json::json!(api_ps), "{a}");
    assert_eq!(
        a["land_verdict"],
        format!("landed with changeset Ic5000007; {note}"),
        "{a}"
    );
    let b = change(&server, &admin, "web", "Ibb000002");
    assert_eq!(b["state"], "open", "{b}");
    assert!(
        b["land_verdict"]
            .as_str()
            .unwrap()
            .starts_with("ejected: refs/heads/main moved to"),
        "{b}"
    );
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_moved);
    assert!(server.healthy());
}

/// The same reading, for the revert. The unwind's revert CAS on api is
/// applied and answered 503, so the record still says `done` when the
/// job dies, and somebody then pushes on top of the revert. The resumed
/// driver finds the landed commit in the tip's history and, directly on
/// top of it, a commit that is exactly the revert it would have written:
/// api is `reverted` naming that commit, not "not reverted" and not a
/// second revert.
#[test]
fn a_revert_whose_answer_was_lost_is_recognised_under_a_later_push() {
    let (proxy, scratch) = proxied("cs-land-revert-lost");
    let mut server = spawn(&proxy.url, &scratch, "0");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, _) = two_member_changeset(&server, &admin, "Ic5000008");
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();

    let (st, out) = server.post(&format!("{CS}/Ic5000008/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let web_moved = commit(&server, &admin, "web", "main", "hotfix", "hotfix.txt");

    // The driver writes api's manifest twice: the landing CAS, then —
    // having read web's moved trunk — the revert. Lose the answer to the
    // second. Counted on the wire rather than armed on a read, because a
    // restarted node reads manifests for its own reasons and a read is
    // not a position in the driver's sequence; a PUT to this key is.
    let handle = proxy.handle.clone();
    let puts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = puts.clone();
    let api_cas = format!("{api_prefix}/manifest.json");
    let revert_key = api_cas.clone();
    proxy.handle.observe(move |line, _| {
        if line.starts_with("PUT ")
            && line.contains(&api_cas)
            && seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1
        {
            handle.set_plan(
                FaultPlan::new(13).with(
                    FaultRule::new(Fault::ErrorAfter, 1.0)
                        .only_methods(["PUT"])
                        .only_keys([revert_key.clone()]),
                ),
            );
        }
    });
    server.restart_with(&[
        ("STRATUM_LAND_POLL_SECS", "1".into()),
        ("STRATUM_LAND_RECHECK_SECS", "600".into()),
    ]);
    wait_for("the revert to be applied and its answer lost", || {
        proxy.handle.stats().count(Fault::ErrorAfter) == 1
    });
    proxy.handle.clear_observer();
    proxy.handle.clear_plan();
    let revert = tip(&server, &admin, "api", "main").unwrap();
    let entries = log(&server, &admin, "api", "main");
    assert_eq!(
        entries[0]["parents"],
        serde_json::json!([api_ps]),
        "{entries:?}"
    );
    assert!(
        entries[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("Revert api/Iaa000001:"),
        "{entries:?}"
    );
    let cs = changeset(&server, &admin, "Ic5000008");
    assert_eq!(cs["state"], "landing", "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "done".into()),
            ("web".into(), "failed".into())
        ],
        "the record is behind the store: {cs}"
    );

    // Somebody builds on the revert before the reaper gets there.
    let over = commit(&server, &admin, "api", "main", "carrying on", "next.txt");

    server.restart_with(&[("STRATUM_LAND_RECHECK_SECS", "1".into())]);
    let cs = wait_settled(&server, &admin, "Ic5000008");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 2, "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "reverted".into()),
            ("web".into(), "failed".into())
        ],
        "{cs}"
    );
    assert_eq!(
        cs["landing"]["members"][0]["note"],
        serde_json::json!(revert),
        "{cs}"
    );
    // One revert, nothing on top of the push, and the change says so.
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), over);
    assert_eq!(log(&server, &admin, "api", "main").len(), 4);
    let a = change(&server, &admin, "api", "Iaa000001");
    assert_eq!(a["state"], "open", "{a}");
    assert_eq!(
        a["land_verdict"],
        format!(
            "landed, then reverted in {}: web/Ibb000002 — refs/heads/main moved to {} \
             before web/Ibb000002 could land — push a new patchset to land again",
            &revert[..12],
            &web_moved[..12]
        ),
        "{a}"
    );
    assert!(server.healthy());
}

/// The two ways a landed member is left landed by the unwind, in one
/// landing of three: the driver records api and cli `done`, then dies
/// reading web's manifest. Before the reaper resumes it, somebody pushes
/// on top of api, somebody resets cli back to where it was, and web's
/// trunk moves. web fails the landing; cli's trunk no longer has the
/// landed commit in its history and api's has a stranger's commit on
/// top of it — neither is ours to rewind over, so both stay `landed`
/// and say `not reverted`, and a person is told where to look.
#[test]
fn a_landed_member_that_others_moved_or_reset_is_left_landed_and_says_so() {
    let (proxy, scratch) = proxied("cs-land-not-reverted");
    let mut server = spawn_with(
        &proxy.url,
        &scratch,
        "1",
        &[
            ("STRATUM_LAND_RECHECK_SECS", "600"),
            // Whether the job has *failed* or is merely dead when the node
            // restarts below is a race this test does not care to win:
            // either way the landing is picked up within a few seconds.
            ("STRATUM_LAND_LEASE_SECS", "2"),
        ],
    );
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    make_user(&server, "oc@acme.test", "Owner Of Cli", "member");
    repo_with_change(&server, &admin, "api", "oa@acme.test", "Iaa000001");
    let cli_prefix = repo_with_change(&server, &admin, "cli", "oc@acme.test", "Icc000003");
    let web_prefix = repo_with_change(&server, &admin, "web", "ow@acme.test", "Ibb000002");
    approve(&server, "oa@acme.test", "api", "Iaa000001");
    approve(&server, "oc@acme.test", "cli", "Icc000003");
    approve(&server, "ow@acme.test", "web", "Ibb000002");
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000009",
            "title": "three",
            "members": [member("web", "Ibb000002"), member("cli", "Icc000003"), member("api", "Iaa000001")],
            "edges": [
                edge(("api", "Iaa000001"), ("cli", "Icc000003")),
                edge(("cli", "Icc000003"), ("web", "Ibb000002")),
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let cli_old = tip(&server, &admin, "cli", "main").unwrap();
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let cli_ps = change(&server, &admin, "cli", "Icc000003")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();

    // The pre-flight reads web's manifest too, so the fault is armed
    // from the wire, on cli's CAS: everything the driver reads of web
    // after that is refused, retries included, and the job fails with
    // api and cli recorded `done`.
    let handle = proxy.handle.clone();
    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = armed.clone();
    let cli_cas = format!("{cli_prefix}/manifest.json");
    let web_read = format!("{web_prefix}/manifest.json");
    proxy.handle.observe(move |line, _| {
        if line.starts_with("PUT ")
            && line.contains(&cli_cas)
            && !flag.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            handle.set_plan(
                FaultPlan::new(17).with(
                    FaultRule::new(Fault::ErrorAfter, 1.0)
                        .only_methods(["GET"])
                        .only_keys([web_read.clone()]),
                ),
            );
        }
    });
    let (st, out) = server.post(&format!("{CS}/Ic5000009/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let job = out["job"].as_str().unwrap().to_string();
    // The driver's death is read from **its job**, not from a count of
    // faults on the wire. A count says three requests were refused; it
    // does not say the driver has stopped, and clearing the plan while
    // it was still going gave it a working store back — CI landed all
    // three members under a test that is about the landing failing,
    // where this machine had always got there first. The same lesson,
    // and the same fix, as the unwind test further down: wait for the
    // row the reaper itself reads.
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    wait_for("the driver to die reading web", || {
        stratum_control::jobs::get(&db, &org_id, &job)
            .unwrap()
            .is_some_and(|j| j.state == "failed")
    });
    assert!(
        proxy.handle.stats().count(Fault::ErrorAfter) >= 3,
        "web's read was refused, retries included"
    );
    proxy.handle.clear_observer();
    proxy.handle.clear_plan();
    let cs = changeset(&server, &admin, "Ic5000009");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "done".into()),
            ("cli".into(), "done".into()),
            ("web".into(), "pending".into()),
        ],
        "{cs}"
    );
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_ps);
    assert_eq!(tip(&server, &admin, "cli", "main").unwrap(), cli_ps);

    // The world moves under a landing whose driver is dead.
    let api_over = commit(
        &server,
        &admin,
        "api",
        "main",
        "built on the landing",
        "next.txt",
    );
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/cli/reset",
        &admin,
        Some(serde_json::json!({"branch": "main", "to": cli_old})),
    );
    assert_eq!(st, 200, "reset cli/main: {out}");
    let web_moved = commit(&server, &admin, "web", "main", "hotfix", "hotfix.txt");

    server.restart_with(&[("STRATUM_LAND_RECHECK_SECS", "1".into())]);
    let cs = wait_settled(&server, &admin, "Ic5000009");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "done".into()),
            ("cli".into(), "done".into()),
            ("web".into(), "failed".into()),
        ],
        "{cs}"
    );
    let api_note = format!(
        "not reverted: refs/heads/main moved to {} after it landed",
        &api_over[..12]
    );
    let cli_note = format!(
        "not reverted: refs/heads/main moved to {} after it landed",
        &cli_old[..12]
    );
    assert_eq!(
        cs["landing"]["members"][0]["note"],
        serde_json::json!(api_note),
        "{cs}"
    );
    assert_eq!(
        cs["landing"]["members"][1]["note"],
        serde_json::json!(cli_note),
        "{cs}"
    );
    assert_eq!(
        cs["landing"]["members"][2]["note"],
        format!(
            "refs/heads/main moved to {} before web/Ibb000002 could land",
            &web_moved[..12]
        ),
        "{cs}"
    );
    // Nothing of ours was written over anybody's: the trunks are exactly
    // where the other writers left them.
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_over);
    assert_eq!(tip(&server, &admin, "cli", "main").unwrap(), cli_old);
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_moved);
    for (repo, key, note) in [
        ("api", "Iaa000001", &api_note),
        ("cli", "Icc000003", &cli_note),
    ] {
        let c = change(&server, &admin, repo, key);
        assert_eq!(c["state"], "landed", "{c}");
        assert_eq!(
            c["land_verdict"],
            format!("landed with changeset Ic5000009; {note}"),
            "{c}"
        );
    }
    let b = change(&server, &admin, "web", "Ibb000002");
    assert_eq!(b["state"], "open", "{b}");
    assert!(server.healthy());
}

/// The pre-flight reads every member's trunk, twice — once to judge the
/// change, once to fix the tip its CAS will expect — and a store that
/// fails under either read fails the request and begins nothing: the
/// changeset is still open, there is no landing, no job, and the same
/// request is a 202 once the store answers again.
#[test]
fn a_store_failure_during_the_pre_flight_begins_nothing() {
    let (proxy, scratch) = proxied("cs-land-preflight-store");
    let server = spawn(&proxy.url, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, web_prefix) = two_member_changeset(&server, &admin, "Ic500000a");
    let api_manifest = format!("{api_prefix}/manifest.json");
    let deny_api_reads = move || {
        FaultPlan::new(19).with(
            FaultRule::new(Fault::ErrorAfter, 1.0)
                .only_methods(["GET"])
                .only_keys([api_manifest.clone()]),
        )
    };

    // Under the verdict's read. `ErrorAfter` answers 503 to every
    // attempt, so the store client's retries are spent too.
    proxy.handle.set_plan(deny_api_reads());
    let (st, out) = server.post(&format!("{CS}/Ic500000a/land"), &admin, None);
    assert!(
        st >= 500,
        "a store outage is a server error, got {st}: {out}"
    );
    assert_eq!(proxy.handle.stats().count(Fault::ErrorAfter), 3, "retried");
    // The composed verdict reads the same way and fails the same way.
    let (st, out) = server.get(&format!("{CS}/Ic500000a/verdict"), &admin);
    assert!(
        st >= 500,
        "a store outage is a server error, got {st}: {out}"
    );
    assert_eq!(proxy.handle.stats().count(Fault::ErrorAfter), 6, "retried");
    let cs = changeset(&server, &admin, "Ic500000a");
    assert_eq!(cs["state"], "open", "{cs}");
    assert!(cs["landing"].is_null(), "no landing began: {cs}");
    proxy.handle.clear_plan();

    // Under the tip's read: the verdicts are in, and api's manifest
    // fails when the plan comes to fix api's tip. Armed from web's first
    // read so the verdict's read of api is the one that succeeded.
    let handle = proxy.handle.clone();
    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let web_manifest = format!("{web_prefix}/manifest.json");
    proxy.handle.observe({
        let armed = armed.clone();
        move |line, _| {
            if line.starts_with("GET ")
                && line.contains(&web_manifest)
                && !armed.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                handle.set_plan(deny_api_reads());
            }
        }
    });
    let (st, out) = server.post(&format!("{CS}/Ic500000a/land"), &admin, None);
    assert!(
        armed.load(std::sync::atomic::Ordering::SeqCst),
        "the verdicts were never read, so nothing was under test"
    );
    assert!(
        st >= 500,
        "a store outage is a server error, got {st}: {out}"
    );
    // Three more: the store client retried this read too.
    assert_eq!(proxy.handle.stats().count(Fault::ErrorAfter), 9, "retried");
    let cs = changeset(&server, &admin, "Ic500000a");
    assert_eq!(cs["state"], "open", "{cs}");
    assert!(cs["landing"].is_null(), "no landing began: {cs}");
    proxy.handle.clear_observer();
    proxy.handle.clear_plan();

    // The store is back; the same request lands.
    let (st, out) = server.post(&format!("{CS}/Ic500000a/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let cs = wait_settled(&server, &admin, "Ic500000a");
    assert_eq!(cs["state"], "landed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 1, "{cs}");
    assert!(server.healthy());
}

/// The commit point is where the changeset's state is checked *for
/// real*: the handler's early "is it open" is a courtesy, and a changeset
/// abandoned while its pre-flight is reading the store — the store being
/// slow is when people get impatient — is refused at `begin_landing`,
/// after the plan was made and a job was created for it. The refusal
/// names the state, nothing is written, and the job it minted is
/// completed as a no-op rather than left for the reaper to puzzle over.
#[test]
fn a_changeset_abandoned_during_its_pre_flight_is_refused_at_the_commit_point() {
    let (proxy, scratch) = proxied("cs-land-abandoned-in-flight");
    let server = spawn(&proxy.url, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, _) = two_member_changeset(&server, &admin, "Ic500000b");

    // Every read of api's manifest parks until the plan is cleared, and
    // is then relayed — the store was slow, not broken.
    proxy.handle.set_plan(
        FaultPlan::new(23)
            .with(
                FaultRule::new(Fault::Hang, 1.0)
                    .only_methods(["GET"])
                    .only_keys([format!("{api_prefix}/manifest.json")]),
            )
            .hang_for(Duration::from_secs(120)),
    );
    let (st, out) = std::thread::scope(|s| {
        let land = s.spawn(|| server.post(&format!("{CS}/Ic500000b/land"), &admin, None));
        wait_for("the pre-flight to be parked on api's manifest", || {
            proxy.handle.stats().count(Fault::Hang) >= 1
        });
        let (st, out) = server.post(&format!("{CS}/Ic500000b/abandon"), &admin, None);
        assert_eq!(st, 204, "{out}");
        proxy.handle.clear_plan();
        land.join().unwrap()
    });
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], "changeset is abandoned", "{out}");
    let cs = changeset(&server, &admin, "Ic500000b");
    assert_eq!(cs["state"], "abandoned", "{cs}");
    assert!(cs["landing"].is_null(), "no landing began: {cs}");
    // Nothing moved, and the members are ordinary open changes again.
    for repo in ["api", "web"] {
        assert!(!has_file(&server, &admin, repo, "main", "feature.txt"));
    }
    assert_eq!(change(&server, &admin, "api", "Iaa000001")["state"], "open");
    // The job minted for the refused landing was completed as a no-op,
    // not left for the queue: asked the way the lander asks, the queue
    // has no land job to hand out.
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    assert!(
        stratum_control::jobs::claim(&db, "land", 1_000)
            .unwrap()
            .is_none(),
        "the refused landing's job is still claimable"
    );
    assert!(server.healthy());
}

/// The window the plan exists to close: between the pre-flight's read of
/// api's tip and the lander's CAS against it, another node takes a push
/// to api. The CAS meets a manifest that is not the one it expected and
/// the member fails right there — nothing is re-proved against the new
/// tip, because the members were judged together against the tips they
/// were judged against — web is never attempted, and both changes say
/// what happened.
#[test]
fn a_push_that_wins_the_window_between_the_drivers_read_and_its_cas_fails_the_landing() {
    let (proxy, direct, scratch) = proxied_pair("cs-land-cas-window-apply");
    let server = spawn(&proxy.url, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, _) = two_member_changeset(&server, &admin, "Ic500000c");
    let api_old = tip(&server, &admin, "api", "main").unwrap();
    let web_old = tip(&server, &admin, "web", "main").unwrap();
    let other = second_node(&direct, &scratch, &server);

    // The first write to api's manifest — the CAS — parks. The read
    // before it already happened, so the driver is holding a tip that
    // is about to be stale.
    proxy.handle.set_plan(
        FaultPlan::new(29)
            .with(
                FaultRule::new(Fault::Hang, 1.0)
                    .only_methods(["PUT"])
                    .only_keys([format!("{api_prefix}/manifest.json")]),
            )
            .hang_for(Duration::from_secs(120)),
    );
    let (st, out) = server.post(&format!("{CS}/Ic500000c/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    wait_for("the driver's CAS on api to be in flight", || {
        proxy.handle.stats().count(Fault::Hang) == 1
    });
    let hotfix = commit(&other, &admin, "api", "main", "hotfix", "hotfix.txt");
    // Release the CAS: it is relayed as written, against a tip that is
    // gone, and the store refuses it.
    proxy.handle.clear_plan();

    let cs = wait_settled(&server, &admin, "Ic500000c");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 1, "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "failed".into()),
            ("web".into(), "pending".into())
        ],
        "{cs}"
    );
    let note = format!(
        "refs/heads/main moved to {} before api/Iaa000001 could land",
        &hotfix[..12]
    );
    assert_eq!(cs["landing"]["members"][0]["note"], note.as_str(), "{cs}");
    assert!(cs["landing"]["members"][1]["note"].is_null(), "{cs}");
    // The push stands, web is untouched, and both changes are open with
    // the reason on them.
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), hotfix);
    assert_ne!(hotfix, api_old);
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_old);
    let a = change(&server, &admin, "api", "Iaa000001");
    assert_eq!(a["state"], "open", "{a}");
    assert_eq!(a["land_verdict"], format!("ejected: {note}"), "{a}");
    let b = change(&server, &admin, "web", "Ibb000002");
    assert_eq!(b["state"], "open", "{b}");
    assert_eq!(
        b["land_verdict"],
        format!("ejected: not attempted, api/Iaa000001 — {note}"),
        "{b}"
    );
    assert!(server.healthy());
    assert!(other.healthy());
}

/// The same window, on the way back. api landed, web's trunk had moved,
/// and the revert's CAS on api is in flight when another node takes a
/// push on top of api's landed commit. The revert is refused by the
/// store; the driver does not rewind over somebody's push, records that
/// api is landed and was not reverted, and api's change says so.
#[test]
fn a_push_that_wins_the_window_under_the_revert_leaves_the_member_landed() {
    let (proxy, direct, scratch) = proxied_pair("cs-land-cas-window-unwind");
    let mut server = spawn(&proxy.url, &scratch, "0");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, _) = two_member_changeset(&server, &admin, "Ic500000d");
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let other = second_node(&direct, &scratch, &server);

    // The plan is made against web's current tip, then web moves, so the
    // landing will fail at web and unwind api.
    let (st, out) = server.post(&format!("{CS}/Ic500000d/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let hotfix = commit(&server, &admin, "web", "main", "hotfix", "hotfix.txt");

    // The second write to api's manifest is the revert; park it.
    let handle = proxy.handle.clone();
    let puts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let api_manifest = format!("{api_prefix}/manifest.json");
    proxy.handle.observe({
        let puts = puts.clone();
        let api_manifest = api_manifest.clone();
        move |line, _| {
            if line.starts_with("PUT ")
                && line.contains(&api_manifest)
                && puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1
            {
                handle.set_plan(
                    FaultPlan::new(31)
                        .with(
                            FaultRule::new(Fault::Hang, 1.0)
                                .only_methods(["PUT"])
                                .only_keys([api_manifest.clone()]),
                        )
                        .hang_for(Duration::from_secs(120)),
                );
            }
        }
    });
    server.restart_with(&[("STRATUM_LAND_POLL_SECS", "1".into())]);
    wait_for("the revert's CAS on api to be in flight", || {
        proxy.handle.stats().count(Fault::Hang) == 1
    });
    assert_eq!(tip(&other, &admin, "api", "main").unwrap(), api_ps);
    let over = commit(
        &other,
        &admin,
        "api",
        "main",
        "built on the landing",
        "next.txt",
    );
    proxy.handle.clear_observer();
    proxy.handle.clear_plan();

    let cs = wait_settled(&server, &admin, "Ic500000d");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 1, "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "done".into()),
            ("web".into(), "failed".into())
        ],
        "{cs}"
    );
    let note = format!(
        "not reverted: refs/heads/main moved to {} after it landed",
        &over[..12]
    );
    assert_eq!(cs["landing"]["members"][0]["note"], note.as_str(), "{cs}");
    assert_eq!(
        cs["landing"]["members"][1]["note"],
        format!(
            "refs/heads/main moved to {} before web/Ibb000002 could land",
            &hotfix[..12]
        ),
        "{cs}"
    );
    // Nothing was rewound: the push is the tip, the landed commit under
    // it, and no revert anywhere in the history.
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), over);
    let entries = log(&server, &admin, "api", "main");
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert_eq!(entries[1]["commit"], api_ps.as_str(), "{entries:?}");
    let a = change(&server, &admin, "api", "Iaa000001");
    assert_eq!(a["state"], "landed", "{a}");
    assert_eq!(a["landed_commit"], api_ps.as_str(), "{a}");
    assert_eq!(
        a["land_verdict"],
        format!("landed with changeset Ic500000d; {note}"),
        "{a}"
    );
    assert_eq!(change(&server, &admin, "web", "Ibb000002")["state"], "open");
    assert!(server.healthy());
    assert!(other.healthy());
}

/// A member may target a branch its repository does not have yet: the
/// plan records no old tip, the CAS expects the ref to be absent, and
/// landing creates the branch. Reverting such a member has no tree to
/// go back to but the empty one, and the revert says so.
#[test]
fn a_member_landing_onto_an_unborn_branch_creates_it_and_a_revert_empties_it() {
    let store = Minio::shared().bucket("cs-land-unborn-branch").base_url;
    let scratch = Scratch::new("cs-land-unborn-branch");
    let mut server = spawn(&store, &scratch, "0");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    repo_with_change_to(
        &server,
        &admin,
        "api",
        "oa@acme.test",
        "Iaa000001",
        Some("release"),
    );
    repo_with_change(&server, &admin, "web", "ow@acme.test", "Ibb000002");
    approve(&server, "oa@acme.test", "api", "Iaa000001");
    approve(&server, "ow@acme.test", "web", "Ibb000002");
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic500000e",
            "title": "land",
            "members": [member("web", "Ibb000002"), member("api", "Iaa000001")],
            "edges": [edge(("api", "Iaa000001"), ("web", "Ibb000002"))],
        })),
    );
    assert_eq!(st, 201, "{out}");
    assert!(tip(&server, &admin, "api", "release").is_none());
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();

    let (st, out) = server.post(&format!("{CS}/Ic500000e/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let cs = changeset(&server, &admin, "Ic500000e");
    assert_eq!(cs["landing"]["members"][0]["repo"], "api", "{cs}");
    assert_eq!(
        cs["landing"]["members"][0]["ref"], "refs/heads/release",
        "{cs}"
    );
    assert!(cs["landing"]["members"][0]["old"].is_null(), "{cs}");

    // web moves first, so api lands onto the new branch and is then
    // reverted.
    let hotfix = commit(&server, &admin, "web", "main", "hotfix", "hotfix.txt");
    server.restart_with(&[("STRATUM_LAND_POLL_SECS", "1".into())]);
    let cs = wait_settled(&server, &admin, "Ic500000e");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "reverted".into()),
            ("web".into(), "failed".into())
        ],
        "{cs}"
    );
    assert_eq!(
        cs["landing"]["members"][1]["note"],
        format!(
            "refs/heads/main moved to {} before web/Ibb000002 could land",
            &hotfix[..12]
        ),
        "{cs}"
    );
    // The branch exists now, with the landed commit and its revert, and
    // the revert restores the empty tree: nothing at all is on it.
    let entries = log(&server, &admin, "api", "release");
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert_eq!(entries[1]["commit"], api_ps.as_str(), "{entries:?}");
    let revert = entries[0]["commit"].as_str().unwrap();
    assert_eq!(cs["landing"]["members"][0]["note"], revert, "{cs}");
    assert!(
        entries[0]["message"]
            .as_str()
            .unwrap()
            .contains("Restores refs/heads/release to the empty tree because"),
        "{entries:?}"
    );
    assert!(!has_file(&server, &admin, "api", "release", "feature.txt"));
    assert!(!has_file(&server, &admin, "api", "release", "OWNERS"));
    assert!(has_file(&server, &admin, "api", "main", "OWNERS"));
    assert_eq!(change(&server, &admin, "api", "Iaa000001")["state"], "open");
    assert!(server.healthy());
}

/// The land jobs a fleet can hand a driver that it should do nothing
/// with, each answered as a no-op with a result that says which: a job
/// whose landing never began (the node died between minting the job and
/// the commit point), a job whose landing another job is driving right
/// now (a reaper adopted it, or a lease lapsed and was re-claimed), and
/// a job whose landing has already finished. None of them touches the
/// store or the record; the landing they name ends exactly as it would
/// have without them.
#[test]
fn a_land_job_that_names_no_landing_or_somebody_elses_is_a_no_op() {
    let (proxy, direct, scratch) = proxied_pair("cs-land-job-races");
    let server = spawn(&proxy.url, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, _) = two_member_changeset(&server, &admin, "Ic500000f");
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    let cs_id = stratum_control::changesets::get(&db, &org_id, "Ic500000f")
        .unwrap()
        .expect("the changeset")
        .id;
    let done = |id: &str| -> Option<(String, Option<String>)> {
        let j = stratum_control::jobs::get(&db, &org_id, id)
            .unwrap()
            .expect("the job");
        (j.state == "done").then_some((j.state, j.result))
    };

    // A job naming a landing that was never written.
    let orphan = serde_json::json!({
        "changeset_id": cs_id,
        "landing_id": stratum_control::ids::ulid(),
    })
    .to_string();
    let orphan = stratum_control::jobs::create(&db, &org_id, None, "land", Some(&orphan))
        .unwrap()
        .id;
    wait_for("the orphan job to be answered", || done(&orphan).is_some());
    assert_eq!(
        done(&orphan).unwrap().1.as_deref(),
        Some("no-op: landing did not begin")
    );

    // The first node's driver is parked on api's CAS. The plan is armed
    // before the landing is asked for, so the first PUT the driver makes
    // is the one that hangs.
    proxy.handle.set_plan(
        FaultPlan::new(37)
            .with(
                FaultRule::new(Fault::Hang, 1.0)
                    .only_methods(["PUT"])
                    .only_keys([format!("{api_prefix}/manifest.json")]),
            )
            .hang_for(Duration::from_secs(120)),
    );
    let (st, out) = server.post(&format!("{CS}/Ic500000f/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let landing_id = out["landing"].as_str().unwrap().to_string();
    wait_for("the driver to be parked on api's CAS", || {
        proxy.handle.stats().count(Fault::Hang) == 1
    });

    // Only now a second node joins the queue. Started earlier, it raced
    // the first node for the land job itself: the queue has no node
    // affinity, and whichever lander polled first claimed it. When the
    // second node won, its PUT went to the store directly — it is not
    // behind the proxy — and "the driver to be parked" never happened.
    // That is the failure CI produced on a two-core runner while every
    // development machine kept winning the coin toss. With the first
    // driver parked and its lander busy, the rival job below can only be
    // claimed by this node, which is the case the test is about.
    let other = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &direct)
        .data_dir(scratch.path().join("data-b"))
        .db_url(&server.db_url)
        .env("STRATUM_LAND_POLL_SECS", "1")
        .env("STRATUM_LAND_RECHECK_SECS", "600")
        .start();
    let payload = serde_json::json!({
        "changeset_id": cs_id,
        "landing_id": landing_id,
    })
    .to_string();
    let rival = stratum_control::jobs::create(&db, &org_id, None, "land", Some(&payload))
        .unwrap()
        .id;
    wait_for("the rival job to be answered", || done(&rival).is_some());
    assert_eq!(
        done(&rival).unwrap().1.as_deref(),
        Some("no-op: another job is driving this landing")
    );
    let cs = changeset(&server, &admin, "Ic500000f");
    assert_eq!(cs["state"], "landing", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 1, "{cs}");

    // Release the driver; the landing finishes as it would have.
    proxy.handle.clear_plan();
    let cs = wait_settled(&server, &admin, "Ic500000f");
    assert_eq!(cs["state"], "landed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 1, "{cs}");

    // A job naming a landing that has finished.
    let late = stratum_control::jobs::create(&db, &org_id, None, "land", Some(&payload))
        .unwrap()
        .id;
    wait_for("the late job to be answered", || done(&late).is_some());
    assert_eq!(
        done(&late).unwrap().1.as_deref(),
        Some("no-op: landing already landed")
    );
    let cs = changeset(&server, &admin, "Ic500000f");
    assert_eq!(cs["state"], "landed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 1, "{cs}");
    assert!(server.healthy());
    assert!(other.healthy());
}

/// The rescue is bounded. web's manifest cannot be read — the store
/// answers 503 to every GET of it from the moment api lands — so every
/// driver lands nothing new and dies at the same step. Before the cap
/// the reaper handed the landing to a fresh job every recheck, forever:
/// the queue stayed warm, both changes said `landing`, and nobody was
/// told. Now the third failed driver is the last (`STRATUM_JOB_MAX_ATTEMPTS`,
/// the queue's own cap). The plan is failed at the step the drivers died
/// on, one more driver reverts api, and both changes are open again with
/// the last error in their verdicts.
#[test]
fn a_landing_whose_driver_fails_the_same_way_every_time_is_given_up_and_unwound() {
    let (proxy, scratch) = proxied("cs-land-give-up");
    let server = spawn_with(
        &proxy.url,
        &scratch,
        "1",
        &[("STRATUM_JOB_MAX_ATTEMPTS", "3")],
    );
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, web_prefix) = two_member_changeset(&server, &admin, "Ic5000010");
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let web_before = tip(&server, &admin, "web", "main").unwrap();

    // Armed on api's landing CAS so the pre-flight's reads go through;
    // from then on web's manifest is unreadable, to every driver.
    let handle = proxy.handle.clone();
    let api_cas = format!("{api_prefix}/manifest.json");
    let web_manifest = format!("{web_prefix}/manifest.json");
    proxy.handle.observe(move |line, _| {
        if line.starts_with("PUT ") && line.contains(&api_cas) {
            handle.set_plan(
                FaultPlan::new(17).with(
                    FaultRule::new(Fault::ErrorAfter, 1.0)
                        .only_methods(["GET"])
                        .only_keys([web_manifest.clone()]),
                ),
            );
        }
    });
    let (st, out) = server.post(&format!("{CS}/Ic5000010/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let cs = wait_settled(&server, &admin, "Ic5000010");
    proxy.handle.clear_observer();
    proxy.handle.clear_plan();

    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(
        cs["landing"]["attempt"], 4,
        "three drivers died at web and a fourth reverted api: {cs}"
    );
    // At least: telling web's owners it was ejected reads OWNERS through
    // the same manifest, and may have met the fault too.
    assert!(
        proxy.handle.stats().count(Fault::ErrorAfter) >= 9,
        "three drivers, each spending the store client's three tries: {}",
        proxy.handle.stats().count(Fault::ErrorAfter)
    );
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "reverted".into()),
            ("web".into(), "failed".into())
        ]
    );
    let web_note = cs["landing"]["members"][1]["note"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        web_note.starts_with(
            "the landing was given up after 3 attempts; the last failed with: web/Ibb000002: GET "
        ) && web_note.contains("503"),
        "the note says how many drivers died and what the last one said: {web_note}"
    );

    // web never moved; api landed and was put back.
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_before);
    let entries = log(&server, &admin, "api", "main");
    assert_eq!(entries.len(), 3, "base, landed, revert: {entries:?}");
    assert_eq!(entries[1]["commit"], api_ps);
    let revert = entries[0]["commit"].as_str().unwrap();
    assert!(!has_file(&server, &admin, "api", "main", "feature.txt"));

    let web = change(&server, &admin, "web", "Ibb000002");
    assert_eq!(web["state"], "open", "{web}");
    assert_eq!(web["land_verdict"], format!("ejected: {web_note}"), "{web}");
    let api = change(&server, &admin, "api", "Iaa000001");
    assert_eq!(api["state"], "open", "{api}");
    assert_eq!(
        api["land_verdict"],
        format!(
            "landed, then reverted in {}: web/Ibb000002 — {web_note} — push a new patchset to land again",
            &revert[..12]
        ),
        "{api}"
    );
    assert!(server.healthy());
}

/// …and when the unwind cannot run either, the landing is closed as it
/// stands rather than kept warm. Every manifest is unreadable from the
/// moment api lands: three drivers die reading web's, the give-up fails
/// the plan at web and hands it a fourth driver, and that one dies
/// reading api's manifest for the revert. The reaper closes the landing
/// itself: api is left `landed` with a note saying it was not reverted
/// and why, web is open with its error, the changeset is `failed`, and
/// api's trunk still carries the landed commit — the one outcome where a
/// person has to look, and the record says so in every place they would.
#[test]
fn a_landing_that_cannot_be_unwound_either_is_closed_as_it_stands() {
    let (proxy, scratch) = proxied("cs-land-give-up-stands");
    let server = spawn_with(
        &proxy.url,
        &scratch,
        "1",
        &[("STRATUM_JOB_MAX_ATTEMPTS", "3")],
    );
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    let (api_prefix, _) = two_member_changeset(&server, &admin, "Ic5000011");
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let web_before = tip(&server, &admin, "web", "main").unwrap();

    let handle = proxy.handle.clone();
    let api_cas = format!("{api_prefix}/manifest.json");
    proxy.handle.observe(move |line, _| {
        if line.starts_with("PUT ") && line.contains(&api_cas) {
            handle.set_plan(
                FaultPlan::new(19).with(
                    FaultRule::new(Fault::ErrorAfter, 1.0)
                        .only_methods(["GET"])
                        .only_keys(["manifest.json"]),
                ),
            );
        }
    });
    let (st, out) = server.post(&format!("{CS}/Ic5000011/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let cs = wait_settled(&server, &admin, "Ic5000011");
    proxy.handle.clear_observer();
    proxy.handle.clear_plan();

    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(
        cs["landing"]["attempt"], 4,
        "three drivers died at web and a fourth died reverting api: {cs}"
    );
    // At least: the landed member's announcements enqueue a contribution
    // walk, which reads the same manifest and may have met the fault too.
    assert!(
        proxy.handle.stats().count(Fault::ErrorAfter) >= 12,
        "four drivers, each spending the store client's three tries: {}",
        proxy.handle.stats().count(Fault::ErrorAfter)
    );
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "done".into()),
            ("web".into(), "failed".into())
        ]
    );
    let api_note = cs["landing"]["members"][0]["note"]
        .as_str()
        .unwrap()
        .to_string();
    let web_note = cs["landing"]["members"][1]["note"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        api_note.starts_with(
            "not reverted: the landing was given up after 4 attempts; the last failed with: revert api/Iaa000001: GET "
        ) && api_note.contains("503"),
        "{api_note}"
    );
    assert!(
        web_note.starts_with(
            "the landing was given up after 3 attempts; the last failed with: web/Ibb000002: GET "
        ) && web_note.contains("503"),
        "{web_note}"
    );

    // api's trunk carries the landed commit and nothing on top of it.
    assert_eq!(tip(&server, &admin, "api", "main").unwrap(), api_ps);
    assert_eq!(tip(&server, &admin, "web", "main").unwrap(), web_before);
    let api = change(&server, &admin, "api", "Iaa000001");
    assert_eq!(api["state"], "landed", "{api}");
    assert_eq!(
        api["land_verdict"],
        format!("landed with changeset Ic5000011; {api_note}"),
        "{api}"
    );
    let web = change(&server, &admin, "web", "Ibb000002");
    assert_eq!(web["state"], "open", "{web}");
    assert_eq!(web["land_verdict"], format!("ejected: {web_note}"), "{web}");
    assert!(server.healthy());
}

/// A driver resumed after the unwind died must not land past the
/// failure. api lands, web's trunk has moved so web fails, and the store
/// loses api's manifest to the revert — the unwind's read is answered 503
/// until the job dies. The record then says api `done`, web `failed`,
/// ops `pending`. The rescued driver used to walk on to ops, land it
/// onto a changeset that had already failed, and revert it a moment
/// later: two commits on ops's trunk for nothing. It stops at web now;
/// ops is never attempted and says so.
#[test]
fn a_driver_resumed_after_the_unwind_died_lands_nothing_past_the_failure() {
    let (proxy, scratch) = proxied("cs-land-resume-past");
    let mut server = spawn(&proxy.url, &scratch, "0");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    make_user(&server, "oo@acme.test", "Owner Of Ops", "member");
    let api_prefix = repo_with_change(&server, &admin, "api", "oa@acme.test", "Iaa000001");
    repo_with_change(&server, &admin, "web", "ow@acme.test", "Ibb000002");
    repo_with_change(&server, &admin, "ops", "oo@acme.test", "Icc000003");
    approve(&server, "oa@acme.test", "api", "Iaa000001");
    approve(&server, "ow@acme.test", "web", "Ibb000002");
    approve(&server, "oo@acme.test", "ops", "Icc000003");
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000012",
            "title": "land three",
            "members": [
                member("ops", "Icc000003"),
                member("web", "Ibb000002"),
                member("api", "Iaa000001"),
            ],
            "edges": [
                edge(("api", "Iaa000001"), ("web", "Ibb000002")),
                edge(("web", "Ibb000002"), ("ops", "Icc000003")),
            ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let api_ps = change(&server, &admin, "api", "Iaa000001")["patchset"]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let ops_before = tip(&server, &admin, "ops", "main").unwrap();

    let (st, out) = server.post(&format!("{CS}/Ic5000012/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    let job = out["job"].as_str().unwrap().to_string();
    let web_moved = commit(&server, &admin, "web", "main", "hotfix", "hotfix.txt");

    // After api's landing CAS, api's manifest is unreadable — so the
    // unwind's first read, for the revert, is what kills the driver.
    let handle = proxy.handle.clone();
    let api_cas = format!("{api_prefix}/manifest.json");
    let api_manifest = api_cas.clone();
    proxy.handle.observe(move |line, _| {
        if line.starts_with("PUT ") && line.contains(&api_cas) {
            handle.set_plan(
                FaultPlan::new(23).with(
                    FaultRule::new(Fault::ErrorAfter, 1.0)
                        .only_methods(["GET"])
                        .only_keys([api_manifest.clone()]),
                ),
            );
        }
    });
    // A long grace first, so the dead driver's landing holds still while
    // the fault is taken away; then the reaper is let at it. The driver's
    // death is read from its job — the row the reaper itself reads —
    // rather than counted on the wire: the compactor, the CDN packer and
    // the contribution walk all read this manifest on their own clocks
    // and meet the same fault, so a count is not a position in the
    // driver's sequence.
    server.restart_with(&[
        ("STRATUM_LAND_POLL_SECS", "1".into()),
        ("STRATUM_LAND_RECHECK_SECS", "600".into()),
    ]);
    let db = stratum_control::ControlDb::open(&server.db_url).expect("the control plane");
    let org_id = stratum_control::registry::org_by_name(&db, "acme")
        .unwrap()
        .unwrap()
        .id;
    wait_for("the driver to die in the unwind", || {
        stratum_control::jobs::get(&db, &org_id, &job)
            .unwrap()
            .is_some_and(|j| j.state == "failed")
    });
    assert!(
        proxy.handle.stats().count(Fault::ErrorAfter) >= 3,
        "the unwind's read was refused"
    );
    proxy.handle.clear_observer();
    proxy.handle.clear_plan();
    server.restart_with(&[
        ("STRATUM_LAND_POLL_SECS", "1".into()),
        ("STRATUM_LAND_RECHECK_SECS", "1".into()),
    ]);
    let cs = wait_settled(&server, &admin, "Ic5000012");

    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(cs["landing"]["attempt"], 2, "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "reverted".into()),
            ("web".into(), "failed".into()),
            ("ops".into(), "pending".into()),
        ]
    );
    let web_note = format!(
        "refs/heads/main moved to {} before web/Ibb000002 could land",
        &web_moved[..12]
    );
    assert_eq!(cs["landing"]["members"][1]["note"], web_note, "{cs}");

    // ops was never touched: one commit on its trunk, not three.
    assert_eq!(tip(&server, &admin, "ops", "main").unwrap(), ops_before);
    assert_eq!(log(&server, &admin, "ops", "main").len(), 1);
    let ops = change(&server, &admin, "ops", "Icc000003");
    assert_eq!(ops["state"], "open", "{ops}");
    assert_eq!(
        ops["land_verdict"],
        format!("ejected: not attempted, web/Ibb000002 — {web_note}"),
        "{ops}"
    );
    // api landed and was put back by the rescued driver.
    let entries = log(&server, &admin, "api", "main");
    assert_eq!(entries.len(), 3, "base, landed, revert: {entries:?}");
    assert_eq!(entries[1]["commit"], api_ps);
    assert_eq!(change(&server, &admin, "api", "Iaa000001")["state"], "open");
    assert!(server.healthy());
}
