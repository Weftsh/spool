//! A workflow job, written where every reader already looks.
//!
//! The Checks tab, the land gate and the change view all read
//! `check_runs` unioned with a change's `change_checks`. They were built
//! for verdicts other people's build systems reached, and they do not
//! know this feature exists — which is the point. A job that mirrors
//! itself into `check_runs` is visible on the commit, blocks a land when
//! it is red, and appears beside a Buildkite run in the same list, with
//! no UI work and no second code path to keep in step.
//!
//! So this module is small and it is called from **everywhere a job
//! changes state**: dispatch, finish, cancel, the skip cascade, the
//! overdue sweep. A transition that forgets to mirror leaves a check row
//! saying `running` for a job that ended an hour ago — and because the
//! land gate reads that row, it leaves a change that cannot land for a
//! reason nobody can see.
//!
//! `provider` is `HOSTED_PROVIDER` (`"weft"`) and `external_id` is the job's id, so
//! `checks::upsert` takes the atomic `(repo_id, provider, external_id)`
//! path: one row per job, however many times it reports.
//!
//! **Except for a composed run**, which goes to `changeset_checks`
//! instead and never appears in `check_runs` at all. That is not a
//! second code path grafted on: it is the one place the reasoning above
//! inverts. A composed verdict is about a *combination* of commits, so
//! writing it where every reader already looks would be exactly wrong —
//! the change's own land gate would read it as the change's answer. See
//! [`composed`].

use stratum_control::changeset_checks::{self, NewChangesetCheck};
use stratum_control::checks::{self, NewCheckRun, RunState};
use stratum_control::registry;
use stratum_control::workflows::{Run, WorkflowJob};
use stratum_control::ControlDb;

/// The page a mirrored check row sends a reader to: the run's own page
/// on the forge.
///
/// `detail_url` is the only part of a check row that answers "why is
/// this red?" — every other provider fills it with a link into their
/// own build, and a hosted row that leaves it null is a dead end. The
/// address is the **forge's**, not `/dashboard/`: it is what a person
/// pastes to a colleague, and what appears beside a Buildkite link in
/// the same list.
///
/// `public_url` is configuration and may or may not end in a slash, so
/// trim one rather than emitting a `//` that some readers normalise and
/// others do not.
pub fn run_page(public_url: &str, org_name: &str, repo_name: &str, run_id: &str) -> String {
    format!(
        "{}/{org_name}/{repo_name}/checks/runs/{run_id}",
        public_url.trim_end_matches('/')
    )
}

/// The same address for the callers that hold ids rather than names.
///
/// Resolve it **once per run** and hand the result to every
/// `mirror_job` of that run: the alternative is two more queries per
/// job, and a run's cascade re-mirrors every job it has.
///
/// `None` when either row cannot be read — a tombstoned repository, or
/// a database that just failed. A link is worth less than the verdict
/// it decorates, so the row still gets written, with no link, rather
/// than the transition being lost over an address. A caller whose
/// repository is *already* deleted (the delete route itself) holds the
/// names and should call [`run_page`] directly.
pub fn run_page_by_id(
    db: &ControlDb,
    public_url: &str,
    repo_id: &str,
    run_id: &str,
) -> Option<String> {
    let repo = registry::repo_by_id_any(db, repo_id).ok().flatten()?;
    let org = registry::org_by_id(db, &repo.org_id).ok().flatten()?;
    Some(run_page(public_url, &org.name, &repo.name, run_id))
}

/// A job's state as the Checks tab spells it.
///
/// The two vocabularies are nearly the same and deliberately not
/// identical — `passed`/`failed` is what a job did, `passing`/`failing`
/// is what a check says — so the mapping is written out rather than
/// inferred from the strings.
///
/// The fallback exists because `state` arrives as a `String` from a
/// column whose CHECK constraint permits exactly the six above; a
/// seventh cannot be read back. `Queued` is the safe answer if that ever
/// stops being true: a check that says "not finished" holds a land until
/// somebody looks, where guessing `Passing` would let unknown code
/// through on a state we did not understand.
pub fn map_state(state: &str) -> RunState {
    match state {
        "running" => RunState::Running,
        "passed" => RunState::Passing,
        "failed" => RunState::Failing,
        "skipped" => RunState::Skipped,
        "cancelled" => RunState::Cancelled,
        _ => RunState::Queued,
    }
}

/// What a job is called in a list of checks: `<workflow> / <cell>`.
///
/// The separator matters more than it looks. Two jobs called `test` in
/// two workflows are two different things, and a reader who sees `test`
/// twice with different verdicts has no way to tell which is which —
/// and the required-checks list is configured *by name*, so the
/// ambiguity would reach the land gate as well as the page.
fn check_name(run: &Run, job: &WorkflowJob) -> String {
    format!("{} / {}", run.name, job.key)
}

/// The same verdict, addressed to the changeset instead of the commit —
/// or `None` when this run is not a composed one and the ordinary
/// `check_runs` path is right.
///
/// This fork is the reason `changeset_checks` exists at all. A file that
/// says `on: [change, changeset]` produces two runs at one commit with
/// the *same* check name: the change's own, and the composed one. Both
/// in `check_runs` and they collide on identity-by-name, so the
/// per-change land gate — which reads that table by name — would take
/// the composed verdict as the member's own answer and hold, or release,
/// a change on a build of a combination it is only one part of.
///
/// A composed run with no `changeset_id` or no `composition` cannot
/// happen — the two are written together with the event — so a run
/// missing either falls through to `check_runs` rather than being
/// dropped. That is the safe direction: a verdict in the wrong table is
/// visible and wrong, where a verdict written nowhere is a check that
/// never reports and a change that waits forever.
fn composed<'a>(
    run: &'a Run,
    repo_id: &'a str,
    external_id: &'a str,
    name: &'a str,
    state: RunState,
    detail_url: Option<&'a str>,
) -> Option<NewChangesetCheck<'a>> {
    if run.event != "changeset" {
        return None;
    }
    Some(NewChangesetCheck {
        changeset_id: run.changeset_id.as_deref()?,
        composition: run.composition.as_deref()?,
        repo_id,
        run_id: &run.id,
        external_id,
        name,
        state: state.as_str(),
        detail_url,
    })
}

/// Write this job's current state to its `check_runs` row, and return
/// that row's id.
///
/// `detail_url` is the run's page — see [`run_page`]. It is a parameter
/// rather than something this function derives because the one caller
/// that matters most cannot derive it: the delete route mirrors the
/// cancellation of a repository it has just tombstoned, and a lookup by
/// id would come back empty exactly there.
pub fn mirror_job(
    db: &ControlDb,
    repo_id: &str,
    run: &Run,
    job: &WorkflowJob,
    detail_url: Option<&str>,
) -> Result<String, String> {
    let name = check_name(run, job);
    if let Some(check) = composed(
        run,
        repo_id,
        &job.id,
        &name,
        map_state(&job.state),
        detail_url,
    ) {
        return Ok(changeset_checks::upsert(db, &check)?.id);
    }
    let row = checks::upsert(
        db,
        repo_id,
        &NewCheckRun {
            commit_sha: &run.commit_sha,
            ref_name: run.ref_name.as_deref(),
            provider: checks::HOSTED_PROVIDER,
            external_id: Some(&job.id),
            name: &name,
            run_number: None,
            event: Some(&run.event),
            state: map_state(&job.state),
            detail_url,
            actor: None,
            started_at: job.started_at,
            completed_at: job.completed_at,
        },
    )?;
    Ok(row.id)
}

/// A run that was settled with no job at all, as one check row.
///
/// This is the case that is easy to leave out and expensive to leave
/// out: a workflow file we refused, a fork change nobody has approved,
/// a deployment with no runner. Writing nothing looks *identical* to CI
/// that has not started yet, so a reader waits for a check that will
/// never arrive and the change sits there. One red row naming the file
/// is the whole fix.
///
/// `blocked` is `Queued` rather than `Failing`, because nothing is wrong
/// with the change — it is waiting on a person — and a red check would
/// tell its author to go and fix code that is fine.
pub fn mirror_refusal(
    db: &ControlDb,
    repo_id: &str,
    run: &Run,
    detail_url: Option<&str>,
) -> Result<String, String> {
    let state = if run.state == "blocked" {
        RunState::Queued
    } else {
        RunState::Failing
    };
    if let Some(check) = composed(run, repo_id, &run.id, &run.file, state, detail_url) {
        return Ok(changeset_checks::upsert(db, &check)?.id);
    }
    let row = checks::upsert(
        db,
        repo_id,
        &NewCheckRun {
            commit_sha: &run.commit_sha,
            ref_name: run.ref_name.as_deref(),
            provider: checks::HOSTED_PROVIDER,
            external_id: Some(&run.id),
            name: &run.file,
            run_number: None,
            event: Some(&run.event),
            state,
            detail_url,
            actor: None,
            // Nothing ran. A start here — the run's creation was the
            // first version's choice — makes the Checks tab print a
            // duration for a run that has none: "0s" beside a refused
            // file, which reads as an instant pass, and "3m so far",
            // climbing forever, beside a blocked one waiting on a person.
            started_at: None,
            completed_at: run.completed_at,
        },
    )?;
    Ok(row.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_control::registry::{self, NewRepo, RepoKind};
    use stratum_control::workflows::{self, NewJob, NewRun};

    fn world(hint: &str) -> (ControlDb, String, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        let repo = registry::create_repo(
            &db,
            &org.id,
            &NewRepo {
                name: "app",
                kind: RepoKind::Native,
                public: false,
                description: None,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        (db, org.id, repo.id)
    }

    fn newrun(sha: &str) -> NewRun<'_> {
        NewRun {
            file: ".weft/ci.yml",
            name: "ci",
            commit_sha: sha,
            ref_name: Some("main"),
            event: "push",
            change_key: None,
            changeset_id: None,
            composition: None,
            from_fork: false,
        }
    }

    /// Every job state has a check state, and the fallback is the
    /// cautious one — a check that says "not finished" holds a land
    /// until somebody looks, where a guessed `passing` would let code
    /// through on a word we did not understand.
    #[test]
    fn every_job_state_maps_and_an_unknown_one_holds_rather_than_passes() {
        assert_eq!(map_state("queued"), RunState::Queued);
        assert_eq!(map_state("running"), RunState::Running);
        assert_eq!(map_state("passed"), RunState::Passing);
        assert_eq!(map_state("failed"), RunState::Failing);
        assert_eq!(map_state("skipped"), RunState::Skipped);
        assert_eq!(map_state("cancelled"), RunState::Cancelled);
        assert_eq!(map_state("banana"), RunState::Queued);
    }

    /// One row per job however many times it reports, named so two
    /// workflows with a `test` job are told apart, and visible through
    /// the read the Checks tab actually uses.
    #[test]
    fn a_job_mirrors_into_one_check_row_that_follows_it() {
        let (db, org, repo) = world("wf-mirror");
        let sha = "a".repeat(40);
        let run = workflows::create_run(
            &db,
            &org,
            &repo,
            &newrun(&sha),
            &[NewJob {
                job_id: "build",
                key: "build (linux)",
                matrix: r#"{"os":"linux"}"#,
                needs: &[],
                spec: "{}",
                ..Default::default()
            }],
        )
        .unwrap();
        let job = workflows::jobs_of(&db, &run.id).unwrap().remove(0);

        let page = run_page_by_id(&db, "https://forge.example/", &repo, &run.id).unwrap();
        let id = mirror_job(&db, &repo, &run, &job, Some(&page)).unwrap();
        let rows = checks::latest_for_commit(&db, &repo, &sha).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].name, "ci / build (linux)");
        assert_eq!(rows[0].state, "queued");
        assert_eq!(rows[0].provider, "weft");
        assert_eq!(rows[0].external_id.as_deref(), Some(job.id.as_str()));
        // The row links to the run's page on the forge, resolved from
        // ids alone and with the configured trailing slash trimmed.
        assert_eq!(
            rows[0].detail_url.as_deref(),
            Some(format!("https://forge.example/acme/app/checks/runs/{}", run.id).as_str())
        );

        // Claim, then finish, mirroring at each transition — the same
        // row moves rather than a second one appearing.
        let labels = vec!["self-hosted".to_string()];
        let route = workflows::RunnerRoute {
            runner_id: "box",
            org_id: &org,
            group_id: "",
            labels: &labels,
            all_repos: true,
            allow_public: false,
        };
        let claimed = workflows::claim_self_hosted(&db, &route, 60_000)
            .unwrap()
            .unwrap();
        let run_now = workflows::run_by_id(&db, &run.id).unwrap().unwrap();
        assert_eq!(
            mirror_job(&db, &repo, &run_now, &claimed, Some(&page)).unwrap(),
            id
        );
        assert_eq!(
            checks::latest_for_commit(&db, &repo, &sha).unwrap()[0].state,
            "running"
        );

        workflows::finish(&db, &claimed.id, "failed", None, Some("boom")).unwrap();
        let done = workflows::job(&db, &claimed.id).unwrap().unwrap();
        let run_now = workflows::run_by_id(&db, &run.id).unwrap().unwrap();
        assert_eq!(
            mirror_job(&db, &repo, &run_now, &done, Some(&page)).unwrap(),
            id
        );
        let rows = checks::latest_for_commit(&db, &repo, &sha).unwrap();
        assert_eq!(rows.len(), 1, "one row, four reports: {rows:?}");
        assert_eq!(rows[0].state, "failing");
        assert!(rows[0].completed_at.is_some());
    }

    /// A run with no jobs still says something, because saying nothing
    /// is indistinguishable from CI that has not started — and a reader
    /// waiting on a check that will never arrive is the failure mode.
    #[test]
    fn a_run_with_no_jobs_still_leaves_a_row_a_reader_can_see() {
        let (db, org, repo) = world("wf-mirror-refusal");
        let sha = "b".repeat(40);
        let refused = workflows::create_settled_run(
            &db,
            &org,
            &repo,
            &newrun(&sha),
            "failed",
            Some(".weft/ci.yml:4 unknown key `uses`"),
            None,
        )
        .unwrap();
        let page = run_page_by_id(&db, "https://forge.example", &repo, &refused.id).unwrap();
        mirror_refusal(&db, &repo, &refused, Some(&page)).unwrap();
        let rows = checks::latest_for_commit(&db, &repo, &sha).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, ".weft/ci.yml");
        assert_eq!(rows[0].state, "failing");
        // A refused file is the row most in need of a link: the reason
        // it was refused is on the run's page and nowhere else.
        assert_eq!(rows[0].detail_url.as_deref(), Some(page.as_str()));
        // Nothing ran, so the row has no start. The first version wrote
        // the run's *creation* as its start, and the Checks tab — which
        // rightly shows no duration for a run that never started —
        // then printed "0s" beside a refused file, which reads as an
        // instant pass.
        assert!(rows[0].started_at.is_none(), "{:?}", rows[0]);

        // Blocked is *waiting*, not wrong: a red check here would send
        // the author to fix code that is fine.
        let sha2 = "c".repeat(40);
        let blocked = workflows::create_settled_run(
            &db,
            &org,
            &repo,
            &newrun(&sha2),
            "blocked",
            Some("waiting for a maintainer to approve this fork's change"),
            Some(workflows::BlockedReason::Fork),
        )
        .unwrap();
        let page2 = run_page_by_id(&db, "https://forge.example", &repo, &blocked.id).unwrap();
        mirror_refusal(&db, &repo, &blocked, Some(&page2)).unwrap();
        let rows = checks::latest_for_commit(&db, &repo, &sha2).unwrap();
        assert_eq!(rows[0].state, "queued");
        assert_eq!(rows[0].detail_url.as_deref(), Some(page2.as_str()));
        assert!(rows[0].completed_at.is_none());
        // And the same for a blocked one, where the invented start was
        // worse: with no end to pair it with, the tab counted "3m 47s so
        // far" upward, forever, beside a run that was waiting on a
        // person and doing nothing at all.
        assert!(rows[0].started_at.is_none(), "{:?}", rows[0]);
    }

    /// The address is the forge's, one slash between every part
    /// however the deployment spells `STRATUM_PUBLIC_URL`, and a
    /// repository that is no longer there resolves to no link rather
    /// than to a wrong one.
    #[test]
    fn the_run_page_is_the_forges_address_and_survives_a_trailing_slash() {
        assert_eq!(
            run_page("https://forge.example", "acme", "app", "r1"),
            "https://forge.example/acme/app/checks/runs/r1"
        );
        assert_eq!(
            run_page("https://forge.example/", "acme", "app", "r1"),
            "https://forge.example/acme/app/checks/runs/r1",
            "a configured trailing slash must not double up"
        );
        // Not /dashboard/: this is the address a person pastes to a
        // colleague, beside links into other people's build systems.
        assert!(!run_page("https://forge.example", "acme", "app", "r1").contains("/dashboard/"));

        let (db, _org, repo) = world("wf-mirror-page");
        assert_eq!(
            run_page_by_id(&db, "https://forge.example", "repo_nosuchrepo", "r1"),
            None,
            "an unknown repository has no page, and no link is better than a wrong one"
        );
        assert_eq!(
            run_page_by_id(&db, "https://forge.example", &repo, "r1").as_deref(),
            Some("https://forge.example/acme/app/checks/runs/r1")
        );
    }
}
