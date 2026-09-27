//! The contribution graph and follows, end to end against a real
//! server, driven with the stock `git` CLI.
//!
//! Three claims are worth an end-to-end test rather than a unit test,
//! because all three are properties of the whole path — push, job,
//! walk, lookup, read — and each of them is a claim somebody could
//! reasonably doubt.
//!
//! **An unproved address counts for nothing.** Anybody can write
//! anybody's address into `git config user.email`. If a graph went green
//! on that, every graph on the platform would be worth nothing, and
//! nobody would move their history here. The test pushes two commits
//! that differ *only* in whether the author address is a proved address
//! on the account, and one square appears.
//!
//! **A stranger learns nothing about a private repository from
//! somebody's graph.** This is a new way to ask "does this person have a
//! private repository", and the answer has to be a number with nothing
//! attached. The test asserts on the raw response text, not on parsed
//! fields, because a leak is exactly the field nobody thought to parse.
//!
//! **Authorship is never charged to the pusher.** Nothing here asserts
//! a duration — that would be a flake waiting to happen — but the push
//! returns before any of these squares exist, and every assertion below
//! is written as a poll for that reason.

use std::path::Path;
use std::time::{Duration, Instant};
use stratum_testkit::browser::Browser;
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::mailbox::Mailbox;
use stratum_testkit::{Minio, Server};

const PASSWORD: &str = "a long enough password";

/// The server under test, with the walker polling fast enough that a
/// test does not spend its life waiting for a five-second tick.
fn spawn(store_url: &str, scratch: &Scratch, hint: &str, mail: &Mailbox) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_CONTRIB_POLL_SECS", "1");
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    b.start()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

fn mailed_token(mail: &Mailbox, address: &str, key: &str) -> String {
    let msg = mail.wait_for(address, Duration::from_secs(10));
    let link = msg.link().unwrap_or_else(|| panic!("no link in {msg:?}"));
    let marker = format!("#{key}=");
    let raw = link
        .split_once(&marker)
        .unwrap_or_else(|| panic!("{link} carries no #{key}="))
        .1;
    urldecode(raw)
}

/// Sign somebody up through the product's own flow. Their sign-up
/// address is proved by the flow itself, which is what makes it usable
/// as an authorship address.
fn signup<'a>(server: &'a Server, mail: &Mailbox, handle: &str, email: &str) -> Browser<'a> {
    let (st, body) = server.req(
        "POST",
        "/v1/auth/signup",
        "",
        Some(serde_json::json!({
            "handle": handle,
            "email": email,
            "name": handle,
            "password": PASSWORD,
        })),
    );
    assert_eq!(st, 202, "signup {handle}: {body}");
    let token = mailed_token(mail, email, "verify");
    let mut b = Browser::new(server);
    let (st, body) = b.req(
        "POST",
        "/v1/auth/verify",
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 200, "verify {handle}: {body}");
    b
}

/// Add a second address to an account and prove it, the way the product
/// does. This is the interesting case: a decade of somebody's history
/// is signed with addresses that are not their login.
fn add_proved_address(b: &mut Browser, mail: &Mailbox, handle: &str, address: &str) {
    let (st, body) = b.req(
        "POST",
        &format!("/v1/users/{handle}/emails"),
        Some(serde_json::json!({ "email": address })),
    );
    assert_eq!(st, 202, "claim {address}: {body}");
    let token = mailed_token(mail, address, "verify-email");
    let (st, body) = b.req(
        "POST",
        &format!("/v1/users/{handle}/emails/verify"),
        Some(serde_json::json!({ "token": token })),
    );
    assert_eq!(st, 200, "prove {address}: {body}");
}

/// Claim an address and stop there. The row lands with `verified_at`
/// NULL, which is the exact shape of the gaming attempt: a claim is not
/// a proof, and until the mailbox answers it must colour nothing.
fn claim_address(b: &mut Browser, handle: &str, address: &str) {
    let (st, body) = b.req(
        "POST",
        &format!("/v1/users/{handle}/emails"),
        Some(serde_json::json!({ "email": address })),
    );
    assert_eq!(st, 202, "claim {address}: {body}");
}

/// A repository with one commit per `(address, day, trailer)`, each on
/// its own date so the squares can be told apart.
fn build_repo(dir: &Path, commits: &[(&str, &str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    gitcli::git(dir, &["init", "-q", "-b", "main"]);
    for (i, (address, date, message)) in commits.iter().enumerate() {
        std::fs::write(dir.join(format!("f{i}.txt")), format!("{i}\n")).unwrap();
        gitcli::git(dir, &["add", "-A"]);
        // `--author`, not `-c user.email`: the testkit's git wrapper
        // exports `GIT_AUTHOR_EMAIL`, and the environment beats
        // `-c` — so a fixture written the config way silently commits
        // as the harness and tests nothing about addresses at all.
        gitcli::git(
            dir,
            &[
                "commit",
                "-q",
                &format!("--author=Author <{address}>"),
                "--date",
                &format!("{date}T12:00:00+0000"),
                "-m",
                message,
            ],
        );
    }
}

fn push(server: &Server, dir: &Path, token: &str, org: &str, repo: &str) {
    let url = server.authed_url(token, org, repo);
    // Tags too. An annotated tag is a ref whose tip is not a commit,
    // and the walk used to die on the first one — every real project
    // has releases, and the first large mirror had a thousand.
    gitcli::git(dir, &["push", "-q", "--tags", &url, "main:main"]);
}

/// Poll the graph until `want` says yes, or fail with the last body.
///
/// A poll rather than a sleep, because the whole point of the design is
/// that the push does not wait for the walk — so a test that assumed a
/// fixed delay would be asserting the opposite of the property.
fn graph_until(
    server: &Server,
    handle: &str,
    query: &str,
    what: &str,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = serde_json::Value::Null;
    while Instant::now() < deadline {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/users/{handle}/contributions?{query}"),
            "",
            None,
        );
        assert_eq!(st, 200, "graph for {handle}: {body}");
        if want(&body) {
            return body;
        }
        last = body;
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{what}; last graph was {last}");
}

/// The count on one day, or 0 when nothing landed there.
fn day_count(graph: &serde_json::Value, date: &str) -> i64 {
    graph["days"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .find(|d| d["date"] == date)
        .and_then(|d| d["count"].as_i64())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------

/// The anti-gaming rule, end to end.
#[test]
fn a_proved_address_colours_a_square_and_an_unproved_one_colours_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e");
    let scratch = Scratch::new("contribs");
    let mail = Mailbox::temp("contribs");
    let server = spawn(&bucket.base_url, &scratch, "contribs", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    // The address a decade of her commits is actually signed with.
    add_proved_address(&mut ada, &mail, "ada", "ada@old-laptop.example");
    // And one she has only *claimed*. This is the gaming attempt in its
    // purest form — the row exists on her account, and it must still
    // count for nothing until the mailbox answers.
    claim_address(&mut ada, "ada", "ada@unproved.example");

    // The repository lives in somebody else's namespace and is pushed
    // with a *service* token, so nothing here is attributable by
    // "pushed-by" and a proved address is the only ground left. That is
    // what isolates the rule under test — and it is also the shape of
    // the case the feature exists for: her work, in a project that is
    // not hers.
    let admin = server.bootstrap_org("acme");
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");

    let work = scratch.path().join("widget");
    build_repo(
        &work,
        &[
            // Proved: hers.
            ("ada@old-laptop.example", "2026-03-02", "real work"),
            // Unproved: anybody could have written this line, and it
            // must colour nothing at all.
            ("ada@example.com.evil.test", "2026-03-05", "impostor"),
            // Also unproved: a *stranger's* address, which is the same
            // answer from the other direction.
            ("nobody@example.invalid", "2026-03-06", "stranger"),
            // Claimed on her own account and not proved. The row is
            // hers; the mailbox has said nothing.
            ("ada@unproved.example", "2026-03-07", "claimed, not proved"),
        ],
    );
    // A release: an annotated tag object at the tip, the shape that
    // ended every walk of a real project with "commit without tree".
    gitcli::git(&work, &["tag", "-a", "v1", "-m", "release one"]);
    // And a ref that names a blob outright — `git tag` allows it, and
    // some projects keep notes or assets that way. Nothing to count,
    // nothing to die on.
    let blob = gitcli::git(&work, &["rev-parse", "HEAD:f0.txt"]);
    gitcli::git(&work, &["tag", "asset", blob.trim()]);

    push(&server, &work, &admin, "acme", "widget");

    let g = graph_until(
        &server,
        "ada",
        "from=2026-01-01&to=2026-12-31",
        "the proved commit never appeared on ada's graph",
        |g| g["total"].as_i64() == Some(1),
    );

    assert_eq!(
        day_count(&g, "2026-03-02"),
        1,
        "the proved address did not colour its day: {g}"
    );
    assert_eq!(
        day_count(&g, "2026-03-05"),
        0,
        "an address ada never proved coloured a square — anybody could \
         write that string into a commit: {g}"
    );
    assert_eq!(day_count(&g, "2026-03-06"), 0, "{g}");
    assert_eq!(
        day_count(&g, "2026-03-07"),
        0,
        "an address she claimed but never proved coloured a square — a \
         claim is not a proof: {g}"
    );
    assert_eq!(g["total"], 1, "exactly one of three commits is hers: {g}");
    // A public repository's square carries its name; that is the whole
    // difference between it and a private one.
    let repos = g["days"].as_array().unwrap()[0]["repos"]
        .as_array()
        .unwrap();
    assert_eq!(repos.len(), 1, "{g}");
    assert_eq!(repos[0]["org"], "acme");
    assert_eq!(repos[0]["name"], "widget");

    // A second push must not count the commits the first one already
    // counted. That is the frontier doing its job, and a graph that
    // doubled on a re-push would be a number nobody could explain.
    //
    // The second push carries **one new commit**, and the wait is for
    // *that* commit rather than for a stretch of wall-clock. Sleeping
    // and asserting the total is unchanged is the same test written to
    // pass against a second walk that had not started yet — which is no
    // test at all. `total == 2` cannot be true until the second job has
    // run, so what follows it is a measurement.
    std::fs::write(work.join("new.txt"), "new\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &[
            "commit",
            "-q",
            "--author=Author <ada@old-laptop.example>",
            "--date",
            "2026-03-20T12:00:00+0000",
            "-m",
            "later work",
        ],
    );
    push(&server, &work, &admin, "acme", "widget");
    let again = graph_until(
        &server,
        "ada",
        "from=2026-01-01&to=2026-12-31",
        "the second push's commit never appeared",
        |g| g["total"].as_i64() == Some(2),
    );
    assert_eq!(
        day_count(&again, "2026-03-02"),
        1,
        "a re-push counted the first push's work a second time: {again}"
    );
    assert_eq!(day_count(&again, "2026-03-20"), 1, "{again}");
    assert_eq!(day_count(&again, "2026-03-05"), 0, "{again}");
    assert_eq!(day_count(&again, "2026-03-07"), 0, "{again}");

    // A range that is not a range is the caller's mistake, in a
    // sentence, and not a 500.
    let (st, body) = server.req(
        "GET",
        "/v1/users/ada/contributions?from=2026-13-45&to=2026-12-31",
        "",
        None,
    );
    assert_eq!(st, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("YYYY"),
        "{body}"
    );
    let (st, _) = server.req(
        "GET",
        "/v1/users/ada/contributions?from=2026-12-31&to=2026-01-01",
        "",
        None,
    );
    assert_eq!(st, 400);
    // A blank date is a cleared picker, not a malformed one: it reads
    // as absence and the default window answers. Same rule as the blank
    // `?limit=` on the contributors route below, and the same rule
    // `/checks/runs` applies to every filter it takes.
    let (st, blank) = server.req("GET", "/v1/users/ada/contributions?from=&to=", "", None);
    assert_eq!(
        st, 200,
        "a blank ?from=&to= was refused rather than read as a cleared \
         filter: {blank}"
    );
    // And it is genuinely the default window, not some degenerate one:
    // the span is the same 364 days an absent `from`/`to` produces.
    // Asserted as a span rather than against literal dates so the
    // clock cannot make it flake.
    let day = |k: &str| {
        stratum_control::contribs::day_from_iso(blank[k].as_str().unwrap_or_default())
            .unwrap_or_else(|e| panic!("{k} is not a date: {e}: {blank}"))
    };
    assert_eq!(
        day("to") - day("from"),
        364,
        "a blank window is not the default window: {blank}"
    );
    // A handle nobody has is the same 404 the profile gives.
    assert_eq!(
        server
            .req("GET", "/v1/users/nobody-here/contributions", "", None)
            .0,
        404
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The leak test. It asserts on the *raw text* of the response, because
/// the field a leak arrives in is the field nobody thought to parse.
#[test]
fn a_stranger_learns_nothing_about_a_private_repository_from_a_graph() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e-private");
    let scratch = Scratch::new("contribs-private");
    let mail = Mailbox::temp("contribs-private");
    let server = spawn(&bucket.base_url, &scratch, "contribs_private", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let admin = server.bootstrap_org("acme");
    for (name, public) in [("widget", true), ("skunkworks", false)] {
        let (st, body) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name, "public": public })),
        );
        assert_eq!(st, 201, "{body}");
    }

    let open = scratch.path().join("widget");
    build_repo(&open, &[("ada@example.com", "2026-04-01", "in the open")]);
    push(&server, &open, &admin, "acme", "widget");

    let secret = scratch.path().join("skunkworks");
    build_repo(
        &secret,
        &[
            ("ada@example.com", "2026-04-01", "the unreleased thing"),
            ("ada@example.com", "2026-04-02", "more of it"),
        ],
    );
    push(&server, &secret, &admin, "acme", "skunkworks");

    let range = "from=2026-01-01&to=2026-12-31";

    // Opt in **first**, and wait for the number that proves *both*
    // walks are finished.
    //
    // The order is the whole reliability of this test and it was wrong
    // once. Waiting for the public repository's square and then reading
    // the private one in the same breath passes on a fast machine and
    // fails under a full-suite load, because the second walk is a
    // second background job and nothing said it had run. Worse, it
    // makes the opted-*out* assertions below meaningless when they do
    // pass: "a stranger sees only the public commit" is equally true of
    // a server that has not counted the private one yet, so the test
    // would be green against a graph that leaked nothing because it
    // knew nothing.
    //
    // Opting in first is what makes `total == 3` observable, and
    // `total == 3` is the only signal that says both jobs are done.
    let (st, body) = ada.req(
        "PATCH",
        "/v1/users/ada",
        Some(serde_json::json!({ "contrib_private_optin": true })),
    );
    assert_eq!(st, 200, "{body}");

    let g = graph_until(
        &server,
        "ada",
        range,
        "one public and two private commits never all appeared",
        |g| g["total"].as_i64() == Some(3),
    );
    // Every byte the server would send, re-rendered from what it sent:
    // the assertions below are on the whole document rather than on the
    // fields somebody thought to name, because a leak arrives in the
    // field nobody thought to parse.
    let raw = g.to_string();

    assert_eq!(g["private_included"], true, "{g}");
    assert_eq!(
        day_count(&g, "2026-04-01"),
        2,
        "the private commit did not join the day's total: {g}"
    );
    assert_eq!(day_count(&g, "2026-04-02"), 1, "{g}");

    // The whole leak surface, asserted on the bytes. A repository whose
    // name a stranger must not learn does not appear anywhere in the
    // response — not as a name, not as a title, not in a link.
    assert!(
        !raw.contains("skunkworks"),
        "the private repository's name is in the graph a stranger reads: {raw}"
    );
    assert!(
        !raw.contains("unreleased"),
        "a private commit message is in the graph: {raw}"
    );
    // The public one is named, which is what makes the absence above a
    // real finding rather than an empty response passing by accident.
    assert!(raw.contains("widget"), "{raw}");
    // ...and the day that mixes both names exactly one repository.
    let mixed = g["days"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["date"] == "2026-04-01")
        .unwrap();
    assert_eq!(mixed["repos"].as_array().unwrap().len(), 1, "{mixed}");
    // The day that is *only* private names none — a green square with
    // nothing behind it, which is the entire contract.
    let only_private = g["days"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["date"] == "2026-04-02")
        .unwrap();
    assert!(
        only_private["repos"].as_array().unwrap().is_empty(),
        "{only_private}"
    );

    // The owner reading her own graph sees exactly what the stranger
    // sees. A public page that says different things to different
    // readers is a page nobody can quote.
    let (_, mine) = ada.req("GET", &format!("/v1/users/ada/contributions?{range}"), None);
    assert_eq!(mine, g, "the owner's view differs from a stranger's");

    // No `from`, no `to` — which is what the profile page actually
    // requests, and the only call that reaches the default window at
    // all. Every other test here names a range, so the server's own
    // notion of "the last year ending today" had no coverage: a broken
    // default would have failed on the page and passed in the suite.
    //
    // Asserted as a shape rather than as dates, because the answer
    // moves every midnight and a test that pins today's date is a test
    // that fails on a Tuesday.
    let (st, dflt) = server.req("GET", "/v1/users/ada/contributions", "", None);
    assert_eq!(st, 200, "the default window was refused: {dflt}");
    // The window is `from`/`to`, not the length of `days`: the response
    // carries only the days that have something on them and the client
    // draws the empty squares. Asserting a dense year here would be
    // asserting a payload shape nobody chose — and it is what this test
    // did first, which is how I learned the difference.
    let from = stratum_control::contribs::day_from_iso(dflt["from"].as_str().expect("from"))
        .expect("from parses");
    let to = stratum_control::contribs::day_from_iso(dflt["to"].as_str().expect("to"))
        .expect("to parses");
    let span = to - from + 1;
    assert!(
        (365..=366).contains(&span),
        "the default window spans {span} days, which is not a year: {dflt}"
    );
    // Ordered, and every day inside the window it claims.
    let days = dflt["days"].as_array().expect("days");
    let dates: Vec<&str> = days.iter().map(|d| d["date"].as_str().unwrap()).collect();
    let mut sorted = dates.clone();
    sorted.sort_unstable();
    assert_eq!(dates, sorted, "the default window is not in date order");
    for d in days {
        let day = d["day"].as_i64().expect("day") as i32;
        assert!(
            day >= from && day <= to,
            "a day outside the window it reported: {d} not in {from}..={to}"
        );
    }

    // Opting back out withdraws it again, retroactively — and this
    // assertion means something only because the walk above proved the
    // private rows exist. A stranger is now back to the public commit
    // alone, with no way to tell a quiet fortnight from a busy private
    // one.
    let (st, _) = ada.req(
        "PATCH",
        "/v1/users/ada",
        Some(serde_json::json!({ "contrib_private_optin": false })),
    );
    assert_eq!(st, 200);
    let (_, back) = server.req(
        "GET",
        &format!("/v1/users/ada/contributions?{range}"),
        "",
        None,
    );
    assert_eq!(back["total"], 1, "{back}");
    assert_eq!(back["private_included"], false, "{back}");
    assert_eq!(day_count(&back, "2026-04-01"), 1, "{back}");
    assert_eq!(
        day_count(&back, "2026-04-02"),
        0,
        "a day that is only private work is absent entirely once she \
         opts out: {back}"
    );
    assert!(
        !back.to_string().contains("skunkworks"),
        "the private repository is named to an opted-out reader: {back}"
    );

    // Making the repository public shows the same work in full detail,
    // retroactively — which is the reason the rows are raw rather than
    // pre-summed.
    let (st, body) = server.req(
        "PATCH",
        "/v1/orgs/acme/repos/skunkworks",
        &admin,
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(st, 200, "{body}");
    let (_, now) = server.req(
        "GET",
        &format!("/v1/users/ada/contributions?{range}"),
        "",
        None,
    );
    assert!(
        now.to_string().contains("skunkworks"),
        "a repository made public did not retroactively show its work: {now}"
    );
    assert_eq!(now["total"], 3, "{now}");
    assert_eq!(
        now["private_included"], false,
        "nothing private is left to include: {now}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The second ground a commit can count on, and the one case that must
/// count for nobody.
#[test]
fn an_unknown_address_counts_for_the_pusher_and_an_agents_commit_for_nobody() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e-pusher");
    let scratch = Scratch::new("contribs-pusher");
    let mail = Mailbox::temp("contribs-pusher");
    let server = spawn(&bucket.base_url, &scratch, "contribs_pusher", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");
    // A token that acts as *her*, which is what makes the push a fact
    // about a person rather than about a machine.
    let (st, minted) = ada.req(
        "POST",
        "/v1/orgs/ada/tokens",
        Some(serde_json::json!({"scopes": ["repo:write"], "label": "laptop"})),
    );
    assert_eq!(st, 201, "{minted}");
    let hers = minted["token"].as_str().unwrap().to_string();

    let work = scratch.path().join("widget");
    build_repo(
        &work,
        &[
            // An address nobody has proved — a `git config user.email`
            // from a machine she set up years ago. She pushed it, and
            // that is a fact about our own server rather than a claim
            // in a file, so it counts.
            ("ada@some-old-box.invalid", "2026-05-01", "hers, unproved"),
            // The same, except an agent had a hand in it. It counts for
            // nobody: a graph that goes green because an agent ran
            // overnight is measuring the agent.
            (
                "ada@some-old-box.invalid",
                "2026-05-02",
                "with help\n\nCo-authored-by: Claude <noreply@anthropic.com>",
            ),
            // And a *human* pair-programmed commit, carrying the very
            // same trailer in the very same spelling GitHub's UI writes.
            // This is the one that separates "an agent was involved"
            // from "a trailer was present", and getting it wrong takes
            // the collaborative work a maintainer is proudest of off
            // their graph on the day they migrate.
            (
                "ada@some-old-box.invalid",
                "2026-05-03",
                "paired on it\n\nCo-authored-by: Bob <bob@example.invalid>",
            ),
        ],
    );
    push(&server, &work, &hers, "ada", "widget");

    let g = graph_until(
        &server,
        "ada",
        "from=2026-01-01&to=2026-12-31",
        "the pushed-by commits never appeared",
        |g| g["total"].as_i64() == Some(2),
    );
    assert_eq!(
        day_count(&g, "2026-05-01"),
        1,
        "a commit she pushed with an address nobody proved did not \
         count for her: {g}"
    );
    assert_eq!(
        day_count(&g, "2026-05-02"),
        0,
        "an agent-assisted commit inflated a human's graph: {g}"
    );
    assert_eq!(
        day_count(&g, "2026-05-03"),
        1,
        "a commit two humans wrote together counted for neither — \
         `Co-authored-by` is GitHub's pair-programming trailer, not an \
         agent signal: {g}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn following_is_a_persons_act_and_nobody_follows_themselves() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e-follows");
    let scratch = Scratch::new("contribs-follows");
    let mail = Mailbox::temp("contribs-follows");
    let server = spawn(&bucket.base_url, &scratch, "contribs_follows", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let mut bob = signup(&server, &mail, "bob", "bob@example.com");
    let admin = server.bootstrap_org("acme");

    // Nobody follows anybody, and that is an honest pair of zeroes
    // rather than an absent field.
    let (st, body) = server.req("GET", "/v1/users/ada/follow", "", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["followers"], 0);
    assert_eq!(body["following"], 0);
    assert_eq!(
        body["you_follow"], false,
        "a signed-out reader follows nobody: {body}"
    );

    let (st, body) = bob.req("PUT", "/v1/users/ada/follow", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["followers"], 1, "{body}");
    assert_eq!(body["you_follow"], true, "{body}");

    // Following twice is following once.
    let (_, body) = bob.req("PUT", "/v1/users/ada/follow", None);
    assert_eq!(
        body["followers"], 1,
        "a second follow counted twice: {body}"
    );

    // Following yourself is a sentence, not a 500 out of the CHECK.
    let (st, body) = ada.req("PUT", "/v1/users/ada/follow", None);
    assert_eq!(st, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("yourself"),
        "{body}"
    );

    // A service token has no opinion about a person.
    let (st, body) = server.req("PUT", "/v1/users/ada/follow", &admin, None);
    assert_eq!(st, 401, "a token followed somebody: {body}");

    // The listings name people, in both directions.
    let (st, body) = server.req("GET", "/v1/users/ada/followers", "", None);
    assert_eq!(st, 200, "{body}");
    let people = body["people"].as_array().unwrap();
    assert_eq!(people.len(), 1, "{body}");
    assert_eq!(people[0]["handle"], "bob");
    let (_, body) = server.req("GET", "/v1/users/bob/following", "", None);
    assert_eq!(body["people"].as_array().unwrap()[0]["handle"], "ada");
    // ...and the empty direction is an empty list, not an error.
    let (st, body) = server.req("GET", "/v1/users/ada/following", "", None);
    assert_eq!(st, 200, "{body}");
    assert!(body["people"].as_array().unwrap().is_empty(), "{body}");

    // Ada's own view of her page tells her Bob is not somebody *she*
    // follows, which is a different fact from the count.
    let (_, body) = ada.req("GET", "/v1/users/bob/follow", None);
    assert_eq!(body["followers"], 0, "{body}");
    assert_eq!(body["following"], 1, "bob follows one person: {body}");
    assert_eq!(body["you_follow"], false, "{body}");

    // Unfollowing removes the row; unfollowing again is the state you
    // are already in, not an error.
    let (st, body) = bob.req("DELETE", "/v1/users/ada/follow", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["followers"], 0, "{body}");
    let (st, body) = bob.req("DELETE", "/v1/users/ada/follow", None);
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["followers"], 0, "{body}");

    // A handle nobody has is a 404, before any credential question.
    assert_eq!(
        server
            .req("GET", "/v1/users/nobody-here/follow", "", None)
            .0,
        404
    );
    // ...including a name that could never have been stored, which must
    // never reach a query.
    assert_eq!(
        server
            .req("GET", "/v1/users/not%20a%20handle/follow", "", None)
            .0,
        404
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The two operational knobs, and what each one honestly does.
#[test]
fn the_walker_can_be_turned_off_and_its_bound_drops_history_rather_than_doubling_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e-knobs");
    let scratch = Scratch::new("contribs-knobs");
    let mail = Mailbox::temp("contribs-knobs");
    // Off. An operator who does not want the walker running must be
    // able to say so, and the push path must not notice: the whole
    // design is that authorship is never charged to the pusher.
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_hint("contribs_knobs")
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_CONTRIB_POLL_SECS", "0");
    for (k, v) in mail.env() {
        b = b.env(k, v);
    }
    let mut server = b.start();

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let admin = server.bootstrap_org("acme");
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "widget", "public": true })),
    );
    assert_eq!(st, 201, "{body}");
    let _ = &mut ada;

    let work = scratch.path().join("widget");
    build_repo(
        &work,
        &[
            ("ada@example.com", "2026-06-01", "one"),
            ("ada@example.com", "2026-06-02", "two"),
            ("ada@example.com", "2026-06-03", "three"),
        ],
    );
    push(&server, &work, &admin, "acme", "widget");
    std::thread::sleep(Duration::from_secs(3));
    let range = "from=2026-01-01&to=2026-12-31";
    let (st, body) = server.req(
        "GET",
        &format!("/v1/users/ada/contributions?{range}"),
        "",
        None,
    );
    assert_eq!(
        st, 200,
        "the graph is still served with the walker off: {body}"
    );
    assert_eq!(
        body["total"], 0,
        "the walker ran with its poll set to 0: {body}"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);

    // Now the bound. One visit per job: the tip is counted and the tail
    // of history is *dropped*, not carried. That is the deliberate
    // choice — carrying a partial walk means resuming from a frontier,
    // and a merge straddling the boundary would then be counted twice,
    // which is a square greener than the person's work and one nobody
    // can correct. Undercounting says so; over-counting cannot.
    server.restart_with(&[
        ("STRATUM_CONTRIB_POLL_SECS", "1".into()),
        ("STRATUM_CONTRIB_MAX_VISITS", "1".into()),
    ]);
    let g = graph_until(
        &server,
        "ada",
        range,
        "the bounded walk counted nothing at all",
        |g| g["total"].as_i64() == Some(1),
    );
    assert_eq!(
        day_count(&g, "2026-06-03"),
        1,
        "the newest commit is the one a single visit reaches: {g}"
    );
    assert_eq!(
        g["total"], 1,
        "a bounded walk carried more than it was allowed: {g}"
    );

    // And the frontier moved, so the dropped tail stays dropped rather
    // than being re-walked and counted on top of what is already there.
    // One new commit again, so the wait is for the thing the assertion
    // depends on rather than for a stretch of wall-clock.
    std::fs::write(work.join("new.txt"), "new\n").unwrap();
    gitcli::git(&work, &["add", "-A"]);
    gitcli::git(
        &work,
        &[
            "commit",
            "-q",
            "--author=Author <ada@example.com>",
            "--date",
            "2026-06-20T12:00:00+0000",
            "-m",
            "four",
        ],
    );
    push(&server, &work, &admin, "acme", "widget");
    let again = graph_until(
        &server,
        "ada",
        range,
        "the second bounded walk counted nothing",
        |g| g["total"].as_i64() == Some(2),
    );
    assert_eq!(day_count(&again, "2026-06-20"), 1, "{again}");
    assert_eq!(
        day_count(&again, "2026-06-03"),
        1,
        "the one commit the first bounded walk reached was counted \
         again: {again}"
    );
    for dropped in ["2026-06-01", "2026-06-02"] {
        assert_eq!(
            day_count(&again, dropped),
            0,
            "history the bound dropped came back on a later walk, which \
             is the double-count the bound exists to avoid: {again}"
        );
    }

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

// ---------------------------------------------------------------------
// `GET /v1/orgs/:org/repos/:repo/contributors`
// ---------------------------------------------------------------------

/// Poll the contributors of one repository until `want` says yes.
///
/// A poll for the same reason `graph_until` is one: the push returns
/// before the walker has counted anything, so a test that read once
/// would be asserting the opposite of the design.
fn contributors_until(
    server: &Server,
    org: &str,
    repo: &str,
    what: &str,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = serde_json::Value::Null;
    while Instant::now() < deadline {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/{org}/repos/{repo}/contributors"),
            "",
            None,
        );
        assert_eq!(st, 200, "contributors of {org}/{repo}: {body}");
        if want(&body) {
            return body;
        }
        last = body;
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("{what}; last answer was {last}");
}

/// The handles in a contributors response, in the order they arrived.
fn handles(body: &serde_json::Value) -> Vec<String> {
    body["contributors"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|c| c["handle"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The rail's own read: who worked here, most first, to a signed-out
/// stranger looking at a public project.
///
/// The totals are asserted rather than the order alone. An endpoint that
/// returned the right people with the wrong numbers would render a rail
/// that looks entirely plausible and attributes somebody else's work.
#[test]
fn contributors_are_ranked_with_their_totals_and_a_stranger_reads_a_public_repo() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e-people");
    let scratch = Scratch::new("contribs-people");
    let mail = Mailbox::temp("contribs-people");
    let server = spawn(&bucket.base_url, &scratch, "contribs_people", &mail);

    let _ada = signup(&server, &mail, "ada", "ada@example.com");
    let _bob = signup(&server, &mail, "bob", "bob@example.com");
    let admin = server.bootstrap_org("acme");
    for name in ["widget", "untouched"] {
        let (st, body) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name, "public": true })),
        );
        assert_eq!(st, 201, "{body}");
    }

    // Pushed with a *service* token, so nothing here is attributable to
    // the pusher and the proved sign-up addresses are the only ground —
    // which is what makes the two totals below a fact about authorship.
    let work = scratch.path().join("widget");
    build_repo(
        &work,
        &[
            ("ada@example.com", "2026-07-01", "one"),
            ("ada@example.com", "2026-07-02", "two"),
            ("ada@example.com", "2026-07-03", "three"),
            ("bob@example.com", "2026-07-04", "bob's one"),
        ],
    );
    push(&server, &work, &admin, "acme", "widget");

    // Signed out: `""` is no credential at all, and a public project's
    // contributor rail is part of what a visitor is deciding on.
    let body = contributors_until(
        &server,
        "acme",
        "widget",
        "the contributors never appeared to a signed-out reader",
        |b| b["contributors"].as_array().map(Vec::len) == Some(2),
    );
    assert_eq!(
        handles(&body),
        ["ada", "bob"],
        "not ranked by total, most first: {body}"
    );
    let people = body["contributors"].as_array().unwrap();
    assert_eq!(people[0]["commits"], 3, "{body}");
    assert_eq!(people[1]["commits"], 1, "{body}");
    // A user id a client can link on, and a day it can render.
    assert!(
        people[0]["user_id"].as_str().is_some_and(|s| !s.is_empty()),
        "{body}"
    );
    assert!(
        people[0]["last_at"].as_i64().unwrap_or(0) > 0,
        "the most recent contributing day is missing: {body}"
    );
    assert!(
        people[0]["last_at"].as_i64() < people[1]["last_at"].as_i64(),
        "bob's day is the later one: {body}"
    );

    // `?limit=` is passed through: one asked for is one returned, and
    // it is the top of the ranking rather than an arbitrary row.
    let (st, one) = server.req(
        "GET",
        "/v1/orgs/acme/repos/widget/contributors?limit=1",
        "",
        None,
    );
    assert_eq!(st, 200, "{one}");
    assert_eq!(handles(&one), ["ada"], "{one}");
    // A limit past the ceiling is a good question asked greedily, not an
    // error — it comes back clamped, with everybody we have.
    let (st, lots) = server.req(
        "GET",
        "/v1/orgs/acme/repos/widget/contributors?limit=100000",
        "",
        None,
    );
    assert_eq!(st, 200, "a large limit was refused: {lots}");
    assert_eq!(handles(&lots), ["ada", "bob"], "{lots}");

    // A repository nobody has committed to is an empty list and a 200.
    // An empty list is a fact about a real repository; a 404 there would
    // be a different and false statement, and a client would render it
    // as a broken page.
    let (st, empty) = server.req(
        "GET",
        "/v1/orgs/acme/repos/untouched/contributors",
        "",
        None,
    );
    assert_eq!(st, 200, "an empty repository was not served: {empty}");
    assert!(
        empty["contributors"].as_array().is_some_and(Vec::is_empty),
        "an empty repository must answer an empty list, not a null or a \
         missing field: {empty}"
    );

    // A limit that is not a number is nobody's intention, so it is named
    // rather than silently replaced with a default — a page quietly
    // serving something other than what was asked for is the harder bug
    // to find.
    let (st, bad) = server.req(
        "GET",
        "/v1/orgs/acme/repos/widget/contributors?limit=lots",
        "",
        None,
    );
    assert_eq!(st, 400, "an unparseable limit was not refused: {bad}");
    assert!(
        bad["error"].as_str().unwrap_or_default().contains("limit"),
        "the refusal does not name the problem: {bad}"
    );

    // A *blank* limit is a cleared control, not a malformed number.
    // `/checks/runs` next door routes every parameter through a helper
    // that reads blank as absent, and this route read `limit` straight
    // out of the map — so a client that always sets every key got a 400
    // from one endpoint and the default from the other, for the same
    // query string. Same rail, same increment, two rules.
    let (st, blank) = server.req(
        "GET",
        "/v1/orgs/acme/repos/widget/contributors?limit=",
        "",
        None,
    );
    assert_eq!(
        st, 200,
        "a blank ?limit= was refused rather than read as a cleared \
         filter, which is what /checks/runs does with the same value: \
         {blank}"
    );
    assert_eq!(handles(&blank), ["ada", "bob"], "{blank}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

/// The masking claim: a private repository's contributors answer a
/// stranger *exactly* as a repository that was never created answers
/// them, and a member reading their own private repository gets the rows.
///
/// The second half is the one no other test can see fail. A stray
/// `WHERE public` in the query would leave every assertion in this file
/// green except that one — the rail would simply be empty for every
/// private project, which reads as "nobody has worked here yet".
#[test]
fn a_private_repositorys_contributors_are_masked_exactly_as_the_repository_is() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e-mask");
    let scratch = Scratch::new("contribs-mask");
    let mail = Mailbox::temp("contribs-mask");
    let server = spawn(&bucket.base_url, &scratch, "contribs_mask", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    // A perfectly real account with no business in ada's namespace. A
    // signed-in stranger is a different masking case from a signed-out
    // one, and they are told apart by status code.
    let mut carol = signup(&server, &mail, "carol", "carol@example.com");

    // Hers, private, in her own namespace.
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/ada/repos",
        Some(serde_json::json!({ "name": "skunkworks", "public": false })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, minted) = ada.req(
        "POST",
        "/v1/orgs/ada/tokens",
        Some(serde_json::json!({"scopes": ["repo:write"], "label": "laptop"})),
    );
    assert_eq!(st, 201, "{minted}");
    let hers = minted["token"].as_str().unwrap().to_string();

    let secret = scratch.path().join("skunkworks");
    build_repo(
        &secret,
        &[
            ("ada@example.com", "2026-08-01", "the unreleased thing"),
            ("ada@example.com", "2026-08-02", "more of it"),
        ],
    );
    push(&server, &secret, &hers, "ada", "skunkworks");

    // The owner sees her own work. This is a poll, so it also pins the
    // moment the rows exist — every masking assertion below is made
    // *after* there is something real to leak, which is what stops them
    // passing against a server that simply has not counted yet.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mine = loop {
        let (st, body) = ada.req("GET", "/v1/orgs/ada/repos/skunkworks/contributors", None);
        assert_eq!(st, 200, "the owner was refused her own repository: {body}");
        if body["contributors"].as_array().map(Vec::len) == Some(1) {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "the owner never saw her own private repository's \
             contributors — a visibility filter in the aggregate would \
             look exactly like this: {body}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(handles(&mine), ["ada"], "{mine}");
    assert_eq!(
        mine["contributors"][0]["commits"], 2,
        "the owner's own total is short: {mine}"
    );

    // Now the masking, asserted as an *equality* rather than against a
    // literal number: what matters is that the two answers cannot be
    // told apart, not which code they happen to be.
    let private = "/v1/orgs/ada/repos/skunkworks/contributors";
    let absent = "/v1/orgs/ada/repos/no-such-repo-here/contributors";

    let (st_private, b_private) = server.req("GET", private, "", None);
    let (st_absent, b_absent) = server.req("GET", absent, "", None);
    assert_eq!(
        (st_private, &b_private),
        (st_absent, &b_absent),
        "a signed-out stranger can tell a private repository from one \
         that does not exist: {st_private} {b_private} vs {st_absent} \
         {b_absent}"
    );
    assert!(
        !b_private.to_string().contains("ada@example.com"),
        "a refusal carried a contributor's address: {b_private}"
    );

    let (st_private, b_private) = carol.req("GET", private, None);
    let (st_absent, b_absent) = carol.req("GET", absent, None);
    assert_eq!(
        (st_private, &b_private),
        (st_absent, &b_absent),
        "a signed-in stranger can tell a private repository from one \
         that does not exist: {st_private} {b_private} vs {st_absent} \
         {b_absent}"
    );
    assert!(
        !b_private.to_string().contains("skunkworks"),
        "the refusal names the repository it is hiding: {b_private}"
    );

    // Made public, the same rows are a stranger's to read — which is
    // what proves the refusals above were about visibility and not about
    // an endpoint that simply never answers.
    let (st, body) = ada.req(
        "PATCH",
        "/v1/orgs/ada/repos/skunkworks",
        Some(serde_json::json!({ "public": true })),
    );
    assert_eq!(st, 200, "{body}");
    let (st, open) = server.req("GET", private, "", None);
    assert_eq!(st, 200, "{open}");
    assert_eq!(handles(&open), ["ada"], "{open}");

    // The server is still serving after every refusal above. A server
    // that wedges and refuses everything would pass every masking
    // assertion in this test and be a total failure.
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
