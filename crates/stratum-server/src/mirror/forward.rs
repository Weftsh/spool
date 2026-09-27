//! Write-through: a push to a mirror goes to its origin first.
//!
//! A mirror used to be read-only, and the refusal named the origin:
//! "push there instead". That is right for a copy that exists to be
//! cloned from and wrong for what a mirror is increasingly for — the
//! remote an agent or a CI job is pointed at, so that nothing has to
//! move and GitHub stays where the people are. So a push here is
//! **forwarded**: the objects are indexed into the seed clone the sync
//! already keeps, pushed to the origin under the credential the sync
//! already fetches with, and only once the origin has taken them is
//! the mirror brought up to date and the client told `ok`.
//!
//! # Synchronous, on purpose
//!
//! The origin is canonical. Accepting the push locally and forwarding
//! it later would let the two disagree — a refused forward leaving the
//! mirror ahead of an origin that never heard of the commit — and the
//! whole point of the mirror is that it never disagrees. So the push
//! waits for the origin's answer and repeats it: a protected branch,
//! a non-fast-forward, a missing permission, all in the origin's own
//! words, as an in-band `ng` the way stock git prints them.
//!
//! # What it costs
//!
//! One fetch before (the seed must hold the bases a thin pack was built
//! against, and a cold node has no seed at all), the push, and one
//! fetch after (the push moved the origin; the seed's refs follow it
//! the same way a sync's would). Every step runs on the blocking pool
//! under the same locks a sync takes, so a webhook arriving for our own
//! push finds nothing left to do.

use crate::mirror::origin::OriginProvider;
use std::path::Path;
use stratum_control::registry::Repo;
use stratum_engine::gitcmd::{git, run, run_with_stdin};
use stratum_proto::receive::Update;

const ZERO: &str = "0000000000000000000000000000000000000000";

/// Why a forwarded push did not land. Each variant maps to one answer
/// on the wire and one status over REST; the wire always says the
/// sentence, REST also says which kind it was.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// The origin answered the commands one by one. Aligned with the
    /// push's updates; `Ok` is a command that was fine on its own and
    /// refused only because the push is atomic.
    Each(Vec<Result<(), String>>),
    /// The mirror was behind its origin, so the value the client built
    /// on was stale before it pushed. The mirror has caught up; the
    /// client fetches and tries again.
    Behind(String),
    /// Refused here, before the origin heard of it: a ref name the
    /// layer does not take, the default branch, the storage cap.
    Local(String),
    /// The credential the mirror syncs with cannot write to the origin.
    PermissionDenied(String),
    /// The mirror has no credential that could write to its origin.
    NoCredential(String),
    /// The origin did not answer.
    Unreachable(String),
    /// Another node was syncing this mirror and did not finish in time.
    Busy(String),
}

impl Refusal {
    /// The sentence the client reads, when there is one for the whole
    /// push. `Each` has one per command instead.
    pub fn sentence(&self) -> Option<&str> {
        match self {
            Refusal::Each(_) => None,
            Refusal::Behind(s)
            | Refusal::Local(s)
            | Refusal::PermissionDenied(s)
            | Refusal::NoCredential(s)
            | Refusal::Unreachable(s)
            | Refusal::Busy(s) => Some(s),
        }
    }

    /// One verdict per command, for the wire report.
    pub fn verdicts(&self, n: usize) -> Vec<Result<(), String>> {
        match self {
            Refusal::Each(v) => v.clone(),
            other => vec![Err(other.sentence().unwrap_or_default().to_string()); n],
        }
    }
}

/// The permission a forwarded push needs and an installation made
/// before this feature does not hold. Same shape as the runner's
/// `ADMIN_WRITE_DENIED`: the name of the permission, that it was added
/// after the install, and where to approve it.
pub const CONTENTS_WRITE_DENIED: &str = "this GitHub App installation cannot push to this \
     repository — it needs the `Contents: write` permission, which was added after the App \
     was installed. Approve it on GitHub under the App's installation settings";

/// A mirror registered from a pasted URL, with no installation behind
/// it, fetches its origin as a stranger. A stranger cannot push.
/// A mirror whose first sync has not landed has no layout to build a
/// commit on or resolve a rev against. Not a crash — a "not ready yet",
/// said cleanly rather than as a leaked `manifest.json: HTTP 404`.
pub const NO_LAYOUT_YET: &str =
    "the mirror has no layout yet — it serves and forwards only after its first sync from the origin";

pub fn no_credential_msg(repo: &Repo) -> String {
    match &repo.origin_url {
        Some(o) => format!(
            "this mirror has no credential that can push to its origin ({o}); push there \
             directly, or connect the GitHub App and attach the installation to this mirror"
        ),
        None => "this mirror has no credential that can push to its origin".into(),
    }
}

/// The sentence for a push whose old values the mirror could not vouch
/// for: it lagged its origin, and has now caught up.
pub fn behind_msg() -> String {
    "the mirror was behind its origin and has just caught up; fetch and push again".into()
}

/// The checks the wire engine makes before touching a store, made here
/// before touching the origin: a name the layer would not take back on
/// a fetch, and the one deletion that leaves every clone empty.
pub fn local_guards(repo: &Repo, updates: &[Update]) -> Result<(), Refusal> {
    for u in updates {
        let rest = u
            .name
            .strip_prefix("refs/heads/")
            .or_else(|| u.name.strip_prefix("refs/tags/"));
        let ok_name = rest.is_some_and(|r| {
            !r.is_empty()
                && r.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
        });
        if !ok_name {
            return Err(Refusal::Local(format!("ref name {} not accepted", u.name)));
        }
        if u.new == ZERO && u.name == format!("refs/heads/{}", repo.default_branch) {
            return Err(Refusal::Local(format!(
                "'{}' is the default branch: point HEAD elsewhere first",
                repo.default_branch
            )));
        }
    }
    Ok(())
}

/// What `git push --porcelain --atomic` said, read the way the product
/// needs it. Pure: the phrasing table is pinned by unit tests and by
/// the fixtures `scripts/manual-mirror-push.sh` records from real
/// GitHub.
///
/// `stdout` is the porcelain report, one line per command:
/// `<flag>\t<from>:<to>\t<summary>`, flag `!` for a refusal with the
/// reason in parentheses. `stderr` is what the remote and the
/// transport said. `exit_ok` is git's own verdict.
pub fn classify(
    updates: &[Update],
    stdout: &str,
    stderr: &str,
    exit_ok: bool,
) -> Result<(), Refusal> {
    if exit_ok {
        return Ok(());
    }
    // Per-command refusals: the origin (or git, for a lease) named the
    // command. A stale lease means the mirror lagged, and that is a
    // different answer from "the origin said no".
    let mut named: Vec<(String, String)> = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(flag), Some(spec), Some(summary)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if flag != "!" {
            continue;
        }
        let to = spec.split_once(':').map(|(_, t)| t).unwrap_or(spec);
        let reason = summary
            .split_once('(')
            .and_then(|(_, r)| r.strip_suffix(')'))
            .unwrap_or(summary)
            .trim()
            .to_string();
        named.push((to.to_string(), reason));
    }
    if named.iter().any(|(_, r)| r == "stale info") {
        return Err(Refusal::Behind(behind_msg()));
    }
    if !named.is_empty() {
        // What the origin said for itself: GitHub explains a protected
        // branch on `remote:` lines, and the porcelain reason is one
        // phrase. The person reading the report wants both.
        let said = remote_lines(stderr);
        // The spellings of "refused only because a sibling was". They
        // differ by who refused: git says one thing when its own lease
        // check failed a sibling before sending, the receiving end says
        // another when it refused one.
        //
        // `atomic transaction failed` is what **real GitHub** answers,
        // observed by `scripts/manual-mirror-push.sh protected` on
        // 2026-09-16 and recorded in
        // `fixtures/mirror-push/protected-atomic.stdout`. It was missing
        // here, and the cost was not cosmetic: in an atomic push where
        // one ref hit a protected branch, every innocent sibling was
        // reported as refused *for its own reason*, with the protected
        // branch's `GH006` explanation attached to it. The person would
        // have gone looking for a problem with a branch that was fine.
        let sibling = |r: &str| {
            r == "atomic transaction failed"
                || r == "atomic push failed"
                || r == "atomic push failure"
        };
        let each = updates
            .iter()
            .map(|u| match named.iter().find(|(to, _)| to == &u.name) {
                Some((_, reason)) if !sibling(reason) => Err(match &said {
                    Some(s) => format!("origin refused: {reason}: {s}"),
                    None => format!("origin refused: {reason}"),
                }),
                _ => Ok(()),
            })
            .collect::<Vec<_>>();
        // Every command refused with a sibling reason and none
        // named: the reason is in stderr, not the report.
        if each.iter().all(|v| v.is_ok()) {
            return Err(Refusal::Unreachable(transport_sentence(stderr)));
        }
        return Err(Refusal::Each(each));
    }
    // No command was named: the transport or the credential.
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("returned error: 403")
        || lower.contains("write access to repository not granted")
        || lower.contains("permission to")
    {
        return Err(Refusal::PermissionDenied(CONTENTS_WRITE_DENIED.into()));
    }
    if lower.contains("returned error: 401")
        || lower.contains("authentication failed")
        || lower.contains("could not read username")
        || lower.contains("terminal prompts disabled")
    {
        return Err(Refusal::NoCredential(
            "the origin refused the mirror's credential".into(),
        ));
    }
    Err(Refusal::Unreachable(transport_sentence(stderr)))
}

/// The `remote:` lines of a push's stderr, joined — bounded, because a
/// hook can say anything at any length and this rides a pkt-line.
fn remote_lines(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.trim().strip_prefix("remote:"))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("; "))
    }
}

fn transport_sentence(stderr: &str) -> String {
    let said = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("no answer");
    format!("origin unreachable: {said}")
}

/// Index the client's pack into the seed and push the commands to the
/// origin. The seed has already been fetched, so a thin pack's bases
/// are there to fix against.
///
/// Answers the classified outcome; the caller decides what a refusal
/// means for the mirror's own state.
pub fn push_to_origin(seed: &Path, updates: &[Update], pack: Option<&[u8]>) -> Result<(), Refusal> {
    if let Some(pack) = pack {
        if pack.len() < 32 || &pack[..4] != b"PACK" {
            return Err(Refusal::Local("no pack in push".into()));
        }
        // index-pack verifies every hash and delta against the seed's
        // own objects. A refused push leaves the pack behind,
        // unreachable: the sync walks from refs and never packs it, and
        // `gc --auto` below trims it once there is enough to trim.
        run_with_stdin(
            git(seed).args(["index-pack", "--fix-thin", "--stdin"]),
            pack,
        )
        .map_err(|e| Refusal::Local(format!("pack verification failed: {e}")))?;
    }
    let mut cmd = git(seed);
    cmd.args(["push", "--porcelain", "--atomic", "--no-verify"]);
    for u in updates {
        // The client's `old` is the value it built on, which is what
        // the mirror advertised: exactly `--force-with-lease`, the rule
        // the wire engine applies to every push it lands itself. An
        // empty expectation means "must not exist yet".
        let expect = if u.old == ZERO { "" } else { u.old.as_str() };
        cmd.arg(format!("--force-with-lease={}:{expect}", u.name));
    }
    cmd.arg("origin");
    for u in updates {
        if u.new == ZERO {
            cmd.arg(format!(":{}", u.name));
        } else {
            cmd.arg(format!("{}:{}", u.new, u.name));
        }
    }
    let out = cmd
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .map_err(|e| Refusal::Unreachable(format!("spawn git: {e}")))?;
    let verdict = classify(
        updates,
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
        out.status.success(),
    );
    if verdict.is_err() {
        let _ = run(git(seed).args(["gc", "--auto", "-q"]));
    }
    verdict
}

/// Whether this mirror has a credential that could push at all.
///
/// Knowable before any subprocess, and said at the advert so the
/// client never builds a pack for nothing: a GitHub mirror registered
/// without an installation fetches its origin as a stranger.
pub fn credential_check(provider: &dyn OriginProvider, repo: &Repo) -> Result<(), Refusal> {
    if provider.can_push(repo) {
        Ok(())
    } else {
        Err(Refusal::NoCredential(no_credential_msg(repo)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(name: &str, old: &str, new: &str) -> Update {
        Update {
            old: old.into(),
            new: new.into(),
            name: name.into(),
        }
    }
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn repo() -> Repo {
        Repo {
            id: "r".into(),
            org_id: "o".into(),
            name: "widget".into(),
            description: None,
            homepage: None,
            kind: stratum_control::registry::RepoKind::Mirror,
            public: true,
            default_branch: "main".into(),
            origin_url: Some("acme/widget".into()),
            origin_provider: Some("github".into()),
            origin_installation: None,
            last_sync_at: None,
            last_synced_commit: None,
            sync_error: None,
            created_at: 0,
        }
    }

    #[test]
    fn a_successful_push_is_ok_whatever_it_printed() {
        assert_eq!(
            classify(&[up("refs/heads/x", ZERO, A)], "", "", true),
            Ok(())
        );
    }

    /// The origin refused one command by name; the sibling was fine on
    /// its own and fell with it. The report says which was which.
    #[test]
    fn an_origin_refusal_is_reported_per_command() {
        let updates = [
            up("refs/heads/main", A, B),
            up("refs/heads/feature", ZERO, B),
        ];
        let stdout = "To https://github.com/acme/widget.git\n\
                      !\trefs/heads/main:refs/heads/main\t[remote rejected] (protected branch hook declined)\n\
                      !\trefs/heads/feature:refs/heads/feature\t[remote rejected] (atomic transaction failed)\n\
                      Done\n";
        let stderr = "remote: error: GH006: Protected branch update failed for refs/heads/main.\n\
                      remote: \n\
                      remote: - Changes must be made through a pull request.\n";
        let got = classify(&updates, stdout, stderr, false);
        assert_eq!(
            got,
            Err(Refusal::Each(vec![
                Err(
                    "origin refused: protected branch hook declined: error: GH006: Protected \
                     branch update failed for refs/heads/main.; - Changes must be made through \
                     a pull request."
                        .into()
                ),
                Ok(()),
            ]))
        );
        // Without a remote explanation, the phrase stands alone.
        let got = classify(&updates, stdout, "", false);
        assert!(
            matches!(&got, Err(Refusal::Each(v))
                if v[0] == Err("origin refused: protected branch hook declined".into())),
            "{got:?}"
        );
    }

    /// git's own lease check firing means the mirror advertised a value
    /// the origin had already moved past: not the origin's refusal,
    /// ours to repair.
    #[test]
    fn a_stale_lease_is_the_mirror_being_behind() {
        let updates = [up("refs/heads/main", A, B)];
        let stdout =
            "To file:///o.git\n!\trefs/heads/main:refs/heads/main\t[rejected] (stale info)\nDone\n";
        assert_eq!(
            classify(&updates, stdout, "", false),
            Err(Refusal::Behind(behind_msg()))
        );
    }

    #[test]
    fn a_403_from_the_transport_is_the_missing_permission() {
        let updates = [up("refs/heads/main", A, B)];
        let stderr = "remote: Write access to repository not granted.\n\
                      fatal: unable to access 'https://github.com/acme/widget.git/': The requested URL returned error: 403\n";
        let got = classify(&updates, "", stderr, false);
        assert!(
            matches!(&got, Err(Refusal::PermissionDenied(s)) if s.contains("Contents: write")),
            "{got:?}"
        );
    }

    #[test]
    fn a_401_is_no_credential_and_the_rest_is_unreachable() {
        let updates = [up("refs/heads/main", A, B)];
        let stderr = "fatal: unable to access 'https://github.com/x/y.git/': The requested URL returned error: 401\n";
        assert!(matches!(
            classify(&updates, "", stderr, false),
            Err(Refusal::NoCredential(_))
        ));
        let stderr = "fatal: unable to access 'https://github.com/x/y.git/': Could not resolve host: github.com\n";
        assert_eq!(
            classify(&updates, "", stderr, false),
            Err(Refusal::Unreachable(
                "origin unreachable: fatal: unable to access 'https://github.com/x/y.git/': Could not resolve host: github.com".into()
            ))
        );
    }

    /// Every command "atomic push failed" and none named is git telling
    /// us the failure was somewhere else — read stderr, do not report
    /// the whole push as fine.
    #[test]
    fn an_unnamed_atomic_failure_reads_the_transport() {
        let updates = [up("refs/heads/main", A, B)];
        let stdout = "!\trefs/heads/main:refs/heads/main\t[rejected] (atomic push failed)\n";
        assert!(matches!(
            classify(&updates, stdout, "error: something", false),
            Err(Refusal::Unreachable(_))
        ));
    }

    /// The classifier reads git's phrasing, never a bare number: a
    /// repository called `rfc-403` in the URL is not a permission
    /// refusal, and a pid ending in 401 is not a credential one. Same
    /// class as the origin probe's `rfc-403` bug.
    #[test]
    fn a_number_in_a_path_is_not_a_status() {
        let updates = [up("refs/heads/main", A, B)];
        for stderr in [
            "fatal: unable to access 'https://github.com/acme/rfc-403.git/': Could not resolve host\n",
            "fatal: '/tmp/stratum-4018/x-401.git' does not appear to be a git repository\n",
        ] {
            assert!(
                matches!(classify(&updates, "", stderr, false), Err(Refusal::Unreachable(_))),
                "{stderr}"
            );
        }
    }

    #[test]
    fn the_local_guards_refuse_what_the_wire_engine_refuses() {
        let r = repo();
        assert_eq!(local_guards(&r, &[up("refs/heads/x", ZERO, A)]), Ok(()));
        assert!(matches!(
            local_guards(&r, &[up("refs/pull/1/head", ZERO, A)]),
            Err(Refusal::Local(s)) if s.contains("not accepted")
        ));
        assert!(matches!(
            local_guards(&r, &[up("refs/heads/main", A, ZERO)]),
            Err(Refusal::Local(s)) if s.contains("default branch")
        ));
        assert!(matches!(
            local_guards(&r, &[up("refs/heads/", ZERO, A)]),
            Err(Refusal::Local(_))
        ));
    }

    /// Every case the manual gate records, replayed through the
    /// classifier. Until `scripts/manual-mirror-push.sh fixtures` has
    /// run, the files are what the docs led us to believe and the
    /// provenance says so; once it has, they are the wire, and a
    /// classifier that reads the wire wrong goes red here rather than
    /// on a customer's push. The e2e hook in `tests/mirror_e2e.rs` is
    /// held to the same recorded remote line.
    #[test]
    fn mirror_push_fixtures_classify_like_the_fake() {
        const PROVENANCE: &str =
            include_str!("../../../stratum-testkit/fixtures/mirror-push/provenance.json");
        const E2E: &str = include_str!("../../tests/mirror_e2e.rs");
        let cases: &[(&str, &str, &str)] = &[
            (
                "push",
                include_str!("../../../stratum-testkit/fixtures/mirror-push/push.stdout"),
                include_str!("../../../stratum-testkit/fixtures/mirror-push/push.stderr"),
            ),
            (
                "delete",
                include_str!("../../../stratum-testkit/fixtures/mirror-push/delete.stdout"),
                include_str!("../../../stratum-testkit/fixtures/mirror-push/delete.stderr"),
            ),
            (
                "stale",
                include_str!("../../../stratum-testkit/fixtures/mirror-push/stale.stdout"),
                include_str!("../../../stratum-testkit/fixtures/mirror-push/stale.stderr"),
            ),
            (
                "nonff",
                include_str!("../../../stratum-testkit/fixtures/mirror-push/nonff.stdout"),
                include_str!("../../../stratum-testkit/fixtures/mirror-push/nonff.stderr"),
            ),
            (
                "protected",
                include_str!("../../../stratum-testkit/fixtures/mirror-push/protected.stdout"),
                include_str!("../../../stratum-testkit/fixtures/mirror-push/protected.stderr"),
            ),
            (
                "protected-atomic",
                include_str!(
                    "../../../stratum-testkit/fixtures/mirror-push/protected-atomic.stdout"
                ),
                include_str!(
                    "../../../stratum-testkit/fixtures/mirror-push/protected-atomic.stderr"
                ),
            ),
            (
                "denied",
                include_str!("../../../stratum-testkit/fixtures/mirror-push/denied.stdout"),
                include_str!("../../../stratum-testkit/fixtures/mirror-push/denied.stderr"),
            ),
        ];
        let prov: serde_json::Value = serde_json::from_str(PROVENANCE).unwrap();
        let observed = prov["observed"].as_bool().unwrap_or(false);
        if !observed {
            eprintln!(
                "mirror-push fixtures are a BELIEF, not an observation: run \
                 scripts/manual-mirror-push.sh all, then fixtures"
            );
        }
        for (name, stdout, stderr) in cases {
            let case = &prov["cases"][name];
            // A case the manual gate did not observe carries no
            // provenance. `denied` is the one: it needs a second
            // installation of the same App that genuinely lacks
            // `Contents: write`, and the run that recorded these had
            // none. Skip it, loudly, rather than replay the old
            // hand-written belief as though it were the wire — a case
            // not observed is a NOTE, never a pass.
            let Some(exit_ok) = case["exit_ok"].as_bool() else {
                eprintln!(
                    "mirror-push fixture `{name}`: not observed by \
                     scripts/manual-mirror-push.sh; skipped, not claimed"
                );
                continue;
            };
            let updates: Vec<Update> = case["updates"]
                .as_array()
                .expect("updates")
                .iter()
                .map(|u| Update {
                    old: u["old"].as_str().unwrap().into(),
                    new: u["new"].as_str().unwrap().into(),
                    name: u["ref"].as_str().unwrap().into(),
                })
                .collect();
            let got = classify(&updates, stdout, stderr, exit_ok);
            match *name {
                "push" | "delete" | "nonff" => assert_eq!(got, Ok(()), "{name}"),
                "stale" => assert!(matches!(got, Err(Refusal::Behind(_))), "{name}: {got:?}"),
                "protected" => {
                    let Err(Refusal::Each(v)) = &got else {
                        panic!("{name}: {got:?}")
                    };
                    let reason = v[0].as_ref().unwrap_err();
                    assert!(
                        reason.contains(prov["beliefs"]["protected_reason"].as_str().unwrap()),
                        "{name}: {reason}"
                    );
                    assert!(
                        reason.contains(prov["beliefs"]["protected_remote"].as_str().unwrap()),
                        "{name}: the remote's explanation rides along: {reason}"
                    );
                }
                "protected-atomic" => {
                    let Err(Refusal::Each(v)) = &got else {
                        panic!("{name}: {got:?}")
                    };
                    assert!(
                        v[0].is_err(),
                        "{name}: the protected branch is the refused one"
                    );
                    assert_eq!(
                        v[1],
                        Ok(()),
                        "{name}: the sibling was fine on its own: {v:?}"
                    );
                }
                "denied" => assert!(
                    matches!(&got, Err(Refusal::PermissionDenied(s)) if s.contains("Contents: write")),
                    "{name}: {got:?}"
                ),
                other => panic!("no expectation for fixture {other}"),
            }
        }
        // The e2e suite's hook speaks GitHub's recorded line, so what the
        // hermetic tests assert about a refusal is what a person would
        // see against the real origin.
        let remote_line = prov["beliefs"]["protected_remote"].as_str().unwrap();
        assert!(
            E2E.contains(remote_line),
            "tests/mirror_e2e.rs's pre-receive hook must say {remote_line:?}"
        );
    }

    #[test]
    fn verdicts_fan_a_whole_push_sentence_out_per_command() {
        let v = Refusal::Busy("busy".into()).verdicts(2);
        assert_eq!(v, vec![Err("busy".to_string()), Err("busy".to_string())]);
        let e = Refusal::Each(vec![Ok(()), Err("no".into())]);
        assert_eq!(e.verdicts(2), vec![Ok(()), Err("no".to_string())]);
        assert_eq!(e.sentence(), None);
    }

    #[test]
    fn the_no_credential_sentence_names_the_origin() {
        let r = repo();
        assert!(no_credential_msg(&r).contains("acme/widget"));
        let mut r2 = r.clone();
        r2.origin_url = None;
        assert!(!no_credential_msg(&r2).contains('('));
    }
}
