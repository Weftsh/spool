//! Reverting a landed changeset with one call: a new changeset whose
//! members put back, in every repository, the paths the landed one
//! changed — on top of the trunk as it is now, so work landed since on
//! other paths is kept — with the original's edges reversed, reviewed
//! and landed like any other. And everything the call refuses, whole,
//! before writing a single branch: a changeset nothing of which has
//! landed, a path somebody changed since, a trunk or repository that is
//! gone, a key already taken.
//!
//! Everything is asserted the way a person finds out — the changeset
//! view, the change view, the trunk's refs, files and trees, a real
//! clone — never by reading tables.

use std::time::Duration;

use stratum_testkit::faultproxy::{Fault, FaultPlan, FaultRule};
use stratum_testkit::{gitcli, gitcli::Scratch, FaultProxy, Minio, Server};

const PASSWORD: &str = "a long enough password";
const CS: &str = "/v1/orgs/acme/changesets";

/// A server whose lander polls every second.
fn spawn(store_url: &str, scratch: &Scratch, poll: &str) -> Server {
    spawn_with(store_url, scratch, poll, &[])
}

fn spawn_with(store_url: &str, scratch: &Scratch, poll: &str, env: &[(&str, &str)]) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .data_dir(scratch.path().join("data"))
        .db_hint("changeset-revert-e2e")
        .env("STRATUM_LAND_POLL_SECS", poll)
        .env("STRATUM_LAND_RECHECK_SECS", "1")
        // Nobody on the wire but the lander, for the reason spelled out
        // in `changeset_landing_e2e`: the faults these tests arm are on
        // a repository's manifest, and four background workers read that
        // same manifest on clocks of their own.
        .env("STRATUM_COMPACT_POLL_SECS", "0")
        .env("STRATUM_CDNPACK_POLL_SECS", "0")
        .env("STRATUM_CONTRIB_POLL_SECS", "0")
        .env("STRATUM_NOTIFY_POLL_SECS", "0")
        // The changeset notifier too. Nothing here reads mail, so all it
        // ever did was run on its own clock — and once it happened to
        // run after a test deleted a member's repository, which made it
        // the only cover for the worker's "repository is gone" arm, on
        // some runs and not others. That arm has a test of its own in
        // `changeset_notify_e2e` now; here it is off on purpose.
        .env("STRATUM_CHANGESET_NOTIFY_POLL_SECS", "0");
    for (k, v) in env {
        b = b.env(k, *v);
    }
    b.start()
}

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
        Scratch::new(hint),
    )
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

/// A signed-in person's POST with a JSON body.
fn as_person_json(
    server: &Server,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let r = ureq::post(&format!("{}{path}", server.base))
        .set("Cookie", cookie)
        .set("Content-Type", "application/json");
    let resp = match r.send_string(&body.to_string()) {
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

/// Commit `operations` to a branch through the commit API; the commit.
fn commit_ops(
    server: &Server,
    token: &str,
    repo: &str,
    branch: &str,
    message: &str,
    operations: serde_json::Value,
) -> String {
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        token,
        Some(serde_json::json!({
            "branch": branch,
            "message": message,
            "operations": operations,
        })),
    );
    assert_eq!(st, 201, "commit to {repo}/{branch}: {out}");
    out["commit"].as_str().unwrap().to_string()
}

/// Commit one file, whose content is the message.
fn commit(
    server: &Server,
    token: &str,
    repo: &str,
    branch: &str,
    message: &str,
    path: &str,
) -> String {
    commit_ops(
        server,
        token,
        repo,
        branch,
        message,
        serde_json::json!([{"op": "put", "path": path, "content": message}]),
    )
}

fn put(path: &str, content: &str) -> serde_json::Value {
    serde_json::json!({"op": "put", "path": path, "content": content})
}

fn delete(path: &str) -> serde_json::Value {
    serde_json::json!({"op": "delete", "path": path})
}

/// A repository whose trunk has `OWNERS` naming `owner`, `readme` and
/// `old.txt`; a `feature` branch one commit ahead that adds
/// `feature.txt` and `sub/dir/only.txt`, rewrites `readme` and deletes
/// `old.txt` — one of each kind of change a revert has to undo; and an
/// open change `key` from that branch, aimed at `target` (which may be a
/// branch the repository does not have yet). Returns the store prefix.
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
    commit_ops(
        server,
        admin,
        repo,
        "main",
        "trunk",
        serde_json::json!([
            put("OWNERS", &format!("{owner}\n")),
            put("readme", "v1\n"),
            put("old.txt", "to be deleted\n"),
        ]),
    );
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        admin,
        Some(serde_json::json!({"name": "feature", "from": "main"})),
    );
    assert_eq!(st, 201, "branch feature in {repo}: {out}");
    commit_ops(
        server,
        admin,
        repo,
        "feature",
        &format!("change {repo}\n\nChange-Id: {key}\n"),
        serde_json::json!([
            put("feature.txt", &format!("change {repo}\n")),
            put("sub/dir/only.txt", "deep\n"),
            put("readme", "v2\n"),
            delete("old.txt"),
        ]),
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

fn repo_with_change(server: &Server, admin: &str, repo: &str, owner: &str, key: &str) -> String {
    repo_with_change_to(server, admin, repo, owner, key, None)
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

/// Land `key` and wait for it to have.
fn land(server: &Server, admin: &str, key: &str) -> serde_json::Value {
    let (st, out) = server.post(&format!("{CS}/{key}/land"), admin, None);
    assert_eq!(st, 202, "{out}");
    let cs = wait_settled(server, admin, key);
    assert_eq!(cs["state"], "landed", "{cs}");
    cs
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

/// The content of `path` on `branch`, or None when it is not there.
fn file(server: &Server, admin: &str, repo: &str, branch: &str, path: &str) -> Option<String> {
    let url = format!(
        "{}/v1/orgs/acme/repos/{repo}/files/{path}?at=refs/heads/{branch}",
        server.base
    );
    match ureq::get(&url)
        .set("Authorization", &format!("Bearer {admin}"))
        .call()
    {
        Ok(r) => Some(r.into_string().unwrap()),
        Err(ureq::Error::Status(404, _)) => None,
        Err(e) => panic!("{repo}/{branch}:{path}: {e}"),
    }
}

/// The names and modes of the entries of `dir` on `branch`, or None when
/// there is no such directory.
fn tree(
    server: &Server,
    admin: &str,
    repo: &str,
    branch: &str,
    dir: &str,
) -> Option<Vec<(String, String)>> {
    let path = if dir.is_empty() {
        format!("/v1/orgs/acme/repos/{repo}/tree?at=refs/heads/{branch}")
    } else {
        format!("/v1/orgs/acme/repos/{repo}/tree/{dir}?at=refs/heads/{branch}")
    };
    let (st, out) = server.get(&path, admin);
    if st == 404 {
        return None;
    }
    assert_eq!(st, 200, "{out}");
    Some(
        out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_string(),
                    e["mode"].as_str().unwrap().to_string(),
                )
            })
            .collect(),
    )
}

fn log(server: &Server, admin: &str, repo: &str, branch: &str) -> Vec<serde_json::Value> {
    let (st, out) = server.get(
        &format!("/v1/orgs/acme/repos/{repo}/log?rev=refs/heads/{branch}"),
        admin,
    );
    assert_eq!(st, 200, "{out}");
    out["entries"].as_array().unwrap().clone()
}

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

/// The member of a changeset view that is in `repo`.
fn member_in<'a>(cs: &'a serde_json::Value, repo: &str) -> &'a serde_json::Value {
    cs["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["repo"] == repo)
        .unwrap_or_else(|| panic!("no member in {repo}: {cs}"))
}

fn revert(
    server: &Server,
    token: &str,
    key: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    server.post(&format!("{CS}/{key}/revert"), token, Some(body))
}

/// The ordinary revert. A two-member changeset lands, adding, rewriting
/// and deleting files in each repository; work then lands on top in
/// both. One call makes a revert changeset: an open change in each
/// repository, on a `revert/<key>` branch off the trunk as it is now,
/// whose commit puts every path the member changed back to how it was —
/// the deleted file returns, the rewritten one is its old self, the
/// added ones are gone, and the directory they emptied is gone with
/// them, because git records no empty directories — and leaves the
/// later work alone. The new changeset lands web before api, the
/// reverse of the original; each side names the other; the revert
/// changes need the same owners' approval as the originals did, and are
/// landed the same way. What the revert changeset lands is a tree a real
/// `git` clone `fsck`s clean. A second revert of the same changeset is
/// refused, because every path it would put back has changed since;
/// the revert changeset is itself a changeset, so it can be reverted.
#[test]
fn a_landed_changeset_is_reverted_by_one_call_and_the_revert_lands_like_any_other() {
    let store = Minio::shared().bucket("cs-revert-happy").base_url;
    let scratch = Scratch::new("cs-revert-happy");
    let server = spawn(&store, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    two_member_changeset(&server, &admin, "Ic5000001");
    let cs = land(&server, &admin, "Ic5000001");
    let api_landed = cs["landing"]["members"][0]["new"]
        .as_str()
        .unwrap()
        .to_string();
    let web_landed = cs["landing"]["members"][1]["new"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        tip(&server, &admin, "api", "main").as_deref(),
        Some(api_landed.as_str())
    );
    // Work lands on top, in both, on paths the changeset did not touch.
    let api_later = commit(&server, &admin, "api", "main", "later\n", "later.txt");
    let web_later = commit(&server, &admin, "web", "main", "later\n", "later.txt");

    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000001",
        serde_json::json!({"key": "Ic5000002"}),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["key"], "Ic5000002", "{out}");
    assert_eq!(out["state"], "open", "{out}");
    assert_eq!(out["title"], "Revert \"land\"", "{out}");
    assert_eq!(out["body"], "Reverts changeset Ic5000001.", "{out}");
    assert_eq!(out["reverts"], "Ic5000001", "{out}");
    assert_eq!(out["reverted_by"], serde_json::json!([]), "{out}");
    assert!(out["landing"].is_null(), "{out}");
    // The original's edge was api → web; the revert's is web → api.
    assert_eq!(out["order"].as_array().unwrap().len(), 2, "{out}");
    assert_eq!(out["order"][0]["repo"], "web", "{out}");
    assert_eq!(out["order"][1]["repo"], "api", "{out}");
    assert_eq!(out["edges"].as_array().unwrap().len(), 1, "{out}");
    assert_eq!(out["edges"][0]["from"]["repo"], "web", "{out}");
    assert_eq!(out["edges"][0]["to"]["repo"], "api", "{out}");
    let original = changeset(&server, &admin, "Ic5000001");
    assert_eq!(
        original["reverted_by"],
        serde_json::json!(["Ic5000002"]),
        "{original}"
    );
    assert_eq!(original["state"], "landed", "{original}");

    // Each member: an open change on `revert/Ic5000002`, aimed at the
    // trunk, one commit on top of the trunk as it is now, with a message
    // that says what it reverts and a Change-Id of its own.
    let api_key = member_in(&out, "api")["change"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    let web_key = member_in(&out, "web")["change"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(api_key, web_key);
    for (repo, key, landed, later) in [
        ("api", &api_key, &api_landed, &api_later),
        ("web", &web_key, &web_landed, &web_later),
    ] {
        let c = change(&server, &admin, repo, key);
        assert_eq!(c["state"], "open", "{c}");
        assert_eq!(c["target_branch"], "main", "{c}");
        assert_eq!(c["title"], format!("Revert \"change {repo}\""), "{c}");
        let ps = c["patchset"]["commit"].as_str().unwrap().to_string();
        assert_eq!(
            tip(&server, &admin, repo, "revert/Ic5000002").as_deref(),
            Some(ps.as_str()),
            "{repo}"
        );
        assert_eq!(c["patchset"]["parent"], serde_json::json!(later), "{c}");
        let message = c["patchset"]["message"].as_str().unwrap();
        assert!(
            message.contains(&format!(
                "Reverts {repo}/{}, landed by changeset Ic5000001 as {landed}.",
                if repo == "api" {
                    "Iaa000001"
                } else {
                    "Ibb000002"
                }
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!("Change-Id: {key}\n")),
            "{message}"
        );
        // The branch has what the trunk has, with the change undone.
        assert_eq!(
            file(&server, &admin, repo, "revert/Ic5000002", "feature.txt"),
            None
        );
        assert_eq!(
            file(
                &server,
                &admin,
                repo,
                "revert/Ic5000002",
                "sub/dir/only.txt"
            ),
            None
        );
        assert_eq!(
            tree(&server, &admin, repo, "revert/Ic5000002", "sub"),
            None,
            "{repo}: an emptied directory must go"
        );
        assert_eq!(
            file(&server, &admin, repo, "revert/Ic5000002", "readme").as_deref(),
            Some("v1\n")
        );
        assert_eq!(
            file(&server, &admin, repo, "revert/Ic5000002", "old.txt").as_deref(),
            Some("to be deleted\n")
        );
        assert_eq!(
            file(&server, &admin, repo, "revert/Ic5000002", "later.txt").as_deref(),
            Some("later\n")
        );
        // The trunk itself has not moved.
        assert_eq!(
            tip(&server, &admin, repo, "main").as_deref(),
            Some(later.as_str())
        );
    }

    // Reviewed like any change: the same OWNERS govern the same paths.
    let (st, v) = server.get(&format!("{CS}/Ic5000002/verdict"), &admin);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["landable"], false, "{v}");
    let (st, out) = server.post(&format!("{CS}/Ic5000002/land"), &admin, None);
    assert_eq!(st, 409, "{out}");
    approve(&server, "ow@acme.test", "web", &web_key);
    approve(&server, "oa@acme.test", "api", &api_key);
    let cs = land(&server, &admin, "Ic5000002");
    assert_eq!(
        landing_states(&cs),
        vec![("web".into(), "done".into()), ("api".into(), "done".into())],
        "{cs}"
    );
    for repo in ["api", "web"] {
        assert_eq!(file(&server, &admin, repo, "main", "feature.txt"), None);
        assert_eq!(
            file(&server, &admin, repo, "main", "readme").as_deref(),
            Some("v1\n")
        );
        assert_eq!(
            file(&server, &admin, repo, "main", "old.txt").as_deref(),
            Some("to be deleted\n")
        );
        assert_eq!(
            file(&server, &admin, repo, "main", "later.txt").as_deref(),
            Some("later\n")
        );
        assert_eq!(tree(&server, &admin, repo, "main", "sub"), None);
        let entries = log(&server, &admin, repo, "main");
        assert_eq!(entries.len(), 4, "{entries:?}");
        assert!(
            entries[0]["message"]
                .as_str()
                .unwrap()
                .starts_with(&format!("Revert \"change {repo}\"")),
            "{entries:?}"
        );
        // I11: what the revert built is a tree git itself accepts.
        gitcli::clone_and_fsck(
            &server.authed_url(&admin, "acme", repo),
            &scratch.path().join(format!("clone-{repo}")),
        );
    }
    let a = change(&server, &admin, "api", &api_key);
    assert_eq!(a["state"], "landed", "{a}");
    assert_eq!(
        change(&server, &admin, "api", "Iaa000001")["state"],
        "landed"
    );

    // Reverting it again: every path it would put back has changed
    // since — by the revert — and the refusal names them, per member,
    // in landing order.
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000001",
        serde_json::json!({"key": "Ic5000003"}),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        "api/Iaa000001: refs/heads/main has changed since it landed at feature.txt, old.txt, readme, sub/dir/only.txt",
        "{out}"
    );
    assert_eq!(out["conflicts"].as_array().unwrap().len(), 2, "{out}");
    assert_eq!(out["conflicts"][0]["repo"], "api", "{out}");
    assert_eq!(out["conflicts"][1]["repo"], "web", "{out}");
    assert_eq!(out["conflicts"][1]["change"], "Ibb000002", "{out}");
    assert_eq!(
        out["conflicts"][1]["changed"],
        serde_json::json!(["feature.txt", "old.txt", "readme", "sub/dir/only.txt"]),
        "{out}"
    );
    assert!(tip(&server, &admin, "api", "revert/Ic5000003").is_none());
    // Nothing was made under the refused key.
    let (st, out) = server.get(&format!("{CS}/Ic5000003"), &admin);
    assert_eq!(st, 404, "{out}");
    // An org this token cannot see is no org at all.
    let (st, out) = server.post(
        "/v1/orgs/nobody/changesets/Ic5000001/revert",
        &admin,
        Some(serde_json::json!({"key": "Ic5000003"})),
    );
    assert_eq!(st, 404, "{out}");

    // The revert of the revert puts the change back: a changeset like any
    // other, whose members are opened the same way.
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000002",
        serde_json::json!({
            "key": "Ic5000004",
            "title": "Reland it",
            "body": "The revert was wrong.",
        }),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["reverts"], "Ic5000002", "{out}");
    assert_eq!(out["title"], "Reland it", "{out}");
    assert_eq!(out["body"], "The revert was wrong.", "{out}");
    // Its order is the original's again.
    assert_eq!(out["order"][0]["repo"], "api", "{out}");
    assert_eq!(out["order"][1]["repo"], "web", "{out}");
    assert_eq!(
        file(&server, &admin, "api", "revert/Ic5000004", "feature.txt").as_deref(),
        Some("change api\n")
    );
    assert_eq!(
        changeset(&server, &admin, "Ic5000002")["reverted_by"],
        serde_json::json!(["Ic5000004"])
    );
    // The list carries the links too.
    let (st, out) = server.get(CS, &admin);
    assert_eq!(st, 200, "{out}");
    let listed: Vec<(String, serde_json::Value)> = out["changesets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["key"].as_str().unwrap().to_string(), c["reverts"].clone()))
        .collect();
    assert!(
        listed.contains(&("Ic5000004".into(), serde_json::json!("Ic5000002"))),
        "{listed:?}"
    );
    assert!(
        listed.contains(&("Ic5000001".into(), serde_json::Value::Null)),
        "{listed:?}"
    );
    assert!(server.healthy());
}

/// Every refusal, and that each is whole: nothing is written under the
/// refused key in any repository, including the ones that could have
/// been reverted. A changeset that has not landed; a key that is not
/// one; a key already taken; a path changed on trunk since the landing,
/// named; a `revert/<key>` branch already in one repository; a member
/// whose repository has been deleted; a `failed` changeset whose landed
/// members were all reverted by the unwind, so there is nothing to
/// revert. Authorisation is composing's: a viewer is refused, and a
/// person without write on one member's repository is told the
/// changeset does not exist.
#[test]
fn a_revert_that_cannot_be_made_whole_is_refused_and_writes_nothing() {
    let store = Minio::shared().bucket("cs-revert-refusals").base_url;
    let scratch = Scratch::new("cs-revert-refusals");
    let mut server = spawn(&store, &scratch, "1");
    let admin = server.bootstrap_org("acme");
    make_user(&server, "oa@acme.test", "Owner Of Api", "member");
    make_user(&server, "ow@acme.test", "Owner Of Web", "member");
    make_user(&server, "vic@acme.test", "Vic", "viewer");
    two_member_changeset(&server, &admin, "Ic5000001");

    // Nothing of it has landed.
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000001",
        serde_json::json!({"key": "Ic5000002"}),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"], "changeset is open: nothing of it has landed",
        "{out}"
    );
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000009",
        serde_json::json!({"key": "Ic5000002"}),
    );
    assert_eq!(st, 404, "{out}");

    land(&server, &admin, "Ic5000001");
    // A key that is not one, and a key already taken.
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000001",
        serde_json::json!({"key": "not a key"}),
    );
    assert_eq!(st, 400, "{out}");
    assert_eq!(out["error"], "invalid changeset key \"not a key\"", "{out}");
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000001",
        serde_json::json!({"key": "Ic5000001"}),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(out["error"], "changeset Ic5000001 already exists", "{out}");
    let (st, out) = server.post(
        &format!("{CS}/Ic5000001/revert"),
        &admin,
        Some(serde_json::json!({})),
    );
    assert!(st == 400 || st == 422, "{st} {out}");

    // A viewer may not: composing a changeset needs write on every
    // member, and so does making one that reverts it.
    let vic = sign_in(&server, "vic@acme.test");
    let (st, out) = as_person_json(
        &server,
        &vic,
        &format!("{CS}/Ic5000001/revert"),
        serde_json::json!({"key": "Ic5000002"}),
    );
    assert_eq!(st, 404, "viewer: {out}");
    assert!(tip(&server, &admin, "api", "revert/Ic5000002").is_none());

    // web's trunk has moved on a path the changeset changed: refused,
    // named, and api — which could have been reverted — has nothing
    // written under the key.
    let web_edit = commit(&server, &admin, "web", "main", "v3\n", "readme");
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000001",
        serde_json::json!({"key": "Ic5000002"}),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"], "web/Ibb000002: refs/heads/main has changed since it landed at readme",
        "{out}"
    );
    assert_eq!(
        out["conflicts"],
        serde_json::json!([{
            "repo": "web",
            "change": "Ibb000002",
            "changed": ["readme"],
            "why": "refs/heads/main has changed since it landed at readme",
        }]),
        "{out}"
    );
    assert!(tip(&server, &admin, "api", "revert/Ic5000002").is_none());
    assert!(tip(&server, &admin, "web", "revert/Ic5000002").is_none());
    let (st, out) = server.get("/v1/orgs/acme/repos/api/changes", &admin);
    assert_eq!(st, 200, "{out}");
    assert_eq!(
        out["changes"].as_array().unwrap().len(),
        1,
        "no revert change was opened: {out}"
    );
    let (st, out) = server.get(&format!("{CS}/Ic5000002"), &admin);
    assert_eq!(st, 404, "{out}");
    // Putting readme back by hand makes the revert possible again: what
    // matters is the content at the path now, not the history.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/revert",
        &admin,
        Some(serde_json::json!({"branch": "main", "expected_head": web_edit})),
    );
    assert_eq!(st, 201, "{out}");

    // The revert branch is already there in one repository.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/api/branches",
        &admin,
        Some(serde_json::json!({"name": "revert/Ic5000002", "from": "main"})),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000001",
        serde_json::json!({"key": "Ic5000002"}),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"], "api/Iaa000001: revert/Ic5000002 already exists",
        "{out}"
    );
    assert_eq!(out["conflicts"][0]["branch_exists"], true, "{out}");
    assert!(tip(&server, &admin, "web", "revert/Ic5000002").is_none());
    let (st, out) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/api/branches/revert%2FIc5000002",
        &admin,
        None,
    );
    assert_eq!(st, 204, "{out}");

    // A member's repository is gone.
    let (st, out) = server.req("DELETE", "/v1/orgs/acme/repos/web", &admin, None);
    assert_eq!(st, 204, "{out}");
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000001",
        serde_json::json!({"key": "Ic5000002"}),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"], "web/Ibb000002: the repository no longer exists",
        "{out}"
    );
    assert!(tip(&server, &admin, "api", "revert/Ic5000002").is_none());

    // A failed changeset whose one landed member the unwind reverted:
    // nothing is landed, so there is nothing to revert. Built the way the
    // landing suite builds it: the lander held off, web's trunk moved,
    // the lander let go.
    let cli = repo_with_change(&server, &admin, "cli", "oa@acme.test", "Icc000003");
    let _ = cli;
    repo_with_change(&server, &admin, "docs", "ow@acme.test", "Idd000004");
    approve(&server, "oa@acme.test", "cli", "Icc000003");
    approve(&server, "ow@acme.test", "docs", "Idd000004");
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic5000005",
            "title": "fails",
            "members": [member("docs", "Idd000004"), member("cli", "Icc000003")],
            "edges": [edge(("cli", "Icc000003"), ("docs", "Idd000004"))],
        })),
    );
    assert_eq!(st, 201, "{out}");
    server.restart_with(&[("STRATUM_LAND_POLL_SECS", "0".into())]);
    let (st, out) = server.post(&format!("{CS}/Ic5000005/land"), &admin, None);
    assert_eq!(st, 202, "{out}");
    commit(&server, &admin, "docs", "main", "hotfix\n", "hotfix.txt");
    server.restart_with(&[("STRATUM_LAND_POLL_SECS", "1".into())]);
    let cs = wait_settled(&server, &admin, "Ic5000005");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("cli".into(), "reverted".into()),
            ("docs".into(), "failed".into())
        ],
        "{cs}"
    );
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000005",
        serde_json::json!({"key": "Ic5000006"}),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        "nothing of changeset Ic5000005 is landed: every member that landed was reverted",
        "{out}"
    );
    assert!(server.healthy());
}

/// A `failed` changeset can leave a member landed: the driver died after
/// api's CAS, somebody built on top of it, and web's trunk moved, so
/// the resumed landing failed and api — under a stranger's commit — was
/// not the unwind's to rewind. It is the revert's, and only api's: the
/// revert changeset has one member, which puts api's paths back on top
/// of the stranger's commit and leaves that commit alone.
#[test]
fn a_failed_changesets_left_landed_member_is_what_the_revert_reverts() {
    let (proxy, scratch) = proxied("cs-revert-left-landed");
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
            && proxy.handle.stats().count(Fault::ErrorAfter) >= 1
    });
    proxy.handle.clear_plan();
    let api_over = commit(
        &server,
        &admin,
        "api",
        "main",
        "built on the landing\n",
        "next.txt",
    );
    commit(&server, &admin, "web", "main", "hotfix\n", "hotfix.txt");
    server.restart_with(&[("STRATUM_LAND_RECHECK_SECS", "1".into())]);
    let cs = wait_settled(&server, &admin, "Ic5000007");
    assert_eq!(cs["state"], "failed", "{cs}");
    assert_eq!(
        landing_states(&cs),
        vec![
            ("api".into(), "done".into()),
            ("web".into(), "failed".into())
        ],
        "{cs}"
    );
    assert_eq!(
        tip(&server, &admin, "api", "main").as_deref(),
        Some(api_over.as_str())
    );

    // The store failing under the revert's own read is a 500 that names
    // the member, and writes nothing: the same key is free to try again
    // once the store is back.
    proxy.handle.set_plan(
        FaultPlan::new(12).with(
            FaultRule::new(Fault::ErrorAfter, 1.0)
                .only_methods(["GET"])
                .only_keys([format!("{api_prefix}/manifest.json")]),
        ),
    );
    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000007",
        serde_json::json!({"key": "Ic5000008"}),
    );
    proxy.handle.clear_plan();
    assert_eq!(st, 500, "{out}");
    let err = out["error"].as_str().unwrap();
    assert!(err.starts_with("api/Iaa000001: "), "{out}");
    assert!(err.contains("503"), "{out}");
    let (st, out) = server.get(&format!("{CS}/Ic5000008"), &admin);
    assert_eq!(st, 404, "{out}");

    let (st, out) = revert(
        &server,
        &admin,
        "Ic5000007",
        serde_json::json!({"key": "Ic5000008"}),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["members"].as_array().unwrap().len(), 1, "{out}");
    assert_eq!(out["members"][0]["repo"], "api", "{out}");
    assert_eq!(out["edges"], serde_json::json!([]), "{out}");
    assert_eq!(out["reverts"], "Ic5000007", "{out}");
    let key = out["members"][0]["change"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    let c = change(&server, &admin, "api", &key);
    assert_eq!(c["patchset"]["parent"], serde_json::json!(api_over), "{c}");
    assert_eq!(
        file(&server, &admin, "api", "revert/Ic5000008", "next.txt").as_deref(),
        Some("built on the landing\n")
    );
    assert_eq!(
        file(&server, &admin, "api", "revert/Ic5000008", "feature.txt"),
        None
    );
    assert_eq!(
        file(&server, &admin, "api", "revert/Ic5000008", "old.txt").as_deref(),
        Some("to be deleted\n")
    );
    // web's change is open, not landed, and no revert was made of it.
    assert_eq!(change(&server, &admin, "web", "Ibb000002")["state"], "open");
    assert!(tip(&server, &admin, "web", "revert/Ic5000008").is_none());
    assert!(server.healthy());
}

/// A member landed onto a branch that did not exist put a whole tree
/// on it; its revert puts back the nothing that was there — every
/// path gone, the empty tree — and a branch deleted since the landing
/// is a refusal that says so. And the mode of what is put back is the
/// mode it had: a script that was executable when the change made it
/// plain is executable again, which only a real `git` push can set up
/// and a real clone can check.
#[test]
fn a_revert_puts_back_an_empty_tree_an_executable_bit_and_refuses_a_missing_branch() {
    let store = Minio::shared().bucket("cs-revert-shapes").base_url;
    let scratch = Scratch::new("cs-revert-shapes");
    let server = spawn(&store, &scratch, "1");
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
    // web, built with the real git CLI: its trunk has an executable and
    // the change makes it plain — a mode change, which the commit API
    // cannot express and a real clone can check.
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({"name": "web", "public": false})),
    );
    assert_eq!(st, 201, "{out}");
    commit(&server, &admin, "web", "main", "ow@acme.test\n", "OWNERS");
    let url = server.authed_url(&admin, "acme", "web");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);
    std::fs::write(work.join("build.sh"), "#!/bin/sh\necho hi\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            work.join("build.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "a script"]);
    gitcli::git(&work, &["push", "-q", "origin", "main"]);
    gitcli::git(&work, &["checkout", "-q", "-b", "feature"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            work.join("build.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &["commit", "-q", "-m", "plain now\n\nChange-Id: Ibb000002"],
    );
    gitcli::git(&work, &["push", "-q", "origin", "feature"]);
    let mode = |branch: &str| {
        tree(&server, &admin, "web", branch, "")
            .unwrap()
            .into_iter()
            .find(|(n, _)| n == "build.sh")
            .map(|(_, m)| m)
    };
    assert_eq!(mode("main").as_deref(), Some("100755"));
    assert_eq!(mode("feature").as_deref(), Some("100644"));
    let (st, out) = server.post(
        "/v1/orgs/acme/repos/web/changes",
        &admin,
        Some(serde_json::json!({"from": "feature"})),
    );
    assert_eq!(st, 201, "{out}");
    approve(&server, "oa@acme.test", "api", "Iaa000001");
    approve(&server, "ow@acme.test", "web", "Ibb000002");
    let (st, out) = server.post(
        CS,
        &admin,
        Some(serde_json::json!({
            "key": "Ic500000e",
            // As long as a title may be: `Revert "…"` around it would
            // not fit, so the revert falls back to naming the changeset.
            "title": "s".repeat(200),
            "members": [member("web", "Ibb000002"), member("api", "Iaa000001")],
            "edges": [edge(("api", "Iaa000001"), ("web", "Ibb000002"))],
        })),
    );
    assert_eq!(st, 201, "{out}");
    let cs = land(&server, &admin, "Ic500000e");
    assert!(cs["landing"]["members"][0]["old"].is_null(), "{cs}");
    assert!(tip(&server, &admin, "api", "release").is_some());

    let (st, out) = revert(
        &server,
        &admin,
        "Ic500000e",
        serde_json::json!({"key": "Ic500000f"}),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["title"], "Revert changeset Ic500000e", "{out}");
    // api's revert: the empty tree on top of the landed commit.
    let api_key = member_in(&out, "api")["change"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        change(&server, &admin, "api", &api_key)["target_branch"],
        "release"
    );
    assert_eq!(
        tree(&server, &admin, "api", "revert/Ic500000f", ""),
        Some(vec![])
    );
    // web's revert: the script is executable again, in a clone.
    let web_key = member_in(&out, "web")["change"]["key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(mode("revert/Ic500000f").as_deref(), Some("100755"));
    approve(&server, "oa@acme.test", "api", &api_key);
    approve(&server, "ow@acme.test", "web", &web_key);
    land(&server, &admin, "Ic500000f");
    let back = scratch.path().join("back");
    gitcli::clone_and_fsck(&url, &back);
    let listing = gitcli::git(&back, &["ls-tree", "HEAD", "build.sh"]);
    assert!(listing.starts_with("100755 blob"), "{listing}");
    assert_eq!(tree(&server, &admin, "api", "release", ""), Some(vec![]));

    // A trunk deleted since the landing.
    let (st, out) = revert(
        &server,
        &admin,
        "Ic500000f",
        serde_json::json!({"key": "Ic5000010"}),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/api/branches/release",
        &admin,
        None,
    );
    assert_eq!(st, 204, "{out}");
    let (st, out) = server.req(
        "DELETE",
        "/v1/orgs/acme/repos/api/branches/revert%2FIc5000010",
        &admin,
        None,
    );
    assert_eq!(st, 204, "{out}");
    // (Ic5000010 is open, so Ic500000f is still what is landed.)
    let (st, out) = revert(
        &server,
        &admin,
        "Ic500000f",
        serde_json::json!({"key": "Ic5000011"}),
    );
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        out["error"],
        "api/".to_string() + &api_key + ": refs/heads/release no longer exists",
        "{out}"
    );
    assert_eq!(out["conflicts"][0]["no_branch"], true, "{out}");
    assert!(server.healthy());
}
