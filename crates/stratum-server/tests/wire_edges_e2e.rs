//! Protocol edge cases over the real wire: crafted v2 fetch bodies and
//! receive-pack requests that stock git in the happy path never sends —
//! every validation answer the serving and receive fronts define.

use std::process::{Command, Stdio};
use std::time::Duration;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::{Minio, Server};

fn spawn_server(store_url: &str, scratch: &Scratch, extra: &[(&str, String)]) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("wire")
        .data_dir(scratch.path().join("data"))
        .envs(extra)
        .env("STRATUM_COMPACT_POLL_SECS", "86400")
        .start()
}

fn commit(server: &Server, token: &str, rp: &str, path: &str, content: &str) -> String {
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/commits"),
        token,
        Some(serde_json::json!({
            "message": format!("add {path}"),
            "operations": [ { "op": "put", "path": path, "content": content } ],
        })),
    );
    assert_eq!(st, 201, "{out}");
    out["commit"].as_str().unwrap().to_string()
}

fn pkt(line: &str) -> Vec<u8> {
    let mut v = format!("{:04x}", line.len() + 4).into_bytes();
    v.extend_from_slice(line.as_bytes());
    v
}

/// One receive-pack command set with no pack behind it — enough for a
/// deletion, which is what the gzip test uses so the assertion stays
/// about the encoding rather than about pack construction.
fn push_body_for(cmd: &str) -> Vec<u8> {
    let mut body = pkt(&format!("{cmd}\0report-status ofs-delta agent=test\n"));
    body.extend_from_slice(b"0000");
    body
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

/// POST a raw upload-pack body; returns (status, response bytes).
fn upload(server: &Server, token: &str, org: &str, repo: &str, body: &[u8]) -> (u16, Vec<u8>) {
    raw_post(
        server,
        token,
        &format!("{org}/{repo}/git-upload-pack"),
        "application/x-git-upload-pack-request",
        body,
    )
}

fn receive(server: &Server, token: &str, org: &str, repo: &str, body: &[u8]) -> (u16, Vec<u8>) {
    raw_post(
        server,
        token,
        &format!("{org}/{repo}/git-receive-pack"),
        "application/x-git-receive-pack-request",
        body,
    )
}

fn raw_post(server: &Server, token: &str, path: &str, ct: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let resp = ureq::post(&format!("{}/{path}", server.base))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", ct)
        .timeout(Duration::from_secs(30))
        .send_bytes(body);
    match resp {
        Ok(r) | Err(ureq::Error::Status(_, r)) => {
            let st = r.status();
            let mut buf = Vec::new();
            use std::io::Read;
            let _ = r.into_reader().read_to_end(&mut buf);
            (st, buf)
        }
        Err(e) => panic!("raw post {path}: {e}"),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

const BOGUS: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ZERO: &str = "0000000000000000000000000000000000000000";

#[test]
fn fetch_argument_and_want_validation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-fetch");
    let scratch = Scratch::new("wire-fetch");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let tip = commit(&server, &admin, rp, "a.txt", "one\n");

    // Unknown v2 command.
    let mut body = pkt("command=frobnicate\n");
    body.extend_from_slice(b"0001");
    body.extend_from_slice(b"0000");
    let (_, out) = upload(&server, &admin, "acme", "app", &body);
    assert!(text(&out).contains("unsupported command"), "{}", text(&out));

    // An argument-less request (command, no delim section) parses to
    // empty args and fails on the command's own validation, not a parse
    // error.
    let mut body = pkt("command=fetch\n");
    body.extend_from_slice(b"0000");
    let (_, out) = upload(&server, &admin, "acme", "app", &body);
    assert!(
        text(&out).contains("ofs-delta") || text(&out).contains("no wants"),
        "{}",
        text(&out)
    );

    // Wire fetch of a repo that does not exist, with valid credentials:
    // 404 (existence masking holds on the wire too).
    let resp = ureq::get(&format!(
        "{}/acme/ghost-repo/info/refs?service=git-upload-pack",
        server.base
    ))
    .set("Authorization", &format!("Bearer {admin}"))
    .call();
    assert!(matches!(resp, Err(ureq::Error::Status(404, _))), "{resp:?}");

    // A repo whose storage was deleted out from under a live row: the
    // advert answers 404 rather than a 500.
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "wiped" })),
    );
    let (_, wiped) = server.req("GET", "/v1/orgs/acme/repos/wiped", &admin, None);
    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    store
        .delete(&format!(
            "o/{}/r/{}/prod/manifest.json",
            wiped["org_id"].as_str().unwrap(),
            wiped["id"].as_str().unwrap()
        ))
        .unwrap();
    // The v2 advert is static, but the first command that reads the
    // manifest answers 404.
    let mut lsbody = pkt("command=ls-refs\n");
    lsbody.extend_from_slice(b"0001");
    lsbody.extend_from_slice(b"0000");
    let (st, out) = upload(&server, &admin, "acme", "wiped", &lsbody);
    assert!(
        st == 404 || text(&out).contains("HTTP 404"),
        "{st} {}",
        text(&out)
    );

    // No ofs-delta support → refused (dead-ends ledger: no REF-delta serving).
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&[&format!("want {tip}"), "done"]),
    );
    assert!(text(&out).contains("ofs-delta"), "{}", text(&out));

    // No wants at all.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&["ofs-delta", "done"]),
    );
    assert!(text(&out).contains("no wants"), "{}", text(&out));

    // Unknown argument is an error; a "shallow <oid>" arg is tolerated.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&["ofs-delta", "frob=1", &format!("want {tip}"), "done"]),
    );
    assert!(
        text(&out).contains("unsupported fetch arg"),
        "{}",
        text(&out)
    );
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&[
            "ofs-delta",
            &format!("shallow {BOGUS}"),
            &format!("want {tip}"),
            "done",
        ]),
    );
    assert!(text(&out).contains("packfile"), "{}", text(&out));

    // Non-tip want on a flat-refs (unpaged) repo is refused by policy.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&["ofs-delta", &format!("want {BOGUS}"), "done"]),
    );
    assert!(text(&out).contains("not served yet"), "{}", text(&out));

    // Clone-shaped fetch without done is pushed to send done.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&["ofs-delta", &format!("want {tip}")]),
    );
    assert!(text(&out).contains("clone without done"), "{}", text(&out));

    // ls-refs without symrefs: HEAD advertised without a symref target.
    let mut body = pkt("command=ls-refs\n");
    body.extend_from_slice(b"0001");
    body.extend_from_slice(b"0000");
    let (_, out) = upload(&server, &admin, "acme", "app", &body);
    let advert = text(&out);
    assert!(advert.contains(&format!("{tip} HEAD")), "{advert}");
    assert!(!advert.contains("symref-target"), "{advert}");
}

#[test]
fn fetch_negotiation_on_a_compacted_repo() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-nego");
    let scratch = Scratch::new("wire-nego");
    // Paged so non-tip wants are servable; compacted so a spine exists.
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_REF_PAGE_SIZE", "2".into())],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let mut commits = Vec::new();
    for i in 0..9 {
        commits.push(commit(
            &server,
            &admin,
            rp,
            &format!("f{}.txt", i % 3),
            &format!("v{i}\n"),
        ));
    }
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");
    let tip = commits.last().unwrap().clone();
    let old = commits[2].clone();

    // Incremental fetch WITHOUT done: ACK + ready + packfile in one round.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&["ofs-delta", &format!("want {tip}"), &format!("have {old}")]),
    );
    let t = text(&out);
    assert!(
        t.contains(&format!("ACK {old}")) && t.contains("ready"),
        "{t}"
    );
    assert!(t.contains("packfile"), "{t}");

    // Unknown have WITH done on the spine path → loud fallback error.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&[
            "ofs-delta",
            &format!("want {tip}"),
            &format!("have {BOGUS}"),
            "done",
        ]),
    );
    assert!(text(&out).contains("bitmap-path miss"), "{}", text(&out));

    // Non-tip want without done on the paged path → NAK, client retries
    // with done.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&["ofs-delta", &format!("want {old}")]),
    );
    let t = text(&out);
    assert!(t.contains("acknowledgments") && t.contains("NAK"), "{t}");

    // deepen combined with a non-tip want → explicit fallback.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&["ofs-delta", "deepen 1", &format!("want {old}"), "done"]),
    );
    assert!(text(&out).contains("deepen with non-tip"), "{}", text(&out));

    // A want nowhere in the layout, paged path → named refusal.
    let (_, out) = upload(
        &server,
        &admin,
        "acme",
        "app",
        &v2_fetch_body(&["ofs-delta", &format!("want {BOGUS}"), "done"]),
    );
    assert!(
        text(&out).contains("not known to this layout"),
        "{}",
        text(&out)
    );

    // ls-refs with no ref-prefix on a paged repo lists every page.
    let mut body = pkt("command=ls-refs\n");
    body.extend_from_slice(&pkt("symrefs\n"));
    body.extend_from_slice(b"0001");
    body.extend_from_slice(b"0000");
    let (_, out) = upload(&server, &admin, "acme", "app", &body);
    assert!(text(&out).contains("refs/heads/main"), "{}", text(&out));
}

/// Push bodies stock git never sends: malformed updates, unsupported
/// shapes, and pushes that fail connectivity/duplicate validation.
#[test]
fn push_validation_edges() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-push");
    let scratch = Scratch::new("wire-push");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    let tip = commit(&server, &admin, rp, "a.txt", "one\n");

    let url = server.authed_url(&admin, "acme", "app");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);

    // A pack of the tip's closure, for pushes that need real bytes.
    let pack_of = |dir: &std::path::Path, revs: &str| -> Vec<u8> {
        let mut c = Command::new("git");
        c.arg("-C")
            .arg(dir)
            .args(["pack-objects", "--revs", "--stdout", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().unwrap();
        use std::io::{Read, Write};
        child
            .stdin
            .take()
            .unwrap()
            .write_all(revs.as_bytes())
            .unwrap();
        let mut buf = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut buf).unwrap();
        assert!(child.wait().unwrap().success());
        buf
    };

    let push_body = |cmds: &[String], pack: &[u8]| -> Vec<u8> {
        let mut body = Vec::new();
        for (i, c) in cmds.iter().enumerate() {
            let line = if i == 0 {
                format!("{c}\0report-status ofs-delta agent=test\n")
            } else {
                format!("{c}\n")
            };
            body.extend_from_slice(&pkt(&line));
        }
        body.extend_from_slice(b"0000");
        body.extend_from_slice(pack);
        body
    };
    let assert_ng = |out: &[u8], needle: &str| {
        let t = text(out);
        assert!(t.contains("ng ") && t.contains(needle), "{t}");
    };

    // Delim pkt in a receive body is a protocol error.
    let (_, out) = receive(&server, &admin, "acme", "app", b"0001");
    assert!(text(&out).contains("unexpected delim"), "{}", text(&out));

    // Malformed update command.
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&["not a valid line".into()], b""),
    );
    assert!(text(&out).contains("malformed update"), "{}", text(&out));

    // Flush-only body: no updates, clean empty answer.
    let (st, _) = receive(&server, &admin, "acme", "app", b"0000");
    assert_eq!(st, 200);

    // Deletion, bad ref namespace, and a missing pack all answer ng.
    let dummy = pack_of(&clone, &format!("{tip}\n"));
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{tip} {ZERO} refs/heads/main")], &dummy),
    );
    // Deletion is supported now; deleting the branch HEAD points at is
    // not, and the refusal says which rule it hit.
    assert_ng(&out, "default branch");
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        // `refs/tags/*` is accepted now, so the namespace check is
        // exercised with one that still is not: a forge writes
        // `refs/pull/*` and `refs/notes/*` itself, and a client must not
        // be able to push into them.
        &push_body(&[format!("{ZERO} {tip} refs/pull/1/head")], &dummy),
    );
    assert_ng(&out, "not accepted");
    // A branch to aim the stale-value delete at, since `main` is the
    // default branch and would be refused for that reason first.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/app/branches",
        &admin,
        Some(serde_json::json!({ "name": "keepme", "from": "main" })),
    );
    assert!(st == 200 || st == 201, "{st} {out}");

    // Deleting a ref that is not there, and deleting one whose value
    // has moved. Neither is reachable through the CLI — git refuses a
    // delete of a ref it cannot see, and re-reads before sending — so
    // they are sent as crafted command sets, which is also how a
    // confused or malicious client would send them.
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{tip} {ZERO} refs/heads/never-existed")], &[]),
    );
    assert_ng(&out, "does not exist here");
    let stale = "9".repeat(40);
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{stale} {ZERO} refs/heads/keepme")], &[]),
    );
    assert_ng(&out, "stale old value");
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        // And the charset check applies to a tag exactly as to a branch.
        // A tilde rather than a space: the command line is split on
        // spaces, so a space would simply truncate the name here and
        // test nothing.
        &push_body(&[format!("{ZERO} {tip} refs/tags/bad~name")], &dummy),
    );
    assert_ng(&out, "not accepted");
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{ZERO} {tip} refs/heads/nopack")], b""),
    );
    assert_ng(&out, "no pack");

    // Stale old value and phantom old value.
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{BOGUS} {tip} refs/heads/main")], &dummy),
    );
    assert_ng(&out, "stale old value");
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{BOGUS} {tip} refs/heads/ghost")], &dummy),
    );
    assert_ng(&out, "does not exist here");

    // Duplicate objects on a **create**: a new branch at the existing tip,
    // with a pack carrying the already-present closure.
    //
    // This was refused with "already present (concurrent push?)", which
    // made `git branch dup main && git push origin dup` fail whenever the
    // client sent the closure — a create takes nobody's work, so there was
    // nothing to protect. I5 is upheld by dropping the duplicates instead
    // of storing them, so the branch is created and the stream still
    // carries each object once.
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{ZERO} {tip} refs/heads/dup")], &dummy),
    );
    let t = text(&out);
    assert!(
        t.contains("ok refs/heads/dup"),
        "creating a branch at an existing commit was refused: {t}"
    );
    // …and on an *update* they are dropped exactly when the update is a
    // fast-forward, which a no-op update is trivially. The refusal used to
    // apply to every update, and it stranded any fast-forward that brought
    // back a blob the layout held — a `git revert`, a file restored — see
    // `git_e2e::a_fast_forward_that_reintroduces_a_held_blob_is_accepted`.
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{tip} {tip} refs/heads/dup")], &dummy),
    );
    let t = text(&out);
    assert!(
        t.contains("ok refs/heads/dup"),
        "a no-op update carrying held objects was refused: {t}"
    );
    // A *rewrite* carrying them is still refused, because there they are
    // the signature of a client pushing over history it cannot see.
    // `a_force_push_rewrites_an_unprotected_branch_and_never_somebody_\
    // elses_work` is the case that depends on it. Here: move `dup` one
    // commit ahead through the CLI, then craft a push that winds it back
    // to `tip` with `tip`'s closure — the old tip is not an ancestor of
    // the new one, and every object in the pack is already held.
    gitcli::git(&clone, &["checkout", "-q", "-b", "dup", tip.as_str()]);
    std::fs::write(clone.join("ahead.txt"), "ahead\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "ahead"]);
    let ahead = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    gitcli::git(&clone, &["push", "-q", "origin", "dup"]);
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{ahead} {tip} refs/heads/dup")], &dummy),
    );
    assert_ng(&out, "already present");
    assert!(
        gitcli::git(&clone, &["ls-remote", &url, "refs/heads/dup"]).contains(&ahead),
        "a refused rewind moved the branch anyway"
    );

    // Incomplete push: a commit whose parent the server has never seen,
    // packed without that parent.
    gitcli::git(&clone, &["checkout", "-q", "-b", "orphanline"]);
    std::fs::write(clone.join("p1.txt"), "p1\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "unpushed parent"]);
    let parent = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    std::fs::write(clone.join("x1.txt"), "x1\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "tip with missing parent"]);
    let orphan_tip = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let partial = pack_of(&clone, &format!("{orphan_tip}\n^{parent}\n"));
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(
            &[format!("{ZERO} {orphan_tip} refs/heads/incomplete")],
            &partial,
        ),
    );
    assert_ng(&out, "missing object");

    // New tip absent from the push: the same fresh pack, but the update
    // names a tip that is not among the pushed objects.
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "app",
        &push_body(&[format!("{ZERO} {BOGUS} refs/heads/phantom")], &partial),
    );
    assert_ng(&out, "not in push");

    // Sideband report: same rejection with side-band-64k negotiated rides
    // channel 1.
    let mut body = Vec::new();
    body.extend_from_slice(&pkt(&format!(
        "{tip} {ZERO} refs/heads/main\0report-status side-band-64k ofs-delta agent=test\n"
    )));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&dummy);
    let (_, out) = receive(&server, &admin, "acme", "app", &body);
    assert!(text(&out).contains("default branch"), "{}", text(&out));

    // Non-fast-forward via the real CLI: diverge locally, force push.
    //
    // This asserted a refusal. Force push is supported now on any ref the
    // protection rules do not cover — see
    // `a_force_push_rewrites_an_unprotected_branch…` for why dropping the
    // ancestry proof does not drop the protection that matters — so what
    // is asserted here is that it lands and that the layout survives it.
    gitcli::git(&clone, &["checkout", "-q", "main"]);
    gitcli::git(&clone, &["reset", "-q", "--hard", "HEAD"]);
    std::fs::write(clone.join("ff.txt"), "ff\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "ours"]);
    gitcli::git(&clone, &["push", "-q", "origin", "main"]);
    gitcli::git(&clone, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(clone.join("diverge.txt"), "d\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "diverged"]);
    gitcli::git(&clone, &["push", "-q", "-f", "origin", "main"]);
    let rewritten = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let reclone = scratch.path().join("after-force");
    let head = gitcli::clone_and_fsck(&server.authed_url(&admin, "acme", "app"), &reclone);
    assert_eq!(head, rewritten, "the force push did not take");
    assert!(
        reclone.join("diverge.txt").exists() && !reclone.join("ff.txt").exists(),
        "the clone is not the rewritten history"
    );

    // Push into a legacy (epoch-less) layout is refused with guidance.
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "legacy" })),
    );
    let legacy_tip = commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/legacy",
        "l.txt",
        "l\n",
    );
    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let (repo_st, repo_out) = server.req("GET", "/v1/orgs/acme/repos/legacy", &admin, None);
    assert_eq!(repo_st, 200);
    let prefix = format!(
        "o/{}/r/{}/prod",
        repo_out["org_id"].as_str().unwrap(),
        repo_out["id"].as_str().unwrap()
    );
    let mkey = format!("{prefix}/manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&store.get(&mkey).unwrap()).unwrap();
    manifest["epoch"] = serde_json::json!("");
    store
        .put(
            &mkey,
            &serde_json::to_vec(&manifest).unwrap(),
            stratum_store::PutCond::None,
        )
        .unwrap();
    let legacy_pack = pack_of(&clone, &format!("{tip}\n"));
    let (_, out) = receive(
        &server,
        &admin,
        "acme",
        "legacy",
        &push_body(
            &[format!("{legacy_tip} {tip} refs/heads/main")],
            &legacy_pack,
        ),
    );
    assert_ng(&out, "re-ingest before pushing");
}

/// The walk caps become loud fallbacks when exceeded — proven with tiny
/// caps instead of hundred-thousand-object pushes.
#[test]
fn push_walk_caps_fall_back_loudly() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-caps");
    let scratch = Scratch::new("wire-caps");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[("STRATUM_MAX_BFS_VISITS", "1".into())],
    );
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    commit(&server, &admin, "/v1/orgs/acme/repos/app", "a.txt", "one\n");
    let url = server.authed_url(&admin, "acme", "app");
    let clone = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &clone);
    std::fs::write(clone.join("big.txt"), "x\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(&clone, &["commit", "-q", "-m", "over cap"]);
    let err = gitcli::git_expect_err(&clone, &["push", "-q", "origin", "main"]).unwrap();
    assert!(err.contains("too large"), "{err}");

    let scratch2 = Scratch::new("wire-caps2");
    let server2 = spawn_server(
        &bucket.base_url,
        &scratch2,
        &[("STRATUM_MAX_FRONTIER_LOOKUPS", "0".into())],
    );
    let admin2 = server2.bootstrap_org("acme");
    server2.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin2,
        Some(serde_json::json!({ "name": "app" })),
    );
    let rp = "/v1/orgs/acme/repos/app";
    for i in 0..9 {
        commit(
            &server2,
            &admin2,
            rp,
            &format!("f{}.txt", i % 3),
            &format!("v{i}\n"),
        );
    }
    // Compaction gives the repo a locator plane, so frontier existence
    // checks go through lookups — capped at zero they fall back loudly.
    let (st, out) = server2.req("POST", &format!("{rp}/compact"), &admin2, None);
    assert_eq!(st, 200, "{out}");
    let url2 = server2.authed_url(&admin2, "acme", "app");
    let clone2 = scratch2.path().join("clone");
    gitcli::clone_and_fsck(&url2, &clone2);
    std::fs::write(clone2.join("new.txt"), "n\n").unwrap();
    gitcli::git(&clone2, &["add", "-A"]);
    gitcli::git(&clone2, &["commit", "-q", "-m", "needs frontier lookups"]);
    let err = gitcli::git_expect_err(&clone2, &["push", "-q", "origin", "main"]).unwrap();
    assert!(err.contains("precomputed path"), "{err}");
}

/// A mirror of a shallow origin serves shallow clones: shallow-info
/// grafts ride the advert and the product fscks clean — for a plain
/// clone **and** for one that asks for a depth of its own, which takes
/// the deepen path and has to keep echoing the corpus's own grafts
/// rather than dropping them.
#[test]
fn shallow_origin_mirrors_serve_shallow_clones() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-shallow");
    let scratch = Scratch::new("wire-shallow");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");

    // A deep upstream, then a shallow bare "origin" cloned from it.
    let deep = scratch.path().join("deep");
    gitcli::fixture_repo(&deep, 12);
    let shallow = scratch.path().join("shallow.git");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            "--bare",
            "--depth",
            "3",
            &format!("file://{}", deep.display()),
            shallow.to_str().unwrap(),
        ],
    );
    gitcli::git(&shallow, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let origin_url = format!("file://{}", shallow.display());
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/mirrors",
        &admin,
        Some(serde_json::json!({ "name": "shallow", "provider": "generic", "origin": origin_url })),
    );
    assert_eq!(st, 202, "{out}");
    let (st, out) = server.req("POST", "/v1/orgs/acme/mirrors/shallow/sync", &admin, None);
    assert_eq!(st, 200, "{out}");

    // Stock git clones it; the clone carries the graft (shallow file).
    let url = server.authed_url(&admin, "acme", "shallow");
    let dest = scratch.path().join("clone");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, dest.to_str().unwrap()],
    );
    gitcli::fsck(&dest);
    let shallow_file = dest.join(".git/shallow");
    assert!(
        shallow_file.exists()
            && !std::fs::read_to_string(&shallow_file)
                .unwrap()
                .trim()
                .is_empty(),
        "clone of a shallow mirror is itself shallow"
    );

    // And a client that asks for a depth of its own. That request goes
    // down the deepen path, which must still hand over the corpus's own
    // grafts: drop them and `index-pack` refuses the boundary commits
    // whose parents are absent, so the clone fails outright.
    let deep1 = scratch.path().join("clone-depth1");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", "--depth", "1", &url, deep1.to_str().unwrap()],
    );
    gitcli::fsck(&deep1);
    assert!(
        deep1.join(".git/shallow").exists(),
        "a depth-1 clone of a shallow mirror lost the grafts"
    );
}

/// Big-object streaming exercises full sideband chunks, and the empty
/// repo's receive advert exposes the capabilities placeholder.
#[test]
fn sideband_chunking_and_empty_receive_advert() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-band");
    let scratch = Scratch::new("wire-band");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );

    // Empty repo: receive advert carries the capabilities^{} placeholder.
    let resp = ureq::get(&format!(
        "{}/acme/app/info/refs?service=git-receive-pack",
        server.base
    ))
    .set("Authorization", &format!("Bearer {admin}"))
    .call()
    .unwrap();
    let mut advert = Vec::new();
    use std::io::Read;
    resp.into_reader().read_to_end(&mut advert).unwrap();
    assert!(
        text(&advert).contains("capabilities^{}"),
        "{}",
        text(&advert)
    );

    // >32KB of incompressible content forces full sideband chunk emits.
    let big: String = (0..40_000u64)
        .map(|i| char::from(b'a' + (i.wrapping_mul(2_654_435_761) % 26) as u8))
        .collect();
    commit(&server, &admin, "/v1/orgs/acme/repos/app", "big.bin", &big);
    let url = server.authed_url(&admin, "acme", "app");
    let dest = scratch.path().join("clone");
    gitcli::clone_and_fsck(&url, &dest);
    assert_eq!(std::fs::read_to_string(dest.join("big.bin")).unwrap(), big);
}

/// A push whose pack bytes are garbage past the header: index-pack in
/// quarantine rejects it and the pusher sees the ng report, not a hang.
#[test]
fn garbage_pack_bytes_are_rejected_in_quarantine() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-garbage");
    let scratch = Scratch::new("wire-garbage");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    let tip = commit(&server, &admin, "/v1/orgs/acme/repos/app", "a.txt", "x\n");

    let mut fake_pack = Vec::new();
    fake_pack.extend_from_slice(b"PACK");
    fake_pack.extend_from_slice(&2u32.to_be_bytes());
    fake_pack.extend_from_slice(&1u32.to_be_bytes());
    fake_pack.extend_from_slice(&[0xde; 64]); // not a valid entry
    let mut body = Vec::new();
    body.extend_from_slice(&pkt(&format!(
        "{ZERO} {BOGUS} refs/heads/junk\0report-status ofs-delta agent=test\n"
    )));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&fake_pack);
    let (_, out) = receive(&server, &admin, "acme", "app", &body);
    let t = text(&out);
    assert!(t.contains("ng "), "{t}");
    // The repo is untouched.
    let (st, log) = server.req("GET", "/v1/orgs/acme/repos/app/log?limit=1", &admin, None);
    assert_eq!(st, 200);
    assert_eq!(log["entries"][0]["commit"].as_str().unwrap(), tip);
}

/// A manifest rewritten to sha256 (a future format) refuses to serve on
/// this build rather than misreading offsets.
#[test]
fn sha256_layouts_refuse_to_serve_on_the_wire() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-sha256");
    let scratch = Scratch::new("wire-sha256");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "future" })),
    );
    commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/future",
        "a.txt",
        "x\n",
    );

    let store =
        stratum_store::ObjectStore::new(&bucket.base_url, stratum_store::LatencyModel::None);
    let (_, repo) = server.req("GET", "/v1/orgs/acme/repos/future", &admin, None);
    let mkey = format!(
        "o/{}/r/{}/prod/manifest.json",
        repo["org_id"].as_str().unwrap(),
        repo["id"].as_str().unwrap()
    );
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&store.get(&mkey).unwrap()).unwrap();
    manifest["object_format"] = serde_json::json!("sha256");
    store
        .put(
            &mkey,
            &serde_json::to_vec(&manifest).unwrap(),
            stratum_store::PutCond::None,
        )
        .unwrap();

    let url = server.authed_url(&admin, "acme", "future");
    let err = gitcli::git_expect_err(
        scratch.path(),
        &["clone", &url, scratch.path().join("nope").to_str().unwrap()],
    )
    .expect("sha256 layout must refuse");
    let _ = err;
    // The REST surface refuses with the same guard.
    let (st, out) = server.req("GET", "/v1/orgs/acme/repos/future/log", &admin, None);
    assert!(st >= 500, "{out}");
}

/// **`git clone --depth 1`, with the real git CLI, on both paths.**
///
/// The server advertises `fetch=shallow` unconditionally and had two
/// answers to a depth-1 clone: a fast path streaming a **precomputed
/// snapshot artifact**, and — for everything else — `return Err(...)`,
/// which the fronts turn into a 500. Nothing had ever driven either with
/// an actual shallow clone; the only other shallow test here is a mirror
/// of an already-shallow origin echoing its grafts.
///
/// The 500 was not an edge case. The snapshot artifact is built by
/// ingest, so a repository created through the API has none until a
/// compaction has actually run, and compaction no-ops below
/// `wal_entries` (8 by default). **Every new repository therefore
/// answered `git clone --depth 1` with HTTP 500 for its entire early
/// life** — which is precisely when somebody points CI at it, and CI
/// clones shallow. It now serves the full clone plan instead: a correct
/// superset, the same trade the non-tip-want branch already makes.
///
/// Three states, because they are three different answers:
///
/// 1. young repo, no artifact — works, unshallow, at the tip;
/// 2. compacted — genuinely shallow, one commit, `.git/shallow` present;
/// 3. WAL moved past the artifact — superset again, and **at the new
///    tip**, which is the interesting bug: serving the stale artifact
///    would hand the client a clean, sound, wrong repository one commit
///    behind, with no error anywhere.
#[test]
fn a_real_depth_one_clone_works_young_compacted_and_stale() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-depth1");
    let scratch = Scratch::new("wire-depth1");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let rp = "/v1/orgs/acme/repos/app";
    let url = server.authed_url(&admin, "acme", "app");

    // --- 1. a young repository, no snapshot artifact ----------------
    for i in 0..3 {
        commit(&server, &admin, rp, "rev.txt", &format!("v{i}\n"));
    }
    let young = scratch.path().join("young");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", "--depth", "1", &url, young.to_str().unwrap()],
    );
    gitcli::fsck(&young);
    assert_eq!(
        std::fs::read_to_string(young.join("rev.txt")).unwrap(),
        "v2\n",
        "a shallow clone of a young repository is not at the tip"
    );

    // --- 2. compacted: the artifact exists, and depth means depth ---
    //
    // Past `wal_entries` (8), so the compaction actually runs rather
    // than reporting NotNeeded and leaving the layout alone.
    for i in 3..12 {
        commit(&server, &admin, rp, "rev.txt", &format!("v{i}\n"));
    }
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");

    let deep = scratch.path().join("deep");
    let head = gitcli::clone_and_fsck(&url, &deep);
    let shallow = scratch.path().join("shallow-fast");
    gitcli::git(
        scratch.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            shallow.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&shallow);
    assert_eq!(
        gitcli::git(&shallow, &["rev-parse", "HEAD"]).trim(),
        head,
        "a depth-1 clone landed on a different commit than the deep one"
    );
    let count = gitcli::git(&shallow, &["rev-list", "--count", "HEAD"]);
    assert_eq!(
        count.trim(),
        "1",
        "the precomputed path did not serve a shallow clone: {} commits",
        count.trim()
    );
    assert!(
        shallow.join(".git/shallow").exists(),
        "the clone is not marked shallow, so the grafts never arrived"
    );
    // A pack that fscks clean and carries the wrong tree is the failure
    // this asserts against — fsck proves soundness, not relevance.
    assert_eq!(
        std::fs::read_to_string(shallow.join("rev.txt")).unwrap(),
        "v11\n",
        "the shallow clone's working tree is not the tip's content"
    );

    // And it can be deepened, which is what somebody does when the
    // shallow checkout turned out not to be enough: the client sends its
    // grafts back as `shallow <oid>` and asks for what is behind them.
    gitcli::git(&shallow, &["fetch", "-q", "--unshallow"]);
    gitcli::fsck(&shallow);
    assert!(
        !shallow.join(".git/shallow").exists(),
        "still marked shallow after --unshallow"
    );
    assert_eq!(
        gitcli::git(&shallow, &["rev-list", "--count", "HEAD"]).trim(),
        gitcli::git(&deep, &["rev-list", "--count", "HEAD"]).trim(),
        "unshallowing did not bring the whole history"
    );

    // --- 3. the WAL has moved past the artifact ---------------------
    commit(&server, &admin, rp, "rev.txt", "v12\n");
    let stale = scratch.path().join("shallow-stale");
    gitcli::git(
        scratch.path(),
        &["clone", "-q", "--depth", "1", &url, stale.to_str().unwrap()],
    );
    gitcli::fsck(&stale);
    assert_eq!(
        std::fs::read_to_string(stale.join("rev.txt")).unwrap(),
        "v12\n",
        "a stale snapshot was served: the clone is behind the tip"
    );

    assert!(server.healthy());
}

/// **Everything a real repository contains that a plain file is not.**
///
/// The suites here push text files and clone them back. A repository
/// somebody actually migrates carries more than that, and each of these
/// is a distinct thing that can be lost between ingest and serving:
///
/// * an **executable bit** — mode `100755`. Lose it and every script in
///   the repository arrives unrunnable, which surfaces as a build that
///   fails on a fresh clone and works for everyone who cloned before.
/// * a **symlink** — mode `120000`, whose blob is the target path.
///   Written back as a regular file it becomes a one-line text file, and
///   nothing errors.
/// * a **submodule** — mode `160000`, a gitlink pointing at a commit
///   this repository does not have. A server that walks trees expecting
///   every entry to be fetchable can refuse the push outright, and a
///   packer that tries to include the target produces a broken pack.
/// * an **annotated tag** — a fourth object type, not just a ref. Every
///   release a project has ever cut is one of these; serve only the
///   lightweight kind and the history is intact and the releases are
///   gone.
/// * a **non-ASCII path**, which git stores as raw bytes.
/// * an **empty file**, whose blob is the empty object — the one every
///   hand-rolled packer forgets.
///
/// None of it was covered. Pushed with the real git CLI and read back
/// out of a real clone, because these are properties of the bytes on the
/// wire and not of any API's idea of a file.
#[test]
fn a_push_keeps_modes_symlinks_gitlinks_and_annotated_tags() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-modes");
    let scratch = Scratch::new("wire-modes");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/app",
        "seed.txt",
        "seed\n",
    );

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);

    // An executable, a symlink, an empty file, a non-ASCII name.
    std::fs::write(work.join("build.sh"), "#!/bin/sh\necho hi\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            work.join("build.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("build.sh", work.join("run")).unwrap();
    }
    std::fs::write(work.join("empty"), "").unwrap();
    std::fs::write(work.join("café.txt"), "unicode path\n").unwrap();

    gitcli::git(&work, &["add", "-A"]);
    // A submodule, written straight into the index: `git submodule add`
    // would need a second repository on disk and a network fetch, and
    // the thing under test is the gitlink entry, not the porcelain.
    //
    // **After** `add -A`, not before: a gitlink has no working-tree file,
    // so `add -A` reads it as deleted and stages its removal — which
    // silently emptied this half of the test.
    let gitlink = "1".repeat(40);
    gitcli::git(
        &work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{gitlink},vendor/lib"),
        ],
    );
    gitcli::git(&work, &["commit", "-q", "-m", "the awkward tree"]);

    gitcli::git(&work, &["push", "-q", "origin", "main"]);

    // Read it back out of a fresh clone.
    let back = scratch.path().join("back");
    gitcli::clone_and_fsck(&url, &back);

    // Modes, straight out of the tree git built.
    let tree = gitcli::git(&back, &["ls-tree", "-r", "HEAD"]);
    let mode_of = |path: &str| -> String {
        tree.lines()
            .find(|l| l.ends_with(path))
            .unwrap_or_else(|| panic!("{path} is missing from the clone:\n{tree}"))
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    };
    #[cfg(unix)]
    {
        assert_eq!(
            mode_of("build.sh"),
            "100755",
            "the executable bit was lost: every script arrives unrunnable"
        );
        assert_eq!(
            mode_of("run"),
            "120000",
            "a symlink came back as a regular file, which is a one-line \
             text file that silently is not a link"
        );
        assert_eq!(
            std::fs::read_link(back.join("run")).unwrap().to_str(),
            Some("build.sh"),
            "the symlink points somewhere else"
        );
    }
    assert_eq!(mode_of("empty"), "100644");
    assert_eq!(
        std::fs::read_to_string(back.join("empty")).unwrap(),
        "",
        "the empty blob did not survive"
    );
    assert_eq!(
        std::fs::read_to_string(back.join("café.txt")).unwrap(),
        "unicode path\n",
        "a non-ASCII path did not survive the round trip"
    );

    // The gitlink is an entry, not a fetch. `ls-tree -r` does not
    // descend into one, so it is listed by `ls-tree` at its own level.
    let top = gitcli::git(&back, &["ls-tree", "-r", "-t", "HEAD"]);
    assert!(
        top.lines()
            .any(|l| l.starts_with("160000 commit") && l.ends_with("vendor/lib")),
        "the submodule entry was dropped or rewritten:\n{top}"
    );

    assert!(server.healthy());
}

/// **Tags and branch deletion, with the real git CLI.**
///
/// Both were refused outright by `receive.rs`, which accepted
/// `refs/heads/*` and nothing else and rejected any deletion. That meant
/// **no project could cut a release here** — `git push --tags` is how
/// every one of them does it — and a branch, once pushed, could never be
/// removed through git. GitHub does both, and a forge that does not is
/// not a place anybody can actually work.
///
/// Both kinds of tag, because they are different objects: a lightweight
/// tag is a ref pointing straight at a commit, and an annotated tag is a
/// fourth object type carrying its own message and author. Serve only
/// the first and every release a project cut arrives stripped of what
/// was written about it.
#[test]
fn tags_push_and_clone_back_and_a_branch_can_be_deleted() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-tags");
    let scratch = Scratch::new("wire-tags");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let rp = "/v1/orgs/acme/repos/app";
    commit(&server, &admin, rp, "seed.txt", "seed\n");

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);
    std::fs::write(work.join("release.txt"), "1.0\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "cut a release"]);
    gitcli::git(&work, &["tag", "v1-light"]);
    gitcli::git(&work, &["tag", "-a", "v1.0", "-m", "first release"]);
    gitcli::git(&work, &["push", "-q", "origin", "main", "--tags"]);

    // Both tags are on the server, and a fresh clone brings them.
    let remote = gitcli::git(&work, &["ls-remote", &url]);
    assert!(remote.contains("refs/tags/v1.0"), "{remote}");
    assert!(remote.contains("refs/tags/v1-light"), "{remote}");

    let back = scratch.path().join("back");
    gitcli::clone_and_fsck(&url, &back);
    let tags = gitcli::git(&back, &["tag", "-l"]);
    assert!(tags.contains("v1-light"), "lightweight tag missing: {tags}");
    assert!(tags.contains("v1.0"), "annotated tag missing: {tags}");

    // The annotated one is still an object of its own, with its message.
    let kind = gitcli::git(&back, &["cat-file", "-t", "v1.0"]);
    assert_eq!(
        kind.trim(),
        "tag",
        "the annotated tag was flattened to a commit, so every release a \
         project cut loses what was written about it"
    );
    assert!(
        gitcli::git(&back, &["cat-file", "-p", "v1.0"]).contains("first release"),
        "the tag object lost its message"
    );
    // …and it resolves to the commit it was cut on.
    assert_eq!(
        gitcli::git(&back, &["rev-list", "-n", "1", "v1.0"]).trim(),
        gitcli::git(&back, &["rev-parse", "HEAD"]).trim()
    );

    // A tag on an *existing* commit sends a pack with nothing new in it,
    // which is its own edge: the push must be accepted, not read as an
    // empty or malformed one.
    gitcli::git(&work, &["tag", "v1.0.1"]);
    gitcli::git(&work, &["push", "-q", "origin", "v1.0.1"]);
    assert!(
        gitcli::git(&work, &["ls-remote", &url]).contains("refs/tags/v1.0.1"),
        "a tag on an existing commit was not accepted"
    );

    // --- deletion ---------------------------------------------------
    gitcli::git(&work, &["push", "-q", "origin", "HEAD:refs/heads/scratch"]);
    assert!(gitcli::git(&work, &["ls-remote", &url]).contains("refs/heads/scratch"));
    gitcli::git(&work, &["push", "-q", "origin", ":refs/heads/scratch"]);
    let remote = gitcli::git(&work, &["ls-remote", &url]);
    assert!(
        !remote.contains("refs/heads/scratch"),
        "the branch survived its deletion:\n{remote}"
    );
    // A tag can go the same way.
    gitcli::git(&work, &["push", "-q", "origin", ":refs/tags/v1.0.1"]);
    assert!(
        !gitcli::git(&work, &["ls-remote", &url]).contains("refs/tags/v1.0.1"),
        "the tag survived its deletion"
    );

    // The repository is unharmed by all of that: still clones, still
    // fscks, still has main and the release tag.
    let after = scratch.path().join("after");
    gitcli::clone_and_fsck(&url, &after);
    assert!(gitcli::git(&after, &["tag", "-l"]).contains("v1.0"));
    assert_eq!(
        std::fs::read_to_string(after.join("release.txt")).unwrap(),
        "1.0\n"
    );

    // **And the tags survive compaction**, which rebuilds the whole
    // layout by re-ingesting a materialized seed. A fold that restored
    // only branches would drop every release a project had ever cut, at
    // a moment nobody is watching — the WAL crossing a threshold — and
    // no clone before it would have noticed.
    for i in 0..10 {
        commit(&server, &admin, rp, "churn.txt", &format!("v{i}\n"));
    }
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");

    let folded = scratch.path().join("folded");
    gitcli::clone_and_fsck(&url, &folded);
    let tags = gitcli::git(&folded, &["tag", "-l"]);
    assert!(
        tags.contains("v1.0") && tags.contains("v1-light"),
        "compaction dropped the tags: {tags}"
    );
    assert_eq!(
        gitcli::git(&folded, &["cat-file", "-t", "v1.0"]).trim(),
        "tag",
        "compaction flattened the annotated tag into its commit"
    );
    assert!(
        gitcli::git(&folded, &["cat-file", "-p", "v1.0"]).contains("first release"),
        "compaction lost the tag object's message"
    );

    assert!(server.healthy());
}

/// The deletions that must still be refused.
///
/// Deleting the branch HEAD points at leaves a repository that every
/// clone reports as empty, and a protected branch is protected against
/// removal for the same reason it is protected against a push — the
/// land queue is the only road to trunk, and deleting trunk is not a
/// way around it.
#[test]
fn deleting_the_default_or_a_protected_branch_is_refused_in_words() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-deldeny");
    let scratch = Scratch::new("wire-deldeny");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let rp = "/v1/orgs/acme/repos/app";
    commit(&server, &admin, rp, "seed.txt", "seed\n");
    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);

    // The default branch.
    let err = gitcli::git_expect_err(&work, &["push", "-q", "origin", ":refs/heads/main"])
        .expect("deleting the default branch should be refused");
    assert!(
        err.contains("default branch"),
        "the refusal does not say why: {err}"
    );

    // A protected one.
    gitcli::git(&work, &["push", "-q", "origin", "HEAD:refs/heads/release"]);
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/protections"),
        &admin,
        Some(serde_json::json!({ "branch": "release" })),
    );
    assert_eq!(st, 201, "{out}");
    let err = gitcli::git_expect_err(&work, &["push", "-q", "origin", ":refs/heads/release"])
        .expect("deleting a protected branch should be refused");
    assert!(
        err.contains("protected") && err.contains("cannot be deleted"),
        "the refusal does not say why, in terms of what was attempted: {err}"
    );

    // Both refusals left the refs alone.
    let remote = gitcli::git(&work, &["ls-remote", &url]);
    assert!(remote.contains("refs/heads/main"), "{remote}");
    assert!(remote.contains("refs/heads/release"), "{remote}");
    assert!(server.healthy());
}

/// Deleting a ref out of a **paged** ref store.
///
/// `ref_pages` is the sharded ref store a repository grows into when its
/// refs outgrow the manifest, and every ref operation has two
/// implementations because of it: one that edits the flat list and one
/// that rewrites a content-addressed page. Only the flat one was
/// exercised by the deletion tests above, because their repositories are
/// small — so half of the deletion path shipped unrun.
///
/// It matters more now than it did yesterday: until tags could be
/// pushed, a repository's ref count was bounded by its branches. A
/// project migrating in with five thousand tags pages immediately.
///
/// `STRATUM_REF_PAGE_MAX=2` forces paging at a size a test can build.
/// The claim is that a deletion out of a page removes exactly the ref
/// asked for — the neighbours in its own page, and the refs in the pages
/// either side, all still resolve. That last part is the reason
/// `refpages::remove` keeps an emptied page instead of splicing it out:
/// `page_index` picks a page by comparing names against `first`, so a
/// hole in the range would strand every ref that used to live in it.
#[test]
fn a_ref_deletes_out_of_a_paged_ref_store_without_stranding_its_neighbours() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-paged-del");
    let scratch = Scratch::new("wire-paged-del");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        // Two knobs, and they are not the same one. `PAGE_SIZE` opts
        // ingest (and so compaction) into building a sharded ref store
        // at all; `PAGE_MAX` is the threshold at which a later write
        // splits a page. Setting only the second builds no pages, and
        // the test then passes without going near the paged code —
        // which is exactly what it did first time round.
        &[
            ("STRATUM_REF_PAGE_SIZE", "2".into()),
            ("STRATUM_REF_PAGE_MAX", "2".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let rp = "/v1/orgs/acme/repos/app";
    commit(&server, &admin, rp, "seed.txt", "seed\n");

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);

    // Enough refs to need several pages at a page max of two, with names
    // that sort across them.
    let names = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];
    for n in &names {
        gitcli::git(
            &work,
            &["push", "-q", "origin", &format!("HEAD:refs/heads/{n}")],
        );
    }
    // Compaction is what builds the page store out of the flat list.
    for i in 0..10 {
        commit(&server, &admin, rp, "churn.txt", &format!("v{i}\n"));
    }
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");

    let before = gitcli::git(&work, &["ls-remote", &url]);
    for n in &names {
        assert!(
            before.contains(&format!("refs/heads/{n}")),
            "{n} is missing before the deletion:\n{before}"
        );
    }

    // Delete one from the middle, where its page has a neighbour and
    // there are pages on both sides of it.
    gitcli::git(&work, &["push", "-q", "origin", ":refs/heads/charlie"]);

    let after = gitcli::git(&work, &["ls-remote", &url]);
    assert!(
        !after.contains("refs/heads/charlie"),
        "the deletion did not take:\n{after}"
    );
    for n in names.iter().filter(|n| **n != "charlie") {
        assert!(
            after.contains(&format!("refs/heads/{n}")),
            "deleting charlie stranded {n} — a page was spliced out and \
             the ranges no longer cover:\n{after}"
        );
    }
    assert!(after.contains("refs/heads/main"), "{after}");

    // A push that **updates and deletes in one command set**, which is
    // what `git push origin main :branch` sends and what a CI cleanup
    // step does every day. The two halves take different branches of the
    // ref transaction and the mixed case had neither.
    // Catch up first: the REST commits above moved trunk, and a stale
    // clone's update half is refused by git itself before anything
    // reaches the server.
    gitcli::git(&work, &["fetch", "-q", "origin"]);
    gitcli::git(&work, &["reset", "-q", "--hard", "origin/main"]);
    std::fs::write(work.join("mixed.txt"), "mixed\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "update and delete at once"]);
    gitcli::git(
        &work,
        &[
            "push",
            "-q",
            "origin",
            "HEAD:refs/heads/main",
            ":refs/heads/echo",
        ],
    );
    let after = gitcli::git(&work, &["ls-remote", &url]);
    assert!(
        !after.contains("refs/heads/echo"),
        "the deletion half of a mixed push was dropped:\n{after}"
    );
    assert!(
        after.contains("refs/heads/main"),
        "the update half of a mixed push was dropped:\n{after}"
    );

    // Deleting its page-neighbour too, so a page genuinely empties.
    gitcli::git(&work, &["push", "-q", "origin", ":refs/heads/delta"]);
    let after = gitcli::git(&work, &["ls-remote", &url]);
    for n in ["alpha", "bravo", "foxtrot", "main"] {
        assert!(
            after.contains(&format!("refs/heads/{n}")),
            "emptying a page stranded {n}:\n{after}"
        );
    }

    // And the repository still serves a sound clone with the refs it has.
    let back = scratch.path().join("back");
    gitcli::clone_and_fsck(&url, &back);
    assert!(server.healthy());
}

/// **`git push --force`, which is how a contributor answers review.**
///
/// Any update whose old tip was not an ancestor of the new one used to be
/// refused — on every branch, protected or not — so a rebased review
/// branch could not be pushed at all. The only remedy was to push under a
/// new name and abandon the old one, which loses the change's history and
/// every comment attached to it. GitHub allows a force push on any branch
/// its protection rules do not cover, and that is what this pins.
///
/// The third case is the one that makes dropping the ancestry proof safe,
/// and it is asserted here rather than argued: a client whose `old` value
/// is not what the ref currently holds is still refused. That is
/// `--force-with-lease` semantics, applied to every push whether it asks
/// for them or not — you may overwrite the history you looked at, and
/// never somebody else's push that landed while you were rebasing. Plain
/// `--force` against GitHub has no such check.
#[test]
fn a_force_push_rewrites_an_unprotected_branch_and_never_somebody_elses_work() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-force");
    let scratch = Scratch::new("wire-force");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let rp = "/v1/orgs/acme/repos/app";
    commit(&server, &admin, rp, "seed.txt", "seed\n");

    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);

    // A review branch with two revisions of the same idea on it.
    gitcli::git(&work, &["checkout", "-q", "-b", "review"]);
    std::fs::write(work.join("feature.txt"), "first attempt\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "first attempt"]);
    gitcli::git(&work, &["push", "-q", "origin", "review"]);
    let first = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Feedback arrives; the author rewrites rather than piling a fixup on
    // top, which is the whole point of a review branch.
    gitcli::git(&work, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(work.join("feature.txt"), "second attempt\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &["commit", "-q", "-m", "second attempt, addressing review"],
    );
    let second = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    assert_ne!(first, second);

    gitcli::git(&work, &["push", "-q", "-f", "origin", "review"]);

    // The branch is the rewritten history, and a fresh clone agrees.
    let remote = gitcli::git(&work, &["ls-remote", &url]);
    assert!(
        remote.contains(&second),
        "the force push did not move the branch:\n{remote}"
    );
    let back = scratch.path().join("back");
    gitcli::clone_and_fsck(&url, &back);
    gitcli::git(&back, &["fetch", "-q", "origin", "review"]);
    gitcli::git(&back, &["checkout", "-q", "-B", "review", "origin/review"]);
    assert_eq!(
        std::fs::read_to_string(back.join("feature.txt")).unwrap(),
        "second attempt\n",
        "the clone still carries the history that was rewritten away"
    );
    assert_eq!(
        gitcli::git(&back, &["rev-list", "--count", "review"]).trim(),
        "2",
        "the rewritten commit is still on the branch"
    );

    // --- and it never takes somebody else's push -------------------
    //
    // A second worktree that is one push behind. Its `old` value is the
    // commit it last saw, which is no longer what the ref holds, so its
    // force push is refused even though it asked for one.
    let other = scratch.path().join("other");
    gitcli::clone_and_fsck(&url, &other);
    gitcli::git(&other, &["fetch", "-q", "origin", "review"]);
    gitcli::git(&other, &["checkout", "-q", "-B", "review", "origin/review"]);

    // Meanwhile the first author pushes again.
    std::fs::write(work.join("feature.txt"), "third attempt\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "third"]);
    gitcli::git(&work, &["push", "-q", "-f", "origin", "review"]);
    let third = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // The stale worktree rewrites from where *it* thinks the branch is.
    std::fs::write(other.join("feature.txt"), "clobber\n").unwrap();
    gitcli::git(&other, &["add", "-A"]);
    gitcli::git(&other, &["commit", "-q", "-m", "clobbering"]);
    let err = gitcli::git_expect_err(&other, &["push", "-q", "-f", "origin", "review"])
        .expect("a force push from a stale client should be refused");
    // Refused, and told to fetch first. Which of the two guards catches
    // it is not the claim — the ref precondition and the duplicate-object
    // check both stand between a stale client and somebody else's work,
    // and either arriving first is correct. What matters is that it is
    // refused, that the refusal says what to do, and that the branch is
    // untouched.
    assert!(
        err.contains("fetch") && err.contains("retry") || err.contains("stale old value"),
        "a stale force push was not refused with usable advice: {err}"
    );
    let remote = gitcli::git(&work, &["ls-remote", &url]);
    assert!(
        remote.contains(&third),
        "a stale force push took the branch anyway:\n{remote}"
    );

    // --- a protected branch still refuses one ----------------------
    let (st, out) = server.req(
        "POST",
        &format!("{rp}/protections"),
        &admin,
        Some(serde_json::json!({ "branch": "review" })),
    );
    assert_eq!(st, 201, "{out}");
    gitcli::git(&work, &["fetch", "-q", "origin"]);
    gitcli::git(&work, &["reset", "-q", "--hard", "origin/review"]);
    std::fs::write(work.join("feature.txt"), "past the fence\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "--amend", "-q", "-m", "rewritten"]);
    let err = gitcli::git_expect_err(&work, &["push", "-q", "-f", "origin", "review"])
        .expect("force-pushing a protected branch should be refused");
    assert!(
        err.contains("protected"),
        "a protected branch was rewritten, or refused without saying why: {err}"
    );
    assert!(
        gitcli::git(&work, &["ls-remote", &url]).contains(&third),
        "a refused force push moved the branch anyway"
    );

    assert!(server.healthy());
}

/// **Two real `git push` processes racing for one branch.**
///
/// The CAS is covered by *injection* — `faults_e2e` drives 412s through a
/// proxy and proves the retry loop converges and its budget is bounded.
/// That is the mechanism. What it does not show is the outcome two people
/// actually get, because an injected conflict has no second writer behind
/// it: nobody's work is at stake.
///
/// Here both racers are real, both are pushing a genuine commit on the
/// same base, and the claim is the one that matters to them: **exactly
/// one lands, and the other is told to fetch rather than quietly
/// losing.** A push that reports success and is not in the history is
/// the worst outcome in a version control system — the author has the
/// commit locally, believes it is shared, and finds out days later.
///
/// The loser's retry is asserted too. A refusal that cannot be recovered
/// from by doing the obvious thing is only half an answer.
#[test]
fn two_racing_pushes_produce_exactly_one_winner_and_a_recoverable_loser() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-race");
    let scratch = Scratch::new("wire-race");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/app",
        "seed.txt",
        "seed\n",
    );
    let url = server.authed_url(&admin, "acme", "app");

    // Two clones of the same tip, each with its own commit on top.
    let a = scratch.path().join("racer-a");
    let b = scratch.path().join("racer-b");
    let base = gitcli::clone_and_fsck(&url, &a);
    assert_eq!(gitcli::clone_and_fsck(&url, &b), base);
    for (dir, name) in [(&a, "a"), (&b, "b")] {
        std::fs::write(dir.join(format!("{name}.txt")), format!("from {name}\n")).unwrap();
        gitcli::git(dir, &["add", "-A"]);
        gitcli::git(dir, &["commit", "-q", "-m", &format!("racer {name}")]);
    }
    let tip_a = gitcli::git(&a, &["rev-parse", "HEAD"]).trim().to_string();
    let tip_b = gitcli::git(&b, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(tip_a, tip_b);

    // Released together, so the two receive-packs genuinely overlap.
    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let results: Vec<_> = [(a.clone(), "a"), (b.clone(), "b")]
        .into_iter()
        .map(|(dir, name)| {
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                let out = gitcli::git_expect_err(&dir, &["push", "-q", "origin", "main"]);
                (name, out)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().expect("racer thread"))
        .collect();

    // `git_expect_err` gives Ok(stderr) when the command failed, and Err
    // when it unexpectedly succeeded — so a winner shows up as Err here.
    let winners: Vec<&str> = results
        .iter()
        .filter(|(_, r)| r.is_err())
        .map(|(n, _)| *n)
        .collect();
    let losers: Vec<(&str, String)> = results
        .iter()
        .filter_map(|(n, r)| r.as_ref().ok().map(|e| (*n, e.clone())))
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "expected exactly one push to land; winners={winners:?} losers={losers:?}"
    );

    // The loser was told, in words, and told the right thing.
    let (_, why) = &losers[0];
    assert!(
        why.contains("fetch") || why.contains("stale") || why.contains("rejected"),
        "the losing push did not say what happened: {why}"
    );

    // Trunk is the winner's commit and nobody else's, and the loser's
    // commit is genuinely absent rather than merely unreferenced.
    let expected = if winners[0] == "a" { &tip_a } else { &tip_b };
    let orphan = if winners[0] == "a" { &tip_b } else { &tip_a };
    let check = scratch.path().join("check");
    let head = gitcli::clone_and_fsck(&url, &check);
    assert_eq!(head, *expected, "trunk is not the winner's commit");
    let remote = gitcli::git(&check, &["ls-remote", &url]);
    assert!(
        !remote.contains(orphan.as_str()),
        "the losing push is advertised anyway:\n{remote}"
    );

    // And the loser recovers by doing the obvious thing.
    let loser_dir = if winners[0] == "a" { &b } else { &a };
    gitcli::git(loser_dir, &["fetch", "-q", "origin"]);
    gitcli::git(loser_dir, &["rebase", "-q", "origin/main"]);
    gitcli::git(loser_dir, &["push", "-q", "origin", "main"]);
    let after = scratch.path().join("after");
    gitcli::clone_and_fsck(&url, &after);
    assert_eq!(
        gitcli::git(&after, &["rev-list", "--count", "HEAD"]).trim(),
        "3",
        "the loser's retry did not land on top of the winner"
    );
    assert!(after.join("a.txt").exists() && after.join("b.txt").exists());

    assert!(server.healthy());
}

/// **A deletion racing a push for the same ref.**
///
/// New today: until deletion worked, the only way two writers could
/// contend for a ref was by both moving it forward, and the loser's
/// recovery was to rebase. A deletion is different in kind — one writer
/// is removing the thing the other is building on — and it takes a
/// different path through the engine, because a delete-only push skips
/// the quarantine and the connectivity walk entirely and goes straight
/// to the ref transaction.
///
/// Two orderings, one outcome each, and neither may be "both": either
/// the branch is gone and the push is refused because the ref it named
/// is not there, or the push landed and the deletion is refused because
/// the value it named has moved. What must never happen is a deletion
/// that reports success while the pushed commit survives on the ref, or
/// a push that reports success onto a ref that has been removed — both
/// leave a writer believing something the repository does not agree
/// with.
#[test]
fn a_deletion_racing_a_push_leaves_one_coherent_outcome() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-delrace");
    let scratch = Scratch::new("wire-delrace");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/app",
        "seed.txt",
        "seed\n",
    );
    let url = server.authed_url(&admin, "acme", "app");

    let pusher = scratch.path().join("pusher");
    let deleter = scratch.path().join("deleter");
    gitcli::clone_and_fsck(&url, &pusher);
    gitcli::clone_and_fsck(&url, &deleter);
    gitcli::git(&pusher, &["push", "-q", "origin", "HEAD:refs/heads/topic"]);
    gitcli::git(&pusher, &["fetch", "-q", "origin"]);
    gitcli::git(&deleter, &["fetch", "-q", "origin"]);
    // What both clients believe `topic` holds, and what both will claim
    // as the old value below. Pinning it is what makes this a race with
    // exactly one winner — see the leases on the two pushes.
    let base = gitcli::git(&pusher, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    std::fs::write(pusher.join("more.txt"), "more work\n").unwrap();
    gitcli::git(&pusher, &["add", "-A"]);
    gitcli::git(&pusher, &["commit", "-q", "-m", "more work"]);
    let pushed = gitcli::git(&pusher, &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
    let g1 = gate.clone();
    let p = pusher.clone();
    let lease = base.clone();
    let push = std::thread::spawn(move || {
        g1.wait();
        gitcli::git_expect_err(
            &p,
            &[
                "push",
                "-q",
                &format!("--force-with-lease=refs/heads/topic:{lease}"),
                "origin",
                "HEAD:refs/heads/topic",
            ],
        )
    });
    let g2 = gate.clone();
    let d = deleter.clone();
    let lease_d = base.clone();
    let del = std::thread::spawn(move || {
        g2.wait();
        gitcli::git_expect_err(
            &d,
            &[
                "push",
                "-q",
                &format!("--force-with-lease=refs/heads/topic:{lease_d}"),
                "origin",
                ":refs/heads/topic",
            ],
        )
    });
    let push_failed = push.join().expect("pusher").is_ok();
    let del_failed = del.join().expect("deleter").is_ok();

    // Whatever the interleaving, the repository has to agree with
    // whichever writer was told it succeeded.
    let check = scratch.path().join("check");
    gitcli::clone_and_fsck(&url, &check);
    let remote = gitcli::git(&check, &["ls-remote", &url]);
    let topic_present = remote.contains("refs/heads/topic");

    // Both writers stated the same lease, so the interleaving cannot
    // launder a lost update. Whichever reaches the manifest CAS second
    // finds `topic` holding something other than `base` — the pushed
    // commit, or nothing — and `receive.rs` refuses it: "stale old value,
    // fetch first" when the ref moved, "does not exist here" when it is
    // gone. Exactly one writer can be told yes.
    //
    // The lease is what makes that true, and pinning it is what makes
    // this test honest. Without it git takes the old value from the ref
    // advertisement it just received, so on a loaded runner the deletion
    // could land *completely* before the pusher advertised — git would
    // then send a create (`0{40} <new>`), which is accepted because
    // nothing is being clobbered, and both writers would be correctly
    // told yes. That is a legal outcome, and it failed this test roughly
    // whenever CI was busy: a red build with nothing behind it.
    //
    // Loosening the assertion to admit it was the wrong repair — it also
    // admits the bug. With the push-side lease deleted from `receive.rs`
    // the loosened version still passed, because a clobbering push
    // leaves exactly the state a legitimate re-create leaves. Pinning
    // the lease keeps the strict invariant *and* removes the false
    // failure.
    if !del_failed {
        assert!(
            !topic_present,
            "the deletion reported success and the branch is still there:\n{remote}"
        );
    }
    if !push_failed {
        assert!(
            topic_present && remote.contains(pushed.as_str()),
            "the push reported success and its commit is not on the ref:\n{remote}"
        );
    }
    assert!(
        push_failed || del_failed,
        "a deletion and a push both reported success on one ref, though both \
         claimed the same old value — one of them was told yes after losing:\n{remote}"
    );

    // Trunk is untouched either way, and the server is still serving.
    assert!(remote.contains("refs/heads/main"), "{remote}");
    assert!(server.healthy());
}

/// **A repository with enough refs to page, and deletions on the page
/// boundaries.**
///
/// `refpages::remove` is new and has one unit test and one e2e behind
/// it, and the e2e deletes from the middle of a page. The interesting
/// cases are the edges, because `page_index` picks a page by comparing a
/// name against each page's `first`: removing the **first** entry of a
/// page moves that boundary, removing the **last** moves the other one,
/// and removing both leaves a page holding nothing at all.
///
/// That last one is why `remove` keeps an emptied page rather than
/// splicing it out of `ref_pages`, and the assertion that actually tests
/// the decision is the one at the end: a *new* ref whose name sorts into
/// the emptied page's range still has somewhere to go. Splice the page
/// out and the range it covered is a hole — every name in it resolves to
/// a neighbouring page and reads as absent, which is a silent, partial
/// disappearance of the ref store rather than a clean failure.
///
/// This matters more since tags became pushable: until today a
/// repository's ref count was bounded by its branches, and a project
/// migrating in with a few thousand tags now pages on arrival.
#[test]
fn refs_delete_off_page_boundaries_without_stranding_the_ranges() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-pagescale");
    let scratch = Scratch::new("wire-pagescale");
    let server = spawn_server(
        &bucket.base_url,
        &scratch,
        &[
            ("STRATUM_REF_PAGE_SIZE", "2".into()),
            ("STRATUM_REF_PAGE_MAX", "2".into()),
        ],
    );
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let rp = "/v1/orgs/acme/repos/app";
    commit(&server, &admin, rp, "seed.txt", "seed\n");
    let url = server.authed_url(&admin, "acme", "app");
    let work = scratch.path().join("work");
    gitcli::clone_and_fsck(&url, &work);

    // Enough refs for many pages, with names that sort predictably so
    // the boundaries are known rather than guessed.
    let names: Vec<String> = (0..24).map(|i| format!("br-{i:02}")).collect();
    for n in &names {
        gitcli::git(
            &work,
            &["push", "-q", "origin", &format!("HEAD:refs/heads/{n}")],
        );
    }
    for i in 0..10 {
        commit(&server, &admin, rp, "churn.txt", &format!("v{i}\n"));
    }
    let (st, out) = server.req("POST", &format!("{rp}/compact"), &admin, None);
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["outcome"], "Compacted", "{out}");

    let present = |ls: &str, n: &str| {
        ls.contains(&format!("refs/heads/{n}\n"))
            || ls.lines().any(|l| l.ends_with(&format!("refs/heads/{n}")))
    };
    let before = gitcli::git(&work, &["ls-remote", &url]);
    for n in &names {
        assert!(present(&before, n), "{n} never reached the page store");
    }

    // At a page size of two the pages are [br-00,br-01], [br-02,br-03]…
    // so these are, in order: the first entry of a page, the last entry
    // of a page, and both entries of one page.
    for n in ["br-04", "br-03", "br-06", "br-07"] {
        gitcli::git(
            &work,
            &["push", "-q", "origin", &format!(":refs/heads/{n}")],
        );
    }

    let after = gitcli::git(&work, &["ls-remote", &url]);
    for n in ["br-04", "br-03", "br-06", "br-07"] {
        assert!(!present(&after, n), "{n} survived its deletion:\n{after}");
    }
    for n in names
        .iter()
        .filter(|n| !["br-04", "br-03", "br-06", "br-07"].contains(&n.as_str()))
    {
        assert!(
            present(&after, n),
            "deleting on a page boundary stranded {n} — the page ranges no \
             longer cover:\n{after}"
        );
    }
    assert!(present(&after, "main"), "{after}");

    // The emptied page's range still has an owner: a new ref that sorts
    // into it lands and comes back.
    gitcli::git(&work, &["push", "-q", "origin", "HEAD:refs/heads/br-10a"]);
    let refilled = gitcli::git(&work, &["ls-remote", &url]);
    assert!(
        present(&refilled, "br-10a"),
        "a ref sorting into an emptied page's range had nowhere to go:\n{refilled}"
    );
    // …and it did not displace its neighbours on the way in.
    for n in ["br-02", "br-05"] {
        assert!(
            present(&refilled, n),
            "{n} lost when the page refilled:\n{refilled}"
        );
    }

    // The repository still clones and fscks with the ref store in that
    // state, which is what a reader actually does with it.
    let back = scratch.path().join("back");
    gitcli::clone_and_fsck(&url, &back);
    assert!(server.healthy());
}

/// **A gzipped RPC body, which is how git sends one whenever gzip helps.**
///
/// `post_rpc` in remote-curl.c compresses a smart-HTTP request whenever
/// it buffered the body and gzip actually made it smaller, and sets
/// `Content-Encoding: gzip`. Nothing asks for that and no setting turns
/// it on: it is a property of the content. A handful of refs makes a
/// body too small for gzip to win and it goes over the wire as-is; a
/// couple of dozen `want` lines for the same oid compress enormously and
/// git switches.
///
/// The server did not decode it, so the pkt parser read deflate bytes as
/// pkt-lines and answered `bad pkt len` — HTTP 500 on `git clone`, for
/// any repository with enough refs. It surfaced while testing paged ref
/// stores and looked for a while like a paging bug, because the trigger
/// is a ref count; it has nothing to do with paging.
///
/// Asserted at the transport rather than through `git clone`, so the
/// test names the actual cause: the same body, sent twice, once plain
/// and once gzipped, has to produce the same answer. A clone-shaped test
/// would go red for this reason and read as "clones are broken", which
/// is where the last few hours went.
#[test]
fn a_gzipped_request_body_is_decoded_on_both_wire_routes() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-gzip");
    let scratch = Scratch::new("wire-gzip");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let tip = commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/app",
        "seed.txt",
        "seed\n",
    );

    let plain = v2_fetch_body(&["ofs-delta", &format!("want {tip}"), "done"]);
    let (st_plain, body_plain) = upload(&server, &admin, "acme", "app", &plain);
    assert_eq!(st_plain, 200, "the uncompressed control failed");
    assert!(
        String::from_utf8_lossy(&body_plain).contains("packfile"),
        "the uncompressed control did not serve a pack"
    );

    // The same bytes, gzipped, with the header git sets.
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    use std::io::Write;
    enc.write_all(&plain).expect("gzip");
    let gz = enc.finish().expect("gzip");
    let resp = ureq::post(&format!("{}/acme/app/git-upload-pack", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", "application/x-git-upload-pack-request")
        .set("Content-Encoding", "gzip")
        .send_bytes(&gz);
    let (st_gz, body_gz) = match resp {
        Ok(r) | Err(ureq::Error::Status(_, r)) => {
            let st = r.status();
            let mut buf = Vec::new();
            use std::io::Read;
            let _ = r.into_reader().read_to_end(&mut buf);
            (st, buf)
        }
        Err(e) => panic!("transport: {e}"),
    };
    assert_eq!(
        st_gz,
        200,
        "a gzipped fetch was refused: {}",
        String::from_utf8_lossy(&body_gz)
    );
    assert!(
        String::from_utf8_lossy(&body_gz).contains("packfile"),
        "a gzipped fetch did not serve a pack"
    );

    // Push takes the same road through `post_rpc`, so it decodes too. A
    // deletion-only body needs no pack, which keeps this about the
    // encoding rather than about pack construction.
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos/app/branches",
        &admin,
        Some(serde_json::json!({ "name": "gone", "from": "main" })),
    );
    assert!(st == 200 || st == 201, "{st} {out}");
    let del = push_body_for(&format!("{tip} {ZERO} refs/heads/gone"));
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&del).expect("gzip");
    let gz = enc.finish().expect("gzip");
    let resp = ureq::post(&format!("{}/acme/app/git-receive-pack", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", "application/x-git-receive-pack-request")
        .set("Content-Encoding", "gzip")
        .send_bytes(&gz);
    let (st_push, body_push) = match resp {
        Ok(r) | Err(ureq::Error::Status(_, r)) => {
            let st = r.status();
            let mut buf = Vec::new();
            use std::io::Read;
            let _ = r.into_reader().read_to_end(&mut buf);
            (st, buf)
        }
        Err(e) => panic!("transport: {e}"),
    };
    assert_eq!(
        st_push,
        200,
        "a gzipped push was refused: {}",
        String::from_utf8_lossy(&body_push)
    );
    assert!(
        String::from_utf8_lossy(&body_push).contains("ok refs/heads/gone"),
        "the gzipped push was decoded but not applied: {}",
        String::from_utf8_lossy(&body_push)
    );

    // **A decompression bomb is refused rather than inflated.**
    //
    // Decoding a request body moves the size check downstream of the
    // decompressor: the request limit is enforced on the bytes that
    // arrive, and a few kilobytes of gzip becomes as much as the sender
    // likes on the way out. Without a cap on the decoded side, one small
    // request allocates until the process dies — and it needs no
    // credential beyond the one that may push.
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&vec![b'0'; 200 * 1024 * 1024]).expect("gzip");
    let bomb = enc.finish().expect("gzip");
    assert!(
        bomb.len() < 1024 * 1024,
        "the fixture is not a bomb: {} bytes compressed",
        bomb.len()
    );
    let resp = ureq::post(&format!("{}/acme/app/git-upload-pack", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", "application/x-git-upload-pack-request")
        .set("Content-Encoding", "gzip")
        .send_bytes(&bomb);
    let st_bomb = match resp {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r.status(),
        Err(e) => panic!("transport: {e}"),
    };
    assert_ne!(st_bomb, 200, "a decompression bomb was accepted");

    // And a body that claims to be gzip and is not fails as a refusal
    // rather than as something stranger further in.
    let resp = ureq::post(&format!("{}/acme/app/git-receive-pack", server.base))
        .set("Authorization", &format!("Bearer {admin}"))
        .set("Git-Protocol", "version=2")
        .set("Content-Type", "application/x-git-receive-pack-request")
        .set("Content-Encoding", "gzip")
        .send_bytes(b"this is not gzip at all");
    let st_bad = match resp {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r.status(),
        Err(e) => panic!("transport: {e}"),
    };
    assert_ne!(st_bad, 200, "a body mislabelled as gzip was accepted");

    // The server is still serving after both.
    assert!(server.healthy());
}

/// A body that is not pkt-lines is the client's mistake and answers 400.
///
/// It answered 500: `err_response` mapped every error it did not
/// recognise to a server failure, so a truncated upload-pack — a proxy
/// cutting a request, a hand-rolled client with one length wrong — read
/// as Weft being down. Found writing the checkout action, whose replay of
/// a refused fetch had a pkt length off by one and was told the server
/// had failed. Both shapes: a length that is not hex, and a length the
/// body does not reach.
#[test]
fn a_malformed_upload_pack_body_is_a_400_not_a_500() {
    let minio = Minio::shared();
    let bucket = minio.bucket("wire-malformed");
    let scratch = Scratch::new("wire-malformed");
    let server = spawn_server(&bucket.base_url, &scratch, &[]);
    let admin = server.bootstrap_org("acme");
    let (st, out) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "app" })),
    );
    assert_eq!(st, 201, "{out}");
    let tip = commit(
        &server,
        &admin,
        "/v1/orgs/acme/repos/app",
        "seed.txt",
        "seed\n",
    );

    // A pkt length that is not hex.
    let (st, out) = upload(&server, &admin, "acme", "app", b"zzzzcommand=fetch\n0000");
    assert_eq!(st, 400, "{}", text(&out));
    assert!(text(&out).contains("bad pkt len"), "{}", text(&out));

    // A pkt length the body does not reach: `want` declared at 0x32 bytes
    // with a short oid behind it.
    let mut short = v2_fetch_body(&["ofs-delta"]);
    short.truncate(short.len() - 4);
    short.extend_from_slice(format!("0032want {}\n0000", &tip[..20]).as_bytes());
    let (st, out) = upload(&server, &admin, "acme", "app", &short);
    assert_eq!(st, 400, "{}", text(&out));
    assert!(
        text(&out).contains("failed to fill whole buffer"),
        "{}",
        text(&out)
    );

    // A well-formed fetch still serves, and the server is still up.
    let good = v2_fetch_body(&["ofs-delta", &format!("want {tip}"), "done"]);
    let (st, out) = upload(&server, &admin, "acme", "app", &good);
    assert_eq!(st, 200, "{}", text(&out));
    assert!(text(&out).contains("packfile"), "{}", text(&out));
    assert!(server.healthy());
}
