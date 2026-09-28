//! Authorship — whose commits are whose — end to end against a real
//! server, driven with the stock `git` CLI, and read back through the
//! one route that shows it: a repository's contributors.
//!
//! Three claims are worth an end-to-end test rather than a unit test,
//! because all three are properties of the whole path — push, job,
//! walk, lookup, read — and each of them is a claim somebody could
//! reasonably doubt.
//!
//! **An unproved address counts for nothing.** Anybody can write
//! anybody's address into `git config user.email`. If a contributor
//! total moved on that, every total on the server would be worth
//! nothing. The test pushes commits that differ *only* in whether the
//! author address is a proved address on the account, and one counts.
//!
//! **A stranger learns nothing about a private repository from its
//! contributors.** Every repository is private to its organization, and
//! the rail is a new way to ask whether one exists: it answers a
//! stranger exactly as a repository that was never created answers them.
//!
//! **Authorship is never charged to the pusher.** Nothing here asserts
//! a duration — that would be a flake waiting to happen — but the push
//! returns before any of these totals exist, and every assertion below
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

/// Poll the contributors of one repository, as `token`, until `want`
/// says yes, or fail with the last body.
///
/// A poll rather than a sleep, because the whole point of the design is
/// that the push does not wait for the walk — so a test that assumed a
/// fixed delay would be asserting the opposite of the property.
fn contributors_until(
    server: &Server,
    org: &str,
    repo: &str,
    token: &str,
    what: &str,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = serde_json::Value::Null;
    while Instant::now() < deadline {
        let (st, body) = server.req(
            "GET",
            &format!("/v1/orgs/{org}/repos/{repo}/contributors"),
            token,
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

/// `(handle, commits)` for every contributor, in the order they arrived.
fn totals(body: &serde_json::Value) -> Vec<(String, i64)> {
    body["contributors"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|c| {
            (
                c["handle"].as_str().unwrap_or_default().to_string(),
                c["commits"].as_i64().unwrap_or(-1),
            )
        })
        .collect()
}

/// The sum of every contributor's commits — the number that moves when
/// a walk lands.
fn total(body: &serde_json::Value) -> i64 {
    totals(body).iter().map(|(_, n)| n).sum()
}

/// A date as the `last_at` a contributor row carries: the start of that
/// day, in milliseconds.
fn day_ms(date: &str) -> i64 {
    i64::from(stratum_control::contribs::day_from_iso(date).unwrap()) * 86_400_000
}

// ---------------------------------------------------------------------

/// The anti-gaming rule, end to end.
#[test]
fn a_proved_address_counts_and_an_unproved_one_counts_for_nobody() {
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
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{body}");

    let work = scratch.path().join("widget");
    build_repo(
        &work,
        &[
            // Proved: hers.
            ("ada@old-laptop.example", "2026-03-02", "real work"),
            // Unproved: anybody could have written this line, and it
            // must count for nothing at all.
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

    let c = contributors_until(
        &server,
        "acme",
        "widget",
        &admin,
        "the proved commit never counted for ada",
        |c| total(c) >= 1,
    );
    assert_eq!(
        totals(&c),
        [("ada".to_string(), 1)],
        "exactly one of four commits is hers, and nobody else is \
         anybody here — an address ada never proved, a stranger's, and \
         one she only claimed all count for nobody: {c}"
    );
    // And it is the proved one: the day it carries is its day, not the
    // later day of any of the unproved three.
    assert_eq!(
        c["contributors"][0]["last_at"],
        day_ms("2026-03-02"),
        "the day counted is not the proved commit's: {c}"
    );

    // A second push must not count the commits the first one already
    // counted. That is the frontier doing its job, and a total that
    // doubled on a re-push would be a number nobody could explain.
    //
    // The second push carries **one new commit**, and the wait is for
    // *that* commit rather than for a stretch of wall-clock. Sleeping
    // and asserting the total is unchanged is the same test written to
    // pass against a second walk that had not started yet — which is no
    // test at all. The new day cannot appear until the second job has
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
    let again = contributors_until(
        &server,
        "acme",
        "widget",
        &admin,
        "the second push's commit never counted",
        |c| c["contributors"][0]["last_at"] == day_ms("2026-03-20"),
    );
    assert_eq!(
        totals(&again),
        [("ada".to_string(), 2)],
        "a re-push counted the first push's work a second time, or an \
         unproved address on the way: {again}"
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
        Some(serde_json::json!({ "name": "widget" })),
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
            // A *human* pair-programmed commit, carrying the very same
            // trailer in the very same spelling GitHub's UI writes an
            // agent's in. This is the one that separates "an agent was
            // involved" from "a trailer was present", and getting it
            // wrong takes the collaborative work a maintainer is
            // proudest of off their record on the day they migrate.
            (
                "ada@some-old-box.invalid",
                "2026-05-02",
                "paired on it\n\nCo-authored-by: Bob <bob@example.invalid>",
            ),
            // The same, except an agent had a hand in it. It counts for
            // nobody: a total that grows because an agent ran overnight
            // is measuring the agent. Newest on purpose — had it
            // counted, it would be the day the row carries.
            (
                "ada@some-old-box.invalid",
                "2026-05-03",
                "with help\n\nCo-authored-by: Claude <noreply@anthropic.com>",
            ),
        ],
    );
    push(&server, &work, &hers, "ada", "widget");

    // One walk, one transaction: the moment anything is counted,
    // everything this push will ever count is.
    let c = contributors_until(
        &server,
        "ada",
        "widget",
        &hers,
        "the pushed-by commits never counted",
        |c| total(c) >= 2,
    );
    assert_eq!(
        totals(&c),
        [("ada".to_string(), 2)],
        "two of three are hers: the unproved address she pushed, and \
         the pair-programmed commit — `Co-authored-by` is GitHub's \
         pair-programming trailer, not an agent signal: {c}"
    );
    assert_eq!(
        c["contributors"][0]["last_at"],
        day_ms("2026-05-02"),
        "an agent-assisted commit inflated a human's total: {c}"
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

    let _ada = signup(&server, &mail, "ada", "ada@example.com");
    let admin = server.bootstrap_org("acme");
    let (st, body) = server.req(
        "POST",
        "/v1/orgs/acme/repos",
        &admin,
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{body}");

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
    let (st, body) = server.req(
        "GET",
        "/v1/orgs/acme/repos/widget/contributors",
        &admin,
        None,
    );
    assert_eq!(
        st, 200,
        "the rail is still served with the walker off: {body}"
    );
    assert_eq!(
        totals(&body),
        [] as [(String, i64); 0],
        "the walker ran with its poll set to 0: {body}"
    );
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);

    // Now the bound. One visit per job: the tip is counted and the tail
    // of history is *dropped*, not carried. That is the deliberate
    // choice — carrying a partial walk means resuming from a frontier,
    // and a merge straddling the boundary would then be counted twice,
    // which is a total higher than the person's work and one nobody can
    // correct. Undercounting says so; over-counting cannot.
    server.restart_with(&[
        ("STRATUM_CONTRIB_POLL_SECS", "1".into()),
        ("STRATUM_CONTRIB_MAX_VISITS", "1".into()),
    ]);
    let c = contributors_until(
        &server,
        "acme",
        "widget",
        &admin,
        "the bounded walk counted nothing at all",
        |c| total(c) >= 1,
    );
    assert_eq!(
        totals(&c),
        [("ada".to_string(), 1)],
        "a bounded walk carried more than it was allowed: {c}"
    );
    assert_eq!(
        c["contributors"][0]["last_at"],
        day_ms("2026-06-03"),
        "the newest commit is the one a single visit reaches: {c}"
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
    let again = contributors_until(
        &server,
        "acme",
        "widget",
        &admin,
        "the second bounded walk counted nothing",
        |c| c["contributors"][0]["last_at"] == day_ms("2026-06-20"),
    );
    assert_eq!(
        totals(&again),
        [("ada".to_string(), 2)],
        "history the bound dropped came back on a later walk, or the one \
         commit the first walk reached was counted again — the \
         double-count the bound exists to avoid: {again}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

// ---------------------------------------------------------------------
// `GET /v1/orgs/:org/repos/:repo/contributors`
// ---------------------------------------------------------------------

/// The rail's own read: who worked here, most first, to a member of the
/// organization — and to nobody outside it, including somebody whose
/// commits are in it.
///
/// The totals are asserted rather than the order alone. An endpoint that
/// returned the right people with the wrong numbers would render a rail
/// that looks entirely plausible and attributes somebody else's work.
#[test]
fn contributors_are_ranked_with_their_totals_for_a_member_and_nobody_else() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e-people");
    let scratch = Scratch::new("contribs-people");
    let mail = Mailbox::temp("contribs-people");
    let server = spawn(&bucket.base_url, &scratch, "contribs_people", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    let _bob = signup(&server, &mail, "bob", "bob@example.com");
    let admin = server.bootstrap_org("acme");
    for name in ["widget", "untouched"] {
        let (st, body) = server.req(
            "POST",
            "/v1/orgs/acme/repos",
            &admin,
            Some(serde_json::json!({ "name": name })),
        );
        assert_eq!(st, 201, "{body}");
    }
    // The reader: a read-only credential of the org, the weakest one
    // that is anybody here.
    let (st, minted) = server.req(
        "POST",
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({ "scopes": ["repo:read"], "label": "rail" })),
    );
    assert_eq!(st, 201, "{minted}");
    let reader = minted["token"].as_str().unwrap().to_string();

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

    let body = contributors_until(
        &server,
        "acme",
        "widget",
        &reader,
        "the contributors never appeared to a reader of the org",
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

    // Nobody outside the org reads it — not anonymously, and not Ada,
    // whose commits are three of the four: being counted in a
    // repository is not a role in it. Each answer is what a repository
    // that does not exist gets.
    let path = "/v1/orgs/acme/repos/widget/contributors";
    let absent = "/v1/orgs/acme/repos/no-such-repo/contributors";
    let (st, out) = server.req("GET", path, "", None);
    assert_eq!(st, 401, "{out}");
    assert_eq!((st, out), server.req("GET", absent, "", None));
    let (st, out) = ada.req("GET", path, None);
    assert_eq!(
        st, 404,
        "a contributor who is not a member read the rail: {out}"
    );
    assert_eq!((st, out), ada.req("GET", absent, None));

    // `?limit=` is passed through: one asked for is one returned, and
    // it is the top of the ranking rather than an arbitrary row.
    let (st, one) = server.req(
        "GET",
        "/v1/orgs/acme/repos/widget/contributors?limit=1",
        &reader,
        None,
    );
    assert_eq!(st, 200, "{one}");
    assert_eq!(handles(&one), ["ada"], "{one}");
    // A limit past the ceiling is a good question asked greedily, not an
    // error — it comes back clamped, with everybody we have.
    let (st, lots) = server.req(
        "GET",
        "/v1/orgs/acme/repos/widget/contributors?limit=100000",
        &reader,
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
        &reader,
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
        &reader,
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
        &reader,
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
/// them, and a member reading it gets the rows.
///
/// The member half is the one no other test can see fail. A stray
/// `WHERE public` in the query would leave every masking assertion in
/// this file green — the rail would simply be empty for every
/// repository, which reads as "nobody has worked here yet".
#[test]
fn a_private_repositorys_contributors_are_masked_exactly_as_the_repository_is() {
    let minio = Minio::shared();
    let bucket = minio.bucket("contribs-e2e-mask");
    let scratch = Scratch::new("contribs-mask");
    let mail = Mailbox::temp("contribs-mask");
    let server = spawn(&bucket.base_url, &scratch, "contribs_mask", &mail);

    let mut ada = signup(&server, &mail, "ada", "ada@example.com");
    // A perfectly real account with no business in ada's organization.
    // A signed-in stranger is a different masking case from a signed-out
    // one, and they are told apart by status code.
    let mut carol = signup(&server, &mail, "carol", "carol@example.com");
    // And a viewer of it, who is not the owner.
    let mut dave = signup(&server, &mail, "dave", "dave@example.com");

    // Hers, in an organization she runs.
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs",
        Some(serde_json::json!({ "name": "acme" })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "skunkworks" })),
    );
    assert_eq!(st, 201, "{body}");
    ada.invite_and_accept("acme", "dave@example.com", "viewer");
    let (st, minted) = ada.req(
        "POST",
        "/v1/orgs/acme/tokens",
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
    push(&server, &secret, &hers, "acme", "skunkworks");

    // The owner sees her own work. This is a poll, so it also pins the
    // moment the rows exist — every masking assertion below is made
    // *after* there is something real to leak, which is what stops them
    // passing against a server that simply has not counted yet.
    let private = "/v1/orgs/acme/repos/skunkworks/contributors";
    let absent = "/v1/orgs/acme/repos/no-such-repo-here/contributors";
    let deadline = Instant::now() + Duration::from_secs(30);
    let mine = loop {
        let (st, body) = ada.req("GET", private, None);
        assert_eq!(st, 200, "the owner was refused her own repository: {body}");
        if body["contributors"].as_array().map(Vec::len) == Some(1) {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "the owner never saw her own repository's contributors — a \
             visibility filter in the aggregate would look exactly like \
             this: {body}"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(handles(&mine), ["ada"], "{mine}");
    assert_eq!(
        mine["contributors"][0]["commits"], 2,
        "the owner's own total is short: {mine}"
    );

    // Now the masking, asserted as an *equality* as well as a code:
    // what matters most is that the two answers cannot be told apart.
    let (st_private, b_private) = server.req("GET", private, "", None);
    let (st_absent, b_absent) = server.req("GET", absent, "", None);
    assert_eq!(st_private, 401, "signed out: {b_private}");
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
    assert_eq!(st_private, 404, "signed in elsewhere: {b_private}");
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

    // A member who is not the owner reads the same rows — which is what
    // proves the refusals above were about who is asking and not about
    // an endpoint that only ever answers its owner.
    let (st, theirs) = dave.req("GET", private, None);
    assert_eq!(st, 200, "{theirs}");
    assert_eq!(
        theirs, mine,
        "a viewer reads a different rail from the owner"
    );

    // The server is still serving after every refusal above. A server
    // that wedges and refuses everything would pass every masking
    // assertion in this test and be a total failure.
    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
