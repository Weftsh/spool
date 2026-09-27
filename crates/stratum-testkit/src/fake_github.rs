//! A hermetic GitHub App API fake: serves the installation-token exchange
//! the `GithubApp` origin provider performs (raw-TCP HTTP, no framework).
//! Repo fetch traffic doesn't come here — tests point the provider's
//! git_base at a file:// directory of bare repos.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many times each repository has been asked for its workflow runs.
///
/// Keyed by `owner/repo` so a refusal schedule belongs to the repository
/// it is about — see the argument at the rate-limit arm of
/// `actions_route`.
type RunCalls = Arc<Mutex<HashMap<String, u64>>>;

/// The RSA private key the "app" is registered with (PKCS#1 PEM, the shape
/// GitHub issues). Test-only material.
pub const TEST_APP_KEY_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAxCDfdgcyehsFCK0ORoxd5jGhCDo5VZdtvofHp0bE+StO8ANG
oYbKKIDBRTGogNduDYsMb0tVelycgXy91Wiinq0eg/CVKZN8BsMYQKZUznCbOzft
JrqmnauG2bTMJqiO6iC844FrIv+Vl8Sv55cfYXnfedY6DR6Yj8V3uoqKGOKOQUAo
9eoB8ZuDxMUxjauXL+qqBX3XJ6U04wP8gfmGQXegdX7QwFrw8z3PspOjHwYLS57d
LrvVZW8qNv1Fb+5/zpy658qsap8pg0+zhQzZLdUPHbwVtvKFLXjUgaWUCidbJ3Sa
OsB6ac72CJr7DSx8igf1eYS4tTHj/AtQSqz+SwIDAQABAoIBAA4k448SygBsH9FP
3A//aHgrS3zMTvBnj5amTe8E16JM9FzmLfsd8Hwgr1xKzmc4SHhs9wEbUp2uVx6v
42ekEgvVpuk5+bytWH9X8eq1QOsNMn/mf4S3AKdHRJFBWVjoxNTvdnlrwgSDBrbf
jHxsD1HLmCLxVAkFLb7Uk71d510PvUMnolSukAlYnQz3KH3+gaJGJctrm3qtDNzg
wyvfUm7GYdybVmrOTh9Szu9aXWuOdbYPxTl6oQYcYpbx0gmlFRhmhT16xQ0xyyCN
6kqn+Ig4aYV11oCTvSmu8zcJZe13o6xxjwzU+tyfkZhVwH8G9b+rlSkpmKAuOaJm
VfzdWSECgYEA7T49C0P0Sa2DGq1YLQS8Ub77YHxBnfItqgx3R5NPhcFfKpO2Wqni
8X9WWYTFTKvlwXaZ7a+o79JgK62DtjSAWEw5cMMfeuNdJUCy+A2DCzCiO3j0GL+U
K/NX1rFgJNH0RvgCilWzgH9/TKqY2X/CuG4jnTtevfrfnKN8uKznnKsCgYEA06J6
waQRHvDQYG0eq5wBmAAgVHZ/3V998q0fdW7Kre5x8GX6aTMWm5DHIJx1rS1zCbtq
nsbruBWjmI21wmoekqz6F/WZS7Elinp8jki4WWkEWHJnkM8Ohf+m0olUo/ee0lZ/
K5yIcLRP/E9TupuH6mQhT6ZWSM1wWw+FxPq85OECgYEAzydeVBzlDQSGCuA7syuE
aHiztN8qyIiz2N0Dtirp8CgWOe4691WKRUbkFkx5nuYmO1SdOc79W1M+CEV9Ubbs
Lq14Jn8qWLp5FdM1sqTRvQ6dSgLmWUnHTs0v8NZ21g/CFcnvJe2JTHWHqWD5EEmf
tDzvuhYiNw78/CBBAlxv7PsCgYBLxv/BWievNnbGMAwtUjzX2iO5WnzKHSkRvZ9o
AvWbdadidoFFLb/Ij/xc1ujjy0RHlc3FcGByl3zuYL9WD31G85zQ+2WaTqGshdMX
dz5a9VlS+hPPK/R9Ul6/P+EInN9HXSVHzlKkWEvTgevvA0WVTakHxf1bMAQs9s/l
CgqcwQKBgDs4zxkcOk0Q+NGOqY/9PTdSY7mknzOHmcOZ1b1erFFuh4YAIgb2Ajti
x7owfGet830/hdatpu1NRkJzlAlfIIyUtK0GfTUFgXDcjijnklhfEwfGeDxqHNnR
hzL3YngJRPeiYDcE6U05aiLrmcHQnFKZeM0WHvV+erp+UlLpDO9B
-----END RSA PRIVATE KEY-----
";

/// What the self-hosted-runner routes have been asked, for a test to
/// read back: the `generate-jitconfig` bodies, the runner deletions, the
/// run cancellations, and the "is this job still going?" reads.
/// Instrumentation only — nothing the fake answers depends on it.
#[derive(Default)]
pub struct RunnerCalls {
    pub jit_requests: Mutex<Vec<serde_json::Value>>,
    /// `owner/repo#runner_id`, in order.
    pub runner_deletes: Mutex<Vec<String>>,
    /// `owner/repo#run_id`, in order.
    pub cancels: Mutex<Vec<String>>,
    /// `owner/repo#job_id` for every job-state read, in order. A test
    /// counts these: the dispatcher reconciles running jobs against
    /// GitHub, and doing that on every poll would spend an
    /// installation's hourly budget on one ordinary build.
    pub job_reads: Mutex<Vec<String>>,
    /// Answers to job-state reads by job id, ahead of the repository-name
    /// rule below: `(status, conclusion)` with the conclusion as the JSON
    /// GitHub sends (`"success"` quoted, `null` bare). A test needs this
    /// when two jobs on one repository must answer differently — the job
    /// a runner was launched for is over, the one it took is not.
    pub job_states: Mutex<HashMap<i64, (String, String)>>,
    /// The next runner id to hand out.
    seq: AtomicU64,
}

pub struct FakeGithub {
    pub base_url: String,
    pub tokens_issued: Arc<AtomicU64>,
    /// The runner routes' record of what was asked of them.
    pub runners: Arc<RunnerCalls>,
    /// Requests this instance has served to the Actions run list, across
    /// every repository and including the refused ones.
    ///
    /// Instrumentation, not behaviour — nothing the fake answers depends
    /// on it. It is here so a test can assert what an incremental poll
    /// stopped *asking* for, which is the half of "incremental" that
    /// saves a rate budget: a poll that still fetches every page and
    /// merely writes fewer rows has saved nothing, and looks identical
    /// from the database.
    ///
    /// Per-instance rather than a static, because the suite runs these
    /// in parallel.
    pub actions_calls: Arc<AtomicU64>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for FakeGithub {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Poke the listener so accept() returns.
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
    }
}

/// Spawn the fake on an ephemeral port. Answers
/// `POST /app/installations/{id}/access_tokens` with a fake installation
/// token when a Bearer JWT is presented; 401 otherwise.
pub fn spawn() -> FakeGithub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake github");
    let addr = listener.local_addr().unwrap();
    let tokens_issued = Arc::new(AtomicU64::new(0));
    let actions_calls = Arc::new(AtomicU64::new(0));
    let run_calls: RunCalls = Arc::new(Mutex::new(HashMap::new()));
    let runners = Arc::new(RunnerCalls::default());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let counter = tokens_issued.clone();
    let actions = actions_calls.clone();
    let per_repo = run_calls.clone();
    let runner_calls = runners.clone();
    let stop = shutdown.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(mut stream) = stream else { continue };
            let counter = counter.clone();
            let actions = actions.clone();
            let per_repo = per_repo.clone();
            let runner_calls = runner_calls.clone();
            let base = format!("http://{addr}");
            std::thread::spawn(move || {
                let _ = handle(
                    &mut stream,
                    &counter,
                    &actions,
                    &per_repo,
                    &runner_calls,
                    &base,
                );
            });
        }
    });
    FakeGithub {
        base_url: format!("http://{addr}"),
        tokens_issued,
        runners,
        actions_calls,
        shutdown,
    }
}

/// `base` is the address this fake is actually listening on.
///
/// It has to reach the `Link` header, because a client is right to refuse
/// a next-page URL pointing off the host it was configured with — handing
/// an installation token to whatever a redirect names is exactly the
/// mistake worth refusing. The first version wrote a `http://LOCAL`
/// placeholder, and a correct client refused to follow it: the fake being
/// wrong and the client being right.
fn handle(
    stream: &mut TcpStream,
    counter: &AtomicU64,
    actions_calls: &AtomicU64,
    run_calls: &RunCalls,
    runners: &RunnerCalls,
    base: &str,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_headers_end(&buf) {
            header_end = pos;
            break;
        }
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut has_bearer = false;
    let mut has_installation_token = false;
    // `Bearer ghu_<who>` is a *user* token from the OAuth exchange below:
    // who it names decides which installations `/user/installations`
    // lists, so a test can arrive as the wrong person on purpose.
    let mut user_token: Option<String> = None;
    let mut content_length = 0usize;
    for l in lines {
        let ll = l.to_ascii_lowercase();
        if let Some(v) = ll.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        if ll.starts_with("authorization:") && ll.contains("bearer ") {
            // The JWT itself is validated for shape only — the fake trusts
            // the provider's RS256 output (unit-verifiable separately).
            has_bearer = l.split_whitespace().last().map(|t| t.matches('.').count()) == Some(2);
        }
        // `token ghs_…` is what an installation token looks like on the
        // wire. Told apart from the JWT deliberately: a client that sent
        // the app JWT where an installation token belongs would work
        // against a fake that accepted either, and fail against GitHub.
        if ll.starts_with("authorization:") && ll.contains("token ghs_") {
            has_installation_token = true;
        }
        if ll.starts_with("authorization:") && ll.contains("bearer ghu_") {
            user_token = l
                .split_whitespace()
                .last()
                .and_then(|t| t.strip_prefix("ghu_"))
                .map(str::to_string);
        }
    }

    // Drain the request body before answering. A POST's headers and its
    // body are two writes on the client side, so they can land as two
    // segments; the loop above stops at `\r\n\r\n` and would leave the
    // body sitting unread in the receive queue. Closing a socket with
    // unread bytes queued makes TCP send an RST rather than a FIN, and on
    // macOS a reset connection then refuses `setsockopt` with EINVAL —
    // which is how a correct client reading a well-formed response died
    // with "Error encountered in a header: Invalid argument (os error
    // 22)" in one run out of four, only under full-workspace load, and
    // only because scheduling decided whether the two segments coalesced.
    let want = header_end + 4 + content_length;
    while buf.len() < want {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    let body_text = String::from_utf8_lossy(&buf[header_end + 4..]).to_string();

    // The self-hosted-runner routes first: they share the `/repos/`
    // prefix with the metadata route below, and that one matches
    // anything after it.
    if let Some((status, headers, body)) = runners_route(
        &request_line,
        has_installation_token,
        &body_text,
        runners,
        run_calls,
    ) {
        return write_response(stream, &status, &headers, &body);
    }

    // The issue-import routes, for the same reason.
    if let Some((status, headers, body)) =
        issue_route(&request_line, has_installation_token, counter, base)
    {
        return write_response(stream, &status, &headers, &body);
    }

    // Actions runs, for the same reason and with the same hazard: without
    // a route of its own, `/repos/acme/widget/actions/runs` falls through
    // to the metadata route below and answers a *repository* — a 200 with
    // a plausible JSON body, which is the worst possible answer to give a
    // client that is about to look for `workflow_runs` in it.
    if let Some((status, headers, body)) = actions_route(
        &request_line,
        has_installation_token,
        actions_calls,
        run_calls,
        base,
    ) {
        return write_response(stream, &status, &headers, &body);
    }

    let (status, body) = if request_line.starts_with("POST /login/oauth/access_token") {
        // The user-authorization exchange. A code is `code_owning_<id>`
        // — the person who controls exactly that installation — or
        // `code_for_<who>` for a named person; anything else is what
        // GitHub says to a bad or spent code: a 200 with an `error`
        // field, which a client checking only the status would miss.
        let code = body_text
            .split('&')
            .find_map(|kv| kv.strip_prefix("code="))
            .unwrap_or_default();
        // `code_silent` is "GitHub is not answering": the connection is
        // dropped without a byte, which is what a client sees as a
        // transport error. Installation 4006 does the same for the
        // installation-token call, and for the same reason — it is the
        // only way to produce that arm hermetically.
        if code == "code_silent" {
            return Ok(());
        }
        let client_id = body_text
            .split('&')
            .find_map(|kv| kv.strip_prefix("client_id="))
            .unwrap_or_default();
        // `code_of_<client_id>_owning_<id>`: a code GitHub minted for one
        // App's OAuth client. Real GitHub refuses it from any other
        // client with `bad_verification_code` — which is what production
        // said on 2026-09-15 when the Runners App's code reached the
        // mirror App's exchange — so the fake does too. A mismatch
        // empties the code, which falls through to that refusal below.
        let code = match code.strip_prefix("code_of_") {
            Some(rest) => match rest.split_once("_owning_") {
                Some((client, id)) if client == client_id => format!("code_owning_{id}"),
                _ => String::new(),
            },
            None => code.to_string(),
        };
        let code = code.as_str();
        // `code_owning_<id>` and `code_for_<who>` are the install
        // callback's shapes. `code_as_…`, `code_unverified_…`,
        // `code_noemail_…` and `code_badmail_…` are the sign-in ones,
        // and they keep their prefix in the token, because `fake_user`
        // reads it to decide who arrives.
        let who = if let Some(rest) = code.strip_prefix("code_owning_") {
            Some(format!("owning_{rest}"))
        } else if let Some(rest) = code.strip_prefix("code_for_") {
            Some(rest.to_string())
        } else {
            code.strip_prefix("code_")
                .filter(|rest| {
                    ["as_", "unverified_", "noemail_", "badmail_"]
                        .iter()
                        .any(|p| rest.starts_with(p))
                })
                .map(str::to_string)
        };
        if let Some(who) = who {
            (
                "200 OK",
                format!("{{\"access_token\":\"ghu_{who}\",\"token_type\":\"bearer\"}}"),
            )
        } else {
            (
                "200 OK",
                "{\"error\":\"bad_verification_code\",\"error_description\":\"The code passed is incorrect or expired.\"}"
                    .to_string(),
            )
        }
    } else if request_line.starts_with("GET /user/emails") {
        // Which of this person's addresses GitHub has itself proved.
        // The whole reason a sign-in through GitHub may skip our own
        // confirmation mail, so the fake is careful to answer the
        // *shape* GitHub answers: a flat list, `primary` and `verified`
        // as separate booleans, and a 403 — not an empty list — when
        // the App was never granted the `Email addresses` permission.
        // An empty list would have let a client that ignores the status
        // pass its tests and then read "no proved address" in
        // production for every person alive.
        match user_token.as_deref().map(fake_user) {
            Some(u) => match u.email {
                // BELIEF, and a shaky one: that an App which was never
                // granted `Email addresses` is refused with **403**.
                // Nothing in GitHub's documentation says so, and the
                // first real probe pointed the other way — a token
                // missing the `user` scope is answered **404 Not
                // Found**, which is GitHub's habit for "you may not
                // know whether this exists". That is a different
                // mechanism (token scope, not App permission), so it
                // does not settle this arm; it does mean the 403 here
                // is unverified. `scripts/manual-github-signin.sh
                // noperm` accepts either and records which arrives.
                //
                // What the product does NOT depend on: it maps every
                // non-2xx to `None` alike. What it DOES depend on is
                // that this is not a 200 carrying an empty array — a
                // client reading that as "no proved address" would be
                // wrong for every person alive.
                EmailMode::Forbidden => (
                    "403 Forbidden",
                    "{\"message\":\"Resource not accessible by integration\"}".to_string(),
                ),
                EmailMode::Malformed => (
                    "200 OK",
                    "[{\"email\":\"not-an-address\",\"primary\":true,\
                      \"verified\":true,\"visibility\":\"private\"}]"
                        .to_string(),
                ),
                mode => (
                    "200 OK",
                    format!(
                        "[{{\"email\":\"{}@example.com\",\"primary\":true,\"verified\":{},\
                          \"visibility\":\"private\"}},\
                         {{\"email\":\"{}@users.noreply.github.com\",\"primary\":false,\
                          \"verified\":true,\"visibility\":null}}]",
                        u.login,
                        matches!(mode, EmailMode::Verified),
                        u.login
                    ),
                ),
            },
            None => (
                "401 Unauthorized",
                "{\"message\":\"Requires authentication\"}".to_string(),
            ),
        }
    } else if request_line.starts_with("GET /user ")
        || request_line.starts_with("GET /user?")
        || request_line.starts_with("GET /user\r")
    {
        // The same silence, one call later: the token exchange worked
        // and the identity read did not. A different arm in the caller,
        // so it needs its own way in.
        if user_token.as_deref() == Some("silent") {
            return Ok(());
        }
        // Who the token belongs to. `id` is the immutable half and the
        // only half anything may be keyed on; `login` is renameable,
        // which is exactly what one test renames.
        match user_token.as_deref().map(fake_user) {
            Some(u) => (
                "200 OK",
                format!(
                    "{{\"id\":{},\"login\":\"{}\",\"name\":\"{}\",\"type\":\"User\"}}",
                    u.id, u.login, u.name
                ),
            ),
            None => (
                "401 Unauthorized",
                "{\"message\":\"Requires authentication\"}".to_string(),
            ),
        }
    } else if request_line.starts_with("GET /user/installations") {
        // What the person behind a user token may reach. `owning_<id>`
        // controls that installation alone; `acme` controls the App's
        // first, `ada` its second, and anybody else nothing.
        match user_token.as_deref() {
            Some(who) => {
                let ids: Vec<&str> = match who {
                    "acme" => vec!["4001"],
                    "ada" => vec!["4002"],
                    w => match w.strip_prefix("owning_") {
                        Some(id) if id == "4001" || id == "4002" => vec![id],
                        _ => vec![],
                    },
                };
                let items: Vec<String> = ids
                    .iter()
                    .map(|id| {
                        let login = if *id == "4001" { "acme-inc" } else { "ada" };
                        format!("{{\"id\":{id},\"account\":{{\"login\":\"{login}\"}}}}")
                    })
                    .collect();
                (
                    "200 OK",
                    format!(
                        "{{\"total_count\":{},\"installations\":[{}]}}",
                        items.len(),
                        items.join(",")
                    ),
                )
            }
            None => (
                "401 Unauthorized",
                "{\"message\":\"Requires authentication\"}".to_string(),
            ),
        }
    } else if request_line.starts_with("POST /app/installations/")
        && request_line.contains("/access_tokens")
    {
        // Installation 4006 is "GitHub is not answering": the connection
        // is dropped without a byte, which is what a client sees as a
        // transport error — the arm every runner call has for an
        // unreachable API, and the only way to produce it hermetically.
        if request_line.contains("/4006/") {
            return Ok(());
        }
        if has_bearer {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            (
                "201 Created",
                format!("{{\"token\":\"ghs_fake_{n}\",\"expires_at\":\"2099-01-01T00:00:00Z\"}}"),
            )
        } else {
            ("401 Unauthorized", "{\"message\":\"bad jwt\"}".to_string())
        }
    } else if let Some(id) = request_line
        .strip_prefix("GET /app/installations/")
        .and_then(|r| r.split_whitespace().next())
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    {
        // One installation, as the App sees it: who it is installed on,
        // whether that is a person or an organisation, and what it may
        // do. `4001` is an organisation and `4002` a person, both holding
        // what the runner feature needs; `4003` is an organisation that
        // installed the App before the feature existed and has not
        // approved the new permissions, which is how a real installation
        // that predates a manifest change looks.
        if !has_bearer {
            ("401 Unauthorized", "{\"message\":\"bad jwt\"}".to_string())
        } else {
            match id {
                "4001" => (
                    "200 OK",
                    installation_json(4001, "acme-inc", "Organization", Approved::Full),
                ),
                "4002" => (
                    "200 OK",
                    installation_json(4002, "ada", "User", Approved::Full),
                ),
                "4003" => (
                    "200 OK",
                    installation_json(4003, "noadmin-inc", "Organization", Approved::Original),
                ),
                // Approved for runners, never for pushing: the shape an
                // installation made before write-through mirrors has.
                "4007" => (
                    "200 OK",
                    installation_json(4007, "prepush-inc", "Organization", Approved::Runners),
                ),
                // One the App may not read (a suspended App, say), and
                // one GitHub is rate-limiting: the two non-404 refusals
                // `GithubApp::installation` has to tell a person apart.
                "4004" => {
                    return write_response(
                        stream,
                        "403 Forbidden",
                        &[
                            ("X-RateLimit-Limit".into(), "5000".into()),
                            ("X-RateLimit-Remaining".into(), "4999".into()),
                        ],
                        r#"{"message":"Resource not accessible by integration"}"#,
                    )
                }
                "4005" => {
                    return write_response(
                        stream,
                        "403 Forbidden",
                        &[
                            ("Retry-After".into(), "7".into()),
                            ("X-RateLimit-Remaining".into(), "0".into()),
                        ],
                        r#"{"message":"API rate limit exceeded"}"#,
                    )
                }
                _ => ("404 Not Found", "{\"message\":\"Not Found\"}".to_string()),
            }
        }
    } else if request_line.starts_with("GET /app/installations") {
        // The App listing its own installations, authenticated with the
        // app JWT. Two of them, because one is not enough to catch a
        // client that ignores the id it was asked about.
        if has_bearer {
            (
                "200 OK",
                r#"[{"id":4001,"account":{"login":"acme-inc"}},
                    {"id":4002,"account":{"login":"ada"}}]"#
                    .to_string(),
            )
        } else {
            ("401 Unauthorized", "{\"message\":\"bad jwt\"}".to_string())
        }
    } else if request_line.starts_with("GET /installation/repositories") {
        // What an installation may read. Authenticated with an
        // *installation* token, not the JWT — `token ghs_…`, which is
        // the header the provider must send and a different shape from
        // the one above.
        if has_installation_token {
            ("200 OK", repositories_page(&request_line))
        } else {
            (
                "401 Unauthorized",
                "{\"message\":\"requires installation token\"}".to_string(),
            )
        }
    } else if let Some(full_name) = request_line
        .strip_prefix("GET /repos/")
        .and_then(|r| r.split_whitespace().next())
    {
        // One repository's public metadata, unauthenticated — what the
        // mirror path reads to learn the upstream's own star count.
        //
        // `stars-none/*` answers a body with no `stargazers_count` at
        // all, and `private/*` answers 404. Both exist so a test can
        // tell the three outcomes apart: a number we were told, a field
        // the origin did not send, and an origin that refused us. All
        // three must end as different states, and only the first may
        // ever produce a count.
        let (owner, name) = full_name.split_once('/').unwrap_or((full_name, "repo"));
        // Two ways to be a 404, sharing an answer because GitHub gives
        // them the same one.
        //
        // `private/*` is the fixture: a repository we are not allowed to
        // be told about. The `name.contains('/')` half is a guard — this
        // route matches *everything* under `/repos/`, so without it a
        // path the fake does not serve (`…/actions/workflows`, `…/pulls`)
        // was answered with a **200 carrying a repository**. That is the
        // worst available answer: a client looking for a list in that
        // body finds none and reports "this project has no CI" rather
        // than "the fake does not serve this route".
        if name.contains('/') || owner == "private" {
            ("404 Not Found", "{\"message\":\"Not Found\"}".to_string())
        } else if owner == "stars-none" {
            (
                "200 OK",
                format!(
                    "{{\"full_name\":\"{full_name}\",\"private\":false,\
                       \"default_branch\":\"main\",\"description\":\"no count here\"}}"
                ),
            )
        } else {
            // 60300 is the number from the product argument: a mirrored
            // project's real reputation, which must appear beside our
            // own count and never inside it.
            let stars = if name == "quiet" { 0 } else { 60_300 };
            (
                "200 OK",
                format!(
                    "{{\"full_name\":\"{full_name}\",\"private\":false,\
                       \"default_branch\":\"main\",\"description\":\"a mirrored project\",\
                       \"size\":128,\"stargazers_count\":{stars}}}"
                ),
            )
        }
    } else {
        ("404 Not Found", "{}".to_string())
    };
    write_response(stream, status, &[], &body)
}

/// Whether `GET /user/emails` proves this person's primary address.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EmailMode {
    /// Primary and verified: the ordinary case, and the only one a
    /// sign-in may trust an address from.
    Verified,
    /// Primary, and GitHub has *not* proved it. Somebody who typed an
    /// address into their profile and never clicked the link.
    Unverified,
    /// The App may not read addresses at all — it was never granted the
    /// `Email addresses` permission, or this person declined it.
    Forbidden,
    /// Primary, verified, and **not an address**. A provider answering
    /// something unstorable is not hypothetical — a client that assumed
    /// otherwise would push the refusal down into `users::create` and
    /// surface it as "something went wrong".
    Malformed,
}

/// The person a `ghu_…` user token names.
struct FakeUser {
    id: i64,
    login: String,
    name: String,
    email: EmailMode,
}

/// Read a user token's payload as a person.
///
/// The token is whatever the OAuth exchange minted, so a test chooses
/// who arrives by choosing the `code` it sends:
///
/// * `code_as_<id>_<login>` — proved primary address, the ordinary case.
/// * `code_unverified_<id>_<login>` — primary address, unproved.
/// * `code_noemail_<id>_<login>` — `GET /user/emails` answers 403.
///
/// The id is spelled separately from the login on purpose: a login is
/// renameable on GitHub, so a test can send `code_as_501_ada` and later
/// `code_as_501_ada-lovelace` and assert the same account is reached.
///
/// Anything else — `code_for_acme`, `code_owning_4001`, the shapes the
/// install callback's tests already use — is that login with a derived
/// id, so those tokens keep answering these routes rather than 404ing
/// into a test that reads as a product failure.
fn fake_user(who: &str) -> FakeUser {
    for (prefix, email) in [
        ("as_", EmailMode::Verified),
        ("unverified_", EmailMode::Unverified),
        ("noemail_", EmailMode::Forbidden),
        ("badmail_", EmailMode::Malformed),
    ] {
        if let Some(rest) = who.strip_prefix(prefix) {
            if let Some((id, login)) = rest.split_once('_') {
                if let Ok(id) = id.parse::<i64>() {
                    return FakeUser {
                        id,
                        login: login.to_string(),
                        name: format!("Person {login}"),
                        email,
                    };
                }
            }
        }
    }
    FakeUser {
        // A stand-in that is stable for a given login and cannot collide
        // with the ids tests spell out, which are small.
        id: 900_000 + who.bytes().map(i64::from).sum::<i64>(),
        login: who.to_string(),
        name: format!("Person {who}"),
        email: EmailMode::Verified,
    }
}

/// One installation's detail, as `GET /app/installations/{id}` answers
/// it. `full` is an installation holding everything the GitHub-runner
/// feature needs — `administration: write` for the just-in-time runner
/// registration and `actions: write` for cancelling a run we refused —
/// and `!full` is one that has only what the App asked for before that
/// feature existed.
/// Which generation of the App's manifest an installation approved.
///
/// `Full` holds everything the App asks for today. `Runners` approved
/// `Administration: write` and `Actions: write` but predates
/// `Contents: write` — the shape every installation made between the
/// runner feature and write-through mirrors has, until its owner
/// approves again. `Original` predates both.
#[derive(Clone, Copy)]
enum Approved {
    Full,
    Runners,
    Original,
}

fn installation_json(id: u64, login: &str, target_type: &str, approved: Approved) -> String {
    let (perms, events) = match approved {
        Approved::Full => (
            r#"{"actions":"write","administration":"write","contents":"write","issues":"read","metadata":"read"}"#,
            r#"["push","workflow_job"]"#,
        ),
        Approved::Runners => (
            r#"{"actions":"write","administration":"write","contents":"read","issues":"read","metadata":"read"}"#,
            r#"["push","workflow_job"]"#,
        ),
        Approved::Original => (
            r#"{"actions":"read","contents":"read","issues":"read","metadata":"read"}"#,
            r#"["push"]"#,
        ),
    };
    format!(
        "{{\"id\":{id},\"account\":{{\"login\":\"{login}\",\"type\":\"{target_type}\"}},\
           \"target_type\":\"{target_type}\",\"permissions\":{perms},\"events\":{events},\
           \"suspended_at\":null}}"
    )
}

/// The self-hosted-runner routes: register a just-in-time runner,
/// remove one, cancel a run. Answered before the general dispatch.
///
/// Returns `None` for a request this does not handle.
///
/// What the fake answers here was checked against the real API by
/// `scripts/manual-github-runners.sh` on 2026-09-07, and two of its
/// beliefs did not survive: a just-in-time runner carries **only** the
/// labels it was asked for (lowercased) — no default `self-hosted`,
/// `linux` or `x64` — and a 422 is a `message` with no `errors` list,
/// while a label with a space is accepted. Cancelling an already
/// finished run answering **409** did hold. What is here now is what
/// was observed.
fn runners_route(
    request_line: &str,
    has_installation_token: bool,
    body_text: &str,
    runners: &RunnerCalls,
    refusals: &RunCalls,
) -> Option<Answer> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    let rest = path.strip_prefix("/repos/")?;
    let mut seg = rest.split('/');
    let owner = seg.next()?;
    let repo = seg.next()?;
    if seg.next()? != "actions" {
        return None;
    }
    let kind = seg.next()?;
    let tail: Vec<&str> = seg.collect();
    let call = match (method, kind, tail.as_slice()) {
        ("POST", "runners", ["generate-jitconfig"]) => "jit",
        ("DELETE", "runners", [_id]) => "delete",
        ("POST", "runs", [_id, "cancel"]) => "cancel",
        // What GitHub says one job is doing. The dispatcher asks this
        // about jobs its own row still calls `running`, because a
        // `workflow_job` completion can be missed and a row that keeps
        // saying `running` keeps a task alive and holds a slot in the
        // organisation's concurrency cap.
        ("GET", "jobs", [_id]) => "job",
        _ => return None,
    };

    if !has_installation_token {
        return Some((
            "401 Unauthorized".into(),
            vec![],
            r#"{"message":"requires installation token"}"#.into(),
        ));
    }

    // The two rate limits, alternating per repository so one test sees
    // both the refusal and the recovery — the same schedule and the
    // same argument as `actions_route`, on a counter of its own so the
    // two routes' schedules cannot shift each other.
    let n = refusals
        .lock()
        .unwrap()
        .entry(format!("runners:{owner}/{repo}"))
        .and_modify(|c| *c += 1)
        .or_insert(1)
        .to_owned();
    if !n.is_multiple_of(2) {
        if owner == "ratelimited" {
            return Some((
                "403 Forbidden".into(),
                vec![
                    ("Retry-After".into(), "2".into()),
                    ("X-RateLimit-Remaining".into(), "0".into()),
                ],
                r#"{"message":"API rate limit exceeded"}"#.into(),
            ));
        }
        if owner == "budgetspent" {
            let reset = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                + 120;
            return Some((
                "403 Forbidden".into(),
                vec![
                    ("X-RateLimit-Limit".into(), "5000".into()),
                    ("X-RateLimit-Remaining".into(), "0".into()),
                    ("X-RateLimit-Reset".into(), reset.to_string()),
                ],
                r#"{"message":"API rate limit exceeded for installation ID 777."}"#.into(),
            ));
        }
    }

    // A job's state, keyed off the repository name so a test can choose
    // the answer it needs:
    //
    //   `finished/*`  — completed/success: the ordinary case where our
    //                   completion delivery went missing;
    //   `cancelled/*` — completed/cancelled: a run superseded by the
    //                   next push, whose runner must be stopped;
    //   `gone/*`      — 404, a job deleted with its run;
    //   `garbled/*`   — 200 with a body that is not JSON: an answer the
    //                   dispatcher cannot read, which must leave the job
    //                   alone and be asked again next pass;
    //   `waiting/*`, `nodelete/*` — queued: no runner has taken it;
    //   anything else — still in_progress, which must be left alone.
    //
    // `conclusion` is null while a job is in progress, which is how
    // GitHub sends it and why the caller reads the two fields
    // separately rather than trusting one.
    if call == "job" {
        let id: i64 = tail.first().and_then(|t| t.parse().ok()).unwrap_or(0);
        runners
            .job_reads
            .lock()
            .unwrap()
            .push(format!("{owner}/{repo}#{id}"));
        let pinned = runners.job_states.lock().unwrap().get(&id).cloned();
        let (status, conclusion) = match (pinned.as_ref(), owner) {
            (Some((s, c)), _) => (s.as_str(), c.as_str()),
            (None, "finished") => ("completed", "\"success\""),
            (None, "cancelled") => ("completed", "\"cancelled\""),
            // A job no runner has taken. This is what an idle runner's
            // job looks like from GitHub's side, and `nodelete/*` is
            // the idle sweep's own repository: a runner that never got
            // a job and cannot be removed, so both answers belong to it.
            (None, "waiting" | "nodelete") => ("queued", "null"),
            (None, "gone") => {
                return Some((
                    "404 Not Found".into(),
                    vec![],
                    r#"{"message":"Not Found"}"#.into(),
                ))
            }
            (None, "garbled") => {
                return Some(("200 OK".into(), vec![], "<html>not json</html>".into()))
            }
            // A secondary rate limit: 403 carrying `Retry-After`, which
            // is the shape that tells it from a permission refusal.
            // Unconditional, unlike `ratelimited/*` above, which
            // alternates on a call counter the dispatcher also bumps —
            // a caller asking about one job needs an answer that does
            // not depend on how many other calls got there first.
            (None, "jobbusy") => {
                return Some((
                    "403 Forbidden".into(),
                    vec![
                        ("Retry-After".into(), "2".into()),
                        ("X-RateLimit-Remaining".into(), "0".into()),
                    ],
                    r#"{"message":"You have exceeded a secondary rate limit"}"#.into(),
                ))
            }
            // A plain refusal: 403 with no rate-limit headers at all,
            // which is what an installation that has lost `Actions:
            // read` is answered with.
            (None, "jobdenied") => {
                return Some((
                    "403 Forbidden".into(),
                    vec![],
                    r#"{"message":"Resource not accessible by integration"}"#.into(),
                ))
            }
            _ => ("in_progress", "null"),
        };
        return Some((
            "200 OK".into(),
            vec![],
            format!(
                r#"{{"id":{id},"status":"{status}","conclusion":{conclusion},"name":"build"}}"#
            ),
        ));
    }

    // A permission the installation does not hold. A 403 **with a
    // budget still on it**, which is what tells it from the primary rate
    // limit above; `noadmin/*` lacks `administration: write` and
    // `noactionswrite/*` lacks `actions: write`, which are different
    // permissions refused by different routes.
    let denied = |what: &str| {
        Some((
            "403 Forbidden".into(),
            vec![
                ("X-RateLimit-Limit".into(), "5000".into()),
                ("X-RateLimit-Remaining".into(), "4999".into()),
            ],
            format!(r#"{{"message":"Resource not accessible by integration ({what})"}}"#),
        ))
    };
    let not_found = || {
        Some((
            "404 Not Found".into(),
            vec![],
            r#"{"message":"Not Found"}"#.into(),
        ))
    };

    match call {
        "jit" => {
            if owner == "noadmin" {
                return denied("administration: write");
            }
            if owner == "private" {
                return not_found();
            }
            let body: serde_json::Value = serde_json::from_str(body_text).unwrap_or_default();
            runners.jit_requests.lock().unwrap().push(body.clone());
            let name = body["name"].as_str().unwrap_or_default().to_string();
            let asked: Vec<String> = body["labels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            // GitHub's 422s, as observed by the manual gate on
            // 2026-09-07: a `message` and nothing else — no `errors`
            // list — and a label with a space in it is *accepted*. Only
            // an empty list and an over-long label are refused.
            if asked.is_empty() {
                return Some((
                    "422 Unprocessable Entity".into(),
                    vec![],
                    r#"{"message":"Invalid request.\n\nInvalid property /labels: 1 item required; only 0 were supplied.","status":"422"}"#.into(),
                ));
            }
            if name.is_empty() || body["runner_group_id"].as_i64().is_none() {
                return Some((
                    "422 Unprocessable Entity".into(),
                    vec![],
                    r#"{"message":"Invalid request.\n\nInvalid property /name: required.","status":"422"}"#.into(),
                ));
            }
            if let Some(long) = asked.iter().find(|l| l.len() >= 256) {
                return Some((
                    "422 Unprocessable Entity".into(),
                    vec![],
                    format!(
                        r#"{{"message":"Invalid Argument - Label '{long}' is not valid. Labels must be less than 256 characters in length","status":"422"}}"#
                    ),
                ));
            }
            let id = 500 + runners.seq.fetch_add(1, Ordering::SeqCst);
            // Exactly the labels asked for, lowercased and deduplicated,
            // every one `read-only` with id 0 — what the real API
            // answered on 2026-09-07. It attaches **no** default
            // `self-hosted`/`linux`/`x64`; the fake once believed it did,
            // and a runner registered on that belief would never have
            // taken a `runs-on: [self-hosted, weft]` job.
            let mut labels: Vec<String> = Vec::new();
            for l in asked.iter().map(|l| l.to_ascii_lowercase()) {
                if !labels.contains(&l) {
                    labels.push(l);
                }
            }
            let labels_json: Vec<String> = labels
                .iter()
                .map(|l| format!(r#"{{"id":0,"name":"{l}","type":"read-only"}}"#))
                .collect();
            let runner_file = format!(r#"{{"agentName":"{name}","ephemeral":true}}"#);
            let cfg = base64_std(
                serde_json::json!({ ".runner": base64_std(runner_file.as_bytes()) })
                    .to_string()
                    .as_bytes(),
            );
            Some((
                "201 Created".into(),
                vec![],
                format!(
                    r#"{{"runner":{{"id":{id},"name":"{name}","os":"unknown","status":"offline","busy":false,"version":"2.337.0","labels":[{}],"runner_group_id":1}},"encoded_jit_config":"{cfg}"}}"#,
                    labels_json.join(",")
                ),
            ))
        }
        "delete" => {
            let id = tail[0];
            if id == "0" {
                return not_found();
            }
            // A repository whose runners the installation may register
            // but not remove — the refusal `delete_runner` logs and
            // otherwise ignores, the runner being ephemeral anyway.
            if owner == "nodelete" {
                return denied("administration: write (delete)");
            }
            runners
                .runner_deletes
                .lock()
                .unwrap()
                .push(format!("{owner}/{repo}#{id}"));
            Some(("204 No Content".into(), vec![], String::new()))
        }
        _ => {
            let id = tail[0];
            if owner == "noactionswrite" {
                return denied("actions: write");
            }
            // A run the installation cannot see: the refusal that is
            // neither a permission nor "already over".
            if owner == "private" {
                return not_found();
            }
            if id == "0" {
                // BELIEF: a run that is already over. Recorded by the
                // manual gate.
                return Some((
                    "409 Conflict".into(),
                    vec![],
                    r#"{"message":"Cannot cancel a workflow run that is completed."}"#.into(),
                ));
            }
            runners
                .cancels
                .lock()
                .unwrap()
                .push(format!("{owner}/{repo}#{id}"));
            Some(("202 Accepted".into(), vec![], "{}".into()))
        }
    }
}

/// Standard base64 with padding — what `encoded_jit_config` is.
fn base64_std(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The issue-import routes, answered before the general dispatch above.
///
/// Returns `None` for a request this does not handle, so the original
/// routes keep working untouched.
/// One HTTP answer: status line, extra headers, body.
type Answer = (String, Vec<(String, String)>, String);

fn issue_route(
    request_line: &str,
    has_installation_token: bool,
    calls: &AtomicU64,
    base: &str,
) -> Option<Answer> {
    let path = request_line.split_whitespace().nth(1)?;
    let rest = path.strip_prefix("/repos/")?;
    let (full, query) = match rest.split_once('?') {
        Some((f, q)) => (f, q),
        None => (rest, ""),
    };
    let mut seg = full.split('/');
    let owner = seg.next()?;
    let repo = seg.next()?;
    let kind = seg.next()?;
    if kind != "issues" && kind != "labels" && kind != "milestones" {
        return None;
    }

    // Issues need the installation token, exactly as the real API does.
    // Answering without one would let a client that forgot the header
    // pass here and fail against GitHub.
    if !has_installation_token {
        return Some((
            "401 Unauthorized".into(),
            vec![],
            r#"{"message":"requires installation token"}"#.into(),
        ));
    }

    // An installation without `issues: read`. **403 with GitHub's own
    // wording**, not an empty list: an import that quietly produces
    // nothing looks exactly like a project that never had issues, and
    // that is the failure the design calls out by name.
    if owner == "noperm" {
        return Some((
            "403 Forbidden".into(),
            vec![],
            r#"{"message":"Resource not accessible by integration"}"#.into(),
        ));
    }

    let param = |name: &str, default: usize| -> usize {
        query
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let state = query
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .unwrap_or("open");
    let per_page = param("per_page", 30).clamp(1, 100);
    let page = param("page", 1).max(1);

    // Rate limiting, which is the shape of a real import rather than an
    // edge case: 5,000 requests an hour against a repository with
    // thousands of issues and their comments. `ratelimited/*` refuses
    // every third call with GitHub's 403 + `Retry-After`, so an importer
    // that ignores the header spins and one that honours it finishes.
    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;

    // **A rate limit that lands on a named phase.**
    //
    // `ratelimited/*` above counts calls, and an import walks labels,
    // then milestones, then issues — so the every-third rule always
    // refuses the *issues* page and never the two before it. Those two
    // arms of the importer could not be reached from here at all, which
    // is precisely how they went untested: the suite looked as though it
    // covered rate limiting because one of the three arms did.
    //
    // A budget does not care which endpoint spends it. These owners say
    // which phase gets the refusal, so each arm is reachable on its own
    // and the test names the case rather than counting on arithmetic.
    if matches!(
        (owner, kind),
        ("ratelimited-labels", "labels") | ("ratelimited-milestones", "milestones")
    ) {
        return Some((
            "403 Forbidden".into(),
            vec![
                ("Retry-After".into(), "1".into()),
                ("X-RateLimit-Remaining".into(), "0".into()),
            ],
            r#"{"message":"API rate limit exceeded"}"#.into(),
        ));
    }

    // Refusals that land on a **named phase**, for the same reason the
    // rate limits above do. `noperm/*` refuses the very first call an
    // import makes — labels — so every later arm that translates a
    // refusal sat behind a phase that had already stopped. A permission
    // can be lost between two calls of one run, and a provider can
    // refuse one endpoint and not another.
    // `full`, not `path`: the query string is still on `path`, so
    // `ends_with("/comments")` is false for every real request.
    let comments_path = kind == "issues" && full.ends_with("/comments");
    if (owner == "noperm-issues" && kind == "issues" && !comments_path)
        || (owner == "noperm-comments" && comments_path)
    {
        return Some((
            "403 Forbidden".into(),
            vec![],
            r#"{"message":"Resource not accessible by integration"}"#.into(),
        ));
    }

    // A refusal that is **not** the missing-permission one. GitHub says
    // "Resource not accessible by integration" for that, and the import
    // translates it into the sentence about `issues: read`; anything else
    // has to keep its status and body, or an operator reading the job
    // gets told to grant a permission that was never the problem.
    if owner == "brokenupstream" {
        return Some((
            "500 Internal Server Error".into(),
            vec![],
            r#"{"message":"Server Error"}"#.into(),
        ));
    }

    if owner == "ratelimited" && n.is_multiple_of(3) {
        return Some((
            "403 Forbidden".into(),
            vec![
                ("Retry-After".into(), "1".into()),
                ("X-RateLimit-Remaining".into(), "0".into()),
            ],
            r#"{"message":"API rate limit exceeded"}"#.into(),
        ));
    }

    // Comments on one issue: /repos/{o}/{r}/issues/{n}/comments
    let tail: Vec<&str> = seg.collect();
    if kind == "issues" && tail.len() == 2 && tail[1] == "comments" {
        let num: usize = tail[0].parse().ok()?;
        // Paged, and the next page is named in `Link` — the same rule as
        // the issue list, because it is the same API. A conversation that
        // fits in one response never walks the importer's second lap.
        let total = comment_count(owner, num);
        let last = total.div_ceil(per_page).max(1);
        let mut headers = rate_headers(owner, n);
        if page < last {
            headers.push((
                "Link".into(),
                format!(
                    "<{base}/repos/{owner}/{repo}/issues/{num}/comments?per_page={per_page}&page={}>; rel=\"next\"",
                    page + 1
                ),
            ));
        }
        return Some((
            "200 OK".into(),
            headers,
            comments_json(owner, repo, num, per_page, page),
        ));
    }
    if kind == "labels" {
        // `messy/*` is a real export's rough edges rather than a tidy
        // fixture: a label with no `name` at all, and the same name
        // twice. Both are things GitHub has sent and both are things the
        // importer is written to skip — and neither could be produced
        // here, so the skips were untested and one of them could have
        // been a duplicate-key failure that stopped a whole migration.
        let body = if owner == "messy" {
            r#"[{"color":"ededed","description":"a label with no name"},
                {"name":"bug","color":"d73a4a","description":"something is broken"},
                {"name":"bug","color":"00ff00","description":"the same name again"}]"#
        } else {
            r#"[{"name":"bug","color":"d73a4a","description":"something is broken"},
                {"name":"good first issue","color":"7057ff","description":"a gentle way in"}]"#
        };
        return Some(("200 OK".into(), rate_headers(owner, n), body.into()));
    }
    if kind == "milestones" {
        // Likewise: a milestone with no title, and a closed one. The
        // closed arm decides a stored state and had never been taken, so
        // every imported milestone was open whatever the origin said.
        let body = if owner == "messy" {
            r#"[{"number":9,"description":"no title on this one","state":"open"},
                {"number":1,"title":"v1.0","description":"the first one",
                 "state":"open","due_on":"2024-12-01T00:00:00Z"},
                {"number":2,"title":"v0.9","description":"already shipped",
                 "state":"closed","due_on":"2024-01-01T00:00:00Z"}]"#
        } else {
            r#"[{"number":1,"title":"v1.0","description":"the first one",
                 "state":"open","due_on":"2024-12-01T00:00:00Z"}]"#
        };
        return Some(("200 OK".into(), rate_headers(owner, n), body.into()));
    }
    if kind != "issues" || !tail.is_empty() {
        return None;
    }

    // The issue list, paged. `Link` is the only place the next page is
    // named — a client that guesses page numbers works here and breaks
    // on a repository whose last page happens to be full.
    let total = issue_count(owner);
    let shown = match state {
        "open" => total - total / 4,
        "closed" => total / 4,
        _ => total,
    };
    let last = shown.div_ceil(per_page).max(1);
    let mut headers = rate_headers(owner, n);
    if page < last {
        headers.push((
            "Link".into(),
            format!(
                "<{base}/repos/{owner}/{repo}/issues?state={state}&per_page={per_page}&page={}>; rel=\"next\",                  <{base}/repos/{owner}/{repo}/issues?state={state}&per_page={per_page}&page={last}>; rel=\"last\"",
                page + 1
            ),
        ));
    }
    Some((
        "200 OK".into(),
        headers,
        issues_page(owner, repo, per_page, page, state),
    ))
}

/// `GET /repos/{owner}/{repo}/actions/runs`, paged exactly as GitHub pages
/// it.
///
/// The verdicts a Checks tab aggregates arrive here, and nothing about
/// them is a single shape: a run is `status` *and* `conclusion`, two
/// fields whose combination is the state, and the fixture deliberately
/// walks every combination a real repository produces — including one
/// conclusion this codebase has never heard of. GitHub has added
/// conclusions before (`skipped`, `stale`, `action_required` all arrived
/// after the endpoint shipped) and will again; a client that meets its
/// first unknown one in production, rather than here, either panics or
/// quietly calls it green.
///
/// Kept out of `issue_route` rather than folded into it: that function's
/// rate-limit counter refuses every third call for *every* owner, so a
/// paging test sharing it ends up testing the refusal schedule. Rate
/// limiting is here too, but keyed on the owner name — `ratelimited/*`
/// alone — so `many/*` pages without ever meeting a 403.
fn actions_route(
    request_line: &str,
    has_installation_token: bool,
    calls: &AtomicU64,
    refusals: &RunCalls,
    base: &str,
) -> Option<Answer> {
    let path = request_line.split_whitespace().nth(1)?;
    let rest = path.strip_prefix("/repos/")?;
    let (full, query) = match rest.split_once('?') {
        Some((f, q)) => (f, q),
        None => (rest, ""),
    };
    let mut seg = full.split('/');
    let owner = seg.next()?;
    let repo = seg.next()?;
    if seg.next()? != "actions" {
        return None;
    }
    // Only the run list. `/actions/workflows`, `/actions/jobs/{id}` and
    // the rest of that namespace are not served, and answering them with
    // runs would let a client ask for the wrong thing and pass.
    if seg.next()? != "runs" || seg.next().is_some() {
        return None;
    }

    if !has_installation_token {
        return Some((
            "401 Unauthorized".into(),
            vec![],
            r#"{"message":"requires installation token"}"#.into(),
        ));
    }

    // An installation without `actions: read`. GitHub's own wording, and
    // a 403 rather than an empty `workflow_runs` array, because those two
    // answers are told apart nowhere else: a project with no CI and a
    // project we are not allowed to see the CI of look identical to a
    // client that only counts runs. The Checks tab has to say which.
    if owner == "noperm" {
        return Some((
            "403 Forbidden".into(),
            // With a budget still on it, which is what GitHub sends and
            // what makes this the hard case: the refusal a rate limit
            // has to be told apart from is *also* a 403 carrying
            // rate-limit headers. Only a remaining of **zero** means
            // "come back later". Sending no headers here would let a
            // client that keys on their mere presence pass.
            vec![
                ("X-RateLimit-Limit".into(), "5000".into()),
                ("X-RateLimit-Remaining".into(), "4999".into()),
            ],
            r#"{"message":"Resource not accessible by integration"}"#.into(),
        ));
    }

    // A spent budget, which on this endpoint is the *other* 403 — and the
    // one that must not be read as the missing permission above. An
    // aggregator that keys on the status alone tells a person to go and
    // grant a permission they already granted, and stops polling.
    //
    // `ratelimited/*` refuses every other call, so one test covers both
    // halves of the contract: that a client backs off, and that it comes
    // back and finishes. Refusing every call would only ever prove the
    // first, and a poller that backed off for ever would pass it.
    //
    // Counted **per repository**, not per fake. The first version shared
    // one counter with `actions_calls` below, which made the parity
    // depend on how many runs *other* repositories had been read in the
    // same `spawn()` — so a second test in the same world, or a poller
    // that swept two repositories, would find the first call already
    // recovered and the whole schedule off by one. A refusal schedule
    // that depends on what somebody else asked for is not a schedule
    // anyone can write a test against.
    // Every request that reaches the fixture, refusals included. This one
    // is the fake's own instrumentation rather than its behaviour: it is
    // what lets a test assert an incremental poll stops *asking*, which is
    // the half of "incremental" that actually saves a rate budget — a poll
    // that still fetches every page and merely writes less has saved
    // nothing.
    calls.fetch_add(1, Ordering::SeqCst);
    let n = refusals
        .lock()
        .unwrap()
        .entry(format!("{owner}/{repo}"))
        .and_modify(|c| *c += 1)
        .or_insert(1)
        .to_owned();
    if !n.is_multiple_of(2) {
        // GitHub has **two** rate limits and spells them differently, so
        // the fake has to be able to say both. A client that reads only
        // one of them classifies the other as a permission denial, tells
        // a maintainer to grant something they already granted, and
        // stops polling for good.
        //
        // `ratelimited/*` is the **secondary** limit — too much, too
        // fast — which carries `Retry-After`, a duration.
        if owner == "ratelimited" {
            return Some((
                "403 Forbidden".into(),
                vec![
                    ("Retry-After".into(), "2".into()),
                    ("X-RateLimit-Remaining".into(), "0".into()),
                ],
                r#"{"message":"API rate limit exceeded"}"#.into(),
            ));
        }
        // `budgetspent/*` is the **primary** limit — the hourly budget
        // gone — which carries a remaining of zero and a reset that is an
        // absolute unix timestamp, and **no `Retry-After` whatever**.
        // Until this fixture existed there was no way in the tree to
        // produce that response, so the test named for exactly this
        // confusion passed against a client that could not tell the two
        // apart. A fixture that can only produce the easy half of a
        // distinction is what makes a test decoration.
        if owner == "budgetspent" {
            let reset = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                + 120;
            return Some((
                "403 Forbidden".into(),
                vec![
                    ("X-RateLimit-Limit".into(), "5000".into()),
                    ("X-RateLimit-Remaining".into(), "0".into()),
                    ("X-RateLimit-Reset".into(), reset.to_string()),
                ],
                r#"{"message":"API rate limit exceeded for installation ID 777."}"#.into(),
            ));
        }
    }

    let param = |name: &str, default: usize| -> usize {
        query
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let per_page = param("per_page", 30).clamp(1, 100);
    let page = param("page", 1).max(1);

    let total = run_count(owner);
    let last = total.div_ceil(per_page).max(1);
    let mut headers = rate_headers(owner, n);
    if page < last {
        headers.push((
            "Link".into(),
            format!(
                "<{base}/repos/{owner}/{repo}/actions/runs?per_page={per_page}&page={}>; rel=\"next\", <{base}/repos/{owner}/{repo}/actions/runs?per_page={per_page}&page={last}>; rel=\"last\"",
                page + 1
            ),
        ));
    }
    Some((
        "200 OK".into(),
        headers,
        runs_page(owner, repo, per_page, page),
    ))
}

/// How many workflow runs a repository has, decided by its name — the
/// same fixture convention the issue routes use, so a test names the
/// shape it wants in the URL it already has to write.
fn run_count(owner: &str) -> usize {
    match owner {
        "many" => 250,
        "empty" => 0,
        _ => 7,
    }
}

/// One page of `GET /repos/{owner}/{repo}/actions/runs`.
///
/// Newest first, which is GitHub's order and the order a Checks tab wants
/// anyway; the run *ids* descend with it while `run_number` does too, and
/// they are deliberately different numbers. They are easy to confuse and
/// the confusion is silent: an aggregator keyed on `run_number` collapses
/// two repositories' runs together, and one keyed on `id` that displays it
/// shows a person a number they have never seen in the Actions UI.
fn runs_page(owner: &str, repo: &str, per_page: usize, page: usize) -> String {
    let total = run_count(owner);
    let all: Vec<usize> = (1..=total).rev().collect();
    let start = (page.max(1) - 1) * per_page;
    let items: Vec<String> = all
        .iter()
        .skip(start)
        .take(per_page)
        .map(|n| run_json(owner, repo, *n))
        .collect();
    format!(
        r#"{{"total_count":{total},"workflow_runs":[{}]}}"#,
        items.join(",")
    )
}

/// One workflow run, in GitHub's shape.
///
/// `n % 7` picks the outcome, so every page of seven covers the whole
/// space once: the four terminal states, both in-flight ones, and an
/// `action_required` conclusion that this codebase does not map. That
/// last one is the point of the fixture — the correct behaviour is to
/// degrade to "we do not know yet", and the incorrect one is to read it
/// as a pass, which turns an unmapped verdict into a green tick.
///
/// Two fields are missing on purpose as well, because both are missing on
/// real runs: `actor` is null for a deleted account, and `run_started_at`
/// is simply absent on runs old enough to predate the field. A client that
/// unwraps either panics on a real repository, and a client that defaults
/// the timestamp to zero dates the run to 1970.
fn run_json(owner: &str, repo: &str, n: usize) -> String {
    let (status, conclusion) = match n % 7 {
        0 => ("completed", "\"success\""),
        1 => ("in_progress", "null"),
        2 => ("completed", "\"failure\""),
        3 => ("queued", "null"),
        4 => ("completed", "\"cancelled\""),
        5 => ("completed", "\"skipped\""),
        _ => ("completed", "\"action_required\""),
    };
    let actor = if n == 5 {
        "null".to_string()
    } else {
        format!(r#"{{"login":"octocat-{n}","id":{}}}"#, 1000 + n)
    };
    // Absent, not null: the field is simply not in the object on old
    // runs, and "missing key" is a different code path from "key with a
    // null in it" for most deserialisers.
    let started = if n == 2 {
        String::new()
    } else {
        format!(
            r#""run_started_at":"2024-05-{:02}T09:00:00Z","#,
            (n % 28) + 1
        )
    };
    // A tag build has no branch. Null rather than an empty string, which
    // is what GitHub sends and what a client must not print as "".
    let branch = if n == 6 {
        "null".to_string()
    } else {
        r#""main""#.to_string()
    };
    format!(
        r#"{{"id":{},"name":"CI","run_number":{n},"head_sha":"{:040x}",
             "head_branch":{branch},"event":"push","status":"{status}",
             "conclusion":{conclusion},"actor":{actor},{started}
             "updated_at":"2024-05-{:02}T09:30:00Z",
             "html_url":"https://github.com/{owner}/{repo}/actions/runs/{}"}}"#,
        90_000_000 + n,
        n,
        (n % 28) + 1,
        90_000_000 + n
    )
}

/// The budget headers every GitHub response carries.
fn rate_headers(owner: &str, calls: u64) -> Vec<(String, String)> {
    let limit: i64 = if owner == "ratelimited" { 10 } else { 5000 };
    vec![
        ("X-RateLimit-Limit".into(), limit.to_string()),
        (
            "X-RateLimit-Remaining".into(),
            limit.saturating_sub(calls as i64).max(0).to_string(),
        ),
    ]
}

/// Write one response, with whatever extra headers the route asked for.
///
/// Extracted because the issue routes need headers the original fake had
/// no way to send: `Link` carries GitHub's pagination and is the *only*
/// place the next page is named — a client that guesses page numbers
/// instead of following it works here and breaks on a real repository
/// whose last page is short. `Retry-After` and `X-RateLimit-Remaining`
/// are how a real import learns to slow down, and an importer that has
/// never seen them has never been tested against the thing that will
/// actually happen to it.
fn write_response(
    stream: &mut TcpStream,
    status: &str,
    headers: &[(String, String)],
    body: &str,
) -> std::io::Result<()> {
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// How many issues a repository has, decided by its name.
///
/// Fixture-driven rather than configurable, so a test names the shape it
/// wants in the URL it already has to write and nothing has to be wired
/// through. `many/*` is deliberately larger than one page: an importer
/// tested only against a repository that fits in a single response has
/// never exercised the loop that matters.
fn issue_count(owner: &str) -> usize {
    match owner {
        "many" => 250,
        "empty" => 0,
        _ => 7,
    }
}

/// Whether entry `n` is a pull request rather than an issue.
///
/// **GitHub's `/issues` endpoint returns pull requests too**, marked only
/// by the presence of a `pull_request` object. That is the single most
/// likely thing for an importer to get wrong, because the field is easy
/// to miss and the failure is silent: every PR arrives as an issue, the
/// numbers still look right, and a project's tracker is quietly full of
/// things that were never issues. Every third one here is a PR.
fn is_pull_request(n: usize) -> bool {
    n.is_multiple_of(3)
}

/// One page of `GET /repos/{owner}/{repo}/issues`.
///
/// Numbers descend, as GitHub's do by default, so an importer that wants
/// them ascending has to say so or sort — and an importer that assumes
/// ascending will import backwards against the real thing.
fn issues_page(owner: &str, repo: &str, per_page: usize, page: usize, state: &str) -> String {
    let total = issue_count(owner);
    let all: Vec<usize> = (1..=total).rev().collect();
    let visible: Vec<usize> = all
        .into_iter()
        .filter(|n| match state {
            // Every fourth issue is closed. `state=open` is GitHub's
            // default, which is a trap of its own: an import that does
            // not ask for `all` silently loses every closed issue, and a
            // closed issue is most of a project's institutional memory.
            "open" => !n.is_multiple_of(4),
            "closed" => n.is_multiple_of(4),
            _ => true,
        })
        .collect();
    let start = (page.max(1) - 1) * per_page;
    let mut items: Vec<String> = visible
        .iter()
        .skip(start)
        .take(per_page)
        .map(|n| issue_json(owner, repo, *n))
        .collect();
    // An entry with **no `number`**, on `messy/*`'s first page only.
    //
    // The number is the one field an import cannot invent — it is what
    // `#4721` in somebody's commit message means — so the importer skips
    // an entry that lacks it rather than renumbering. Nothing could send
    // it one, so that skip was a line nobody had ever executed, next to
    // the branch that assigns numbers.
    if owner == "messy" && page <= 1 {
        items.insert(
            0,
            r#"{"title":"an entry with no number","state":"open",
                "user":{"login":"ghost","id":9},"labels":[],"comments":0,
                "created_at":"2024-01-01T10:00:00Z",
                "updated_at":"2024-01-01T10:00:00Z","closed_at":null,
                "html_url":"https://github.com/messy/x/issues/0"}"#
                .to_string(),
        );
    }
    format!("[{}]", items.join(","))
}

/// One issue, in GitHub's shape.
///
/// The parts an importer must not lose: the number (which appears in
/// commit messages and links all over the internet), the state, the
/// author login, and both timestamps. `user` can be `null` on GitHub for
/// a deleted account, and entry 5 is exactly that — an importer that
/// unwraps it panics on a real export.
fn issue_json(owner: &str, repo: &str, n: usize) -> String {
    let closed = n.is_multiple_of(4);
    let pr = if is_pull_request(n) {
        format!(
            r#","pull_request":{{"url":"https://api.github.com/repos/{owner}/{repo}/pulls/{n}"}}"#
        )
    } else {
        String::new()
    };
    let user = if n == 5 {
        "null".to_string()
    } else {
        format!(r#"{{"login":"octocat-{n}","id":{}}}"#, 1000 + n)
    };
    let labels = if n.is_multiple_of(2) {
        r#"[{"name":"bug","color":"d73a4a","description":"something is broken"}]"#
    } else {
        "[]"
    };
    // **A milestone on the issue**, which is how GitHub sends the link:
    // an object on the issue, not a list on the milestone. Every second
    // issue is on `v1.0` (number 1) and issue 7 points at a milestone
    // number that is not in the milestones page at all — a milestone
    // deleted upstream, which an importer must survive rather than
    // losing the issue over a dangling pointer.
    let milestone = if n == 7 {
        r#","milestone":{"number":404,"title":"deleted upstream"}"#.to_string()
    } else if n.is_multiple_of(2) {
        r#","milestone":{"number":1,"title":"v1.0"}"#.to_string()
    } else {
        String::new()
    };
    format!(
        r#"{{"number":{n},"title":"issue {n}","body":"body of {n}",
             "state":"{}","user":{user},"labels":{labels}{milestone},
             "comments":{},"created_at":"2024-01-{:02}T10:00:00Z",
             "updated_at":"2024-02-{:02}T10:00:00Z","closed_at":{},
             "html_url":"https://github.com/{owner}/{repo}/issues/{n}"{pr}}}"#,
        if closed { "closed" } else { "open" },
        n % 3,
        (n % 28) + 1,
        (n % 28) + 1,
        if closed {
            "\"2024-03-01T10:00:00Z\""
        } else {
            "null"
        }
    )
}

/// How many comments issue `n` carries.
///
/// `chatty/*` gives every issue **150**, which is the case the importer's
/// comment loop exists for and the one no fixture could produce: at 100
/// per page a conversation of 150 is two pages, and an importer that
/// reads only the first silently truncates the half of a migration a
/// project most wants to keep. Everyone else keeps `n % 3`, so most
/// issues have none and the empty path stays exercised.
fn comment_count(owner: &str, n: usize) -> usize {
    if owner == "chatty" {
        150
    } else {
        n % 3
    }
}

/// One page of comments on one issue.
fn comments_json(owner: &str, repo: &str, n: usize, per_page: usize, page: usize) -> String {
    let total = comment_count(owner, n);
    let start = (page.max(1) - 1) * per_page;
    let items: Vec<String> = (start..total.min(start + per_page))
        .map(|i| {
            format!(
                r#"{{"id":{},"body":"comment {i} on {n}",
                     "user":{{"login":"commenter-{i}","id":{}}},
                     "created_at":"2024-01-{:02}T11:00:00Z",
                     "updated_at":"2024-01-{:02}T11:00:00Z",
                     "html_url":"https://github.com/{owner}/{repo}/issues/{n}#issuecomment-{}"}}"#,
                n * 100 + i,
                2000 + i,
                (i % 28) + 1,
                (i % 28) + 1,
                n * 100 + i
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// One page of an installation's repositories.
///
/// Enough of them to page through, and mixed public/private so a picker
/// has something to distinguish. The shape is GitHub's: a `repositories`
/// array under a `total_count`, with `size` in kilobytes.
fn repositories_page(request_line: &str) -> String {
    let param = |name: &str, default: usize| -> usize {
        request_line
            .split(['?', '&'])
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let per_page = param("per_page", 30).clamp(1, 100);
    let page = param("page", 1).max(1);
    let all: Vec<(&str, bool, &str)> = vec![
        ("acme-inc/widget", false, "the public one"),
        ("acme-inc/ledger", true, "private, needs the installation"),
        ("acme-inc/atlas", true, "also private"),
        ("acme-inc/docs", false, "public docs"),
    ];
    let start = (page - 1) * per_page;
    let items: Vec<String> = all
        .iter()
        .skip(start)
        .take(per_page)
        .enumerate()
        .map(|(i, (name, private, desc))| {
            format!(
                r#"{{"full_name":"{name}","private":{private},"default_branch":"main",
                     "description":"{desc}","size":{}}}"#,
                (start + i + 1) * 16
            )
        })
        .collect();
    format!(
        r#"{{"total_count":{},"repositories":[{}]}}"#,
        all.len(),
        items.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The bug this guards: the fake used to answer as soon as it had the
    /// request headers and close, leaving a POST body it never read in the
    /// kernel receive queue. TCP turns a close with unread bytes queued
    /// into an RST instead of a FIN, and on macOS `setsockopt` on a reset
    /// socket returns EINVAL — so ureq, which re-arms the read timeout
    /// before every buffered line, failed on the first *header* of a
    /// response it had already received: "Error encountered in a header:
    /// Invalid argument (os error 22)". It reproduced roughly one run in
    /// four, only under full-workspace load, because scheduling decided
    /// whether the client's two writes reached the fake as one segment.
    ///
    /// Coalescing is not something a client can control, so the split is
    /// forced from the other end instead: `handle` reads in 4096-byte
    /// chunks, so headers padded to end *exactly* on that boundary mean
    /// its terminating read cannot also have picked up the body. One
    /// segment or two, the body is left behind — deterministically.
    ///
    /// The assertion is the mechanism, not the platform's symptom: a
    /// clean shutdown reads as EOF, a reset reads as an error. That holds
    /// everywhere, whereas the EINVAL is macOS's alone.
    #[test]
    fn a_post_body_is_drained_so_the_close_is_a_fin_and_not_a_reset() {
        let gh = spawn();
        let mut sock = TcpStream::connect(gh.base_url.trim_start_matches("http://")).unwrap();
        sock.set_nodelay(true).unwrap();

        let mut head = String::from(
            "POST /app/installations/777/access_tokens HTTP/1.1\r\n\
             Host: fake\r\nAuthorization: Bearer a.b.c\r\nContent-Length: 2\r\n",
        );
        // A filler header sized so the blank line lands on byte 4096.
        let pad = 4096 - head.len() - "X-Pad: \r\n\r\n".len();
        head.push_str(&format!("X-Pad: {}\r\n\r\n", "p".repeat(pad)));
        assert_eq!(head.len(), 4096);
        sock.write_all(head.as_bytes()).unwrap();
        sock.write_all(b"{}").unwrap();

        // Read to the end of the response. `Connection: close` means the
        // fake hangs up afterwards, so a well-behaved exchange ends in a
        // zero-length read; a reset surfaces as an error instead.
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut got = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&tmp[..n]),
                Err(e) => panic!("the fake reset the connection: {e}"),
            }
        }
        let text = String::from_utf8_lossy(&got);
        assert!(text.starts_with("HTTP/1.1 201 Created"), "{text}");
        assert!(text.contains("ghs_fake_1"), "{text}");
    }

    /// And the whole exchange, through a real HTTP client, with the same
    /// forced split: the fake is only useful if ureq survives it.
    #[test]
    fn a_real_client_completes_the_token_exchange_repeatedly() {
        let gh = spawn();
        let url = format!("{}/app/installations/777/access_tokens", gh.base_url);
        for i in 0..20 {
            let resp = ureq::post(&url)
                .set("Authorization", "Bearer a.b.c")
                .timeout(Duration::from_secs(10))
                .send_string("{}")
                .unwrap_or_else(|e| panic!("attempt {i}: {e}"));
            assert_eq!(resp.status(), 201);
        }
        assert_eq!(gh.tokens_issued.load(Ordering::SeqCst), 20);
    }
}

#[cfg(test)]
mod issue_route_tests {
    use super::*;

    /// A real HTTP client, because the traps this fake exists to expose
    /// are on the wire: a `Link` header, a `Retry-After`, a status code.
    /// Asserting over the fixture functions directly would test the
    /// fixtures and not the thing an importer will actually meet.
    fn get(gh: &FakeGithub, path: &str) -> (u16, String, Option<String>) {
        let resp = ureq::get(&format!("{}{path}", gh.base_url))
            .set("Authorization", "token ghs_fake_1")
            .call();
        match resp {
            Ok(r) | Err(ureq::Error::Status(_, r)) => {
                let status = r.status();
                let link = r.header("Link").map(|s| s.to_string());
                (status, r.into_string().unwrap_or_default(), link)
            }
            Err(e) => panic!("transport: {e}"),
        }
    }

    #[test]
    fn issues_need_an_installation_token_the_way_the_real_api_does() {
        let gh = spawn();
        // No Authorization header at all.
        let r = ureq::get(&format!("{}/repos/acme/widget/issues", gh.base_url)).call();
        let status = match r {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(s, _)) => s,
            Err(e) => panic!("transport: {e}"),
        };
        assert_eq!(
            status, 401,
            "a client that forgot the token passed here and would fail against GitHub"
        );
    }

    /// `state` defaults to `open` on GitHub, and that default loses every
    /// closed issue — which is most of a project's institutional memory.
    /// An importer has to ask for `all`; this is the fixture that makes
    /// forgetting visible.
    #[test]
    fn the_default_state_hides_closed_issues_and_all_does_not() {
        let gh = spawn();
        let (st, open, _) = get(&gh, "/repos/acme/widget/issues?per_page=100");
        assert_eq!(st, 200);
        let (_, all, _) = get(&gh, "/repos/acme/widget/issues?state=all&per_page=100");
        let open_n = open.matches("\"number\"").count();
        let all_n = all.matches("\"number\"").count();
        assert!(
            all_n > open_n,
            "the fixture has no closed issues, so nothing here can catch \
             an importer that never asks for them: {open_n} vs {all_n}"
        );
        assert!(all.contains("\"state\":\"closed\""), "{all}");
    }

    /// Pull requests come back from `/issues`, marked only by the
    /// presence of a `pull_request` object. It is the easiest thing in
    /// this API to miss and the failure is silent: every PR arrives as an
    /// issue and the numbers still look right.
    #[test]
    fn pull_requests_are_mixed_into_the_issue_list() {
        let gh = spawn();
        let (_, body, _) = get(&gh, "/repos/acme/widget/issues?state=all&per_page=100");
        assert!(
            body.contains("\"pull_request\""),
            "no pull request in the list, so an importer that ignores the \
             marker passes: {body}"
        );
        assert!(
            body.matches("\"pull_request\"").count() < body.matches("\"number\"").count(),
            "everything is a pull request, which is not the shape either"
        );
    }

    /// A deleted GitHub account leaves `user: null`. An importer that
    /// unwraps it panics on a real export, and nothing else here would
    /// produce one.
    #[test]
    fn an_issue_can_have_no_author() {
        let gh = spawn();
        let (_, body, _) = get(&gh, "/repos/acme/widget/issues?state=all&per_page=100");
        assert!(body.contains("\"user\":null"), "{body}");
    }

    /// The next page is named **only** in `Link`. A client that guesses
    /// page numbers works against a fixture and breaks on a repository
    /// whose last page happens to be exactly full.
    #[test]
    fn paging_is_advertised_in_link_and_stops_at_the_end() {
        let gh = spawn();
        let (st, first, link) = get(&gh, "/repos/many/widget/issues?state=all&per_page=100");
        assert_eq!(st, 200);
        // Parsed rather than counted: an issue's `number` is not the
        // only `"number"` in its JSON — a milestone has one too.
        let page: Vec<serde_json::Value> = serde_json::from_str(&first).expect("a page of issues");
        assert_eq!(page.len(), 100);
        let link = link.expect("a full first page must advertise the next one");
        assert!(link.contains("rel=\"next\""), "{link}");
        assert!(link.contains("page=2"), "{link}");
        assert!(link.contains("rel=\"last\""), "{link}");

        // 250 issues at 100 a page: the third page is short and must not
        // advertise a fourth.
        let (_, last, link) = get(
            &gh,
            "/repos/many/widget/issues?state=all&per_page=100&page=3",
        );
        let last_page: Vec<serde_json::Value> =
            serde_json::from_str(&last).expect("a page of issues");
        assert_eq!(last_page.len(), 50);
        assert!(
            link.is_none(),
            "the last page advertised another one: {link:?}"
        );
    }

    /// An installation without `issues: read` gets GitHub's own 403 and
    /// its wording — **not** an empty list. An import that quietly
    /// produces nothing is indistinguishable from a project that never
    /// had issues, which is the failure this whole design names.
    #[test]
    fn a_missing_permission_is_a_refusal_and_not_an_empty_list() {
        let gh = spawn();
        let (st, body, _) = get(&gh, "/repos/noperm/widget/issues?state=all");
        assert_eq!(st, 403, "{body}");
        assert!(
            body.contains("Resource not accessible by integration"),
            "an importer matching on GitHub's wording would not recognise \
             this: {body}"
        );
        assert!(!body.contains("[]"), "{body}");
    }

    /// Rate limiting is the shape of a real import, not an edge case.
    #[test]
    fn a_rate_limited_repository_refuses_with_retry_after() {
        let gh = spawn();
        let mut refusals = 0;
        let mut saw_retry_after = false;
        for _ in 0..6 {
            let resp = ureq::get(&format!(
                "{}/repos/ratelimited/widget/issues?state=all",
                gh.base_url
            ))
            .set("Authorization", "token ghs_fake_1")
            .call();
            if let Err(ureq::Error::Status(403, r)) = resp {
                refusals += 1;
                if r.header("Retry-After").is_some() {
                    saw_retry_after = true;
                }
            }
        }
        assert!(refusals > 0, "nothing was rate limited in six calls");
        assert!(
            saw_retry_after,
            "refused without Retry-After, so an importer has nothing to \
             honour and can only spin"
        );
    }

    /// The budget is on every response, not only on the refusals — which
    /// is how an importer slows down *before* being refused.
    #[test]
    fn every_answer_carries_the_remaining_budget() {
        let gh = spawn();
        let r = ureq::get(&format!("{}/repos/acme/widget/issues", gh.base_url))
            .set("Authorization", "token ghs_fake_1")
            .call()
            .expect("ok");
        assert!(r.header("X-RateLimit-Remaining").is_some());
        assert!(r.header("X-RateLimit-Limit").is_some());
    }

    #[test]
    fn comments_labels_and_milestones_answer_in_githubs_shape() {
        let gh = spawn();
        let (st, comments, _) = get(&gh, "/repos/acme/widget/issues/2/comments");
        assert_eq!(st, 200);
        assert!(
            comments.contains("\"body\":\"comment 0 on 2\""),
            "{comments}"
        );

        // An issue with no comments is an empty array, not a 404.
        let (st, none, _) = get(&gh, "/repos/acme/widget/issues/3/comments");
        assert_eq!(st, 200);
        assert_eq!(none.trim(), "[]", "{none}");

        let (st, labels, _) = get(&gh, "/repos/acme/widget/labels");
        assert_eq!(st, 200);
        assert!(labels.contains("good first issue"), "{labels}");
        // A hex colour, which is what GitHub sends and what our own
        // schema deliberately refuses — the importer has to map it.
        assert!(labels.contains("\"color\":\"d73a4a\""), "{labels}");

        let (st, milestones, _) = get(&gh, "/repos/acme/widget/milestones");
        assert_eq!(st, 200);
        assert!(milestones.contains("\"title\":\"v1.0\""), "{milestones}");
    }

    /// A repository with nothing in it is an empty list and a 200 —
    /// distinguishable from the 403 above, which is the whole point.
    #[test]
    fn an_empty_tracker_is_an_empty_list_and_not_a_refusal() {
        let gh = spawn();
        let (st, body, link) = get(&gh, "/repos/empty/widget/issues?state=all");
        assert_eq!(st, 200);
        assert_eq!(body.trim(), "[]");
        assert!(link.is_none());
    }

    /// The routes this file already served must keep working: the issue
    /// dispatch runs first and shares the `/repos/` prefix with the
    /// metadata route, which matches anything after it.
    #[test]
    fn the_repository_metadata_route_still_answers() {
        let gh = spawn();
        let body = ureq::get(&format!("{}/repos/acme-inc/atlas", gh.base_url))
            .call()
            .expect("metadata")
            .into_string()
            .unwrap();
        assert!(body.contains("\"stargazers_count\":60300"), "{body}");
    }
}

#[cfg(test)]
mod import_walk_tests {
    use super::*;

    /// Walk the fake the way an importer will, end to end.
    ///
    /// Every other test here checks one answer. This one is the shape of
    /// the whole job — labels, then milestones, then every page of
    /// issues, then the comments on each — and it exists because the
    /// individual answers being right is not the same claim as the loop
    /// being walkable. A fixture that answers each request correctly and
    /// cannot be paged to the end would pass all of them.
    ///
    /// It is also the nearest thing to a manual pass available before
    /// there is an importer to run by hand: it fails if the fake cannot
    /// support the job it exists to support.
    fn follow_link_next(link: Option<&str>) -> Option<String> {
        // The importer must read the URL out of `Link` rather than
        // incrementing a page counter, so the walk does too. GitHub's
        // header is `<url>; rel="next", <url>; rel="last"`.
        let link = link?;
        link.split(',').find_map(|part| {
            if !part.contains("rel=\"next\"") {
                return None;
            }
            let start = part.find('<')? + 1;
            let end = part.find('>')?;
            Some(part[start..end].to_string())
        })
    }

    #[test]
    fn an_importer_can_walk_a_whole_repository_without_guessing() {
        let gh = spawn();
        let auth = ("Authorization", "token ghs_fake_1");

        let labels = ureq::get(&format!("{}/repos/many/widget/labels", gh.base_url))
            .set(auth.0, auth.1)
            .call()
            .expect("labels")
            .into_string()
            .unwrap();
        assert!(labels.contains("good first issue"), "{labels}");

        let milestones = ureq::get(&format!("{}/repos/many/widget/milestones", gh.base_url))
            .set(auth.0, auth.1)
            .call()
            .expect("milestones")
            .into_string()
            .unwrap();
        assert!(milestones.contains("v1.0"), "{milestones}");

        // Every page of issues, following `Link` rather than counting.
        let mut url = format!(
            "{}/repos/many/widget/issues?state=all&per_page=100",
            gh.base_url
        );
        let mut numbers: Vec<i64> = Vec::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages <= 10, "the walk did not terminate");
            let r = ureq::get(&url).set(auth.0, auth.1).call().expect("issues");
            let next = follow_link_next(r.header("Link"));
            let body = r.into_string().unwrap();
            // Parsed, not scraped. This used to split on `"number":`
            // and take the digits after it, which was a proxy for "one
            // per issue" that stopped being true the moment an issue
            // carried a **milestone** — GitHub's milestone object has a
            // `number` of its own, so the walk counted 251 issues in a
            // repository of 250 and the paging test counted 150 on a
            // page of 100. The fixture was right and the assertion was
            // reading it wrong, which is the more dangerous way round.
            let page: Vec<serde_json::Value> =
                serde_json::from_str(&body).expect("a page of issues");
            for it in &page {
                numbers.push(it["number"].as_i64().expect("an issue number"));
            }
            match next {
                // Followed verbatim. The header names the address this
                // fake is really listening on, because a client is right
                // to refuse a next-page URL pointing off the host it was
                // configured with.
                Some(n) => url = n,
                None => break,
            }
        }

        assert_eq!(pages, 3, "250 issues at 100 a page is three pages");
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(
            numbers.len(),
            250,
            "the walk did not see every issue exactly once"
        );
        assert_eq!(numbers.first(), Some(&1));
        assert_eq!(numbers.last(), Some(&250));

        // Comments for a sample, including one that has none — the case
        // that must be an empty array rather than a 404.
        for n in [1usize, 2, 3] {
            let r = ureq::get(&format!(
                "{}/repos/many/widget/issues/{n}/comments",
                gh.base_url
            ))
            .set(auth.0, auth.1)
            .call()
            .expect("comments");
            assert_eq!(r.status(), 200);
            let body = r.into_string().unwrap();
            assert!(body.starts_with('['), "{body}");
        }
    }

    /// The same walk against a repository that refuses every third call.
    /// An importer that honours `Retry-After` finishes; one that does not
    /// cannot, and this is where it finds that out.
    #[test]
    fn a_walk_survives_rate_limiting_by_honouring_retry_after() {
        let gh = spawn();
        let mut attempts = 0;
        let mut got = None;
        while attempts < 6 {
            attempts += 1;
            let resp = ureq::get(&format!(
                "{}/repos/ratelimited/widget/issues?state=all&per_page=100",
                gh.base_url
            ))
            .set("Authorization", "token ghs_fake_1")
            .call();
            match resp {
                Ok(r) => {
                    got = Some(r.into_string().unwrap());
                    break;
                }
                Err(ureq::Error::Status(403, r)) => {
                    // What the header is *for*: a real importer sleeps
                    // this long. The test only asserts it is present and
                    // parseable, because sleeping in a unit test buys
                    // nothing.
                    let after: u64 = r
                        .header("Retry-After")
                        .and_then(|v| v.parse().ok())
                        .expect("a 403 from rate limiting must say when to come back");
                    assert!(after <= 60, "an unreasonable Retry-After: {after}");
                }
                Err(e) => panic!("transport: {e}"),
            }
        }
        let body = got.expect("six attempts against a one-in-three refusal never succeeded");
        assert!(body.contains("\"number\""), "{body}");
    }
}

#[cfg(test)]
mod actions_route_tests {
    use super::*;

    /// Through a real HTTP client, for the same reason the issue routes
    /// are: the traps are on the wire — a status code and a `Link`
    /// header — and asserting over the fixture functions would test the
    /// fixtures rather than what a client will meet.
    fn get(gh: &FakeGithub, path: &str) -> (u16, String, Option<String>) {
        let resp = ureq::get(&format!("{}{path}", gh.base_url))
            .set("Authorization", "token ghs_fake_1")
            .call();
        match resp {
            Ok(r) | Err(ureq::Error::Status(_, r)) => {
                let status = r.status();
                let link = r.header("Link").map(|s| s.to_string());
                (status, r.into_string().unwrap_or_default(), link)
            }
            Err(e) => panic!("transport: {e}"),
        }
    }

    #[test]
    fn actions_runs_need_an_installation_token_the_way_the_real_api_does() {
        let gh = spawn();
        let r = ureq::get(&format!("{}/repos/acme/widget/actions/runs", gh.base_url)).call();
        let status = match r {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(s, _)) => s,
            Err(e) => panic!("transport: {e}"),
        };
        assert_eq!(
            status, 401,
            "a client that forgot the token passed here and would fail against GitHub"
        );
    }

    /// The whole reason this route exists separately: without it the
    /// request falls through to the repository-metadata route, which
    /// matches anything after `/repos/` and would answer a 200 carrying a
    /// repository. A client looking for `workflow_runs` in that body finds
    /// none and reports "no CI".
    #[test]
    fn an_actions_request_is_not_answered_with_repository_metadata() {
        let gh = spawn();
        let (st, body, _) = get(&gh, "/repos/acme/widget/actions/runs?per_page=100");
        assert_eq!(st, 200, "{body}");
        assert!(body.contains("\"workflow_runs\""), "{body}");
        assert!(
            !body.contains("stargazers_count"),
            "the metadata route answered an Actions request: {body}"
        );
    }

    /// Every combination of `status` and `conclusion` a real repository
    /// produces, in one page — including a conclusion this codebase does
    /// not know. A client that has only ever seen `success` and `failure`
    /// meets its first unknown one in production otherwise.
    #[test]
    fn one_page_covers_every_status_and_conclusion_including_an_unknown_one() {
        let gh = spawn();
        let (_, body, _) = get(&gh, "/repos/acme/widget/actions/runs?per_page=100");
        for want in [
            r#""status":"in_progress""#,
            r#""status":"queued""#,
            r#""conclusion":"success""#,
            r#""conclusion":"failure""#,
            r#""conclusion":"cancelled""#,
            r#""conclusion":"skipped""#,
            // Real, and unmapped here on purpose.
            r#""conclusion":"action_required""#,
            r#""conclusion":null"#,
        ] {
            assert!(body.contains(want), "no {want} in the page: {body}");
        }
    }

    /// The fields that are absent on real runs are absent here. Both are
    /// silent failures otherwise: an unwrapped `actor` panics on a deleted
    /// account, and a `run_started_at` defaulted to zero dates the run to
    /// 1970 in whatever renders it.
    #[test]
    fn a_run_can_have_no_actor_no_branch_and_no_start_time() {
        let gh = spawn();
        let (_, body, _) = get(&gh, "/repos/acme/widget/actions/runs?per_page=100");
        assert!(body.contains(r#""actor":null"#), "{body}");
        assert!(body.contains(r#""head_branch":null"#), "{body}");
        let runs = body.matches(r#""run_number""#).count();
        let started = body.matches("run_started_at").count();
        assert!(
            started < runs,
            "every run has a start time, so nothing here catches a client \
             that assumes the field is always sent: {started} of {runs}"
        );
    }

    /// Paging is advertised in `Link` and nowhere else, exactly as it is
    /// for issues — a client that increments a page counter works against
    /// a fixture and stops one page early when the last page is full.
    #[test]
    fn run_paging_is_advertised_in_link_and_stops_at_the_end() {
        let gh = spawn();
        let (st, first, link) = get(&gh, "/repos/many/widget/actions/runs?per_page=100");
        assert_eq!(st, 200);
        assert_eq!(first.matches(r#""run_number""#).count(), 100);
        let link = link.expect("a full first page must advertise the next one");
        assert!(link.contains("rel=\"next\""), "{link}");
        assert!(link.contains("page=2"), "{link}");

        let (_, last, link) = get(&gh, "/repos/many/widget/actions/runs?per_page=100&page=3");
        assert_eq!(last.matches(r#""run_number""#).count(), 50);
        assert!(
            link.is_none(),
            "the last page advertised another one: {link:?}"
        );
    }

    /// An installation without `actions: read` is refused, and a
    /// repository with no CI answers an empty list. Those are different
    /// facts and the fake has to be able to say both, or the client can
    /// only ever be tested against one of them.
    #[test]
    fn a_missing_actions_permission_is_a_refusal_and_an_empty_repo_is_a_list() {
        let gh = spawn();
        let (st, body, _) = get(&gh, "/repos/noperm/widget/actions/runs");
        assert_eq!(st, 403, "{body}");
        assert!(
            body.contains("Resource not accessible by integration"),
            "{body}"
        );

        let (st, body, link) = get(&gh, "/repos/empty/widget/actions/runs");
        assert_eq!(st, 200);
        assert!(body.contains(r#""workflow_runs":[]"#), "{body}");
        assert!(link.is_none());
    }

    /// A spent budget is a 403 **with `Retry-After`**, and it alternates
    /// so a client can be seen to come back.
    ///
    /// This is the arm that makes `ActionsRuns::RateLimited` reachable at
    /// all: the client maps it, the poller backs off on it, and until the
    /// fake could produce one neither half was ever executed. The two 403s
    /// on this endpoint mean opposite things — "grant a permission" and
    /// "wait two seconds" — and `Retry-After` is the only thing that tells
    /// them apart, so the assertion is on the header, not the status.
    #[test]
    fn a_rate_limited_repository_refuses_runs_with_retry_after_and_then_recovers() {
        let gh = spawn();
        let mut refused = 0;
        let mut ok = 0;
        for _ in 0..4 {
            let resp = ureq::get(&format!(
                "{}/repos/ratelimited/widget/actions/runs",
                gh.base_url
            ))
            .set("Authorization", "token ghs_fake_1")
            .call();
            match resp {
                Ok(r) => {
                    ok += 1;
                    assert!(r.into_string().unwrap().contains("workflow_runs"));
                }
                Err(ureq::Error::Status(403, r)) => {
                    refused += 1;
                    let after: u64 = r.header("Retry-After").and_then(|v| v.parse().ok()).expect(
                        "a rate-limit 403 without Retry-After is indistinguishable \
                             from a missing permission, and a client can only either \
                             spin or give up",
                    );
                    assert!(
                        after > 0 && after <= 60,
                        "an unreasonable Retry-After: {after}"
                    );
                }
                Err(e) => panic!("transport: {e}"),
            }
        }
        assert_eq!(refused, 2, "the budget never ran out in four calls");
        assert_eq!(
            ok, 2,
            "it never recovered, so a poller that backed off for ever would pass"
        );
        assert_eq!(gh.actions_calls.load(Ordering::SeqCst), 4);
    }

    /// The **primary** limit: a spent hourly budget, which carries a
    /// remaining of zero and an absolute reset, and no `Retry-After`.
    ///
    /// Nothing in the tree could produce this response before, which is
    /// why a client that read only `Retry-After` classified it as a
    /// permission denial and passed every test we had — including the one
    /// named for that exact confusion.
    #[test]
    fn a_spent_primary_budget_refuses_without_a_retry_after_header() {
        let gh = spawn();
        let resp = ureq::get(&format!(
            "{}/repos/budgetspent/widget/actions/runs",
            gh.base_url
        ))
        .set("Authorization", "token ghs_fake_1")
        .call();
        let Err(ureq::Error::Status(403, r)) = resp else {
            panic!("a spent budget was not refused");
        };
        assert!(
            r.header("Retry-After").is_none(),
            "the fixture attached a Retry-After, which is the secondary \
             limit's header and makes the primary limit untestable"
        );
        assert_eq!(r.header("x-ratelimit-remaining"), Some("0"));
        let reset: u64 = r
            .header("x-ratelimit-reset")
            .and_then(|v| v.parse().ok())
            .expect("a primary limit must say when the budget returns");
        // An absolute unix timestamp, not a duration. A client that
        // sleeps for this number sleeps for fifty-five years.
        assert!(reset > 1_700_000_000, "not a unix timestamp: {reset}");
    }

    /// A permission denial is a 403 **carrying rate-limit headers with a
    /// budget still on it**, which is the case a client keying on the
    /// presence of those headers gets wrong.
    #[test]
    fn a_permission_denial_carries_a_nonzero_budget() {
        let gh = spawn();
        let resp = ureq::get(&format!("{}/repos/noperm/widget/actions/runs", gh.base_url))
            .set("Authorization", "token ghs_fake_1")
            .call();
        let Err(ureq::Error::Status(403, r)) = resp else {
            panic!("noperm was not refused");
        };
        assert!(r.header("Retry-After").is_none());
        let remaining: u64 = r
            .header("x-ratelimit-remaining")
            .and_then(|v| v.parse().ok())
            .expect("GitHub sends the budget on a permission denial too");
        assert!(
            remaining > 0,
            "a permission denial with a zero budget is indistinguishable \
             from a spent one, so this fixture cannot tell them apart"
        );
    }

    /// The refusal schedule belongs to the repository it is about, and
    /// another repository's reads cannot shift it.
    ///
    /// The first version counted every Actions request on one counter, so
    /// the parity of `ratelimited/*` depended on how many runs anybody
    /// else had read in the same `spawn()` — a second test in the same
    /// world, or a poller sweeping two repositories, would find its first
    /// call already recovered and the schedule off by one. That is the
    /// worst kind of fixture bug: it makes a correct client look wrong,
    /// in a way that moves when you add an unrelated test.
    #[test]
    fn another_repositorys_reads_do_not_shift_the_refusal_schedule() {
        let gh = spawn();
        // Three unrelated reads first, which is what would have flipped
        // the parity when the counter was shared.
        for _ in 0..3 {
            let (st, _, _) = get(&gh, "/repos/acme/widget/actions/runs");
            assert_eq!(st, 200);
        }
        let (st, body, _) = get(&gh, "/repos/ratelimited/widget/actions/runs");
        assert_eq!(
            st, 403,
            "the first read of this repository was not the refused one: {body}"
        );
        let (st, _, _) = get(&gh, "/repos/ratelimited/widget/actions/runs");
        assert_eq!(st, 200, "and the second was not the recovery");

        // Every one of those five reached the route, refusals included.
        assert_eq!(gh.actions_calls.load(Ordering::SeqCst), 5);
    }

    /// The refusal is keyed on the owner, so the paging fixture never
    /// meets one — a walk that is refused on a schedule tests the
    /// schedule rather than the paging.
    #[test]
    fn rate_limiting_does_not_leak_into_the_paging_fixture() {
        let gh = spawn();
        for page in 1..=3 {
            let (st, body, _) = get(
                &gh,
                &format!("/repos/many/widget/actions/runs?per_page=100&page={page}"),
            );
            assert_eq!(st, 200, "page {page} was refused: {body}");
        }
    }

    /// Neighbours in the `/actions/` namespace are not this route, and
    /// answering them with runs would let a client ask for the wrong thing
    /// and pass.
    #[test]
    fn only_the_run_list_is_served_from_the_actions_namespace() {
        let gh = spawn();
        for path in [
            "/repos/acme/widget/actions/workflows",
            "/repos/acme/widget/actions/runs/12345",
        ] {
            let (st, body, _) = get(&gh, path);
            assert_ne!(
                st, 200,
                "{path} answered as though it were the run list: {body}"
            );
        }
    }
}
