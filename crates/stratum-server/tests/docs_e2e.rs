//! The docs are executable: every ```bash block in the Repos quickstart
//! runs verbatim against a live server — only the placeholder host and
//! env vars are substituted. If the docs drift from the API, this fails.

use std::process::{Child, Command};
use std::time::{Duration, Instant};
use stratum_testkit::gitcli::Scratch;
use stratum_testkit::minio::{ROOT_PASSWORD, ROOT_USER};
use stratum_testkit::Minio;

const QUICKSTART: &str = include_str!("../../../docs/guide/quickstart-repos.md");

struct Server {
    child: Child,
    base: String,
    db_url: String,
    store_url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        // SIGINT takes the server's graceful-shutdown path, which also
        // lets an instrumented (coverage) child flush its profile;
        // SIGKILL only as a bounded fallback.
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

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    let db_url = stratum_testkit::pg::test_db_url("docs");
    let (child, bind) = stratum_testkit::server::spawn_on_free_port(|bind| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        cmd.env_clear()
            // Coverage runs need the instrumented child to write its profile.
            .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("STRATUM_STORE_URL", store_url)
            .env("STRATUM_DB_URL", &db_url)
            .env("STRATUM_DATA_DIR", scratch.path().join("data"))
            .env("STRATUM_BIND", bind)
            .env("STRATUM_MIRROR_POLL_SECS", "0")
            .env("AWS_ACCESS_KEY_ID", ROOT_USER)
            .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
            .env("AWS_REGION", "us-east-1");
        cmd
    });
    Server {
        child,
        base: format!("http://{bind}"),
        db_url,
        store_url: store_url.to_string(),
    }
}

fn bash_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        match &mut current {
            None if line.trim() == "```bash" => current = Some(String::new()),
            None => {}
            Some(b) => {
                if line.trim() == "```" {
                    blocks.push(current.take().unwrap());
                } else {
                    b.push_str(line);
                    b.push('\n');
                }
            }
        }
    }
    blocks
}

/// Pull a JSON field out of an API response inside the bash script —
/// python3 is the only dependency, present wherever CI runs.
fn json_get(expr: &str) -> String {
    format!("python3 -c 'import json,sys; print(json.load(sys.stdin){expr})'")
}

#[test]
fn repos_quickstart_runs_verbatim() {
    let minio = Minio::shared();
    let bucket = minio.bucket("docs-qs");
    let scratch = Scratch::new("docs");
    let server = spawn_server(&bucket.base_url, &scratch);

    // Bootstrap the org the docs assume ("acme") via the operator CLI.
    let out = Command::new(env!("CARGO_BIN_EXE_stratum-server"))
        .env_clear()
        // Coverage runs need the instrumented child to write its profile.
        .envs(std::env::var("LLVM_PROFILE_FILE").map(|p| ("LLVM_PROFILE_FILE", p)))
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("STRATUM_STORE_URL", &server.store_url)
        .env("STRATUM_DB_URL", &server.db_url)
        .env("AWS_ACCESS_KEY_ID", ROOT_USER)
        .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
        .env("AWS_REGION", "us-east-1")
        .args(["admin", "bootstrap", "--org", "acme"])
        .output()
        .expect("bootstrap");
    assert!(out.status.success());
    let boot: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let token = boot["admin_token"].as_str().unwrap();

    let blocks = bash_blocks(QUICKSTART);
    assert_eq!(
        blocks.len(),
        6,
        "quickstart-repos.md changed shape; update this runner"
    );

    let hostport = server.base.strip_prefix("http://").unwrap();
    let subst = |b: &str| {
        b.replace(
            "https://x:$WEFT_TOKEN@spool.example.com",
            &format!("http://x:$WEFT_TOKEN@{hostport}"),
        )
        .replace("https://spool.example.com", &format!("http://{hostport}"))
    };

    // Glue between doc blocks supplies the values a reader would have on
    // hand ($OLD_COMMIT etc.) and asserts the promised outcomes.
    let api = format!("http://{hostport}/v1/orgs/acme/repos/session-8412");
    let auth = r#"-H "Authorization: Bearer $WEFT_TOKEN""#;
    let head_of_log = format!(
        "curl -sf {auth} {api}/log | {}",
        json_get("[\"entries\"][0][\"commit\"]")
    );
    let script = format!(
        r#"set -euo pipefail
export WEFT_TOKEN={token}

# --- doc block 1: create the repo ---
{b0}

# --- doc block 2: first commit ---
{b1}

FIRST=$({head_of_log})
export OLD_COMMIT=$FIRST

# --- doc block 3: reads (newest + pinned) ---
{b2}

# --- doc block 4: one file's history ---
# Run here, where the document puts it: section 3, before anything has
# gone sideways. One commit has touched this file so far, and the filter
# should say it was that commit that added it.
{b3}

# The doc block does not pass `curl -f`, so on its own it would print an
# error body and still exit 0. Asserting the outcome is what makes
# running it worth anything.
N=$(curl -sf {auth} "{api}/log?path=src/app.js" | {entries_len})
test "$N" = "1"
curl -sf {auth} "{api}/log?path=src/app.js" | grep -q '"change":"added"'
# A path nothing ever touched is an empty history, not an error.
M=$(curl -sf {auth} "{api}/log?path=nope/never.txt" | {entries_len})
test "$M" = "0"

# a second commit to play the "agent went sideways" step against
BAD=$(curl -sf -X POST {api}/commits {auth} -H "Content-Type: application/json" \
  -d '{{"message":"oops","operations":[{{"op":"put","path":"src/app.js","content":"broken\n"}}]}}' \
  | {commit_field})
export GOOD_COMMIT=$FIRST
export BAD_COMMIT=$BAD

# --- doc block 5: undo ---
{b4}

HEAD_NOW=$({head_of_log})
test "$HEAD_NOW" = "$GOOD_COMMIT"
# the undone commit stays reachable by SHA (undo never erases the record)
curl -sf {auth} "{api}/files/src/app.js?at=$BAD_COMMIT" | grep -q broken

# --- doc block 6: it's still git ---
{b5}

grep -q 'console.log(1)' session-8412/src/app.js
"#,
        b0 = subst(&blocks[0]),
        b1 = subst(&blocks[1]),
        b2 = subst(&blocks[2]),
        b3 = subst(&blocks[3]),
        b4 = subst(&blocks[4]),
        b5 = subst(&blocks[5]),
        commit_field = json_get("[\"commit\"]"),
        entries_len = json_get("[\"entries\"].__len__()"),
    );

    let run = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(scratch.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run quickstart script");
    assert!(
        run.status.success(),
        "quickstart commands failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );

    // And the clone the docs produced is a healthy repository (I11).
    stratum_testkit::gitcli::fsck(&scratch.path().join("session-8412"));
}

/// The OpenAPI document and the router must describe the same API.
///
/// The spec is `include_str!`'d into the binary, so it always *ships*
/// with the server — but nothing until now stopped a new route from being
/// added and never written down, or a documented one from being deleted.
/// This reads both and insists they agree.
#[test]
fn every_documented_route_exists_and_every_api_route_is_documented() {
    const APP: &str = include_str!("../src/app.rs");
    const SPEC: &str = include_str!("../../../docs/openapi.json");

    // Routes the document deliberately does not describe: the git wire
    // (it speaks the git protocol, not JSON), the served web surfaces,
    // and the operational probes the API description says nothing about.
    const UNDOCUMENTED: &[&str] = &[
        "/readyz",
        "/openapi.json",
        "/dashboard",
        "/dashboard/",
        "/dashboard/{path}",
        "/{org}/{repo}/info/refs",
        "/{org}/{repo}/git-upload-pack",
        "/{org}/{repo}/git-receive-pack",
        // The changeset workspace is the same wire, one path deeper; the
        // JSON that names its clone URL is documented under
        // `/v1/orgs/{org}/changesets/{changeset}/workspace`.
        "/{org}/changesets/{key}/info/refs",
        "/{org}/changesets/{key}/git-upload-pack",
        "/{org}/changesets/{key}/git-receive-pack",
    ];

    let mut routed: Vec<String> = Vec::new();
    for (i, _) in APP.match_indices(".route(") {
        let rest = &APP[i + ".route(".len()..];
        let open = match rest.find('"') {
            Some(o) => o,
            None => continue,
        };
        let close = rest[open + 1..].find('"').expect("unterminated route path");
        let path = &rest[open + 1..open + 1 + close];
        // Axum spells parameters `:name` and wildcards `*name`; OpenAPI
        // spells both `{name}`.
        let openapi_shape = path
            .split('/')
            .map(|seg| match seg.strip_prefix([':', '*']) {
                Some(name) => format!("{{{name}}}"),
                None => seg.to_string(),
            })
            .collect::<Vec<_>>()
            .join("/");
        if !routed.contains(&openapi_shape) {
            routed.push(openapi_shape);
        }
    }
    assert!(
        routed.len() > 40,
        "the route scrape found only {} routes — it has stopped working",
        routed.len()
    );

    let spec: serde_json::Value = serde_json::from_str(SPEC).unwrap();
    let documented: Vec<String> = spec["paths"]
        .as_object()
        .expect("paths object")
        .keys()
        .cloned()
        .collect();

    let missing: Vec<&String> = routed
        .iter()
        .filter(|r| !documented.contains(r) && !UNDOCUMENTED.contains(&r.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "routes with no entry in openapi.json: {missing:#?}"
    );

    // The other direction: a documented path that no longer exists sends
    // a reader to a 404.
    let phantom: Vec<&String> = documented
        .iter()
        .filter(|d| !routed.contains(d) && !d.starts_with("/webhooks/"))
        .collect();
    assert!(
        phantom.is_empty(),
        "openapi.json describes routes the server does not serve: {phantom:#?}"
    );
}

/// `scripts/ci-local.sh` and `CLAUDE.md` must know about every job CI
/// runs.
///
/// A local gate that has quietly fallen behind CI is worse than no local
/// gate: you run it, it says "good to push", and CI fails on the job it
/// never heard of. The same goes for the document that tells a reader
/// what the gates *are* — a list of four when there are five is a
/// promise the repo does not keep. So both name every job in the
/// workflow, and this fails when a new one appears without them.
///
/// Deliberately a check on *job names*, not on the commands inside them.
/// Asserting command equality would fail on every wording difference —
/// `npm ci` versus `npm install`, a browser already on the image — and a
/// test that cries wolf gets deleted. The job list is the part that
/// actually goes stale.
/// `.nvmrc` says which Node a person should be on, and CI pins its own.
/// Two numbers in two files drift, and the failure mode is nasty in a
/// specific way: too old and Astro refuses to build, which reads as the
/// site being broken rather than the toolchain being wrong.
///
/// Pinning the major only — not the patch — is deliberate. `.nvmrc` is
/// what `nvm use` reads, and a patch-level pin would send everyone
/// downloading a build CI never asked for.
#[test]
fn the_nvmrc_matches_the_node_version_ci_pins() {
    const WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yml");
    const NVMRC: &str = include_str!("../../../.nvmrc");

    let pinned = WORKFLOW
        .lines()
        .find_map(|l| l.trim().strip_prefix("node-version:"))
        .map(|v| v.trim().trim_matches('"').to_string())
        .expect("ci.yml declares a node-version");

    let wanted = NVMRC.trim().trim_start_matches('v');
    assert_eq!(
        wanted, pinned,
        ".nvmrc says Node {wanted}, ci.yml pins {pinned} — a developer \
         following .nvmrc would not be on the version CI tests with"
    );
}

#[test]
fn the_local_ci_script_covers_every_job_the_workflow_declares() {
    const WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yml");
    const SCRIPT: &str = include_str!("../../../scripts/ci-local.sh");
    const GUIDE: &str = include_str!("../../../CLAUDE.md");

    // Top-level keys under `jobs:` — two-space indent, then a name.
    let mut in_jobs = false;
    let mut jobs: Vec<&str> = Vec::new();
    for line in WORKFLOW.lines() {
        if line.starts_with("jobs:") {
            in_jobs = true;
            continue;
        }
        if !in_jobs {
            continue;
        }
        // A non-indented, non-blank, non-comment line ends the block.
        if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
            break;
        }
        let Some(rest) = line.strip_prefix("  ") else {
            continue;
        };
        if rest.starts_with(' ') || rest.starts_with('#') || rest.trim().is_empty() {
            continue;
        }
        if let Some(name) = rest.strip_suffix(':') {
            jobs.push(name);
        }
    }
    assert!(
        jobs.len() >= 4,
        "failed to parse the workflow's jobs, found {jobs:?}"
    );

    let missing: Vec<&&str> = jobs.iter().filter(|j| !SCRIPT.contains(**j)).collect();
    assert!(
        missing.is_empty(),
        "scripts/ci-local.sh does not mention these CI jobs, so running it \
         locally would report success on work CI rejects: {missing:?}"
    );

    let undocumented: Vec<&&str> = jobs.iter().filter(|j| !GUIDE.contains(**j)).collect();
    assert!(
        undocumented.is_empty(),
        "CLAUDE.md lists the gates a change must survive and does not \
         mention these CI jobs: {undocumented:?}"
    );
}

/// CI runs `npm ci` from a clean checkout; the local script installs
/// only when it has to. "Only when it has to" used to mean "when
/// `node_modules` is missing", so a merge that brought two new packages
/// into the lockfile built the dashboard against the old tree and failed
/// on `Cannot find module '@pierre/diffs'` — red for a reason CI would
/// never see, which is the one kind of red a local gate must not
/// produce. npm records what it installed in
/// `node_modules/.package-lock.json`; the script's `npm_stale` has to
/// answer "install" when that record is missing or older than the real
/// lockfile, and "skip" when it is current. The function is lifted out
/// of the script and run as bash so the test is of the shell that runs,
/// not of a copy.
#[test]
fn the_local_web_job_reinstalls_when_the_lockfile_moved() {
    const SCRIPT: &str = include_str!("../../../scripts/ci-local.sh");
    let start = SCRIPT
        .find("npm_stale() {")
        .expect("scripts/ci-local.sh defines npm_stale");
    let end = SCRIPT[start..]
        .find("\n    }\n")
        .map(|i| start + i + "\n    }\n".len())
        .expect("npm_stale has a closing brace");
    let function = &SCRIPT[start..end];
    assert!(
        function.contains("package-lock.json\" -nt"),
        "npm_stale must compare the lockfile against npm's install record"
    );

    let scratch = Scratch::new("npm-stale");
    let stale = |dir: &std::path::Path| -> bool {
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!("{function}\nnpm_stale \"$1\"", function = function))
            .arg("npm_stale")
            .arg(dir)
            .output()
            .expect("run npm_stale");
        out.status.success()
    };
    let touch = |p: &std::path::Path, secs_ago: u64| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"{}\n").unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        std::fs::File::open(p).unwrap().set_modified(when).unwrap();
    };

    let fresh_checkout = scratch.path().join("fresh");
    touch(&fresh_checkout.join("package-lock.json"), 60);
    assert!(
        stale(&fresh_checkout),
        "no node_modules at all must install"
    );

    let current = scratch.path().join("current");
    touch(&current.join("package-lock.json"), 120);
    touch(&current.join("node_modules/.package-lock.json"), 60);
    assert!(
        !stale(&current),
        "a node_modules installed after the lockfile must not reinstall"
    );

    let merged = scratch.path().join("merged");
    touch(&merged.join("node_modules/.package-lock.json"), 120);
    touch(&merged.join("package-lock.json"), 60);
    assert!(
        stale(&merged),
        "a lockfile newer than npm's install record must install"
    );

    let no_record = scratch.path().join("no-record");
    touch(&no_record.join("package-lock.json"), 60);
    std::fs::create_dir_all(no_record.join("node_modules")).unwrap();
    assert!(
        stale(&no_record),
        "a node_modules with no install record must install"
    );
}

/// Every top-level path the router claims is a namespace nobody can take.
///
/// A namespace is a first path segment — `/<namespace>/<repo>.git` is the
/// git wire — so a name the router already answers for is a name that can
/// never be cloned or pushed to. `dashboard` is the sharp one:
/// `/dashboard/*path` is a catch-all, so an org called `dashboard` would
/// have the SPA answer every git request for it, with no error anyone
/// could act on.
///
/// While namespaces were operator-provisioned this needed a typo to
/// happen. A handle chosen when accepting an invitation makes it a
/// newcomer's choice, so the denylist has to be right — and it has to
/// *stay* right, which is what this is for: add a top-level route
/// without reserving it and this fails.
#[test]
fn no_router_path_can_be_taken_as_a_namespace() {
    const APP: &str = include_str!("../src/app.rs");

    // First path segment of every `.route("/…")` literal that is not
    // already parameterised — `/:org/:repo/…` is the git wire itself and
    // claims nothing.
    let mut claimed: Vec<String> = Vec::new();
    for (i, _) in APP.match_indices(".route(") {
        let rest = &APP[i + ".route(".len()..];
        let Some(open) = rest.find('"') else { continue };
        let Some(close) = rest[open + 1..].find('"') else {
            continue;
        };
        let path = &rest[open + 1..open + 1 + close];
        let Some(seg) = path.strip_prefix('/').and_then(|p| p.split('/').next()) else {
            continue;
        };
        if seg.is_empty() || seg.starts_with(':') || seg.starts_with('*') {
            continue;
        }
        if !claimed.iter().any(|c| c == seg) {
            claimed.push(seg.to_string());
        }
    }
    assert!(
        claimed.len() >= 5,
        "failed to parse the router's top-level paths, found {claimed:?}"
    );

    let missing: Vec<&String> = claimed
        .iter()
        .filter(|seg| !stratum_control::registry::is_reserved(seg))
        .collect();
    assert!(
        missing.is_empty(),
        "these are top-level routes but not reserved namespace names, so a \
         stranger could take one and never be able to clone it: {missing:?}"
    );

    // …and the denylist is checked case-insensitively, because the
    // namespace is. A list that catches one spelling catches nothing.
    for seg in &claimed {
        assert!(
            stratum_control::registry::is_reserved(&seg.to_uppercase()),
            "{seg} is reserved but {} is not",
            seg.to_uppercase()
        );
    }
}

/// Every top-level segment the dashboard SPA claims is a namespace
/// nobody can take.
///
/// The third blind spot of the same kind. The router scraper reads
/// `.route()` literals and the site test reads `web/site/src/pages`;
/// neither can see a segment that exists only in the SPA's own route
/// table, because the SPA is served by the axum `.fallback` and declares
/// nothing to the server. A namespace called `explore` would be a
/// namespace nobody could ever reach — the SPA would answer `/explore`
/// with its explore page forever, and the owner would have no error to
/// act on.
///
/// `TOP_LEVEL` in `web/dashboard/src/routes.ts` is the SPA's side of
/// that contract, and this is the enforcement: add a segment there
/// without reserving it and this fails.
#[test]
fn no_spa_top_level_segment_can_be_taken_as_a_namespace() {
    const ROUTES: &str = include_str!("../../../web/dashboard/src/routes.ts");

    let start = ROUTES
        .find("export const TOP_LEVEL = [")
        .expect("TOP_LEVEL array in web/dashboard/src/routes.ts");
    let body = &ROUTES[start..];
    let end = body.find(']').expect("TOP_LEVEL array is unterminated");
    let mut claimed: Vec<&str> = Vec::new();
    let mut rest = &body[..end];
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        claimed.push(&after[..close]);
        rest = &after[close + 1..];
    }
    assert!(
        claimed.len() >= 5,
        "failed to parse TOP_LEVEL, found {claimed:?}"
    );

    let missing: Vec<&&str> = claimed
        .iter()
        .filter(|seg| !stratum_control::registry::is_reserved(seg))
        .collect();
    assert!(
        missing.is_empty(),
        "the SPA claims these first path segments but they are not \
         reserved namespace names, so a stranger could take one and be \
         shadowed by the dashboard forever: {missing:?}"
    );

    for seg in &claimed {
        assert!(
            stratum_control::registry::is_reserved(&seg.to_uppercase()),
            "{seg} is reserved but {} is not",
            seg.to_uppercase()
        );
    }
}

/// The local gate refuses to start when the disk cannot hold the run.
///
/// This is a behaviour test rather than a grep for the code, because the
/// thing worth protecting is the refusal and not its spelling. Running
/// out of disk happens *mid-job*, and it surfaces as a compiler or a
/// test process dying with an I/O error somewhere unrelated to whatever
/// is actually wrong — so the run gets retried, passes once something
/// else frees a little, and is written off as flaky. An afternoon has
/// already been spent on that in this repository.
///
/// `--only none` is the cheap way in: it runs the preconditions and no
/// job at all, so the only difference between the two invocations below
/// is the precondition itself.
#[test]
fn the_local_ci_script_refuses_to_start_without_room() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/ci-local.sh");

    let refused = std::process::Command::new("bash")
        .arg(script)
        .args(["--only", "none"])
        .env("STRATUM_MIN_FREE_GIB", "999999")
        .output()
        .expect("run scripts/ci-local.sh");
    assert!(
        !refused.status.success(),
        "ci-local started with no room for the run it was asked for"
    );
    let said = String::from_utf8_lossy(&refused.stdout);
    assert!(
        said.contains("clean-build-artifacts.sh"),
        "the refusal did not say how to reclaim space, which is the only \
         thing the reader needs from it:\n{said}"
    );

    // ...and it is a precondition, not a blanket refusal: with room, the
    // same invocation runs.
    let allowed = std::process::Command::new("bash")
        .arg(script)
        .args(["--only", "none"])
        .env("STRATUM_MIN_FREE_GIB", "0")
        .output()
        .expect("run scripts/ci-local.sh");
    assert!(
        allowed.status.success(),
        "ci-local refused a run it had room for:\n{}",
        String::from_utf8_lossy(&allowed.stdout)
    );
}

/// **A misspelt `--only` is an error, not a quiet run of nothing.**
///
/// `--only correctness` — the job is `correctness-gate` — used to match
/// no job, run nothing, and print a summary with no failures under
/// "Only the correctness job ran". That is indistinguishable from a
/// clean run, and it is the same confusion the script refuses elsewhere
/// by never saying "good to push" after a SKIP: a job that did not
/// happen must not read like one that passed.
#[test]
fn a_misspelt_only_job_is_refused_rather_than_running_nothing() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/ci-local.sh");
    let out = std::process::Command::new("bash")
        .arg(script)
        .args(["--only", "correctness"])
        .env("STRATUM_MIN_FREE_GIB", "0")
        .output()
        .expect("run scripts/ci-local.sh");
    assert!(
        !out.status.success(),
        "a job name that does not exist must not exit 0:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("correctness-gate"),
        "the refusal must name the real jobs, or the reader cannot fix \
         the typo:\n{said}"
    );
}

/// The local terraform validate never touches `deploy/terraform/.terraform`.
///
/// CI checks out clean, so its `init -backend=false` only ever meets a
/// directory nobody has initialised. A developer's tree is different: an
/// apply from the repository leaves `.terraform/terraform.tfstate` naming
/// the real S3 backend, and terraform 1.15 still reaches for it — and for
/// deploy credentials — under `-backend=false`. The step failed with "No
/// valid credential sources found" in a tree whose terraform was fine,
/// from the same directory a destroy was running in. The fix is a data
/// directory of the step's own; this drives the step through a fake
/// `terraform` that refuses to run anywhere else. The step is its own
/// job, terraform-validation, since it left the tail of
/// deploy-validation; the fake `docker` whose `info` fails is kept so
/// the script cannot mistake this machine for one with a daemon.
#[test]
fn the_local_ci_script_validates_terraform_in_its_own_data_dir() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root");
    let bin = std::env::temp_dir().join(format!("stratum-tf-fake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir(&bin).expect("fake bin dir");
    let bin = scopeguard_dir(bin);
    let seen = bin.path().join("seen");
    let forbidden = repo.join("deploy/terraform");
    let fake = format!(
        r##"#!/bin/bash
case "$1" in version) echo 'Terraform v99.0.0'; exit 0;; esac
case "$*" in *" fmt "*) exit 0;; esac
printf '%s\t%s\n' "${{TF_DATA_DIR:-<unset>}}" "$*" >> "{seen}"
case "${{TF_DATA_DIR:-}}" in
  ""|"{forbidden}"*) exit 1;;
esac
mkdir -p "$TF_DATA_DIR" || exit 1
exit 0
"##,
        seen = seen.display(),
        forbidden = forbidden.display(),
    );
    std::fs::write(bin.path().join("terraform"), fake).unwrap();
    std::fs::write(bin.path().join("docker"), "#!/bin/bash\nexit 1\n").unwrap();
    for name in ["terraform", "docker"] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            bin.path().join(name),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = std::process::Command::new("bash")
        .arg(repo.join("scripts/ci-local.sh"))
        .args(["--only", "terraform-validation"])
        .env("PATH", path)
        .env("STRATUM_MIN_FREE_GIB", "0")
        .current_dir(&repo)
        .output()
        .expect("run scripts/ci-local.sh");
    let said = String::from_utf8_lossy(&out.stdout);
    let seen = std::fs::read_to_string(&seen).unwrap_or_default();
    assert!(
        said.contains("PASS  terraform-validation") && !said.contains("FAIL  terraform-validation"),
        "the validate step ran terraform without a data dir of its own, or          inside deploy/terraform:\n{said}\nterraform saw:\n{seen}"
    );
    // Every init, validate and test ran, and none of them in the checkout.
    for (want, n, what) in [
        ("init -backend=false", 3, "root, bootstrap and env-lock"),
        ("validate", 2, "root and bootstrap"),
        (
            "-chdir=deploy/terraform/modules/env-lock test",
            1,
            "env-lock",
        ),
    ] {
        assert!(
            seen.matches(want).count() == n,
            "expected {n} `{want}` invocation(s) ({what}):\n{seen}"
        );
    }
    // The workflow runs the same env-lock test, so the local gate and CI
    // cannot disagree about whether a state may refuse the wrong env.
    let workflow = include_str!("../../../.github/workflows/ci.yml");
    assert!(
        workflow.contains("terraform -chdir=deploy/terraform/modules/env-lock test"),
        "ci.yml does not run the env-lock terraform test"
    );
}

/// A directory removed when the test that made it finishes, however.
struct FakeBin(std::path::PathBuf);

impl FakeBin {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for FakeBin {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scopeguard_dir(p: std::path::PathBuf) -> FakeBin {
    FakeBin(p)
}

/// The SPA's page list is the same list on both sides of the wire.
///
/// `TOP_LEVEL` in `routes.ts` says which top-level paths the SPA renders;
/// `SPA_SEGMENTS` in `webassets.rs` says which ones the server hands the
/// SPA. They are one decision written in two languages, and they drifted
/// within an afternoon of being written: the names were reserved so no
/// namespace could shadow them, and the fallback refused reserved names,
/// so `/explore` and `/search` answered 404 and the header's search box
/// led nowhere. Nothing caught it because each half was correct alone.
///
/// `dashboard` is the one deliberate difference: it has real routes and
/// never reaches the fallback.
#[test]
fn the_spa_page_list_matches_on_both_sides() {
    const ROUTES: &str = include_str!("../../../web/dashboard/src/routes.ts");
    const ASSETS: &str = include_str!("../src/webassets.rs");

    fn names(text: &str, marker: &str) -> Vec<String> {
        // From *after* the marker: the Rust declaration spells its own
        // type as `&[&str]`, so slicing from the marker's start and
        // hunting the first `]` finds the one inside the type and parses
        // an empty list — which looks exactly like a list that changed.
        let at = text
            .find(marker)
            .unwrap_or_else(|| panic!("{marker} not found"));
        let body = &text[at + marker.len()..];
        let body = &body[..body.find(']').expect("unterminated list")];
        body.match_indices('"')
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| body[c[0] + 1..c[1]].to_string())
            .collect()
    }

    let mut spa = names(ROUTES, "export const TOP_LEVEL = [");
    let mut server = names(ASSETS, "const SPA_SEGMENTS: &[&str] = &[");
    assert!(spa.len() >= 5, "failed to parse TOP_LEVEL: {spa:?}");
    assert!(
        server.len() >= 5,
        "failed to parse SPA_SEGMENTS: {server:?}"
    );

    // The dashboard has its own routes and never reaches the fallback.
    spa.retain(|s| s != "dashboard");
    spa.sort();
    server.sort();
    assert_eq!(
        spa, server,
        "the SPA renders one set of top-level pages and the server hands it \
         another: whatever is in the first list and not the second answers \
         404, and whatever is in the second and not the first is a name \
         nobody can use for anything"
    );
}

/// Everything a commit refers to is *in* that commit.
///
/// `HEAD` was briefly unbuildable: a view imported
/// `components/star-button`, the import was committed, and the component
/// itself was never added — it existed only untracked on one disk. A
/// fresh clone would not have built, and the committed unit test could
/// not have resolved its own subject.
///
/// **Every local gate was green and had to be.** The build, the test
/// suites and the dist-freshness interlock all read the working tree,
/// and the working tree was complete. The breakage existed only in what
/// was *recorded*, which is the one thing a local run never looks at.
/// It is the "passes here, fails there" family from CLAUDE.md with the
/// sides swapped: it would have failed on every machine that did not
/// happen to have the file, starting with CI.
///
/// It came from two sessions working in one tree, each committing its
/// own paths, with `git add` nobody's obvious job. That will happen
/// again, so this asks the question no other gate asks.
#[test]
fn every_committed_reference_resolves_inside_the_same_commit() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root");

    let out = std::process::Command::new("git")
        .args(["ls-tree", "-r", "HEAD", "--name-only"])
        .current_dir(&root)
        .output()
        .expect("git ls-tree");
    assert!(out.status.success(), "git ls-tree failed");
    let listing = String::from_utf8_lossy(&out.stdout);
    let tracked: std::collections::HashSet<&str> = listing.lines().collect();
    assert!(
        tracked.len() > 100,
        "failed to read the commit's file list, found {}",
        tracked.len()
    );

    // Read the *committed* bytes, never the working tree — the whole
    // point is to see the commit as somebody cloning it would.
    let show = |path: &str| -> Option<String> {
        let out = std::process::Command::new("git")
            .args(["show", &format!("HEAD:{path}")])
            .current_dir(&root)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };

    let mut missing: Vec<String> = Vec::new();

    // Rust: `mod x;` needs `x.rs` or `x/mod.rs` beside it.
    for path in tracked.iter().filter(|p| p.ends_with(".rs")) {
        let Some(text) = show(path) else { continue };
        let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        for line in text.lines().map(str::trim) {
            let Some(rest) = line
                .strip_prefix("pub mod ")
                .or_else(|| line.strip_prefix("mod "))
            else {
                continue;
            };
            let Some(name) = rest.strip_suffix(';') else {
                continue; // `mod x { … }`, an inline module
            };
            let name = name.trim();
            if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let flat = format!("{dir}/{name}.rs");
            let nested = format!("{dir}/{name}/mod.rs");
            if !tracked.contains(flat.as_str()) && !tracked.contains(nested.as_str()) {
                missing.push(format!(
                    "{path} declares `mod {name};` — neither {flat} nor {nested} is in the commit"
                ));
            }
        }
    }

    // The dashboard: `@/…` resolves against its `src`.
    for path in tracked.iter().filter(|p| {
        p.starts_with("web/dashboard/src/") && (p.ends_with(".ts") || p.ends_with(".tsx"))
    }) {
        let Some(text) = show(path) else { continue };
        for (i, _) in text.match_indices("from \"@/") {
            let rest = &text[i + "from \"@/".len()..];
            let Some(end) = rest.find('"') else { continue };
            let spec = &rest[..end];
            let base = format!("web/dashboard/src/{spec}");
            let resolves = ["ts", "tsx", "css"]
                .iter()
                .any(|ext| tracked.contains(format!("{base}.{ext}").as_str()))
                || tracked.contains(format!("{base}/index.ts").as_str())
                || tracked.contains(format!("{base}/index.tsx").as_str())
                || tracked.contains(base.as_str());
            if !resolves {
                missing.push(format!(
                    "{path} imports `@/{spec}`, which is not in the commit"
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "this commit refers to files it does not contain, so a fresh clone \
         would not build:\n  {}",
        missing.join("\n  ")
    );
}

/// Every background worker takes its job lease from the environment.
///
/// A lease is how long a claim survives, so it is also exactly how long
/// a **crashed node's work stays stuck** before another node may take
/// it. That makes it an operational control, not a constant — and one
/// nobody tunes until something dies mid-job, which is the worst moment
/// to discover it is unreachable.
///
/// Four of the five workers already read it from the environment and
/// `cdnpack` did not: 900 seconds hard-coded, so a node dying mid-pack
/// wedged that repository's packing for fifteen minutes, and no test
/// could observe recovery at all because no test could shorten it.
///
/// Asserted structurally rather than per-worker, because the failure
/// mode is a *new* worker copying the shape of an old one. A test naming
/// the five we have would pass forever while the sixth arrives with a
/// literal in it.
#[test]
fn no_worker_hard_codes_its_job_lease() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/workers");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("workers dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read worker");
        for (n, line) in src.lines().enumerate() {
            let Some(rest) = line.split_once("jobs::claim(").map(|(_, r)| r) else {
                continue;
            };
            // The third argument is the lease. A literal there — digits,
            // with or without Rust's underscore separators — is the
            // thing being refused; a named binding is what every worker
            // should be passing.
            let last = rest.rsplit(',').next().unwrap_or("").trim();
            let last = last.trim_end_matches([')', ';']).trim();
            if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit() || c == '_') {
                offenders.push(format!(
                    "{}:{} passes a hard-coded lease {last:?}",
                    path.file_name().unwrap().to_string_lossy(),
                    n + 1
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a job lease is how long a crashed node's work stays stuck, so it \
         must come from the environment via `workers::lease_ms`:\n  {}",
        offenders.join("\n  ")
    );
}

/// The CI integration guide's request bodies must name fields the intake
/// actually has, and its state tables must list the states the code
/// actually accepts.
///
/// Every part of this is a drift that shipped. The guide's field table
/// marked `change` **required** while the commit-scoped shape — the only
/// one a plain push can send, and the only one that fills the Checks tab
/// — needs no change at all. Its `state` row listed three words while the
/// path most readers were on takes six different ones. And all four
/// provider snippets sent the branch name (or the pull request title) as
/// the change key, which is `I` followed by hex: they answered
/// `404 no such change` on every event, so nobody who followed the guide
/// verbatim ever saw a check appear anywhere.
///
/// None of that could fail a test, because prose is not compiled. This
/// compares the prose against the two authorities — `IntakeBody` for the
/// field names, `RunState`/`PATCHSET_CHECK_STATES` for the vocabularies
/// — so a rename or a seventh state breaks the build rather than the
/// reader.
#[test]
fn the_ci_guide_matches_the_intake_it_documents() {
    const GUIDE: &str = include_str!("../../../docs/guide/ci-integration.md");
    const INTAKE: &str = include_str!("../src/api/checks_intake.rs");

    // The fields `IntakeBody` declares, read from its source: `pub name:`
    // for the plain ones and the `rename` for `ref`.
    let mut declared: Vec<String> = Vec::new();
    let body = INTAKE
        .split_once("pub struct IntakeBody {")
        .expect("IntakeBody must still be named that")
        .1
        .split_once("\n}")
        .expect("a closing brace")
        .0;
    for line in body.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("pub ") {
            let field = rest.split(':').next().unwrap_or_default().trim();
            declared.push(field.to_string());
        }
        if let Some(i) = t.find("rename = \"") {
            let r = &t[i + 10..];
            declared.push(r[..r.find('"').expect("closing quote")].to_string());
        }
    }
    assert!(
        declared.contains(&"commit".to_string()) && declared.contains(&"ref".to_string()),
        "the struct scrape found nothing useful: {declared:?}"
    );

    // Every JSON key the guide sends must be one of them. `ref_name` is
    // the Rust field for the wire's `ref`, so it is not a wire key.
    let wire: Vec<&str> = declared
        .iter()
        .map(|s| s.as_str())
        .filter(|s| *s != "ref_name")
        .collect();
    let mut sent: Vec<String> = Vec::new();
    for block in GUIDE.split("printf '{").skip(1) {
        let json = block.split("}'").next().unwrap_or_default();
        for part in json.split(',') {
            if let Some(k) = part.split(':').next() {
                let k = k.trim().trim_matches('"');
                if !k.is_empty() && !k.contains('%') {
                    sent.push(k.to_string());
                }
            }
        }
    }
    assert!(!sent.is_empty(), "no request bodies found in the guide");
    for k in &sent {
        assert!(
            wire.contains(&k.as_str()),
            "the guide sends {k:?}, which `IntakeBody` does not accept \
             (it has {wire:?}) — a snippet nobody can run"
        );
    }

    // No snippet may send a change key. The guide now documents the
    // commit-scoped shape as the one to copy, precisely because a change
    // key cannot be derived from a branch name — which is what every
    // snippet used to do, and why none of them worked.
    assert!(
        !sent.iter().any(|k| k == "change"),
        "a provider snippet sends `change`; the key is `I`+hex from the commit's \
         own trailer and cannot be built from CI environment variables"
    );

    // Both vocabularies, against their authorities.
    let commit_states = stratum_control::checks::RunState::names();
    let patchset_states = stratum_control::changes::PATCHSET_CHECK_STATES.join(", ");
    for word in commit_states.split(", ") {
        assert!(
            GUIDE.contains(&format!("`{word}`")),
            "the guide never mentions the commit-scoped state {word:?}"
        );
    }
    assert!(
        GUIDE.contains(&patchset_states) || GUIDE.contains("`pending`, `passing`, `failing`"),
        "the guide must list the patchset-scoped states as a set: {patchset_states}"
    );
    // The split itself has to be stated, because it is the thing that
    // catches people: `pending` is legal on one side only.
    assert!(
        GUIDE.contains("`pending` is change-scoped only"),
        "the guide must say plainly that `pending` is not a commit-scoped state"
    );
}

/// The public documentation names the product's identifiers — the job
/// environment a `ci.sh` reads, the webhook signature header a CI
/// provider verifies, the runner binary a person installs, the prefix on
/// every token they copy — and each of those was renamed from its
/// `stratum` spelling before anybody outside depended on it. This is what
/// keeps the old spelling from coming back one page at a time: it walks
/// every docs page and the OpenAPI document, and fails on the first
/// old-name identifier it finds.
///
/// Three things that look like the old name are not: the Cargo package is
/// still `stratum-runner` (`cargo build -p stratum-runner`, the path
/// `crates/stratum-runner/`), which the crate rename will move; the
/// operator's own configuration (`STRATUM_MAIL_*`, `STRATUM_STRIPE_*`,
/// the worker cadences) is read by nobody but the person deploying the
/// server and moves with the operator-side rename; and the
/// `stratum+tcp://` family is a mining-pool URL scheme the workflow
/// scanner refuses, which has nothing to do with us.
#[test]
fn the_public_docs_use_no_old_name_identifier() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
            let p = e.expect("dir entry").path();
            if p.is_dir() {
                walk(&p, out);
            } else if matches!(p.extension().and_then(|x| x.to_str()), Some("md" | "astro")) {
                out.push(p);
            }
        }
    }
    walk(&docs, &mut files);
    files.push(docs.join("openapi.json"));
    assert!(files.len() > 20, "found only {} docs files", files.len());

    // The job environment a `ci.sh` reads, and the variables the examples
    // tell a reader to set, under their old names — matched as whole
    // identifiers, because the operator's own settings share the prefix
    // (`STRATUM_REF_PAGE_SIZE` is a server setting, `STRATUM_REF` was the
    // job's ref).
    let old_env = [
        "STRATUM_CI",
        "STRATUM_JOB",
        "STRATUM_SHA",
        "STRATUM_REF",
        "STRATUM_EVENT",
        "STRATUM_CHANGE",
        "STRATUM_WORKSPACE",
        "STRATUM_TOKEN",
        "STRATUM_URL",
    ];
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let names_env = |line: &str, name: &str| {
        line.match_indices(name).any(|(at, _)| {
            let before = line[..at].chars().next_back();
            let after = line[at + name.len()..].chars().next();
            !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
        })
    };
    // The published forms of the old name, each one a contract somebody
    // outside would have copied.
    let old = [
        "STRATUM_MATRIX_", // job env: one per matrix axis
        "X-Stratum-",      // signature, staleness and blob headers
        "x-stratum",       // the same, as a shell would grep for it
        "/stratum-runner", // the installed binary and its paths
        "stratum-runner ", // the command a person types
        "stratum-runner:", // the image tag
        "strat_",          // API token prefix
        "strg_",           // runner registration
        "strr_",           // runner credential
        "stver_",          // e-mail verification
        "strst_",          // password reset
        "\"stratum\"",     // the hosted check-run provider
        "Stratum",         // the brand, in prose
        "stratum-lander",  // the author of a revert
        "stratum: ",       // the in-band git error prefix
    ];
    let mut hits = Vec::new();
    for f in &files {
        let text =
            std::fs::read_to_string(f).unwrap_or_else(|e| panic!("read {}: {e}", f.display()));
        for (n, line) in text.lines().enumerate() {
            if line.contains("-p stratum-runner") || line.contains("crates/stratum-runner") {
                continue;
            }
            let hit = old
                .iter()
                .find(|o| line.contains(**o))
                .or_else(|| old_env.iter().find(|o| names_env(line, o)));
            if let Some(o) = hit {
                hits.push(format!(
                    "{}:{}: `{o}` in {}",
                    f.strip_prefix(&docs).unwrap().display(),
                    n + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "old-name identifiers in the public docs:\n{}",
        hits.join("\n")
    );
}

/// The bootstrap OIDC trust admits both spellings of GitHub's `sub` claim.
///
/// A merge to main went red at the first step of `deploy` with
/// `Not authorized to perform sts:AssumeRoleWithWebIdentity`, which reads
/// as a bootstrap that was never run. It had been run. GitHub had begun
/// issuing **immutable-identifier** subject claims, so the token said
/// `repo:owner@6254949/name@1341182177:ref:refs/heads/main` where the
/// trust policy expected `repo:owner/name:ref:refs/heads/main`, and
/// nothing matched. The rollout is not settled — the repository's own
/// sub-customization API reports the id-bearing prefix while still
/// reporting `use_immutable_subject: false` — so the policy trusts both
/// spellings rather than betting on either, and this test is what says so
/// in both directions.
///
/// The widening is the other half. A pattern is one `*` away from
/// trusting every repository on GitHub, and the failure mode of getting
/// that wrong is silent: the deploy goes green either way. So the
/// negative cases are the point — another owner, another repository,
/// another branch, a pull-request ref, and each role's claim against the
/// other's patterns.
#[test]
fn the_bootstrap_oidc_trust_admits_both_github_subject_formats() {
    const BOOTSTRAP: &str = include_str!("../../../deploy/terraform/bootstrap/main.tf");

    // IAM `StringLike`: `*` matches any run of characters, `?` any one,
    // everything else is literal.
    fn matches(pattern: &str, value: &str) -> bool {
        fn go(p: &[u8], v: &[u8]) -> bool {
            match p.first() {
                None => v.is_empty(),
                Some(b'*') => go(&p[1..], v) || (!v.is_empty() && go(p, &v[1..])),
                Some(b'?') => !v.is_empty() && go(&p[1..], &v[1..]),
                Some(c) => v.first() == Some(c) && go(&p[1..], &v[1..]),
            }
        }
        go(pattern.as_bytes(), value.as_bytes())
    }

    // The `sub` patterns each trust document actually uses, read out of
    // the `locals` block with the repository standing in as acme/widget.
    let subs = |name: &str| -> Vec<String> {
        let start = BOOTSTRAP
            .find(&format!("{name} = ["))
            .unwrap_or_else(|| panic!("deploy/terraform/bootstrap/main.tf declares `{name}`"));
        let body = &BOOTSTRAP[start..start + BOOTSTRAP[start..].find(']').expect("closing `]`")];
        let values: Vec<String> = body
            .match_indices('"')
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| {
                body[c[0] + 1..c[1]]
                    .replace("${local.github_owner}", "acme")
                    .replace("${local.github_name}", "widget")
            })
            .collect();
        assert!(
            values.iter().all(|v| !v.contains("${")),
            "`{name}` interpolates something this test does not substitute: {values:?}"
        );
        values
    };

    // A pattern list nobody wired to a trust document proves nothing, and
    // `StringEquals` over a wildcard matches literally — the infra role's
    // condition was exactly that before this fix.
    for (doc, local) in [("cd_trust", "cd_subs"), ("infra_trust", "infra_subs")] {
        let start = BOOTSTRAP
            .find(&format!("data \"aws_iam_policy_document\" \"{doc}\""))
            .unwrap_or_else(|| panic!("bootstrap declares the {doc} document"));
        let body = &BOOTSTRAP[start..start + BOOTSTRAP[start..].find("\n}\n").expect("end of doc")];
        let sub = body
            .split("condition {")
            .find(|c| c.contains("githubusercontent.com:sub"))
            .unwrap_or_else(|| panic!("{doc} conditions on the sub claim"));
        assert!(
            sub.contains("\"StringLike\""),
            "{doc}'s sub condition is not StringLike, so the wildcards in \
             {local} would be matched literally and GitHub's \
             immutable-id claim would be refused:\n{sub}"
        );
        assert!(
            sub.contains(&format!("local.{local}")),
            "{doc} does not use local.{local}, so this test checks patterns \
             the policy never applies:\n{sub}"
        );
    }

    let cd = subs("cd_subs");
    let infra = subs("infra_subs");

    // Both spellings of each claim, and only for this repository.
    const LEGACY_MAIN: &str = "repo:acme/widget:ref:refs/heads/main";
    const IMMUTABLE_MAIN: &str = "repo:acme@6254949/widget@1341182177:ref:refs/heads/main";
    const LEGACY_INFRA: &str = "repo:acme/widget:environment:infra";
    const IMMUTABLE_INFRA: &str = "repo:acme@6254949/widget@1341182177:environment:infra";

    let refused = [
        // A different owner, in both spellings — including one whose name
        // begins with ours, which a pattern anchored one character short
        // would admit.
        "repo:evil/widget:ref:refs/heads/main",
        "repo:evil@1/widget@2:ref:refs/heads/main",
        "repo:acmex@1/widget@2:ref:refs/heads/main",
        "repo:acmex/widget:environment:infra",
        // A different repository under our owner.
        "repo:acme/other:ref:refs/heads/main",
        "repo:acme@1/other@2:ref:refs/heads/main",
        "repo:acme@1/other@2:environment:infra",
        // A different ref, and the refs an untrusted contributor can move.
        "repo:acme/widget:ref:refs/heads/feature",
        "repo:acme@1/widget@2:ref:refs/heads/feature",
        "repo:acme@1/widget@2:ref:refs/tags/v1",
        "repo:acme/widget:pull_request",
        "repo:acme@1/widget@2:pull_request",
        // Another environment must not reach the infra role.
        "repo:acme@1/widget@2:environment:staging",
    ];

    for (role, patterns, admitted) in [
        ("stratum-cd", &cd, [LEGACY_MAIN, IMMUTABLE_MAIN]),
        ("stratum-infra", &infra, [LEGACY_INFRA, IMMUTABLE_INFRA]),
    ] {
        for sub in admitted {
            assert!(
                patterns.iter().any(|p| matches(p, sub)),
                "{role}'s trust refuses `{sub}`, which is a claim GitHub \
                 mints for this repository: {patterns:?}"
            );
        }
        // Each role's own claim, and nothing else — the cd role must not
        // accept the infra environment, nor infra a bare push to main.
        let others = if role == "stratum-cd" {
            [LEGACY_INFRA, IMMUTABLE_INFRA]
        } else {
            [LEGACY_MAIN, IMMUTABLE_MAIN]
        };
        for sub in refused.iter().copied().chain(others) {
            assert!(
                !patterns.iter().any(|p| matches(p, sub)),
                "{role}'s trust admits `{sub}`, which is not a claim this \
                 repository's deploy can mint: {patterns:?}"
            );
        }
    }
}

/// `.minio-version` is the pin, and nothing may fetch a MinIO that is
/// not it — or from the host that no longer serves one.
///
/// CI used to fetch `.../release/linux-amd64/minio` — the floating
/// *latest* — on every run of all three Rust jobs. A MinIO release could
/// therefore turn this repository red with no commit behind it, in a
/// suite whose stated contract (CLAUDE.md, "Tests are hermetic") is that
/// nothing reaches the real network. That is the same argument the pinned
/// `terraform_version` and `.nvmrc` already won here.
///
/// The shape this guards has changed once, and the change is the reason
/// the second half exists. There used to be **three** fetchers — the
/// workflow, the local gate and the testkit — each picking a platform
/// slug and building a `dl.min.io` URL. All three went stale on the same
/// day, when MinIO withdrew its prebuilt binaries and that host began
/// answering `410 Gone` for every platform of the pin. Nothing noticed,
/// because CI's `actions/cache` was still hitting: three jobs depended on
/// a dead URL and said so nowhere. There is one fetcher now,
/// `scripts/fetch-minio.sh`, which pulls the binary out of the container
/// image; everything else calls it.
///
/// So: the fetcher reads the pin, everybody else either reads the pin or
/// delegates to the fetcher, and **nobody** names the dead host in a URL
/// again. The last of those is the one that would have caught this.
#[test]
fn nothing_downloads_a_minio_other_than_the_pinned_one() {
    const WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yml");
    const SCRIPT: &str = include_str!("../../../scripts/ci-local.sh");
    const TESTKIT: &str = include_str!("../../../crates/stratum-testkit/src/minio.rs");
    const FETCH: &str = include_str!("../../../scripts/fetch-minio.sh");
    const PIN: &str = include_str!("../../../.minio-version");

    let pin = PIN.trim();
    assert!(
        pin.starts_with("RELEASE."),
        ".minio-version should hold a MinIO release tag, got {pin:?}"
    );

    // The one place that resolves an artifact has to read the pin.
    assert!(
        FETCH.contains(".minio-version"),
        "scripts/fetch-minio.sh does not read .minio-version, so the thing \
         every gate runs against can drift from the pin"
    );

    let all = [
        ("ci.yml", WORKFLOW),
        ("scripts/ci-local.sh", SCRIPT),
        ("crates/stratum-testkit/src/minio.rs", TESTKIT),
        ("scripts/fetch-minio.sh", FETCH),
    ];

    for (what, text) in all {
        // Read the pin, or defer to the one thing that does. A copy of
        // the release number is the drift this exists to prevent, and a
        // second fetcher is how the copy gets made.
        assert!(
            text.contains(".minio-version") || text.contains("fetch-minio.sh"),
            "{what} neither reads .minio-version nor delegates to \
             scripts/fetch-minio.sh, so it can drift from the pin"
        );
        // `dl.min.io` answers 410 for every platform of every release:
        // MinIO publishes no prebuilt binaries any more. Naming it in a
        // URL is not a fetch that might fail, it is one that cannot
        // succeed. Matched as a URL so this file and the comments that
        // explain the withdrawal may still say the name.
        assert!(
            !text.contains("https://dl.min.io"),
            "{what} fetches MinIO from dl.min.io, which serves no binaries \
             any more — see scripts/fetch-minio.sh"
        );
        // The floating form, in either quoting. `archive/minio.RELEASE...`
        // is the pinned one and must not match.
        for floating in [
            "release/linux-amd64/minio\n",
            "release/linux-amd64/minio ",
            "release/linux-amd64/minio\"",
            "release/${slug}/minio\"",
            "release/{slug}/minio\"",
        ] {
            assert!(
                !text.contains(floating),
                "{what} still fetches the floating latest MinIO ({floating:?})"
            );
        }
    }
}

/// The toolchain a developer gets and the one CI installs are one number.
///
/// `dtolnay/rust-toolchain@stable` meant every six-week Rust release
/// invalidated the `Swatinem/rust-cache` entry in all three Rust jobs, on
/// every branch, on the same day — and a cold cache is a twenty-five
/// minute job. Worse, it was silent: nothing in the run said "this is slow
/// because Rust shipped this morning".
#[test]
fn the_rust_toolchain_matches_the_version_ci_installs() {
    const WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yml");
    const PINNED: &str = include_str!("../../../rust-toolchain.toml");

    let wanted = PINNED
        .lines()
        .find_map(|l| l.trim().strip_prefix("channel"))
        .and_then(|v| v.trim().strip_prefix('='))
        .map(|v| v.trim().trim_matches('"').to_string())
        .expect("rust-toolchain.toml declares a channel");

    let installs: Vec<String> = WORKFLOW
        .lines()
        .filter_map(|l| l.trim().strip_prefix("toolchain:"))
        .map(|v| v.trim().trim_matches('"').to_string())
        .collect();

    assert!(
        !installs.is_empty(),
        "ci.yml installs no explicit toolchain, so it is back on floating stable"
    );
    for got in &installs {
        assert_eq!(
            got, &wanted,
            "rust-toolchain.toml pins {wanted}, ci.yml installs {got} — a \
             developer following the repository would not be on the \
             toolchain CI tests with"
        );
    }
}

/// A job that starts the disk reclaim in the background must wait for it
/// before it runs anything heavy.
///
/// Reclaiming 28 GB takes 2.2 minutes and it now overlaps the build
/// instead of blocking it. That is only safe because a paired step waits:
/// MinIO answers **507 Insufficient Storage** when its filesystem runs
/// low, and that surfaces as an ordinary PUT failing inside a test —
/// `ancestry_walks` died on `status code 507` with nothing wrong with the
/// code. Dropping the wait would bring that back as a race, which is
/// strictly worse than the two minutes it saves.
#[test]
fn every_job_that_backgrounds_the_disk_reclaim_waits_for_it_first() {
    const WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yml");

    const START: &str = "name: Start reclaiming disk";
    const WAIT: &str = "name: Wait for the disk reclaim";

    // Split on top-level job keys so each block is one job.
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut name = String::new();
    let mut body = String::new();
    let mut in_jobs = false;
    for line in WORKFLOW.lines() {
        if line.starts_with("jobs:") {
            in_jobs = true;
            continue;
        }
        if !in_jobs {
            continue;
        }
        let is_job = line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && !line.trim_start().starts_with('#');
        if is_job {
            if !name.is_empty() {
                blocks.push((name.clone(), std::mem::take(&mut body)));
            }
            name = line.trim().trim_end_matches(':').to_string();
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    if !name.is_empty() {
        blocks.push((name, body));
    }
    assert!(blocks.len() >= 4, "failed to split ci.yml into jobs");

    let mut backgrounded = 0;
    for (job, body) in &blocks {
        if !body.contains(START) {
            continue;
        }
        backgrounded += 1;
        let wait = body
            .find(WAIT)
            .unwrap_or_else(|| panic!("job {job} starts the reclaim and never waits for it"));
        for heavy in ["cargo test", "cargo llvm-cov"] {
            if let Some(at) = body.find(heavy) {
                assert!(
                    wait < at,
                    "job {job} runs `{heavy}` before waiting for the disk it \
                     asked for — that is the 507 back, as a race"
                );
            }
        }
    }
    // One per job that installs a Rust toolchain: correctness and chaos.
    let rust_jobs = WORKFLOW.matches("dtolnay/rust-toolchain").count();
    assert!(
        rust_jobs >= 2 && backgrounded >= rust_jobs,
        "expected every Rust job ({rust_jobs}) to background the reclaim, found {backgrounded}"
    );
}

/// A pull request does not re-run what the push already ran.
///
/// Every branch is tested on push, and a push run's checks attach to the
/// commit and show on a pull request from this repository — so a
/// `pull_request` run of that same head is the whole pipeline a second
/// time, with nothing to show for it but the wait. A fork's pull request
/// is different: its commits were pushed somewhere else, and no push run
/// here ever sees them. So `pull_request` is on, and every job carries
/// the guard that runs it only for a fork's head.
///
/// This test exists because dropping the guard from one job — or adding
/// a job without it — is the easy edit whose only symptom is every PR
/// building twice.
#[test]
fn a_pull_request_does_not_re_run_what_the_push_already_ran() {
    const WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yml");
    const FORKS_ONLY: &str = "if: github.event_name == 'push' || \
                              github.event.pull_request.head.repo.full_name != github.repository";

    let (on, jobs) = WORKFLOW
        .split_once("\njobs:")
        .expect("the workflow has a jobs: block");
    let triggers: Vec<&str> = on
        .lines()
        .filter(|l| l.starts_with("  ") && !l.starts_with("   ") && !l.trim().starts_with('#'))
        .map(str::trim)
        .collect();
    assert!(
        triggers.contains(&"push:"),
        "ci.yml no longer triggers on push; found {triggers:?}"
    );
    assert!(
        on.contains("branches: [\"**\"]"),
        "the push trigger must cover every branch, or a branch without a PR \
         is never tested: {on}"
    );
    if !triggers.iter().any(|t| t.starts_with("pull_request")) {
        return;
    }
    assert!(
        !triggers
            .iter()
            .any(|t| t.starts_with("pull_request_target")),
        "pull_request_target runs a fork's code with this repository's secrets"
    );
    // Each job is a two-space key under `jobs:`; the first line of its
    // body that is not blank or a comment must be the guard.
    let lines: Vec<&str> = jobs.lines().collect();
    let mut names = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let is_job = line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && !line.trim().starts_with('#');
        if !is_job {
            continue;
        }
        let name = line.trim().trim_end_matches(':');
        names.push(name.to_string());
        let first = lines[i + 1..]
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or_default();
        assert_eq!(
            first, FORKS_ONLY,
            "job {name} runs on a pull request from this repository too, \
             building a head its push already built"
        );
    }
    assert!(!names.is_empty(), "no jobs were found in ci.yml");
}

/// Every Rust cache is keyed on the manifest, not just the lockfile.
///
/// `Swatinem/rust-cache` builds its key from `Cargo.lock` and the rustc
/// version. `Cargo.toml` is not in it — so editing a `[profile.*]` leaves
/// the key unchanged, restores dependency artifacts built under the *old*
/// profile, and makes cargo rebuild all of them. It then does not save,
/// because it only writes a cache on a miss, so that rebuild is paid on
/// every run from then on.
///
/// This is not hypothetical and it is not small: adding
/// `[profile.dev.package."*"] opt-level = 3` took the correctness gate's
/// clippy step from **25s to 4m50s on a full cache hit**, and the only
/// symptom was the job total. Nothing in the run says "your cache is
/// answering with the wrong profile".
///
/// So the key hashes `Cargo.toml`. This test exists because the failure
/// is silent in both directions — deleting the `key:` restores the old
/// behaviour, and so does keeping a key that does not mention the
/// manifest.
#[test]
fn every_rust_cache_is_keyed_on_the_manifest() {
    const WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yml");

    let mut caches = 0;
    let lines: Vec<&str> = WORKFLOW.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("Swatinem/rust-cache") {
            continue;
        }
        caches += 1;
        // The `with:` block belongs to this step: everything until the
        // next line at the step's own indentation or shallower.
        let key = lines[i + 1..]
            .iter()
            .take_while(|l| l.trim().is_empty() || l.starts_with("        "))
            .find_map(|l| l.trim().strip_prefix("key:"))
            .unwrap_or_else(|| {
                panic!(
                    "the rust-cache at ci.yml:{} has no `key:`, so a profile \
                     change will restore the wrong artifacts and rebuild every \
                     dependency on every run",
                    i + 1
                )
            });
        assert!(
            key.contains("hashFiles('Cargo.toml')"),
            "the rust-cache at ci.yml:{} is keyed {key:?}, which does not \
             mention the manifest — `Cargo.lock` does not change when a \
             `[profile.*]` does",
            i + 1
        );
    }
    // One per job that installs a Rust toolchain: correctness and chaos.
    let toolchains = WORKFLOW.matches("dtolnay/rust-toolchain").count();
    assert!(
        toolchains >= 2 && caches >= toolchains,
        "expected every Rust job ({toolchains}) to cache; found {caches}"
    );
}

/// Only the addressing boundary turns an organization into a store key.
///
/// R8 — one tenant's data is unreachable from another's request — rests
/// on `stratum_control::registry` being the *only* place a request's
/// strings become an object-store key: `RepoPrefix` and `PackagePrefix`
/// both have private fields and a single constructor, each reachable
/// only after a lookup scoped `WHERE org_id = $1`.
///
/// That was held by types and convention and by nothing that could fail.
/// The types stop a *caller* from forging a prefix; they do not stop a
/// new module from writing `format!("o/{org}/…")` itself, which is how a
/// fourth key-space would quietly acquire its own, unreviewed, idea of
/// where a tenant's bytes live.
///
/// Two files legitimately build keys outside the boundary and are named
/// here rather than exempted silently:
///
/// * `workers/shipper.rs` — the audit shard key-space, `o/<org>/audit/…`,
///   which is org-scoped and has no repository in it;
/// * `workers/gc.rs` — the deleted-repo sweep, deliberately *without* a
///   layout segment so it takes every layout a repository ever had.
///
/// Adding a third means deciding that a new key-space is correct, which
/// is a decision worth making on purpose.
#[test]
fn only_the_addressing_boundary_builds_store_keys() {
    const ALLOWED: &[&str] = &[
        "crates/stratum-control/src/registry.rs",
        "crates/stratum-server/src/workers/shipper.rs",
        "crates/stratum-server/src/workers/gc.rs",
    ];

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
            let p = e.expect("dir entry").path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let mut files = Vec::new();
    walk(&root.join("crates"), &mut files);
    assert!(
        files.len() > 50,
        "failed to walk the crates, found {files:?}"
    );

    // `format!("o/` — the *constructive* form. A bare `"o/a/r/b"` literal
    // is a fixture and cannot become a key for a real organization; a
    // `format!` interpolating an id is exactly what this is about.
    //
    // Unit tests are cut off first. A test asserting the shape of a
    // prefix is not a second key-space, and reading past `#[cfg(test)]`
    // would flag the very module that pins the boundary's behaviour.
    fn product_code(text: &str) -> &str {
        match text.find("#[cfg(test)]") {
            Some(at) => &text[..at],
            None => text,
        }
    }

    let mut offenders: Vec<String> = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&root)
            .expect("under the root")
            .to_string_lossy()
            .replace('\\', "/");
        // Integration tests and the harness are not the product.
        if rel.contains("/tests/") || rel.starts_with("crates/stratum-testkit/") {
            continue;
        }
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if product_code(&text).contains("format!(\"o/") && !ALLOWED.contains(&rel.as_str()) {
            offenders.push(rel);
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "these build an object-store key outside the addressing boundary, so R8 no \
         longer rests on one reviewed place: {offenders:#?}"
    );

    // …and the allowlist is not allowed to rot: every file on it must
    // still build one, or it is a name nobody has checked in months.
    for rel in ALLOWED {
        let text =
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        assert!(
            product_code(&text).contains("format!(\"o/"),
            "{rel} is allowed to build store keys but no longer does — drop it from the list"
        );
    }
}

/// The mirror quickstart quotes the refusals a forwarded push answers
/// with, and the product is where those sentences live. A guide that
/// promised "read-only" would now be describing a different product,
/// and a guide that quoted a sentence the server does not send would
/// leave a person searching the docs for words that never appear.
#[test]
fn the_mirror_quickstart_quotes_the_sentences_a_forwarded_push_sends() {
    const GUIDE: &str = include_str!("../../../docs/guide/quickstart-mirror.md");
    const SSH_GUIDE: &str = include_str!("../../../docs/guide/ssh.md");
    const FORWARD: &str = include_str!("../src/mirror/forward.rs");
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (guide, forward) = (flat(GUIDE), flat(FORWARD));
    for sentence in [
        "this mirror has no credential that can push to its origin",
        "the mirror was behind its origin and has just caught up; fetch and push again",
        "protected branch hook declined",
    ] {
        assert!(
            guide.contains(sentence),
            "the quickstart no longer quotes {sentence:?}"
        );
        assert!(
            forward.contains(sentence),
            "forward.rs no longer sends {sentence:?}"
        );
    }
    // The last of these survived the forwarding change in the "switch
    // the CI checkout" section, three screens below the paragraph that
    // says a push is forwarded, and told the same reader the opposite.
    assert!(
        !guide.contains("read-only, provably-fresh")
            && !guide.contains("keep pushing to your origin exactly as before")
            && !guide.contains("Pushes to the mirror are rejected"),
        "the quickstart still describes a read-only mirror"
    );
    assert!(
        flat(SSH_GUIDE).contains("forwarded to its origin exactly as over HTTPS"),
        "the SSH guide still says mirrors are read-only"
    );
}
