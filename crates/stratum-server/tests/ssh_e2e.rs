//! End-to-end git-over-SSH: the compiled stratum-server binary with the
//! SSH front door enabled, a real OpenSSH client, and stock git. Every
//! produced clone runs `fsck --full --strict` (I11). Covers the whole
//! product feature: key registration via REST, publickey auth, clone /
//! push / fetch over `ssh://`, mirror semantics, revocation, host-key
//! stability, and the rejection matrix.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::minio::{ROOT_PASSWORD, ROOT_USER};
use stratum_testkit::Minio;

struct Server {
    child: Child,
    base: String,
    ssh_port: u16,
    db_url: String,
    store_url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn server_env(cmd: &mut Command, store_url: &str, db: &str) {
    cmd.env_clear()
        .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STRATUM_STORE_URL", store_url)
        .env("STRATUM_DB_URL", db)
        .env("AWS_ACCESS_KEY_ID", ROOT_USER)
        .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
        .env("AWS_REGION", "us-east-1");
}

/// `ssh-keygen` a real keypair; returns (private path, public line).
fn keygen(dir: &Path, name: &str) -> (PathBuf, String) {
    let priv_path = dir.join(name);
    let out = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&priv_path)
        .stdin(Stdio::null())
        .output()
        .expect("run ssh-keygen (openssh-client must be installed)");
    assert!(
        out.status.success(),
        "ssh-keygen: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pub_line = std::fs::read_to_string(priv_path.with_extension("pub")).unwrap();
    (priv_path, pub_line.trim().to_string())
}

/// Spawn a server with an SSH front door.
///
/// `ssh_port` is `None` for "any free one" and `Some` only where a test
/// needs the *same* port a previous process had — restarting on it is
/// how the host-identity guarantee is proved.
///
/// The port is chosen **inside** the retry closure, and that is the
/// whole point. `free_port` binds `:0`, reads the number and drops the
/// listener, so between that and the server binding it anybody may take
/// it — `spawn_on_free_port` exists because of exactly this hazard on
/// the HTTP port. The SSH port had the same hazard and none of the
/// defence: the closure captured one port from outside, so all eight
/// attempts asked for the port that was already taken and failed
/// identically. It surfaced as two ssh_e2e tests failing under
/// `cargo llvm-cov` and passing under `cargo test` minutes earlier,
/// which reads as flake and is not: more binaries running at once simply
/// makes losing the race likelier. A retry that retries the same losing
/// number is not a retry.
fn spawn_server(
    scratch: &Scratch,
    store_url: &str,
    db_url: &str,
    host_key_pem: &str,
    ssh_port: Option<u16>,
) -> Server {
    spawn_server_with(scratch, store_url, db_url, host_key_pem, ssh_port, &[])
}

/// [`spawn_server`], plus environment the test wants the server to see
/// — a payment provider, say.
fn spawn_server_with(
    scratch: &Scratch,
    store_url: &str,
    db_url: &str,
    host_key_pem: &str,
    ssh_port: Option<u16>,
    extra_env: &[(&str, String)],
) -> Server {
    let chosen = std::cell::Cell::new(0u16);
    let (child, bind) = stratum_testkit::server::spawn_on_free_port(|bind| {
        let ssh_port = ssh_port.unwrap_or_else(free_port);
        chosen.set(ssh_port);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        server_env(&mut cmd, store_url, db_url);
        cmd.env("STRATUM_BIND", bind)
            .env("STRATUM_SSH_BIND", format!("127.0.0.1:{ssh_port}"))
            .env("STRATUM_SSH_HOST_KEY", host_key_pem)
            .env(
                "STRATUM_SSH_PUBLIC_URL",
                format!("ssh://git@127.0.0.1:{ssh_port}"),
            )
            .env("STRATUM_DATA_DIR", scratch.path().join("data"));
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd
    });
    let s = Server {
        child,
        base: format!("http://{bind}"),
        ssh_port: chosen.get(),
        db_url: db_url.to_string(),
        store_url: store_url.to_string(),
    };
    s
}

impl Server {
    fn bootstrap_org(&self, org: &str) -> (String, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        server_env(&mut cmd, &self.store_url, &self.db_url);
        let out = cmd
            .args(["admin", "bootstrap", "--org", org])
            .output()
            .expect("run admin bootstrap");
        assert!(
            out.status.success(),
            "bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let token = v["admin_token"].as_str().unwrap().to_string();
        // Token shape: weft_<id>_<secret> — the id is what keys bind to.
        let id = token.split('_').nth(1).unwrap().to_string();
        (token, id)
    }

    fn post(&self, path: &str, token: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let resp = ureq::post(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        flatten(resp)
    }

    fn get_json(&self, path: &str, token: &str) -> (u16, serde_json::Value) {
        flatten(
            ureq::get(&format!("{}{path}", self.base))
                .set("Authorization", &format!("Bearer {token}"))
                .call(),
        )
    }

    fn delete(&self, path: &str, token: &str) -> u16 {
        match ureq::delete(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(e) => panic!("transport: {e}"),
        }
    }

    /// Run a `stratum-server admin …` subcommand against this server's
    /// database, returning its stdout.
    /// An admin command expected to fail; returns its stderr.
    fn admin_err(&self, args: &[&str]) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        server_env(&mut cmd, &self.store_url, &self.db_url);
        let out = cmd.args(args).output().expect("run admin command");
        assert!(
            !out.status.success(),
            "admin {args:?} unexpectedly succeeded"
        );
        String::from_utf8_lossy(&out.stderr).to_string()
    }

    fn admin(&self, args: &[&str]) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        server_env(&mut cmd, &self.store_url, &self.db_url);
        let out = cmd.args(args).output().expect("run admin command");
        assert!(
            out.status.success(),
            "admin {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Sign in as a person and return their session cookie — the way the
    /// dashboard reaches the API, and the only way to act *as* somebody
    /// when they hold no token yet.
    fn login(&self, email: &str, password: &str) -> String {
        let resp = ureq::post(&format!("{}/v1/auth/login", self.base))
            .set("Content-Type", "application/json")
            .send_string(&serde_json::json!({ "email": email, "password": password }).to_string())
            .expect("sign in");
        resp.header("set-cookie")
            .and_then(|c| c.split(';').next())
            .expect("a session cookie")
            .to_string()
    }

    fn post_as(
        &self,
        path: &str,
        cookie: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        flatten(
            ureq::post(&format!("{}{path}", self.base))
                .set("Cookie", cookie)
                .set("Content-Type", "application/json")
                .send_string(&body.to_string()),
        )
    }

    fn ssh_url(&self, org: &str, repo: &str) -> String {
        format!("ssh://git@127.0.0.1:{}/{org}/{repo}.git", self.ssh_port)
    }
}

fn flatten(resp: Result<ureq::Response, ureq::Error>) -> (u16, serde_json::Value) {
    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.into_string().unwrap_or_default();
            (
                status,
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
            )
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            (
                code,
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
            )
        }
        Err(e) => panic!("transport: {e}"),
    }
}

/// The ssh invocation git runs, hermetic: no user/system config, no
/// agent, explicit identity and known_hosts.
fn ssh_command(key: &Path, known_hosts: &Path, strict: &str) -> String {
    format!(
        "ssh -F none -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none \
         -o StrictHostKeyChecking={strict} -o UserKnownHostsFile={} -i {}",
        known_hosts.display(),
        key.display()
    )
}

/// Run git with core.sshCommand injected (gitcli scrubs the environment,
/// so the ssh wiring travels as config, not env).
fn git_ssh(cwd: &Path, ssh_cmd: &str, args: &[&str]) -> String {
    let cfg = format!("core.sshCommand={ssh_cmd}");
    let mut full = vec!["-c", cfg.as_str()];
    full.extend_from_slice(args);
    gitcli::git(cwd, &full)
}

fn git_ssh_expect_err(cwd: &Path, ssh_cmd: &str, args: &[&str]) -> String {
    let cfg = format!("core.sshCommand={ssh_cmd}");
    let mut full = vec!["-c", cfg.as_str()];
    full.extend_from_slice(args);
    gitcli::git_expect_err(cwd, &full).unwrap()
}

#[test]
fn ssh_clone_push_fetch_roundtrip() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-roundtrip");
    let scratch = Scratch::new("ssh-rt");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-rt");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);
    let (admin, admin_id) = server.bootstrap_org("acme");

    // Repo API advertises both transports.
    let (status, repo) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201, "{repo}");
    assert_eq!(
        repo["ssh_clone_url"].as_str().unwrap(),
        server.ssh_url("acme", "app")
    );

    // Register a real keypair against the admin token; the server's
    // stored fingerprint must agree with `ssh-keygen -lf`.
    let (key, pub_line) = keygen(scratch.path(), "dev-key");
    let (status, added) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": pub_line, "token_id": admin_id, "label": "laptop" }),
    );
    assert_eq!(status, 201, "{added}");
    let keygen_fp = Command::new("ssh-keygen")
        .arg("-lf")
        .arg(key.with_extension("pub"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&keygen_fp.stdout)
        .contains(added["fingerprint_sha256"].as_str().unwrap()));

    // Clone the empty repo over ssh, commit, push over ssh.
    let known_hosts = scratch.path().join("known_hosts");
    let ssh = ssh_command(&key, &known_hosts, "accept-new");
    let url = server.ssh_url("acme", "app");
    let work = scratch.path().join("work");
    git_ssh(
        scratch.path(),
        &ssh,
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    std::fs::write(work.join("hello.txt"), "hello over ssh\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "first commit over ssh"]);
    let tip = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    git_ssh(&work, &ssh, &["push", "-q", "origin", "main"]);

    // A fresh ssh clone sees it, fsck-clean (I11), and an incremental
    // fetch after a second push exercises the fetch (haves>0) path.
    let verify = scratch.path().join("verify");
    git_ssh(
        scratch.path(),
        &ssh,
        &["clone", "-q", &url, verify.to_str().unwrap()],
    );
    gitcli::fsck(&verify);
    assert_eq!(
        gitcli::git(&verify, &["rev-parse", "HEAD"]).trim(),
        tip.as_str()
    );
    std::fs::write(work.join("more.txt"), "second\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "second"]);
    let tip2 = gitcli::git(&work, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    git_ssh(&work, &ssh, &["push", "-q", "origin", "main"]);
    git_ssh(&verify, &ssh, &["pull", "-q", "origin", "main"]);
    assert_eq!(
        gitcli::git(&verify, &["rev-parse", "HEAD"]).trim(),
        tip2.as_str()
    );

    // A push with nothing new exchanges only the advertisement.
    git_ssh(&work, &ssh, &["push", "-q", "origin", "main"]);

    // The 64 MiB request cap holds on this transport too: an
    // incompressible oversized pack is refused in-band, not buffered.
    let mut blob = Vec::with_capacity(70 << 20);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    while blob.len() < 70 << 20 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        blob.extend_from_slice(&x.to_le_bytes());
    }
    std::fs::write(work.join("huge.bin"), &blob).unwrap();
    drop(blob);
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "huge"]);
    let err = git_ssh_expect_err(&work, &ssh, &["push", "-q", "origin", "main"]);
    assert!(err.contains("64 MiB"), "{err}");
    gitcli::git(&work, &["reset", "-q", "--hard", "HEAD~1"]);

    // The control plane saw the ssh pushes exactly like http ones.
    let (status, log) = server.get_json("/v1/orgs/acme/repos/app/log?limit=5", &admin);
    assert_eq!(status, 200);
    assert!(log.to_string().contains(&tip2));
    let (_, audit) = server.get_json("/v1/orgs/acme/audit", &admin);
    assert!(audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["action"] == "repo.push"));

    // HTTP and SSH serve the same repo: an http clone matches.
    let http_url = format!(
        "http://x:{admin}@{}/acme/app.git",
        server.base.strip_prefix("http://").unwrap()
    );
    let cross = scratch.path().join("cross");
    assert_eq!(gitcli::clone_and_fsck(&http_url, &cross), tip2);
}

#[test]
fn ssh_rejection_matrix_and_revocation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-matrix");
    let scratch = Scratch::new("ssh-mx");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-mx");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);
    let (admin, admin_id) = server.bootstrap_org("acme");
    server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    let known_hosts = scratch.path().join("known_hosts");

    // An unregistered key never authenticates.
    let (stranger, _) = keygen(scratch.path(), "stranger");
    let ssh_stranger = ssh_command(&stranger, &known_hosts, "accept-new");
    let err = git_ssh_expect_err(
        scratch.path(),
        &ssh_stranger,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "app"),
            scratch.path().join("nope").to_str().unwrap(),
        ],
    );
    assert!(err.contains("Permission denied"), "{err}");

    // A read-scoped key can clone; its push is refused in words.
    let (status, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &admin,
        serde_json::json!({ "scopes": ["repo:read"], "repo": "app", "label": "ro" }),
    );
    assert_eq!(status, 201);
    let (ro_key, ro_pub) = keygen(scratch.path(), "ro-key");
    let (status, ro_added) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": ro_pub, "token_id": minted["id"], "label": "ro" }),
    );
    assert_eq!(status, 201, "{ro_added}");
    let ssh_ro = ssh_command(&ro_key, &known_hosts, "accept-new");
    let ro_clone = scratch.path().join("ro-clone");
    git_ssh(
        scratch.path(),
        &ssh_ro,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "app"),
            ro_clone.to_str().unwrap(),
        ],
    );
    gitcli::git(&ro_clone, &["checkout", "-q", "-b", "main"]);
    std::fs::write(ro_clone.join("x.txt"), "x\n").unwrap();
    gitcli::git(&ro_clone, &["add", "-A"]);
    gitcli::git(&ro_clone, &["commit", "-q", "-m", "attempt"]);
    // It just cloned the repository, so "not found" would be a lie it can
    // see through; the refusal names what a reader can do instead.
    let err = git_ssh_expect_err(&ro_clone, &ssh_ro, &["push", "-q", "origin", "main"]);
    assert!(
        err.contains("you can read acme/app but not push to it"),
        "{err}"
    );
    assert!(!err.contains("repository not found"), "{err}");

    // A repo the org doesn't have: masked (existence masking).
    let err = git_ssh_expect_err(
        scratch.path(),
        &ssh_ro,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "ghost"),
            scratch.path().join("ghost").to_str().unwrap(),
        ],
    );
    assert!(err.contains("repository not found"), "{err}");

    // Key revocation via REST cuts access on the next connection.
    assert_eq!(
        server.delete(
            &format!(
                "/v1/orgs/acme/ssh-keys/{}",
                ro_added["id"].as_str().unwrap()
            ),
            &admin
        ),
        204
    );
    let err = git_ssh_expect_err(
        scratch.path(),
        &ssh_ro,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "app"),
            scratch.path().join("revoked").to_str().unwrap(),
        ],
    );
    assert!(err.contains("Permission denied"), "{err}");
    let (_, listing) = server.get_json("/v1/orgs/acme/ssh-keys", &admin);
    let listed = &listing["keys"].as_array().unwrap();
    let ro_row = listed
        .iter()
        .find(|k| k["id"] == ro_added["id"])
        .expect("revoked key still listed");
    assert!(ro_row["revoked_at"].is_i64());

    // Register the admin key for protocol/exec probes.
    let (key, pub_line) = keygen(scratch.path(), "admin-key");
    let (status, _) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": pub_line, "token_id": admin_id }),
    );
    assert_eq!(status, 201);
    let ssh = ssh_command(&key, &known_hosts, "accept-new");

    // A v0/v1 client is refused loudly, in-band (same gate as HTTP).
    let err = git_ssh_expect_err(
        scratch.path(),
        &ssh,
        &[
            "-c",
            "protocol.version=0",
            "clone",
            "-q",
            &server.ssh_url("acme", "app"),
            scratch.path().join("v0").to_str().unwrap(),
        ],
    );
    assert!(err.contains("protocol v2 required"), "{err}");

    // This is a git host, nothing else: ptys, shells, subsystems, and
    // other exec commands are refused. (Options precede the destination;
    // words after it become the exec command.)
    let raw_ssh = |pre: &[&str], cmd: &[&str]| {
        let mut c = Command::new("ssh");
        c.args([
            "-F",
            "none",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "IdentityAgent=none",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]);
        c.arg("-o")
            .arg(format!("UserKnownHostsFile={}", known_hosts.display()));
        c.arg("-i").arg(&key);
        c.args(pre);
        c.args(["-p", &server.ssh_port.to_string(), "git@127.0.0.1"]);
        c.args(cmd);
        c.output().unwrap()
    };
    assert!(
        !raw_ssh(&["-T"], &[]).status.success(),
        "shell must be refused"
    );
    assert!(
        !raw_ssh(&["-tt"], &[]).status.success(),
        "pty + shell must be refused"
    );
    assert!(
        !raw_ssh(&["-s"], &["sftp"]).status.success(),
        "subsystems must be refused"
    );
    assert!(
        !raw_ssh(&[], &["ls -la /"]).status.success(),
        "non-git exec must be refused"
    );

    // REST edge taxonomy for the key-management API.
    let (status, _) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": pub_line, "token_id": admin_id }),
    );
    assert_eq!(status, 409, "duplicate active fingerprint");
    let (status, body) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": "ssh-ed25519 !!!garbage", "token_id": admin_id }),
    );
    assert_eq!(status, 400, "{body}");
    let (fresh_key, fresh_pub) = keygen(scratch.path(), "fresh");
    drop(fresh_key);
    let (status, body) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": fresh_pub, "token_id": "no-such-token" }),
    );
    assert_eq!(status, 400, "{body}");
    // Existence masking on the management endpoints: unknown org and
    // non-admin credentials both answer 404.
    let (status, _) = server.get_json("/v1/orgs/ghost/ssh-keys", &admin);
    assert_eq!(status, 404);
    let (status, _) = server.post(
        "/v1/orgs/ghost/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": "x", "token_id": "y" }),
    );
    assert_eq!(status, 404);
    assert_eq!(server.delete("/v1/orgs/ghost/ssh-keys/any", &admin), 404);
    let ro_token = minted["token"].as_str().unwrap();
    let (status, _) = server.get_json("/v1/orgs/acme/ssh-keys", ro_token);
    assert_eq!(status, 404, "repo-scoped token cannot manage keys");
    let (status, _) = server.post(
        "/v1/orgs/acme/ssh-keys",
        ro_token,
        serde_json::json!({ "public_key": "x", "token_id": "y" }),
    );
    assert_eq!(status, 404);
    assert_eq!(server.delete("/v1/orgs/acme/ssh-keys/any", ro_token), 404);
    assert_eq!(
        server.delete("/v1/orgs/acme/ssh-keys/nonexistent", &admin),
        404,
        "revoking an unknown key id"
    );

    // Booting with the SSH bind but no host key must fail loudly — a
    // per-boot key would look like a MITM to every client.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
    server_env(&mut cmd, &server.store_url, &server.db_url);
    let out = cmd
        .env("STRATUM_BIND", "127.0.0.1:0")
        .env("STRATUM_SSH_BIND", "127.0.0.1:0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("STRATUM_SSH_HOST_KEY"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ssh_host_key_stability_and_mirror_semantics() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-mirror");
    let scratch = Scratch::new("ssh-mir");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-mir");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);
    let (admin, admin_id) = server.bootstrap_org("acme");
    let (key, pub_line) = keygen(scratch.path(), "dev");
    server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": pub_line, "token_id": admin_id }),
    );
    let known_hosts = scratch.path().join("known_hosts");

    // Mirror a file:// origin, then read it over ssh.
    let origins = scratch.path().join("origins");
    let upstream = origins.join("widget");
    let tip = gitcli::fixture_repo(&upstream, 8);
    let bare = origins.join("widget.git");
    gitcli::git(
        &origins,
        &[
            "clone",
            "-q",
            "--bare",
            upstream.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let origin_url = format!("file://{}", bare.display());
    let (status, mirror) = server.post(
        "/v1/orgs/acme/mirrors",
        &admin,
        serde_json::json!({ "name": "widget", "provider": "generic", "origin": origin_url }),
    );
    assert_eq!(status, 202, "{mirror}");
    let (status, sync) = server.post(
        "/v1/orgs/acme/mirrors/widget/sync",
        &admin,
        serde_json::json!({}),
    );
    assert_eq!(status, 200, "{sync}");

    let ssh = ssh_command(&key, &known_hosts, "accept-new");
    let clone = scratch.path().join("mirror-clone");
    git_ssh(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "widget"),
            clone.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&clone);
    assert_eq!(gitcli::git(&clone, &["rev-parse", "HEAD"]).trim(), tip);

    // M4 over ssh, inverted: the push is forwarded to the origin, which
    // has it before the client hears `ok`, and the mirror serves it.
    std::fs::write(clone.join("through.txt"), "through ssh\n").unwrap();
    gitcli::git(&clone, &["add", "-A"]);
    gitcli::git(
        &clone,
        &["commit", "-q", "-m", "through the mirror over ssh"],
    );
    let pushed = gitcli::git(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    git_ssh(&clone, &ssh, &["push", "-q", "origin", "main"]);
    assert_eq!(
        gitcli::git(&bare, &["rev-parse", "refs/heads/main"]).trim(),
        pushed,
        "the origin took the push first"
    );
    let again = scratch.path().join("mirror-clone-again");
    git_ssh(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "widget"),
            again.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&again);
    assert_eq!(gitcli::git(&again, &["rev-parse", "HEAD"]).trim(), pushed);

    // M2 over ssh: a want the origin never had answers in-band, naming
    // the origin — never a silent stale miss.
    let err = git_ssh_expect_err(
        &clone,
        &ssh,
        &[
            "fetch",
            "-q",
            "origin",
            "1111111111111111111111111111111111111111",
        ],
    );
    assert!(err.contains("mirror"), "{err}");

    // Same env, new process, same port: the host identity is stable, so
    // a strict client (host key pinned by the first connection's
    // accept-new) still connects — the fleet/restart guarantee. The port
    // has to be the one the first process actually took, which is why it
    // is read off the server rather than decided in advance.
    let ssh_port = server.ssh_port;
    drop(server);
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, Some(ssh_port));
    let ssh_strict = ssh_command(&key, &known_hosts, "yes");
    let refresh = scratch.path().join("refresh");
    git_ssh(
        scratch.path(),
        &ssh_strict,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "widget"),
            refresh.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&refresh);
}

/// R8 over SSH: a VALID key that does not own a repo can never see it —
/// cross-org probes and repo-bound tokens crossing repos all answer with
/// the same existence-masking words, for reads and writes alike; a key
/// that may read but not write is told so by name; revoking the BOUND
/// TOKEN (not the key) cuts the key off at the next connection.
///
/// There are no public repositories, so every repository of acme's is
/// what `secret` always was to rival: absent. The half that used to be
/// "a public repository is served to any verified key, and a push to it
/// is refused as a reader's" is now asked of a key that genuinely reads
/// acme — one bound to a read-only acme token.
#[test]
fn ssh_cross_org_isolation_and_token_revocation() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-iso");
    let scratch = Scratch::new("ssh-iso");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-iso");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);
    let (acme_admin, _) = server.bootstrap_org("acme");
    let (rival_admin, rival_id) = server.bootstrap_org("rival");
    let known_hosts = scratch.path().join("known_hosts");

    // acme: one repo with content, one empty, one sibling.
    for body in [
        serde_json::json!({ "name": "secret" }),
        serde_json::json!({ "name": "open" }),
        serde_json::json!({ "name": "sibling" }),
    ] {
        let (status, r) = server.post("/v1/orgs/acme/repos", &acme_admin, body);
        assert_eq!(status, 201, "{r}");
    }
    let (status, c) = server.post(
        "/v1/orgs/acme/repos/secret/commits",
        &acme_admin,
        serde_json::json!({ "message": "s", "operations":
            [{ "op": "put", "path": "s.txt", "content": "classified\n" }] }),
    );
    assert_eq!(status, 201, "{c}");

    // Rival's key is fully valid — for RIVAL's org.
    let (rival_key, rival_pub) = keygen(scratch.path(), "rival-key");
    let (status, _) = server.post(
        "/v1/orgs/rival/ssh-keys",
        &rival_admin,
        serde_json::json!({ "public_key": rival_pub, "token_id": rival_id }),
    );
    assert_eq!(status, 201);
    let ssh_rival = ssh_command(&rival_key, &known_hosts, "accept-new");

    // Reads of another org's repositories are masked, and so is a name
    // the org does not have: the three must be indistinguishable. Before
    // there were only private repositories `open` was served here, to any
    // verified key; now it is as absent to rival as `secret` is.
    for repo in ["secret", "open", "nonexistent"] {
        let err = git_ssh_expect_err(
            scratch.path(),
            &ssh_rival,
            &[
                "clone",
                "-q",
                &server.ssh_url("acme", repo),
                scratch
                    .path()
                    .join(format!("steal-{repo}"))
                    .to_str()
                    .unwrap(),
            ],
        );
        assert!(
            err.contains("repository not found"),
            "cross-org {repo}: {err}"
        );
        // Never a content leak, never a 403 that confirms existence.
        assert!(!err.contains("classified"), "{err}");
        assert!(!err.to_lowercase().contains("denied"), "{err}");
    }

    // Writes are masked identically: push from a local repo straight at
    // the foreign URL (no clone required to attempt it). Not "you can
    // read": rival cannot, and that sentence would confirm the name.
    let attack = scratch.path().join("attack");
    gitcli::fixture_repo(&attack, 1);
    for repo in ["secret", "open", "nonexistent"] {
        let err = git_ssh_expect_err(
            &attack,
            &ssh_rival,
            &["push", "-q", &server.ssh_url("acme", repo), "main"],
        );
        assert!(err.contains("repository not found"), "{repo}: {err}");
        assert!(!err.contains("you can read"), "{repo}: {err}");
    }

    // A key that may read acme and not write it: bound to a read-only
    // acme token. It clones, and its push is refused as what it is — a
    // reader pushing — with the way forward, not with "not found".
    let (status, reader) = server.post(
        "/v1/orgs/acme/tokens",
        &acme_admin,
        serde_json::json!({ "scopes": ["repo:read"], "label": "reader" }),
    );
    assert_eq!(status, 201, "{reader}");
    let (reader_key, reader_pub) = keygen(scratch.path(), "reader-key");
    let (status, out) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &acme_admin,
        serde_json::json!({ "public_key": reader_pub, "token_id": reader["id"] }),
    );
    assert_eq!(status, 201, "{out}");
    let ssh_reader = ssh_command(&reader_key, &known_hosts, "accept-new");
    let read_clone = scratch.path().join("secret-as-reader");
    git_ssh(
        scratch.path(),
        &ssh_reader,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "secret"),
            read_clone.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&read_clone);
    let err = git_ssh_expect_err(
        &attack,
        &ssh_reader,
        &["push", "-q", &server.ssh_url("acme", "open"), "main"],
    );
    assert!(
        err.contains("you can read acme/open but not push to it"),
        "{err}"
    );
    // Nobody's push landed.
    let (status, branches) = server.get_json("/v1/orgs/acme/repos/open/branches", &acme_admin);
    assert_eq!(status, 200, "{branches}");
    assert!(
        branches["branches"]
            .as_array()
            .is_some_and(|b| b.is_empty()),
        "a refused push landed on acme/open: {branches}"
    );

    // A repo-bound token's key stays inside its repo: mint a token bound
    // to acme/secret, bind a key, and probe the sibling.
    let (status, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &acme_admin,
        serde_json::json!({ "scopes": ["repo:write"], "repo": "secret", "label": "bound" }),
    );
    assert_eq!(status, 201, "{minted}");
    let (bound_key, bound_pub) = keygen(scratch.path(), "bound-key");
    let (status, _) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &acme_admin,
        serde_json::json!({ "public_key": bound_pub, "token_id": minted["id"] }),
    );
    assert_eq!(status, 201);
    let ssh_bound = ssh_command(&bound_key, &known_hosts, "accept-new");
    let mine = scratch.path().join("mine");
    git_ssh(
        scratch.path(),
        &ssh_bound,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "secret"),
            mine.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&mine);
    let err = git_ssh_expect_err(
        scratch.path(),
        &ssh_bound,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "sibling"),
            scratch.path().join("not-mine").to_str().unwrap(),
        ],
    );
    assert!(err.contains("repository not found"), "{err}");

    // Revoke the TOKEN (the key row stays active): the key dies with it
    // on the very next connection — the SSH credential chain re-resolves
    // key → token → principal against the database every time.
    assert_eq!(
        server.delete(
            &format!("/v1/orgs/acme/tokens/{}", minted["id"].as_str().unwrap()),
            &acme_admin
        ),
        204
    );
    let err = git_ssh_expect_err(
        scratch.path(),
        &ssh_bound,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "secret"),
            scratch.path().join("dead").to_str().unwrap(),
        ],
    );
    assert!(err.contains("Permission denied"), "{err}");
}

/// Generate the fleet host key once per test; returns (path, PEM).
fn host_key(scratch: &Scratch) -> (PathBuf, String) {
    let (path, _) = keygen(scratch.path(), "host-key");
    let pem = std::fs::read_to_string(&path).unwrap();
    (path, pem)
}

/// A developer's own key over the real transport.
///
/// A personal key names a *person*, not a token, so its authority is
/// re-resolved from their role on every connection. That is the property
/// worth proving end to end: an administrator changes a role in the
/// dashboard and the next `git push` from that laptop obeys it, with no
/// key to re-issue and nothing cached to expire.
#[test]
fn a_personal_ssh_key_follows_its_owners_role() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-personal");
    let scratch = Scratch::new("ssh-personal");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-personal");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);
    let (admin, _) = server.bootstrap_org("acme");
    for name in ["app", "secret"] {
        let (status, _) = server.post(
            "/v1/orgs/acme/repos",
            &admin,
            serde_json::json!({ "name": name }),
        );
        assert_eq!(status, 201);
    }
    server.admin(&[
        "admin",
        "user-create",
        "--org",
        "acme",
        "--email",
        "dev@acme.test",
        "--name",
        "Dev",
        "--password",
        "a long enough password",
        "--role",
        "member",
    ]);
    let cookie = server.login("dev@acme.test", "a long enough password");
    let user_id = flatten(
        ureq::get(&format!("{}/v1/auth/me", server.base))
            .set("Cookie", &cookie)
            .call(),
    )
    .1["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Registering their own key needs no token id — that was the whole
    // UX defect: a developer should never be asked to paste one.
    let (key, pub_line) = keygen(scratch.path(), "dev-personal");
    let (status, added) = server.post_as(
        "/v1/orgs/acme/ssh-keys",
        &cookie,
        serde_json::json!({ "public_key": pub_line, "label": "laptop" }),
    );
    assert_eq!(status, 201, "{added}");
    assert_eq!(added["user_id"].as_str().unwrap(), user_id);
    assert!(added["token_id"].is_null(), "a personal key names no token");

    let known_hosts = scratch.path().join("known_hosts");
    let ssh = ssh_command(&key, &known_hosts, "accept-new");
    let work = scratch.path().join("work");
    git_ssh(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "app"),
            work.to_str().unwrap(),
        ],
    );
    gitcli::git(&work, &["config", "user.email", "dev@acme.test"]);
    gitcli::git(&work, &["config", "user.name", "Dev"]);
    std::fs::write(work.join("a.txt"), "one\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "first"]);
    git_ssh(
        &work,
        &ssh,
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    );

    // A per-repo grant that holds them down to viewer applies over SSH
    // too — the repo is not known until the exec request, so this is the
    // case a naive implementation gets wrong.
    let (status, _) = server.post(
        "/v1/orgs/acme/repos/app/grants",
        &admin,
        serde_json::json!({ "user_id": user_id, "role": "viewer" }),
    );
    assert_eq!(status, 204);
    std::fs::write(work.join("a.txt"), "two\n").unwrap();
    gitcli::git(&work, &["commit", "-qam", "second"]);
    let err = git_ssh_expect_err(
        &work,
        &ssh,
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    );
    // Refused with the reason: they can still read `app`, so the
    // refusal says so and says what to do, rather than pretending the
    // repository they cloned a minute ago is not there.
    assert!(
        err.contains("you can read acme/app but not push to it"),
        "a viewer grant must refuse the push, and say why: {err}"
    );
    // Raising works the same way. Granting member back on this repo lets
    // the same key push again with nothing reissued — a grant that could
    // only ever take access away would be a one-way ratchet.
    let (status, _) = server.post(
        "/v1/orgs/acme/repos/app/grants",
        &admin,
        serde_json::json!({ "user_id": user_id, "role": "member" }),
    );
    assert_eq!(status, 204);
    git_ssh(
        &work,
        &ssh,
        &["push", "-q", "origin", "HEAD:refs/heads/main"],
    );
    let (status, _) = server.post(
        "/v1/orgs/acme/repos/app/grants",
        &admin,
        serde_json::json!({ "user_id": user_id, "role": "viewer" }),
    );
    assert_eq!(status, 204);

    // …and only on that repo: the grant does not follow them elsewhere.
    git_ssh(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "secret"),
            scratch.path().join("other").to_str().unwrap(),
        ],
    );

    // Reading still works under the grant, so the refusal above was
    // about the scope, not about the key having stopped resolving.
    git_ssh(&work, &ssh, &["fetch", "-q", "origin"]);

    // Offboarding: with the membership gone the key reaches nothing.
    //
    // The refusal now lands when the namespace is named rather than at
    // the public-key stage, and it has to: a personal key names a
    // *person*, who may belong to several namespaces, so "is this
    // person a member?" has no answer until the client says a member of
    // *what*. The answer is the same masked "not found" the HTTP
    // transport gives — which is also strictly less than the old
    // "Permission denied", since that distinguished a known key from an
    // unknown one.
    assert_eq!(
        server.delete(&format!("/v1/orgs/acme/members/{user_id}"), &admin),
        204
    );
    let err = git_ssh_expect_err(&work, &ssh, &["fetch", "-q", "origin"]);
    assert!(
        err.contains("not found"),
        "a removed member's key must reach nothing: {err}"
    );
}

/// Disabling an account stops its SSH key, and everything else it holds.
///
/// Bearer tokens and browser sessions each check `disabled_at` in their
/// own resolver; an SSH key resolves through `members::role_of` alone,
/// which did not — so disabling somebody left their laptop key cloning.
///
/// The check then moved once more, from the role lookup to the key
/// lookup, while a key with no role in a namespace could still read that
/// namespace's public repositories — so "no role anywhere" was not the
/// same thing as "reaches nothing". There are no public repositories
/// now, but the property that move bought is the one worth keeping: the
/// key is not a credential at all, exactly as the token is not —
/// refused at authentication, before any repository is named, so a
/// repository that exists and one that does not answer alike.
#[test]
fn a_disabled_account_reaches_nothing_it_used_to() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-disabled");
    let scratch = Scratch::new("ssh-disabled");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-disabled");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);
    let (admin, _) = server.bootstrap_org("acme");
    let (status, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201);
    let (status, _) = server.post(
        "/v1/orgs/acme/repos/app/commits",
        &admin,
        serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "a.txt", "content": "x"}]
        }),
    );
    assert_eq!(status, 201);
    // A second repository the account may read, never cloned before it
    // is disabled: refused as a key, not as a clone that was open.
    let (status, _) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "open" }),
    );
    assert_eq!(status, 201);
    let (status, _) = server.post(
        "/v1/orgs/acme/repos/open/commits",
        &admin,
        serde_json::json!({
            "message": "seed",
            "operations": [{"op": "put", "path": "README", "content": "x"}]
        }),
    );
    assert_eq!(status, 201);
    server.admin(&[
        "admin",
        "user-create",
        "--org",
        "acme",
        "--email",
        "dev@acme.test",
        "--name",
        "Dev",
        "--password",
        "a long enough password",
        "--role",
        "member",
    ]);
    let cookie = server.login("dev@acme.test", "a long enough password");

    // Their key, and a personal token, both work while the account does.
    let (key, pub_line) = keygen(scratch.path(), "dev-disabled");
    let (status, _) = server.post_as(
        "/v1/orgs/acme/ssh-keys",
        &cookie,
        serde_json::json!({ "public_key": pub_line, "label": "laptop" }),
    );
    assert_eq!(status, 201);
    let (status, minted) = server.post_as(
        "/v1/orgs/acme/tokens",
        &cookie,
        serde_json::json!({ "scopes": ["repo:read"], "label": "own" }),
    );
    assert_eq!(status, 201, "{minted}");
    let personal = minted["token"].as_str().unwrap().to_string();

    let known_hosts = scratch.path().join("known_hosts");
    let ssh = ssh_command(&key, &known_hosts, "accept-new");
    let work = scratch.path().join("work");
    git_ssh(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("acme", "app"),
            work.to_str().unwrap(),
        ],
    );
    assert_eq!(server.get_json("/v1/orgs/acme/repos/app", &personal).0, 200);

    // Disable the account. Nothing is revoked by hand.
    server.admin(&["admin", "user-disable", "--email", "dev@acme.test"]);

    // The key stops being a credential, so the connection is refused
    // before a repository is named — the same place the token is
    // refused (401, below) — rather than getting as far as the namespace
    // and being masked there as "not found".
    let err = git_ssh_expect_err(&work, &ssh, &["fetch", "-q", "origin"]);
    assert!(
        err.contains("Permission denied"),
        "a disabled account's ssh key still authenticated: {err}"
    );
    assert!(!err.contains("not found"), "{err}");
    for repo in ["open", "never-was"] {
        let err = git_ssh_expect_err(
            scratch.path(),
            &ssh,
            &["ls-remote", &server.ssh_url("acme", repo)],
        );
        assert!(
            err.contains("Permission denied"),
            "a disabled account's ssh key reached acme/{repo}: {err}"
        );
        assert!(!err.contains("not found"), "{err}");
    }
    // 401: a token owned by a disabled account is an invalid credential,
    // refused before any resource is resolved.
    assert_eq!(
        server.get_json("/v1/orgs/acme/repos/app", &personal).0,
        401,
        "a disabled account's personal token still worked"
    );
    // The admin's own credential is untouched — disabling one person is
    // not an outage.
    assert_eq!(server.get_json("/v1/orgs/acme/repos/app", &admin).0, 200);

    // Disabling somebody who does not exist says so rather than
    // pretending to have done something.
    let err = server.admin_err(&["admin", "user-disable", "--email", "nobody@nowhere.test"]);
    assert!(err.contains("no account"), "{err}");

    // And it is reversible: re-enabling restores exactly what they had,
    // with nothing re-issued.
    server.admin(&["admin", "user-enable", "--email", "dev@acme.test"]);
    git_ssh(&work, &ssh, &["fetch", "-q", "origin"]);
    assert_eq!(
        server.get_json("/v1/orgs/acme/repos/app", &personal).0,
        200,
        "re-enabling must restore the token that was never revoked"
    );
}

/// One laptop key reaches every namespace its owner belongs to.
///
/// This is the defect that shipped with per-user keys: the row carried
/// an org, and the active-fingerprint index is globally unique, so a
/// person in two namespaces registered their key in the first and was
/// refused in the second with "this key is already registered" — able to
/// clone from one of them only. Survivable while orgs were
/// operator-provisioned; unavoidable the moment everyone has their own
/// namespace plus any org they join, which is what every account now
/// arrives with.
#[test]
fn one_personal_key_clones_from_every_namespace_its_owner_belongs_to() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-multiorg");
    let scratch = Scratch::new("ssh-multiorg");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-multiorg");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);

    // Two namespaces, a repo in each, and one person in both. `third`
    // is a namespace they are not in at all.
    let (acme, _) = server.bootstrap_org("acme");
    let (other, _) = server.bootstrap_org("other");
    let (third, _) = server.bootstrap_org("third");
    for (org, token) in [("acme", &acme), ("other", &other)] {
        let (status, body) = server.post(
            &format!("/v1/orgs/{org}/repos"),
            token,
            serde_json::json!({ "name": "app" }),
        );
        assert_eq!(status, 201, "{body}");
        let (status, body) = server.post(
            &format!("/v1/orgs/{org}/repos/app/commits"),
            token,
            serde_json::json!({
                "message": format!("hello from {org}"),
                "operations": [{"op": "put", "path": "who.txt", "content": org}]
            }),
        );
        assert_eq!(status, 201, "{body}");
    }
    server.admin(&[
        "admin",
        "user-create",
        "--org",
        "acme",
        "--email",
        "dev@acme.test",
        "--name",
        "Dev",
        "--password",
        "a long enough password",
        "--role",
        "member",
    ]);
    // The same person, joined to the second namespace as well. Same
    // email, so this reuses the account and only adds the membership.
    server.admin(&[
        "admin",
        "user-create",
        "--org",
        "other",
        "--email",
        "dev@acme.test",
        "--name",
        "Dev",
        "--password",
        "a long enough password",
        "--role",
        "member",
    ]);
    let cookie = server.login("dev@acme.test", "a long enough password");
    let me = flatten(
        ureq::get(&format!("{}/v1/auth/me", server.base))
            .set("Cookie", &cookie)
            .call(),
    )
    .1;
    let orgs: Vec<&str> = me["orgs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert!(orgs.contains(&"acme") && orgs.contains(&"other"), "{me}");

    // One key, registered once.
    let (key, pub_line) = keygen(scratch.path(), "one-laptop");
    let (status, added) = server.post_as(
        "/v1/orgs/acme/ssh-keys",
        &cookie,
        serde_json::json!({ "public_key": pub_line, "label": "laptop" }),
    );
    assert_eq!(status, 201, "{added}");

    // It clones from both namespaces. Before the fix the second failed.
    let known_hosts = scratch.path().join("known_hosts");
    let ssh = ssh_command(&key, &known_hosts, "accept-new");
    for org in ["acme", "other"] {
        let dest = scratch.path().join(format!("clone-{org}"));
        git_ssh(
            scratch.path(),
            &ssh,
            &[
                "clone",
                "-q",
                &server.ssh_url(org, "app"),
                dest.to_str().unwrap(),
            ],
        );
        gitcli::fsck(&dest);
        assert_eq!(
            std::fs::read_to_string(dest.join("who.txt")).unwrap(),
            org,
            "cloned the wrong namespace's repo"
        );
    }

    // The key is one list, not one per namespace — showing a different
    // list per org page would be a lie about what it can reach.
    for org in ["acme", "other"] {
        let (status, listing) = flatten(
            ureq::get(&format!("{}/v1/orgs/{org}/ssh-keys", server.base))
                .set("Cookie", &cookie)
                .call(),
        );
        assert_eq!(status, 200, "{listing}");
        let keys = listing["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1, "on /{org}: {listing}");
        assert_eq!(keys[0]["label"], "laptop");
    }

    // A namespace they are NOT in stays out of reach, and is masked the
    // same way a namespace that does not exist is.
    let (status, body) = server.post(
        "/v1/orgs/third/repos",
        &acme,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 404, "{body}");
    let (status, _) = server.post(
        "/v1/orgs/third/repos",
        &third,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201);
    let denied = git_ssh_expect_err(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("third", "app"),
            scratch.path().join("denied").to_str().unwrap(),
        ],
    );
    assert!(denied.contains("not found"), "{denied}");
    let absent = git_ssh_expect_err(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            &server.ssh_url("ghost", "app"),
            scratch.path().join("absent").to_str().unwrap(),
        ],
    );
    assert!(absent.contains("not found"), "{absent}");

    // An administrator of a namespace this person belongs to sees their
    // key and can revoke it — offboarding has to be able to cut the
    // laptop key they used here. Both broke when personal keys stopped
    // carrying an org: the admin list filtered on `org_id`, so the key
    // vanished from the screen, and the admin revoke matched on it, so
    // the row that *was* shown could not be acted on.
    for org in ["acme", "other"] {
        let (status, listing) = server.get_json(
            &format!("/v1/orgs/{org}/ssh-keys"),
            if org == "acme" { &acme } else { &other },
        );
        assert_eq!(status, 200, "{listing}");
        assert!(
            listing["keys"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k["label"] == "laptop"),
            "an admin of /{org} cannot see a member's key: {listing}"
        );
    }
    // …but an org they are NOT a member of neither sees it nor can touch
    // it. The key is theirs and reaches elsewhere; one org's admin does
    // not get to end their access everywhere.
    let (_, theirs) = server.get_json("/v1/orgs/third/ssh-keys", &third);
    assert!(
        theirs["keys"].as_array().unwrap().is_empty(),
        "a stranger org can see the key: {theirs}"
    );

    // Revoking it revokes it everywhere — one key, one decision.
    let (_, listing) = flatten(
        ureq::get(&format!("{}/v1/orgs/acme/ssh-keys", server.base))
            .set("Cookie", &cookie)
            .call(),
    );
    let key_id = listing["keys"][0]["id"].as_str().unwrap().to_string();
    let status = flatten(
        ureq::delete(&format!("{}/v1/orgs/acme/ssh-keys/{key_id}", server.base))
            .set("Cookie", &cookie)
            .call(),
    )
    .0;
    assert_eq!(status, 204);
    for org in ["acme", "other"] {
        let err = git_ssh_expect_err(
            scratch.path(),
            &ssh,
            &[
                "clone",
                "-q",
                &server.ssh_url(org, "app"),
                scratch
                    .path()
                    .join(format!("after-revoke-{org}"))
                    .to_str()
                    .unwrap(),
            ],
        );
        assert!(!err.is_empty(), "a revoked key still cloned from {org}");
    }
    // Every attack case ends by proving the server is still serving.
    assert_eq!(server.get_json("/v1/orgs/acme/repos", &acme).0, 200);
}

/// The SSH front door quotes the same protection sentence as HTTP: a
/// protected branch refuses `git push` in-band, the refusal costs the
/// server nothing, and unprotecting restores the path.
#[test]
fn ssh_pushes_to_a_protected_branch_are_refused_in_band() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-protect");
    let scratch = Scratch::new("ssh-protect");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-protect");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);
    let (admin, admin_id) = server.bootstrap_org("acme");
    let (status, repo) = server.post(
        "/v1/orgs/acme/repos",
        &admin,
        serde_json::json!({ "name": "app" }),
    );
    assert_eq!(status, 201, "{repo}");
    let (key, pub_line) = keygen(scratch.path(), "dev-key");
    let (status, _) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": pub_line, "token_id": admin_id, "label": "laptop" }),
    );
    assert_eq!(status, 201);
    let known_hosts = scratch.path().join("known_hosts");
    let ssh = ssh_command(&key, &known_hosts, "accept-new");
    let url = server.ssh_url("acme", "app");
    let work = scratch.path().join("work");
    git_ssh(
        scratch.path(),
        &ssh,
        &["clone", "-q", &url, work.to_str().unwrap()],
    );
    gitcli::git(&work, &["checkout", "-q", "-b", "main"]);
    std::fs::write(work.join("hello.txt"), "hello\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "seed"]);
    git_ssh(&work, &ssh, &["push", "-q", "origin", "main"]);

    // Fence up.
    let (status, out) = server.post(
        "/v1/orgs/acme/repos/app/protections",
        &admin,
        serde_json::json!({ "branch": "main" }),
    );
    assert_eq!(status, 201, "{out}");

    // Distinct side-branch work first: protection fences one branch,
    // not the repo. (The side branch carries its own commit — the push
    // engine proves fast-forward from the pushed objects alone, so two
    // branches must not alias one commit here.)
    gitcli::git(&work, &["checkout", "-q", "-b", "side"]);
    std::fs::write(work.join("side.txt"), "side work\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "side work"]);
    git_ssh(&work, &ssh, &["push", "-q", "origin", "side"]);

    gitcli::git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("direct.txt"), "straight to trunk\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(&work, &["commit", "-q", "-m", "direct"]);
    let err = git_ssh_expect_err(&work, &ssh, &["push", "-q", "origin", "main"]);
    assert!(
        err.contains("branch 'main' is protected: land through review"),
        "{err}"
    );
    assert!(err.contains("remote rejected"), "{err}");

    // **And it cannot be deleted**, which is the other half of a fence
    // and the half that was missing entirely: protecting a branch
    // against being *moved* while leaving it deletable protects nothing,
    // because removing trunk is the shortest way around the land queue.
    // Asserted on this front as well as HTTP — two doors reach the same
    // engine, and a rule enforced at one of them is a rule with a way
    // round it.
    // Fenced on `side` rather than `main`: `main` is also the default
    // branch, and that rule fires first, so testing there would prove
    // the wrong guard.
    let (status, out) = server.post(
        "/v1/orgs/acme/repos/app/protections",
        &admin,
        serde_json::json!({ "branch": "side" }),
    );
    assert_eq!(status, 201, "{out}");
    let err = git_ssh_expect_err(&work, &ssh, &["push", "-q", "origin", ":refs/heads/side"]);
    assert!(
        err.contains("protected") && err.contains("cannot be deleted"),
        "{err}"
    );
    // Deleting the default branch is refused too, for its own reason.
    let err = git_ssh_expect_err(&work, &ssh, &["push", "-q", "origin", ":refs/heads/main"]);
    assert!(err.contains("default branch"), "{err}");

    // Fence down on `side` and it deletes normally — so what is being
    // tested is the protection, not a front that refuses every deletion.
    // (This is also the regression test for the deadlock: a delete-only
    // push sends no pack and does not close its side, and reading to EOF
    // here used to hang until the connection dropped ten minutes later.)
    let del = ureq::delete(&format!(
        "{}/v1/orgs/acme/repos/app/protections/side",
        server.base
    ))
    .set("Authorization", &format!("Bearer {admin}"))
    .call();
    assert!(del.is_ok(), "{del:?}");
    git_ssh(&work, &ssh, &["push", "-q", "origin", ":refs/heads/side"]);
    let refs = git_ssh(&work, &ssh, &["ls-remote", &url]);
    assert!(!refs.contains("refs/heads/side"), "{refs}");
    assert!(refs.contains("refs/heads/main"), "{refs}");

    // Fence down: the same push now lands, and the clone stays sound.
    let del = ureq::delete(&format!(
        "{}/v1/orgs/acme/repos/app/protections/main",
        server.base
    ))
    .set("Authorization", &format!("Bearer {admin}"))
    .call();
    assert!(del.is_ok(), "{del:?}");
    git_ssh(&work, &ssh, &["push", "-q", "origin", "main"]);
    let verify = scratch.path().join("verify");
    git_ssh(
        scratch.path(),
        &ssh,
        &["clone", "-q", &url, verify.to_str().unwrap()],
    );
    gitcli::fsck(&verify);
}

/// A repository with a trunk, a `feature` branch carrying one change
/// `key`, and that change opened. Returns the change's tip.
fn ssh_repo_with_change(server: &Server, admin: &str, repo: &str, key: &str) -> String {
    let (st, out) = server.post(
        "/v1/orgs/acme/repos",
        admin,
        serde_json::json!({ "name": repo }),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        admin,
        serde_json::json!({ "branch": "main", "message": "trunk", "operations":
            [{ "op": "put", "path": "readme", "content": "v1\n" }] }),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/branches"),
        admin,
        serde_json::json!({ "name": "feature", "from": "main" }),
    );
    assert_eq!(st, 201, "{out}");
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/commits"),
        admin,
        serde_json::json!({ "branch": "feature",
            "message": format!("change {repo}\n\nChange-Id: {key}\n"), "operations":
            [{ "op": "put", "path": "feature.txt", "content": format!("{repo} over ssh\n") }] }),
    );
    assert_eq!(st, 201, "{out}");
    let sha = out["commit"].as_str().unwrap().to_string();
    let (st, out) = server.post(
        &format!("/v1/orgs/acme/repos/{repo}/changes"),
        admin,
        serde_json::json!({ "from": "feature" }),
    );
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["patchset"]["commit"], sha, "{out}");
    sha
}

/// The changeset workspace over SSH: the view advertises an SSH clone
/// URL, `git clone --recurse-submodules` over it checks every member out
/// at its proposed head — the members fetched over SSH too, because the
/// submodule URLs are relative — a push is refused in-band with a
/// direction, and an identity that may not read every member gets the
/// answer a missing repository gives.
#[test]
fn a_changeset_workspace_clones_over_ssh_and_is_read_only_there_too() {
    let minio = Minio::shared();
    let bucket = minio.bucket("ssh-ws");
    let scratch = Scratch::new("ssh-ws");
    let (_, host_pem) = host_key(&scratch);
    let db = stratum_testkit::pg::test_db_url("ssh-ws");
    let server = spawn_server(&scratch, &bucket.base_url, &db, &host_pem, None);
    let (admin, admin_id) = server.bootstrap_org("acme");
    let known_hosts = scratch.path().join("known_hosts");

    let api = ssh_repo_with_change(&server, &admin, "api", "Iaa000001");
    let web = ssh_repo_with_change(&server, &admin, "web", "Ibb000002");
    let (st, out) = server.post(
        "/v1/orgs/acme/changesets",
        &admin,
        serde_json::json!({ "key": "Ic5000001", "title": "over ssh", "members":
            [{ "repo": "api", "change": "Iaa000001" }, { "repo": "web", "change": "Ibb000002" }] }),
    );
    assert_eq!(st, 201, "{out}");

    // The view names the SSH front door for the workspace and for every
    // member, in the same shape a repository does.
    let (st, view) = server.get_json("/v1/orgs/acme/changesets/Ic5000001/workspace", &admin);
    assert_eq!(st, 200, "{view}");
    let ws_url = format!(
        "ssh://git@127.0.0.1:{}/acme/changesets/Ic5000001.git",
        server.ssh_port
    );
    assert_eq!(view["ssh_clone_url"], ws_url, "{view}");
    assert_eq!(
        view["members"][0]["ssh_clone_url"],
        server.ssh_url("acme", "api")
    );
    assert_eq!(
        view["members"][1]["ssh_clone_url"],
        server.ssh_url("acme", "web")
    );
    let tip = view["tip"].as_str().unwrap().to_string();

    let (key, pub_line) = keygen(scratch.path(), "dev-key");
    let (st, added) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &admin,
        serde_json::json!({ "public_key": pub_line, "token_id": admin_id, "label": "laptop" }),
    );
    assert_eq!(st, 201, "{added}");
    let ssh = ssh_command(&key, &known_hosts, "accept-new");
    let ws = scratch.path().join("ws");
    git_ssh(
        scratch.path(),
        &ssh,
        &[
            "clone",
            "-q",
            "--recurse-submodules",
            &ws_url,
            ws.to_str().unwrap(),
        ],
    );
    gitcli::fsck(&ws);
    gitcli::fsck(&ws.join("api"));
    gitcli::fsck(&ws.join("web"));
    assert_eq!(gitcli::git(&ws, &["rev-parse", "HEAD"]).trim(), tip);
    assert_eq!(
        gitcli::git(&ws.join("api"), &["rev-parse", "HEAD"]).trim(),
        api
    );
    assert_eq!(
        gitcli::git(&ws.join("web"), &["rev-parse", "HEAD"]).trim(),
        web
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("web/feature.txt")).unwrap(),
        "web over ssh\n"
    );
    // `../../api.git` resolved against the SSH superproject URL is the
    // member's SSH URL: the credential that opened the workspace is the
    // one that fetched the members.
    assert_eq!(
        gitcli::git(&ws.join("api"), &["remote", "get-url", "origin"]).trim(),
        server.ssh_url("acme", "api")
    );

    // `ls-remote` is the whole advert and then a lone flush: HEAD as a
    // symref to the one branch, and the branch.
    let refs = git_ssh(scratch.path(), &ssh, &["ls-remote", "--symref", &ws_url]);
    assert_eq!(
        refs.trim(),
        format!("ref: refs/heads/workspace\tHEAD\n{tip}\tHEAD\n{tip}\trefs/heads/workspace"),
        "{refs}"
    );
    // A v0/v1 client is refused loudly, in-band — the same gate as a
    // repository, because the workspace only speaks v2.
    let err = git_ssh_expect_err(
        scratch.path(),
        &ssh,
        &["-c", "protocol.version=0", "clone", "-q", &ws_url, "v0"],
    );
    assert!(err.contains("protocol v2 required"), "{err}");

    // Read-only, in-band, with somewhere to go instead.
    gitcli::git(&ws, &["commit", "-q", "--allow-empty", "-m", "nope"]);
    let err = git_ssh_expect_err(&ws, &ssh, &["push", "-q", "origin", "HEAD:workspace"]);
    assert!(
        err.contains("a changeset workspace is read-only; push to its member repositories"),
        "{err}"
    );

    // A valid key for another org: not "forbidden", "not found" — the
    // same masking as a private repository, over this transport too.
    let (rival_admin, rival_id) = server.bootstrap_org("rival");
    let (rival_key, rival_pub) = keygen(scratch.path(), "rival-key");
    let (st, _) = server.post(
        "/v1/orgs/rival/ssh-keys",
        &rival_admin,
        serde_json::json!({ "public_key": rival_pub, "token_id": rival_id, "label": "rival" }),
    );
    assert_eq!(st, 201);
    let rival_ssh = ssh_command(&rival_key, &known_hosts, "accept-new");
    let err = git_ssh_expect_err(
        scratch.path(),
        &rival_ssh,
        &["clone", "-q", &ws_url, "denied"],
    );
    assert!(err.contains("weft: repository not found"), "{err}");
    let absent = format!(
        "ssh://git@127.0.0.1:{}/acme/changesets/Ic5000009.git",
        server.ssh_port
    );
    let err = git_ssh_expect_err(
        scratch.path(),
        &rival_ssh,
        &["clone", "-q", &absent, "denied2"],
    );
    assert!(err.contains("weft: repository not found"), "{err}");

    // The server is still serving after every refusal.
    let (st, _) = server.get_json("/v1/orgs/acme/changesets/Ic5000001/workspace", &admin);
    assert_eq!(st, 200);
}
