//! The changeset workspace, end to end against a real server and the real
//! `git` CLI: the read view that lists every member at its proposed head,
//! and the composed clone URL — a read-only repository served for the
//! changeset — that `git clone --recurse-submodules` turns into one
//! checkout with every member at that head. Who may clone it is who may
//! read every member; everyone else gets the same answer a private
//! repository gives. Every clone is `fsck --full --strict`-clean (I11),
//! superproject and members alike.

use std::path::Path;
use std::time::Duration;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

const CS: &str = "/v1/orgs/acme/changesets";

struct World {
    server: Server,
    admin: String,
    scratch: Scratch,
}

fn world(hint: &str) -> World {
    world_with(hint, &[])
}

fn world_with(hint: &str, env: &[(&str, &str)]) -> World {
    let minio = Minio::shared();
    let bucket = minio.bucket(hint);
    let scratch = Scratch::new(hint);
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"));
    for (k, v) in env {
        b = b.env(k, *v);
    }
    let server = b.start();
    let admin = server.bootstrap_org("acme");
    World {
        server,
        admin,
        scratch,
    }
}

fn put(path: &str, content: &str) -> serde_json::Value {
    serde_json::json!({"op": "put", "path": path, "content": content})
}

impl World {
    fn commit(
        &self,
        repo: &str,
        branch: &str,
        message: &str,
        ops: Vec<serde_json::Value>,
    ) -> String {
        let (st, out) = self.server.post(
            &format!("/v1/orgs/acme/repos/{repo}/commits"),
            &self.admin,
            Some(serde_json::json!({"branch": branch, "message": message, "operations": ops})),
        );
        assert_eq!(st, 201, "commit to {repo}/{branch}: {out}");
        out["commit"].as_str().unwrap().to_string()
    }

    /// A repository whose trunk has `OWNERS` naming `owner`, a `feature`
    /// branch adding `feature.txt` (content `text`), and an open change
    /// `key` from it. Returns the change's tip.
    fn repo_with_change(
        &self,
        repo: &str,
        public: bool,
        owner: &str,
        key: &str,
        text: &str,
    ) -> String {
        let (st, out) = self.server.post(
            "/v1/orgs/acme/repos",
            &self.admin,
            Some(serde_json::json!({"name": repo, "public": public})),
        );
        assert_eq!(st, 201, "create repo {repo}: {out}");
        self.commit(
            repo,
            "main",
            "trunk",
            vec![put("OWNERS", &format!("{owner}\n")), put("readme", "v1\n")],
        );
        let (st, out) = self.server.post(
            &format!("/v1/orgs/acme/repos/{repo}/branches"),
            &self.admin,
            Some(serde_json::json!({"name": "feature", "from": "main"})),
        );
        assert_eq!(st, 201, "branch feature in {repo}: {out}");
        self.patchset(repo, key, text)
    }

    /// Another patchset of `key` in `repo`. Returns the new tip.
    fn patchset(&self, repo: &str, key: &str, text: &str) -> String {
        let sha = self.commit(
            repo,
            "feature",
            &format!("change {repo}: {text}\n\nChange-Id: {key}\n"),
            vec![put("feature.txt", &format!("{text}\n"))],
        );
        let (st, out) = self.server.post(
            &format!("/v1/orgs/acme/repos/{repo}/changes"),
            &self.admin,
            Some(serde_json::json!({"from": "feature"})),
        );
        assert_eq!(st, 201, "open change in {repo}: {out}");
        assert_eq!(out["patchset"]["commit"], sha, "{out}");
        sha
    }

    fn compose(&self, key: &str, members: &[(&str, &str)]) -> serde_json::Value {
        let members: Vec<_> = members
            .iter()
            .map(|(r, c)| serde_json::json!({"repo": r, "change": c}))
            .collect();
        let (st, out) = self.server.post(
            CS,
            &self.admin,
            Some(serde_json::json!({"key": key, "title": "compose", "members": members})),
        );
        assert_eq!(st, 201, "{out}");
        out
    }

    fn changeset(&self, key: &str) -> serde_json::Value {
        let (st, out) = self.server.get(&format!("{CS}/{key}"), &self.admin);
        assert_eq!(st, 200, "{out}");
        out
    }

    fn workspace_as(&self, key: &str, token: &str) -> (u16, serde_json::Value) {
        self.server.get(&format!("{CS}/{key}/workspace"), token)
    }

    fn workspace(&self, key: &str) -> serde_json::Value {
        let (st, out) = self.workspace_as(key, &self.admin);
        assert_eq!(st, 200, "{out}");
        out
    }

    /// The workspace clone URL with `token` embedded, the way `authed_url`
    /// does it for a repository.
    fn ws_url(&self, token: &str, key: &str) -> String {
        format!(
            "http://x:{token}@{}/acme/changesets/{key}.git",
            self.server.host()
        )
    }

    fn anon_ws_url(&self, key: &str) -> String {
        format!("{}/acme/changesets/{key}.git", self.server.base)
    }

    /// A token that reads exactly one repository of the org.
    fn reader_of(&self, repo: &str) -> String {
        let (st, out) = self.server.post(
            "/v1/orgs/acme/tokens",
            &self.admin,
            Some(serde_json::json!({"scopes": ["repo:read"], "repo": repo, "label": format!("reads {repo}")})),
        );
        assert_eq!(st, 201, "{out}");
        out["token"].as_str().unwrap().to_string()
    }

    fn set_public(&self, repo: &str, public: bool) {
        let (st, out) = self.server.req(
            "PATCH",
            &format!("/v1/orgs/acme/repos/{repo}"),
            &self.admin,
            Some(serde_json::json!({"public": public})),
        );
        assert_eq!(st, 200, "{out}");
    }

    /// `GET …/info/refs?service=git-upload-pack` as git would send it,
    /// with a bearer token or without; returns (status, WWW-Authenticate).
    fn info_refs(&self, key: &str, token: Option<&str>) -> (u16, Option<String>) {
        let mut r = ureq::get(&format!(
            "{}/acme/changesets/{key}.git/info/refs?service=git-upload-pack",
            self.server.base
        ))
        .set("Git-Protocol", "version=2")
        .timeout(Duration::from_secs(30));
        if let Some(t) = token {
            r = r.set("Authorization", &format!("Bearer {t}"));
        }
        match r.call() {
            Ok(resp) => (resp.status(), None),
            Err(ureq::Error::Status(st, resp)) => {
                (st, resp.header("www-authenticate").map(str::to_string))
            }
            Err(e) => panic!("info/refs: {e}"),
        }
    }

    /// POST a raw v2 body to the workspace's upload-pack (or receive-pack)
    /// as the admin.
    fn raw(&self, key: &str, service: &str, body: &[u8]) -> (u16, Vec<u8>) {
        self.raw_as(&self.admin, key, service, &[], body)
    }

    fn raw_as(
        &self,
        token: &str,
        key: &str,
        service: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let mut req = ureq::post(&format!(
            "{}/acme/changesets/{key}.git/{service}",
            self.server.base
        ))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", &format!("application/x-{service}-request"))
        .timeout(Duration::from_secs(30));
        for (k, v) in extra {
            req = req.set(k, v);
        }
        let resp = req.send_bytes(body);
        match resp {
            Ok(r) | Err(ureq::Error::Status(_, r)) => {
                let st = r.status();
                let mut buf = Vec::new();
                use std::io::Read;
                let _ = r.into_reader().read_to_end(&mut buf);
                (st, buf)
            }
            Err(e) => panic!("raw {service}: {e}"),
        }
    }
}

fn pkt(line: &str) -> Vec<u8> {
    let mut v = format!("{:04x}", line.len() + 4).into_bytes();
    v.extend_from_slice(line.as_bytes());
    v
}

fn v2_fetch_body(args: &[&str]) -> Vec<u8> {
    let mut body = pkt("command=fetch\n");
    body.extend_from_slice(&pkt("object-format=sha1\n"));
    body.extend_from_slice(b"0001");
    for a in args {
        body.extend_from_slice(&pkt(&format!("{a}\n")));
    }
    body.extend_from_slice(b"0000");
    body
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

fn head_of(repo: &Path) -> String {
    gitcli::git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

/// `git clone --recurse-submodules` into `dest`, then fsck the
/// superproject and every member checkout: a clone that git accepts but
/// `fsck --full --strict` does not is not a clone (I11).
fn clone_recursive(parent: &Path, url: &str, dest: &Path, extra: &[&str]) {
    std::fs::create_dir_all(parent).unwrap();
    let mut args = vec!["clone", "-q", "--recurse-submodules"];
    args.extend_from_slice(extra);
    args.push(url);
    args.push(dest.to_str().unwrap());
    gitcli::git(parent, &args);
    gitcli::fsck(dest);
    for line in gitcli::git(dest, &["submodule", "status"]).lines() {
        // " <sha> <path> (<describe>)" — a leading space is "in sync".
        assert!(
            line.starts_with(' '),
            "submodule out of step with the superproject: {line:?}"
        );
        let path = line.split_whitespace().nth(1).unwrap();
        gitcli::fsck(&dest.join(path));
    }
}

// ---------------------------------------------------------------------
// The view, the clone, and following the changeset as it moves
// ---------------------------------------------------------------------

#[test]
fn a_recursive_clone_of_the_workspace_checks_every_member_out_at_its_proposed_head() {
    let w = world("cs-ws-clone");
    let api = w.repo_with_change("api", false, "oa@acme.test", "Iaa000001", "change api");
    let web = w.repo_with_change("web", false, "ow@acme.test", "Ibb000002", "change web");
    w.compose("Ic5000001", &[("web", "Ibb000002"), ("api", "Iaa000001")]);

    // The view: every member at its head, in repository order regardless
    // of the order they joined, and the same composition the changeset
    // itself reports — the one its composed CI runs are named by.
    let view = w.workspace("Ic5000001");
    assert_eq!(view["key"], "Ic5000001");
    assert_eq!(view["title"], "compose");
    assert_eq!(view["state"], "open");
    assert_eq!(view["note"], serde_json::Value::Null);
    assert_eq!(view["composition"], w.changeset("Ic5000001")["composition"]);
    let tip = view["tip"].as_str().unwrap().to_string();
    assert_eq!(tip.len(), 40, "{view}");
    assert_eq!(view["clone_url"], w.anon_ws_url("Ic5000001"), "{view}");
    assert!(view["ssh_clone_url"].is_null(), "no SSH front door: {view}");
    let members = view["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{view}");
    assert_eq!(members[0]["repo"], "api");
    assert_eq!(members[0]["change"], "Iaa000001");
    assert_eq!(members[0]["path"], "api");
    assert_eq!(members[0]["commit"], api);
    assert_eq!(members[0]["fetch_ref"], format!("refs/patchsets/{api}"));
    assert_eq!(
        members[0]["clone_url"],
        format!("{}/acme/api.git", w.server.base)
    );
    assert!(members[0]["ssh_clone_url"].is_null());
    assert_eq!(members[1]["repo"], "web");
    assert_eq!(members[1]["commit"], web);

    // The clone: one directory per member, each at its proposed head,
    // fsck-clean all the way down.
    let ws = w.scratch.path().join("ws");
    clone_recursive(w.scratch.path(), &w.ws_url(&w.admin, "Ic5000001"), &ws, &[]);
    assert_eq!(head_of(&ws), tip);
    assert_eq!(
        gitcli::git(&ws, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/workspace"
    );
    assert_eq!(head_of(&ws.join("api")), api);
    assert_eq!(head_of(&ws.join("web")), web);
    assert_eq!(
        std::fs::read_to_string(ws.join("api/feature.txt")).unwrap(),
        "change api\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("web/feature.txt")).unwrap(),
        "change web\n"
    );
    // Relative URLs: the members were fetched over the transport and
    // credential the workspace came in on, and the file says so.
    let gitmodules = std::fs::read_to_string(ws.join(".gitmodules")).unwrap();
    assert_eq!(
        gitmodules,
        "[submodule \"api\"]\n\tpath = api\n\turl = ../../api.git\n\
         [submodule \"web\"]\n\tpath = web\n\turl = ../../web.git\n"
    );
    let subject = gitcli::git(&ws, &["log", "-1", "--format=%s%n%n%b"]);
    assert!(
        subject.starts_with("Workspace of changeset Ic5000001: compose\n"),
        "{subject}"
    );
    assert!(
        subject.contains(&format!(
            "composition {}",
            view["composition"].as_str().unwrap()
        )),
        "{subject}"
    );
    assert!(subject.contains(&format!("api {api}")), "{subject}");
    // The workspace is dated by its newest member tip, not by the clock:
    // the same changeset clones to the same commit from any node.
    assert_eq!(
        gitcli::git(&ws, &["log", "-1", "--format=%an <%ae>"]).trim(),
        "Weft <workspace@weft.sh>"
    );

    // A new patchset on one member is a new composition and a new tip.
    // Every tip is a fresh root — the history is in the members — so a
    // `git pull` has nothing to merge and says so, and the documented
    // fetch-and-reset is what moves a checkout.
    let api2 = w.patchset("api", "Iaa000001", "change api, again");
    let view2 = w.workspace("Ic5000001");
    let tip2 = view2["tip"].as_str().unwrap().to_string();
    assert_ne!(tip2, tip);
    assert_ne!(view2["composition"], view["composition"]);
    assert_eq!(view2["members"][0]["commit"], api2);
    let err = gitcli::git_expect_err(&ws, &["pull", "-q", "origin", "workspace"]).unwrap();
    assert!(err.contains("divergent"), "{err}");
    gitcli::git(&ws, &["fetch", "-q", "origin"]);
    gitcli::git(&ws, &["reset", "-q", "--hard", "origin/workspace"]);
    gitcli::git(&ws, &["submodule", "update", "-q", "--init"]);
    assert_eq!(head_of(&ws), tip2);
    assert_eq!(head_of(&ws.join("api")), api2);
    assert_eq!(head_of(&ws.join("web")), web);
    gitcli::fsck(&ws);
    gitcli::fsck(&ws.join("api"));

    // A member whose commit is on no branch any more is still pinned and
    // still fetchable: the branch it came from is deleted, and a fresh
    // recursive clone checks it out anyway, through refs/patchsets.
    let (st, out) = w
        .server
        .delete("/v1/orgs/acme/repos/api/branches/feature", &w.admin);
    assert_eq!(st, 204, "{out}");
    let ws2 = w.scratch.path().join("ws2");
    clone_recursive(
        w.scratch.path(),
        &w.ws_url(&w.admin, "Ic5000001"),
        &ws2,
        &[],
    );
    assert_eq!(head_of(&ws2.join("api")), api2);

    // A shallow clone is the CI shape, and one commit is already depth 1.
    let ws3 = w.scratch.path().join("ws3");
    clone_recursive(
        w.scratch.path(),
        &w.ws_url(&w.admin, "Ic5000001"),
        &ws3,
        &["--depth", "1"],
    );
    assert_eq!(head_of(&ws3), tip2);
    assert_eq!(head_of(&ws3.join("web")), web);

    // Read-only, said in-band where the person pushing reads it.
    gitcli::git(&ws, &["commit", "-q", "--allow-empty", "-m", "nope"]);
    let err = gitcli::git_expect_err(&ws, &["push", "-q", "origin", "HEAD:workspace"]).unwrap();
    assert!(
        err.contains("a changeset workspace is read-only; push to its member repositories"),
        "{err}"
    );
    let (st, body) = w.raw("Ic5000001", "git-receive-pack", b"0000");
    assert_eq!(st, 200);
    assert!(text(&body).contains("read-only"), "{}", text(&body));
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// Who may clone it
// ---------------------------------------------------------------------

#[test]
fn only_a_reader_of_every_member_may_see_or_clone_the_workspace() {
    let w = world("cs-ws-acl");
    w.repo_with_change("api", false, "oa@acme.test", "Iaa000001", "change api");
    w.repo_with_change("web", false, "ow@acme.test", "Ibb000002", "change web");
    w.compose("Ic5000001", &[("api", "Iaa000001"), ("web", "Ibb000002")]);

    // Read on one member is not read on the changeset: the view and the
    // wire both answer as they would for a repository that does not exist.
    let api_only = w.reader_of("api");
    let (st, out) = w.workspace_as("Ic5000001", &api_only);
    assert_eq!(st, 404, "{out}");
    assert_eq!(w.info_refs("Ic5000001", Some(&api_only)).0, 404);
    let err = gitcli::git_expect_err(
        w.scratch.path(),
        &["clone", "-q", &w.ws_url(&api_only, "Ic5000001"), "denied"],
    )
    .unwrap();
    assert!(err.contains("not found"), "{err}");
    // …and not only the advert: a client that skips it and posts the
    // command straight to either door is masked the same way.
    let fetch = v2_fetch_body(&[
        "want 0000000000000000000000000000000000000000",
        "ofs-delta",
        "done",
    ]);
    let (st, body) = w.raw_as(&api_only, "Ic5000001", "git-upload-pack", &[], &fetch);
    assert_eq!(st, 404, "{}", text(&body));
    let (st, body) = w.raw_as(&api_only, "Ic5000001", "git-receive-pack", &[], b"0000");
    assert_eq!(st, 404, "{}", text(&body));
    // An organisation that does not exist reads as a changeset that does
    // not exist.
    let (st, out) = w
        .server
        .get("/v1/orgs/nobody/changesets/Ic5000001/workspace", &w.admin);
    assert_eq!(st, 404, "{out}");
    // A stranger's token — another org's admin — is refused the same way,
    // and so is a changeset that does not exist, so neither answer says
    // which of the two it was.
    let rival = w.server.bootstrap_org("rival");
    assert_eq!(w.workspace_as("Ic5000001", &rival).0, 404);
    assert_eq!(w.info_refs("Ic5000001", Some(&rival)).0, 404);
    assert_eq!(w.info_refs("Ic5000009", Some(&w.admin)).0, 404);
    assert_eq!(w.workspace_as("Ic5000009", &w.admin).0, 404);
    // No credentials at all: the Basic challenge, so git retries with
    // some — for a private workspace and for one that does not exist.
    let (st, challenge) = w.info_refs("Ic5000001", None);
    assert_eq!(st, 401);
    assert!(
        challenge.as_deref().is_some_and(|c| c.starts_with("Basic")),
        "{challenge:?}"
    );
    assert_eq!(w.info_refs("Ic5000009", None).0, 401);

    // Every member public: the workspace reads and clones anonymously,
    // members included, because the relative URLs resolve to public
    // repositories.
    w.set_public("api", true);
    w.set_public("web", true);
    assert_eq!(w.info_refs("Ic5000001", None).0, 200);
    let (st, out) = w.workspace_as("Ic5000001", "");
    assert_eq!(st, 200, "{out}");
    let anon = w.scratch.path().join("anon");
    clone_recursive(w.scratch.path(), &w.anon_ws_url("Ic5000001"), &anon, &[]);
    assert_eq!(head_of(&anon), out["tip"].as_str().unwrap());
    assert!(anon.join("api/feature.txt").exists());
    assert!(anon.join("web/feature.txt").exists());
    // One member back to private and the whole workspace is private again.
    w.set_public("web", false);
    assert_eq!(w.info_refs("Ic5000001", None).0, 401);
    assert_eq!(w.workspace_as("Ic5000001", "").0, 404);
    assert_eq!(w.info_refs("Ic5000001", Some(&w.admin)).0, 200);

    // The wire is strict about what it will serve: only the workspace
    // tip, only with the capabilities the engine needs, and it says so
    // rather than hanging or serving a guess.
    let tip = w.workspace("Ic5000001")["tip"]
        .as_str()
        .unwrap()
        .to_string();
    let api = w.workspace("Ic5000001")["members"][0]["commit"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, body) = w.raw(
        "Ic5000001",
        "git-upload-pack",
        &v2_fetch_body(&[&format!("want {api}"), "ofs-delta", "done"]),
    );
    assert_eq!(st, 500, "{}", text(&body));
    assert!(
        text(&body).contains("not the changeset workspace tip"),
        "{}",
        text(&body)
    );
    let (st, body) = w.raw(
        "Ic5000001",
        "git-upload-pack",
        &v2_fetch_body(&[&format!("want {tip}"), "done"]),
    );
    assert_eq!(st, 500, "{}", text(&body));
    assert!(text(&body).contains("ofs-delta"), "{}", text(&body));
    let (st, body) = w.raw(
        "Ic5000001",
        "git-upload-pack",
        &v2_fetch_body(&[&format!("want {tip}"), "ofs-delta", "frobnicate", "done"]),
    );
    assert_eq!(st, 500, "{}", text(&body));
    assert!(
        text(&body).contains("unsupported fetch arg"),
        "{}",
        text(&body)
    );
    // A service this door does not speak, and a body that claims to be
    // gzip and is not: refused with the reason, not served as a guess.
    let resp = ureq::get(&format!(
        "{}/acme/changesets/Ic5000001.git/info/refs?service=git-frobnicate",
        w.server.base
    ))
    .set("Authorization", &format!("Bearer {}", w.admin))
    .call();
    match resp {
        Err(ureq::Error::Status(code, _)) => assert_eq!(code, 500),
        Ok(r) => panic!("unknown service accepted: {}", r.status()),
        Err(e) => panic!("transport: {e}"),
    }
    let (st, body) = w.raw_as(
        &w.admin,
        "Ic5000001",
        "git-upload-pack",
        &[("Content-Encoding", "gzip")],
        b"this is not gzip",
    );
    assert_eq!(st, 500, "{}", text(&body));
    assert!(text(&body).contains("gzip request body"), "{}", text(&body));
    // No `done`: the negotiation answer, never a pack the client did not
    // ask for yet.
    let (st, body) = w.raw(
        "Ic5000001",
        "git-upload-pack",
        &v2_fetch_body(&[&format!("want {tip}"), "ofs-delta", &format!("have {api}")]),
    );
    assert_eq!(st, 200, "{}", text(&body));
    assert!(text(&body).contains("acknowledgments"), "{}", text(&body));
    assert!(text(&body).contains("NAK"), "{}", text(&body));
    assert!(!text(&body).contains("packfile"), "{}", text(&body));
    assert!(w.server.healthy());
}

// ---------------------------------------------------------------------
// The honest middle, a member whose repository is gone, and the name
// ---------------------------------------------------------------------

#[test]
fn a_landing_changeset_says_so_and_a_vanished_member_leaves_the_tree() {
    // A lander that polls every ten minutes: `land` moves the changeset
    // to `landing` and nothing moves it further inside the test.
    let w = world_with("cs-ws-landing", &[("STRATUM_LAND_POLL_SECS", "600")]);
    w.repo_with_change("api", false, "oa@acme.test", "Iaa000001", "change api");
    w.repo_with_change("web", false, "ow@acme.test", "Ibb000002", "change web");
    w.compose("Ic5000001", &[("api", "Iaa000001"), ("web", "Ibb000002")]);
    approve(&w, "oa@acme.test", "api", "Iaa000001");
    approve(&w, "ow@acme.test", "web", "Ibb000002");
    let (st, out) = w
        .server
        .post(&format!("{CS}/Ic5000001/land"), &w.admin, None);
    assert_eq!(st, 202, "{out}");
    let view = w.workspace("Ic5000001");
    assert_eq!(view["state"], "landing", "{view}");
    let note = view["note"]
        .as_str()
        .unwrap_or_else(|| panic!("a note: {view}"));
    assert!(note.contains("landing"), "{note}");
    assert!(note.contains("proposed state, not the trunks"), "{note}");
    // Still cloneable — the proposed state, as the note says.
    let ws = w.scratch.path().join("landing");
    clone_recursive(w.scratch.path(), &w.ws_url(&w.admin, "Ic5000001"), &ws, &[]);
    assert_eq!(head_of(&ws), view["tip"].as_str().unwrap());

    // A member whose repository is deleted leaves the composition, the
    // view and the tree together; the last one leaving empties them.
    let cli = w.repo_with_change("cli", false, "oc@acme.test", "Icc000003", "change cli");
    w.repo_with_change("docs", false, "od@acme.test", "Idd000004", "change docs");
    w.compose("Ic5000002", &[("cli", "Icc000003"), ("docs", "Idd000004")]);
    let (st, out) = w.server.delete("/v1/orgs/acme/repos/docs", &w.admin);
    assert_eq!(st, 204, "{out}");
    let view = w.workspace("Ic5000002");
    assert_eq!(view["members"].as_array().unwrap().len(), 1, "{view}");
    assert_eq!(view["members"][0]["repo"], "cli");
    assert_eq!(view["composition"], w.changeset("Ic5000002")["composition"]);
    let ws = w.scratch.path().join("one-left");
    clone_recursive(w.scratch.path(), &w.ws_url(&w.admin, "Ic5000002"), &ws, &[]);
    assert_eq!(head_of(&ws.join("cli")), cli);
    assert!(!ws.join("docs").exists());
    assert_eq!(
        std::fs::read_to_string(ws.join(".gitmodules")).unwrap(),
        "[submodule \"cli\"]\n\tpath = cli\n\turl = ../../cli.git\n"
    );
    let (st, out) = w
        .server
        .delete(&format!("{CS}/Ic5000002/members/cli/Icc000003"), &w.admin);
    assert_eq!(st, 204, "{out}");
    let view = w.workspace("Ic5000002");
    assert_eq!(view["members"], serde_json::json!([]), "{view}");
    assert!(view["composition"].is_null(), "{view}");
    assert!(view["tip"].is_null(), "{view}");
    // Nothing to advertise, and nothing to fetch.
    let refs = gitcli::git(
        w.scratch.path(),
        &["ls-remote", &w.ws_url(&w.admin, "Ic5000002")],
    );
    assert_eq!(refs.trim(), "", "{refs}");
    let err = gitcli::git_expect_err(
        w.scratch.path(),
        &["clone", "-q", &w.ws_url(&w.admin, "Ic5000002"), "empty"],
    );
    // git clones an empty repository without complaint; either answer is
    // fine as long as the server did not invent a commit.
    if err.is_err() {
        assert!(!w
            .scratch
            .path()
            .join("empty/.git/refs/heads/workspace")
            .exists());
    }

    // The wire path `/{org}/changesets/{key}.git` belongs to the
    // platform now, so no repository may take the name that would shadow
    // it, in any spelling.
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos",
        &w.admin,
        Some(serde_json::json!({"name": "changesets"})),
    );
    assert_eq!(st, 400, "{out}");
    assert!(out["error"].as_str().unwrap().contains("reserved"), "{out}");
    let (st, out) = w.server.post(
        "/v1/orgs/acme/repos",
        &w.admin,
        Some(serde_json::json!({"name": "ChangeSets"})),
    );
    assert_eq!(st, 400, "case-folded: {out}");
    assert!(w.server.healthy());
}

const PASSWORD: &str = "a long enough password";

fn approve(w: &World, email: &str, repo: &str, key: &str) {
    let name = email.split('@').next().unwrap();
    w.server
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
            "member",
        ])
        .unwrap_or_else(|e| panic!("user-create {email}: {e}"));
    let mut who = stratum_testkit::browser::Browser::signed_in(&w.server, email, PASSWORD);
    let (st, out) = who.req(
        "POST",
        &format!("/v1/orgs/acme/repos/{repo}/changes/{key}/approve"),
        None,
    );
    assert_eq!(st, 204, "{email} approving {repo}/{key}: {out}");
}
