//! A running `stratum-server`, for end-to-end tests.
//!
//! Every suite under `crates/stratum-server/tests/` used to carry its own
//! copy of this — the same spawn, the same health poll, the same request
//! helper — roughly 120 lines of boilerplate before each file's first
//! assertion, and they had drifted: different argument orders, different
//! stop signals, only some exposing `healthy()`. This is that harness,
//! once.
//!
//! The binary path is passed in rather than resolved here, because
//! `env!("CARGO_BIN_EXE_stratum-server")` only expands inside the package
//! that declares the binary. One macro call per suite; everything else is
//! shared.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::minio::{ROOT_PASSWORD, ROOT_USER};

/// How to stop the child. ECS sends SIGTERM, so at least one suite must
/// exercise that arm of graceful shutdown rather than only SIGINT.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    Int,
    Term,
}

pub struct Server {
    child: Child,
    stop: Stop,
    /// `http://127.0.0.1:<port>` — no trailing slash.
    pub base: String,
    pub db_url: String,
    pub store_url: String,
    bin: String,
    /// Kept so `restart` can put the same process back: a crash test is
    /// only about resumability if what comes back is the same node.
    data_dir: std::path::PathBuf,
    /// Removes the default data dir with the server; `None` when the test
    /// supplied its own and owns it.
    _scratch: Option<crate::tempdir::TempDir>,
    env: Vec<(String, String)>,
    /// Set once this child has been reaped, so nothing signals a pid the
    /// kernel may since have handed to somebody else.
    reaped: bool,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop_child();
    }
}

/// Configure a server before starting it.
pub struct ServerBuilder {
    bin: String,
    store_url: String,
    db_url: Option<String>,
    db_hint: String,
    data_dir: Option<std::path::PathBuf>,
    env: Vec<(String, String)>,
    stop: Stop,
}

impl ServerBuilder {
    /// Use a specific database rather than minting one. Suites that
    /// inspect the control plane directly need the URL they passed in.
    pub fn db_url(mut self, url: &str) -> Self {
        self.db_url = Some(url.to_string());
        self
    }

    /// Hint used to name the freshly-minted test database.
    pub fn db_hint(mut self, hint: &str) -> Self {
        self.db_hint = hint.to_string();
        self
    }

    pub fn data_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// Set one variable, replacing an earlier value for the same name.
    ///
    /// One slot per name, not a list the last of wins: `restart_with`
    /// edits the slot it finds *first*, and `Command::env` honours the
    /// one set *last*, so a name pushed twice — a suite-wide default and
    /// a test's override — used to make a later `restart_with` on that
    /// name edit the shadowed value and change nothing the child saw.
    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        let value = value.into();
        match self.env.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.env.push((key.to_string(), value)),
        }
        self
    }

    pub fn envs(mut self, pairs: &[(&str, String)]) -> Self {
        for (k, v) in pairs {
            self = self.env(k, v.clone());
        }
        self
    }

    /// Stop with SIGTERM instead of SIGINT — the signal ECS sends.
    pub fn stop_with_term(mut self) -> Self {
        self.stop = Stop::Term;
        self
    }

    /// Spawn and block until `/healthz` answers, or panic with why.
    pub fn start(self) -> Server {
        let db_url = self
            .db_url
            .unwrap_or_else(|| crate::pg::test_db_url(&self.db_hint));
        // Resolved once, before the first bind attempt, rather than from
        // the port: a data dir that changes when a port is retried — or
        // when `restart` picks a fresh port — is a different node, and
        // then "it came back" proves nothing about resumability.
        // Claimed rather than adopted (see `tempdir`): a leftover from a
        // killed run under a reused pid would hand this node a stranger's
        // mirrors. Owned by the `Server` so it goes when the server does.
        let (data_dir, scratch) = match self.data_dir {
            Some(d) => (d, None),
            None => {
                let t = crate::tempdir::TempDir::new(&format!("stratum-e2e-{}", self.db_hint))
                    .unwrap_or_else(|e| panic!("server data dir: {e}"));
                (t.path().to_path_buf(), Some(t))
            }
        };
        let (child, bind) = spawn_child(&self.bin, &self.store_url, &db_url, &data_dir, &self.env);
        Server {
            child,
            stop: self.stop,
            base: format!("http://{bind}"),
            db_url,
            store_url: self.store_url,
            bin: self.bin,
            data_dir,
            _scratch: scratch,
            env: self.env,
            reaped: false,
        }
    }
}

/// Spawn the server binary on a free port with the environment every
/// start — first or `restart` — has to reproduce exactly.
fn spawn_child(
    bin: &str,
    store_url: &str,
    db_url: &str,
    data_dir: &std::path::Path,
    env: &[(String, String)],
) -> (Child, String) {
    spawn_on_free_port(|bind| {
        let mut cmd = admin_command(bin);
        cmd.env("STRATUM_STORE_URL", store_url)
            .env("STRATUM_DB_URL", db_url)
            .env("STRATUM_DATA_DIR", data_dir)
            .env("STRATUM_BIND", bind)
            // Background origin polling would make timing nondeterministic;
            // suites that want it turn it back on explicitly.
            .env("STRATUM_MIRROR_POLL_SECS", "0");
        // `{bind}` in a value is the address this attempt binds — the
        // only way a suite can point an env var at the server itself,
        // since the port is not known until here and changes on restart.
        for (k, v) in env {
            cmd.env(k, v.replace("{bind}", bind));
        }
        cmd
    })
}

/// Spawn a server on a free port and block until it is serving, retrying
/// if the port was taken between choosing it and binding it.
///
/// Choosing a port by binding it and letting go is inherently racy: two
/// suites — or two tests in the same binary — can be handed the same
/// number, and the one that loses the race then finds a perfectly
/// *healthy* server on it. Somebody else's server, with somebody else's
/// database. That does not fail like a port clash; it fails like the API
/// forgetting an org exists, hundreds of lines later, in whichever test
/// happened to lose. So the child's early exit is watched for, and the
/// port retried, rather than trusting a health check to tell us the
/// server on the other end is ours.
///
/// Watching for the exit is not enough on its own. The loser exits on
/// "address in use" a few milliseconds after it is spawned, and the
/// winner is *already* healthy on that port — so the health check can
/// succeed before the exit is seen, and this returned a child that was
/// about to die together with a port that belonged to someone else.
/// That is how `repo_meta_e2e` failed under `cargo llvm-cov` on
/// 2026-09-07: `bind 127.0.0.1:44409: Address already in use` on
/// stderr, and the test's first API call answered 401 by a server
/// with a different database. So every spawn is given its own
/// `STRATUM_INSTANCE_ID`, and the server on the other end has to
/// answer it back on `/healthz` before it is trusted; a stranger is
/// killed-and-retried like a clash. The Postgres harness proves its
/// cluster is its own with `SHOW data_directory` for the same reason.
///
/// `build` is given the `127.0.0.1:<port>` to bind and returns the
/// command to run; it is called once per attempt.
pub fn spawn_on_free_port(build: impl Fn(&str) -> Command) -> (Child, String) {
    for attempt in 0..8 {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe socket");
            l.local_addr().expect("probe socket address").port()
        };
        let bind = format!("127.0.0.1:{port}");
        let instance = format!("testkit-{}-{}-{attempt}", std::process::id(), port);
        let mut cmd = build(&bind);
        cmd.env("STRATUM_INSTANCE_ID", &instance)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        let mut child = crate::detach::detached(&mut cmd)
            .spawn()
            .expect("spawn stratum-server");

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                assert!(
                    attempt < 7,
                    "server exited during startup on {bind} ({status}) eight times running"
                );
                break;
            }
            match ureq::get(&format!("http://{bind}/healthz"))
                .timeout(Duration::from_millis(300))
                .call()
            {
                Ok(resp) if resp.header("x-weft-instance") == Some(instance.as_str()) => {
                    return (child, bind)
                }
                Ok(resp) => {
                    let who = resp
                        .header("x-weft-instance")
                        .unwrap_or("<none>")
                        .to_string();
                    let _ = child.kill();
                    let _ = child.wait();
                    assert!(
                        attempt < 7,
                        "a server that is not ours ({who}) answered on {bind} eight times running"
                    );
                    eprintln!("testkit: {bind} is serving {who}, not {instance}; retrying on a fresh port");
                    break;
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(e) => panic!("server never became healthy on {bind}: {e}"),
            }
        }
    }
    unreachable!("the last attempt either returns or panics")
}

/// The env every invocation of the binary needs, scrubbed of the host's.
/// `LLVM_PROFILE_FILE` is forwarded so coverage still sees the child.
/// What a trip through the GitHub sign-in callback ended in.
pub struct GithubSignin {
    pub status: u16,
    /// The `github=` word in the redirect — `ok`, `noaccount`, `expired`,
    /// `noemail` and the rest. Pulled out because every assertion in the
    /// suite is about this and a substring match on the whole URL would
    /// pass for `notyours` when it meant `yours`.
    pub outcome: String,
    pub location: String,
    /// The session the callback issued, if it signed anybody in. `None`
    /// on every refusal, which is the assertion most of them make.
    pub session: Option<String>,
}

/// The value of one named cookie out of a list of `Set-Cookie` headers,
/// taking the **last** — the callback clears its state cookie and sets a
/// session in one response, and a later header for the same name is what
/// a browser keeps.
fn set_cookie_value(headers: &[String], name: &str) -> Option<String> {
    headers.iter().rev().find_map(|h| {
        let pair = h.split(';').next()?;
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

fn admin_command(bin: &str) -> Command {
    let mut c = Command::new(bin);
    c.env_clear()
        .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("AWS_ACCESS_KEY_ID", ROOT_USER)
        .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
        .env("AWS_REGION", "us-east-1");
    c
}

impl Server {
    pub fn builder(bin: &str, store_url: &str) -> ServerBuilder {
        ServerBuilder {
            bin: bin.to_string(),
            store_url: store_url.to_string(),
            db_url: None,
            db_hint: "e2e".to_string(),
            data_dir: None,
            env: Vec::new(),
            stop: Stop::Int,
        }
    }

    /// The child's pid. A fault-proxy `observe` callback holds this and
    /// nothing else — it runs on a proxy thread, mid-request, and cannot
    /// borrow the `Server` — so the kill itself is [`Server::kill_pid`].
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// SIGKILL a pid. Separate from [`Server::kill_now`] because the
    /// interesting kills happen from inside a store request, where the
    /// only thing in hand is the number.
    ///
    /// Note that a SIGKILLed child writes no `LLVM_PROFILE_FILE`
    /// profraw, so a process killed this way contributes *zero* coverage.
    pub fn kill_pid(pid: u32) {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }

    /// A hard crash: SIGKILL, then reap. No graceful shutdown, no
    /// flush, no chance to finish the store operation in flight — which
    /// is the whole point, because that is the state a resumability test
    /// has to face. Idempotent, and safe to call after the child has
    /// already died from a kill sent elsewhere.
    pub fn kill_now(&mut self) {
        if self.reaped {
            return;
        }
        Server::kill_pid(self.child.id());
        let _ = self.child.wait();
        self.reaped = true;
    }

    /// Wait until the child process is gone, reaping it. Returns false if
    /// it was still running at the deadline — a kill point that never
    /// fired is a test bug, and this is how a suite says so loudly
    /// instead of asserting against a server that never died.
    pub fn wait_for_exit(&mut self, within: Duration) -> bool {
        if self.reaped {
            return true;
        }
        let deadline = Instant::now() + within;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.reaped = true;
                    return true;
                }
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => return false,
            }
        }
    }

    /// Bring the node back: same binary, same database, same data dir,
    /// same environment — a fresh port, because the old one may still be
    /// in TIME_WAIT and a restart that has to wait for that is a restart
    /// that flakes. Any child still running is stopped first.
    pub fn restart(&mut self) {
        self.stop_child();
        let (child, bind) = spawn_child(
            &self.bin,
            &self.store_url,
            &self.db_url,
            &self.data_dir,
            &self.env,
        );
        self.child = child;
        self.base = format!("http://{bind}");
        self.reaped = false;
    }

    /// `restart`, with environment changes that stick for every restart
    /// after it too. A crash test usually wants the node that comes back
    /// to have a worker the node that died did not — seed with the poller
    /// off so nothing compacts before the kill point is armed, then bring
    /// it back with the poller on.
    pub fn restart_with(&mut self, overrides: &[(&str, String)]) {
        for (k, v) in overrides {
            match self.env.iter_mut().find(|(name, _)| name == k) {
                Some(slot) => slot.1 = v.clone(),
                None => self.env.push((k.to_string(), v.clone())),
            }
        }
        self.restart();
    }

    /// Stop the child the way this suite asked to stop it, escalating to
    /// SIGKILL if it will not go, and reap it.
    fn stop_child(&mut self) {
        if self.reaped {
            return;
        }
        let sig = match self.stop {
            Stop::Int => "-INT",
            Stop::Term => "-TERM",
        };
        let _ = Command::new("kill")
            .args([sig, &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        self.reaped = true;
    }

    /// Run `stratum-server <args>` against this server's store and DB.
    pub fn admin(&self, args: &[&str]) -> Result<String, String> {
        let out = admin_command(&self.bin)
            .env("STRATUM_STORE_URL", &self.store_url)
            .env("STRATUM_DB_URL", &self.db_url)
            .args(args)
            .output()
            .expect("run admin command");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            Err(String::from_utf8_lossy(&out.stderr).to_string())
        }
    }

    /// Admin command whose stdout is one JSON line. Panics on failure —
    /// use `admin` when a non-zero exit is the thing under test.
    pub fn admin_json(&self, args: &[&str]) -> serde_json::Value {
        let out = self
            .admin(args)
            .unwrap_or_else(|e| panic!("admin {args:?}: {e}"));
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("admin {args:?} json: {e}"))
    }

    /// The stderr of an admin command that is expected to fail. Panics if
    /// it unexpectedly succeeds, so a silently-accepted bad input cannot
    /// pass as a green test.
    ///
    /// A short `lock_timeout` is forced: one of the failures under test is
    /// "another session holds the lock", and the default 5s wait would
    /// turn that assertion into a stall.
    pub fn admin_expect_err(&self, args: &[&str]) -> String {
        let out = admin_command(&self.bin)
            .env("STRATUM_STORE_URL", &self.store_url)
            .env("STRATUM_DB_URL", &self.db_url)
            .env("STRATUM_DB_LOCK_TIMEOUT_MS", "400")
            .args(args)
            .output()
            .expect("run admin command");
        if out.status.success() {
            panic!(
                "admin {args:?} unexpectedly succeeded: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        String::from_utf8_lossy(&out.stderr).to_string()
    }

    /// `admin bootstrap --org NAME` → the org's admin token.
    pub fn bootstrap_org(&self, org: &str) -> String {
        let out = self
            .admin(&["admin", "bootstrap", "--org", org])
            .unwrap_or_else(|e| panic!("bootstrap {org}: {e}"));
        let v: serde_json::Value =
            serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("bootstrap json: {e}"));
        v["admin_token"]
            .as_str()
            .expect("admin_token in bootstrap output")
            .to_string()
    }

    /// The token id — the middle field of `weft_<id>_<secret>`, which is
    /// what SSH key registration binds against.
    pub fn token_id(token: &str) -> String {
        token.split('_').nth(1).unwrap_or_default().to_string()
    }

    /// One request. A 4xx is data, not an error — only a transport
    /// failure panics, so tests can assert on status codes.
    pub fn req(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let (status, body, _) = self.req_full(method, path, token, body);
        (status, body)
    }

    /// As `req`, plus the response headers — for the paths where a header
    /// *is* the contract (ETag, Cache-Control, WWW-Authenticate).
    pub fn req_full(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> (
        u16,
        serde_json::Value,
        std::collections::HashMap<String, String>,
    ) {
        let mut r = ureq::request(method, &format!("{}{path}", self.base));
        if !token.is_empty() {
            r = r.set("Authorization", &format!("Bearer {token}"));
        }
        if body.is_some() {
            r = r.set("Content-Type", "application/json");
        }
        let resp = match body {
            Some(b) => r.send_string(&b.to_string()),
            None => r.call(),
        };
        let resp = match resp {
            Ok(x) => x,
            Err(ureq::Error::Status(_, x)) => x,
            Err(e) => panic!("transport {method} {path}: {e}"),
        };
        let status = resp.status();
        let headers: std::collections::HashMap<String, String> = resp
            .headers_names()
            .into_iter()
            .filter_map(|n| resp.header(&n).map(|v| (n.to_lowercase(), v.to_string())))
            .collect();
        // A body that cannot be read is a transport failure, and it is
        // reported as one — the same way a failed send is above. It used
        // to be swallowed into an empty string, which parses as no JSON,
        // so a connection dropped mid-reply under a loaded coverage run
        // surfaced as `Option::unwrap()` on `out["url"]` with a 200 in
        // hand, and named nothing about what had gone wrong.
        let text = match resp.into_string() {
            Ok(t) => t,
            Err(e) => panic!("reading the {status} reply to {method} {path}: {e}"),
        };
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
            headers,
        )
    }

    pub fn get(&self, path: &str, token: &str) -> (u16, serde_json::Value) {
        self.req("GET", path, token, None)
    }

    /// The sign-in start route, unfollowed: `(status, location)`.
    pub fn follow_start(&self) -> (u16, String) {
        let (status, location, _) = self.raw_get("/v1/auth/github/start", &[]);
        (status, location)
    }

    /// Sign in through GitHub the way a browser does: start the flow,
    /// keep the state cookie it parks, and come back to the callback
    /// with the code the fake mints.
    ///
    /// Both legs run with redirects off, for [`Self::follow_setup_raw`]'s
    /// reason — the redirect *is* the answer — and the state travels in
    /// a cookie, so a helper that dropped it would test the CSRF
    /// refusal on every happy path and nothing else.
    pub fn github_signin(&self, code: &str) -> GithubSignin {
        let (status, location, cookies) = self.raw_get("/v1/auth/github/start", &[]);
        assert_eq!(status, 303, "github start answered {status}: {location}");
        let state = set_cookie_value(&cookies, "weft_ghauth")
            .unwrap_or_else(|| panic!("start parked no state cookie: {cookies:?}"));
        self.github_callback(&format!("code={code}&state={state}"), Some(&state))
    }

    /// The sign-in callback with whatever query and state cookie you
    /// give it — for the forgeries, which is most of what there is to
    /// test about a URL a stranger can construct.
    pub fn github_callback(&self, query: &str, state_cookie: Option<&str>) -> GithubSignin {
        let cookie = state_cookie.map(|s| format!("weft_ghauth={s}"));
        let headers: Vec<(&str, &str)> = cookie
            .as_deref()
            .map(|c| vec![("Cookie", c)])
            .unwrap_or_default();
        let (status, location, cookies) =
            self.raw_get(&format!("/v1/auth/github/callback?{query}"), &headers);
        GithubSignin {
            status,
            outcome: location
                .split("github=")
                .nth(1)
                .unwrap_or_default()
                .split('&')
                .next()
                .unwrap_or_default()
                .to_string(),
            location,
            session: set_cookie_value(&cookies, "stratum_session").filter(|s| !s.is_empty()),
        }
    }

    /// A GET that follows nothing and keeps *every* `Set-Cookie`.
    ///
    /// `ureq`'s `header()` answers only the first of a repeated header,
    /// and the sign-in callback sets two — the spent state cookie and
    /// the new session. Reading one would have made the session
    /// invisible and every happy-path assertion a false negative.
    fn raw_get(&self, path: &str, headers: &[(&str, &str)]) -> (u16, String, Vec<String>) {
        let url = format!("{}{path}", self.base);
        let agent = ureq::builder().redirects(0).build();
        let mut req = agent.get(&url);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let resp = match req.call() {
            Ok(x) => x,
            Err(ureq::Error::Status(_, x)) => x,
            Err(e) => panic!("transport GET {url}: {e}"),
        };
        (
            resp.status(),
            resp.header("location").unwrap_or_default().to_string(),
            resp.all("set-cookie")
                .into_iter()
                .map(str::to_string)
                .collect(),
        )
    }

    /// Hit the GitHub install callback the way a browser arriving from
    /// GitHub does, and report `(status, location)` without following.
    ///
    /// Following is what a browser does and exactly what a test must
    /// not: the redirect *is* the answer here — which outcome the
    /// callback decided rides in its query string — and `ureq` follows
    /// by default, which would turn every one of those into an
    /// indistinguishable 200 from the dashboard.
    /// The install callback with whatever query string you give it —
    /// including none, which is what a bookmark or a poke looks like.
    pub fn follow_setup_raw(&self, query: &str) -> (u16, String) {
        let url = format!("{}/v1/github/setup{query}", self.base);
        let agent = ureq::builder().redirects(0).build();
        let resp = match agent.get(&url).call() {
            Ok(x) => x,
            Err(ureq::Error::Status(_, x)) => x,
            Err(e) => panic!("transport GET {url}: {e}"),
        };
        let status = resp.status();
        let location = resp.header("location").unwrap_or_default().to_string();
        (status, location)
    }

    /// Connect an installation to `org` the way a person does: start the
    /// flow as an admin, then arrive at the callback with the state it
    /// minted. Panics unless the callback says `connect=ok`, because a
    /// suite that then mirrors through the id would be testing the wrong
    /// thing. The server needs `STRATUM_GITHUB_INSTALL_URL` for the start
    /// call to answer at all.
    pub fn connect_installation(&self, org: &str, admin: &str, installation_id: &str) {
        self.connect_installation_app(org, admin, installation_id, None)
    }

    /// `connect_installation` for a named App — `Some("runners")` for a
    /// deployment whose runner feature has an App of its own.
    pub fn connect_installation_app(
        &self,
        org: &str,
        admin: &str,
        installation_id: &str,
        app: Option<&str>,
    ) {
        let query = app.map(|a| format!("?app={a}")).unwrap_or_default();
        let (st, out) = self.post(
            &format!("/v1/orgs/{org}/github/install{query}"),
            admin,
            Some(serde_json::json!({})),
        );
        assert_eq!(st, 200, "start install for {org}: {out}");
        let url = out["url"].as_str().expect("install url");
        let state = url
            .split("state=")
            .nth(1)
            .expect("install url carries a state")
            .split('&')
            .next()
            .unwrap();
        let extra = app.map(|a| format!("&app={a}")).unwrap_or_default();
        let (st, loc) = self.follow_setup_as(
            &format!(
                "?installation_id={installation_id}&state={state}&code=code_owning_{installation_id}{extra}"
            ),
            None,
        );
        assert_eq!(
            st, 303,
            "callback for {org}/{installation_id} answered {st}"
        );
        assert!(
            loc.contains("connect=ok"),
            "connecting {installation_id} to {org}: {loc}"
        );
    }

    /// Arrive at the callback the way GitHub sends a browser: the
    /// installation, the state, and — when the App requests user
    /// authorization — a `code` for the person who controls exactly this
    /// installation (the fake's `code_owning_<id>`). A server whose App
    /// has no OAuth client ignores the code.
    pub fn follow_setup(&self, installation_id: &str, state: &str) -> (u16, String) {
        self.follow_setup_as(
            &format!(
                "?installation_id={installation_id}&state={state}&code=code_owning_{installation_id}"
            ),
            None,
        )
    }

    /// The callback with an arbitrary query and, optionally, a session
    /// cookie — for a return trip that carries no state and has only
    /// the person's own sign-in to go on.
    pub fn follow_setup_as(&self, query: &str, cookie: Option<&str>) -> (u16, String) {
        let (status, location, _) = self.follow_setup_full(query, cookie);
        (status, location)
    }

    /// `follow_setup_as`, also returning the `Set-Cookie` header the
    /// callback answered with — the claim cookie a parked installation
    /// rides in, which the claim route then needs presented back.
    pub fn follow_setup_full(
        &self,
        query: &str,
        cookie: Option<&str>,
    ) -> (u16, String, Option<String>) {
        self.follow_setup_at("/v1/github/setup", query, cookie)
    }

    /// `follow_setup_full` at a chosen callback path — the Runners App's
    /// own `/v1/github/setup/runners`, where GitHub sends its browser
    /// with no `app` in the query.
    pub fn follow_setup_at(
        &self,
        path: &str,
        query: &str,
        cookie: Option<&str>,
    ) -> (u16, String, Option<String>) {
        let url = format!("{}{path}{query}", self.base);
        let agent = ureq::builder().redirects(0).build();
        let mut req = agent.get(&url);
        if let Some(c) = cookie {
            req = req.set("Cookie", c);
        }
        let resp = match req.call() {
            Ok(x) => x,
            Err(ureq::Error::Status(_, x)) => x,
            Err(e) => panic!("transport GET {url}: {e}"),
        };
        let status = resp.status();
        let location = resp.header("location").unwrap_or_default().to_string();
        let set_cookie = resp.header("set-cookie").map(str::to_string);
        (status, location, set_cookie)
    }

    pub fn post(
        &self,
        path: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        self.req("POST", path, token, body)
    }

    pub fn delete(&self, path: &str, token: &str) -> (u16, serde_json::Value) {
        self.req("DELETE", path, token, None)
    }

    /// A git remote with the token embedded, as CI usually clones.
    pub fn authed_url(&self, token: &str, org: &str, repo: &str) -> String {
        let host = self.base.strip_prefix("http://").unwrap_or(&self.base);
        format!("http://x:{token}@{host}/{org}/{repo}.git")
    }

    /// Status only, for suites that assert on codes rather than bodies.
    /// Token is optional so the anonymous case is a first-class call.
    pub fn status_get(&self, path: &str, token: Option<&str>) -> u16 {
        self.req("GET", path, token.unwrap_or_default(), None).0
    }

    pub fn status_post(&self, path: &str, token: &str, body: serde_json::Value) -> u16 {
        self.req("POST", path, token, Some(body)).0
    }

    /// `127.0.0.1:<port>` — for tests that open their own socket.
    pub fn host(&self) -> &str {
        self.base.strip_prefix("http://").unwrap_or(&self.base)
    }

    /// Still alive and serving. Every adversarial case ends with this:
    /// a server that survives an attack by refusing everything has not
    /// passed, it has failed differently.
    pub fn healthy(&self) -> bool {
        ureq::get(&format!("{}/healthz", self.base))
            .timeout(Duration::from_secs(2))
            .call()
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A healthy server on the chosen port that is not the one we
    /// spawned. The stranger is stood up *inside* `build`, on the very
    /// port the picker chose, answering `/healthz` under another
    /// instance id; the child it hands back never binds anything and
    /// never exits on its own. Before the identity check the picker
    /// returned that child and that port at once; now every attempt is
    /// refused, the child is killed each time, and the eighth refusal
    /// is a panic that names the stranger.
    ///
    /// The stranger can lose the port race itself — under a full
    /// workspace run another suite takes the number between the probe
    /// letting go and this bind, which is the very race under test — so
    /// a lost bind hands the picker a child that exits at once and is
    /// retried like any early exit. The property that must hold either
    /// way: the picker never returns, and at least one stranger was
    /// refused by name.
    #[test]
    fn a_healthy_server_that_is_not_ours_is_refused_and_retried() {
        use std::io::{Read, Write};
        let strangers = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let seen = strangers.clone();
        let build = move |bind: &str| {
            let Ok(l) = std::net::TcpListener::bind(bind) else {
                let mut c = Command::new("sh");
                c.args(["-c", "exit 7"]);
                return c;
            };
            *seen.lock().unwrap() += 1;
            std::thread::spawn(move || {
                for stream in l.incoming().flatten() {
                    let mut s = stream;
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf);
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nx-weft-instance: somebody-else\r\n\
                          content-length: 3\r\nconnection: close\r\n\r\nok\n",
                    );
                }
            });
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 30"]);
            c
        };
        let out = std::panic::catch_unwind(|| spawn_on_free_port(build));
        let msg = match out {
            Ok((mut child, bind)) => {
                let _ = child.kill();
                panic!("the picker trusted a stranger on {bind}")
            }
            Err(e) => e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default(),
        };
        let strangers = *strangers.lock().unwrap();
        assert!(strangers >= 1, "no stranger ever held the port: {msg}");
        assert!(msg.contains("eight times running"), "{msg}");
        assert!(
            msg.contains("not ours (somebody-else)") || msg.contains("exited during startup"),
            "{msg}"
        );
    }

    #[test]
    fn a_variable_set_twice_has_one_slot_so_restart_with_edits_the_live_one() {
        let b = Server::builder("stratum-server", "http://store")
            .env("STRATUM_LAND_RECHECK_SECS", "1")
            .env("STRATUM_LAND_RECHECK_SECS", "600")
            .envs(&[
                ("STRATUM_LAND_RECHECK_SECS", "7".into()),
                ("OTHER", "x".into()),
            ]);
        assert_eq!(
            b.env,
            vec![
                ("STRATUM_LAND_RECHECK_SECS".to_string(), "7".to_string()),
                ("OTHER".to_string(), "x".to_string()),
            ]
        );
    }
}
