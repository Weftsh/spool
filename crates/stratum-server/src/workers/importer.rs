//! Importing a project's issues from GitHub.
//!
//! **Bounded and resumable, because the job is neither small nor
//! reliable.** A repository with ten thousand issues is tens of
//! thousands of requests once their conversations are counted, against a
//! budget of five thousand an hour. A run that tried to finish in one go
//! would be killed by a deploy, a lease expiry or the rate limiter, and
//! would start again from nothing.
//!
//! So a run does a bounded amount of work, writes where it got to, and
//! re-enqueues. Rate limiting is not an error here: it is the expected
//! state of a large import, and the answer is to come back later rather
//! than to fail the job. What *is* an error is a refusal — an
//! installation without `issues: read` — and that stops the import with
//! the reason on the job, because an import that quietly produces
//! nothing looks exactly like a project that never had issues.
//!
//! Phases run in order and each keeps its own cursor: labels, then
//! milestones, then issues, then the comments on them. Cross-references
//! would come last, when every target exists — they are not built.

use crate::app::SharedState;
use crate::mirror::origin::Page;
use stratum_control::imports::{self, ImportedComment, ImportedIssue};
use stratum_control::{jobs, registry};

/// How many pages one run walks before writing its cursor and
/// re-enqueueing. Small enough that a killed process loses little,
/// large enough that a big import is not a thousand job rows.
fn page_budget() -> usize {
    super::env_secs("STRATUM_IMPORT_PAGES_PER_RUN", 20) as usize
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    /// Nothing left to do.
    Done { issues: usize, comments: usize },
    /// Budget spent or rate limited; the job has been re-enqueued.
    ///
    /// `retry_after_secs` is what the provider asked for, 0 when it asked
    /// for nothing. The follow-up row is held for that long rather than
    /// the worker being parked, so one organization's rate limit does not
    /// stop everybody else's imports.
    More {
        issues: usize,
        comments: usize,
        retry_after_secs: u64,
    },
    /// The repository, or its origin, is gone.
    NotNeeded,
}

pub fn enqueue(state: &SharedState, org_id: &str, repo_id: &str) -> Result<(), String> {
    jobs::enqueue_unique(&state.db, org_id, repo_id, "import", None)?;
    Ok(())
}

/// Enqueue the follow-up, not claimable for `retry_after_secs`.
///
/// Zero is the ordinary case — a budget spent rather than a refusal —
/// and goes through the plain path so nothing waits that need not.
fn enqueue_after(
    state: &SharedState,
    org_id: &str,
    repo_id: &str,
    retry_after_secs: u64,
) -> Result<(), String> {
    if retry_after_secs == 0 {
        return enqueue(state, org_id, repo_id);
    }
    // Saturating: a provider is free to send a nonsense `Retry-After`,
    // and an overflow that wrapped to the past would spin exactly as the
    // bug this replaces did.
    let until = crate::workers::not_before_ms(retry_after_secs);
    jobs::enqueue_unique_after(&state.db, org_id, repo_id, "import", None, until)?;
    Ok(())
}

pub fn spawn(state: SharedState) {
    let poll = super::env_period("STRATUM_IMPORT_POLL_SECS", 30);
    if poll.is_zero() {
        return;
    }
    // The lease is how long a crashed node's import stays stuck. Modest,
    // because a run is bounded by design and re-enqueues rather than
    // running long — there is nothing here that legitimately takes
    // half an hour.
    let lease_ms = super::lease_ms("STRATUM_IMPORT_LEASE_SECS", 300);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "import", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(o) => {
                        // **Complete first, then re-enqueue, in that
                        // order.** `jobs_active_per_repo` covers
                        // `import`, and its predicate is `state IN
                        // ('queued','running')` — so a follow-up
                        // enqueued from inside `run_one` conflicts with
                        // the very row that is running it, `ON CONFLICT
                        // DO NOTHING` no-ops, and the import stops dead
                        // at its first budget with no error anywhere.
                        //
                        // It did exactly that for as long as the index
                        // covered this kind, and nothing here noticed:
                        // every fixture in `import_e2e` finished inside
                        // the twenty-page budget, so the resume path had
                        // never been walked. `a_large_tracker_resumes…`
                        // walks it now.
                        //
                        // The trade, both directions, because a later
                        // editor will be tempted by each: this order
                        // leaves a window in which a node dying between
                        // the two statements leaves no follow-up row —
                        // nothing is lost, the phase cursors are already
                        // written and the next enqueue resumes from
                        // them, but that import waits for one. The other
                        // order loses the re-enqueue to a conflict every
                        // single time, which is not a window but a wall.
                        let _ = jobs::complete(&state.db, &job.id, Some(&format!("{o:?}")));
                        if let ImportOutcome::More {
                            retry_after_secs, ..
                        } = o
                        {
                            if let Some(repo_id) = &job.repo_id {
                                // The call is its own statement so the
                                // `Err` arm stays one line: a multi-line
                                // `if let Err(..) = f(..)` spreads the
                                // same unreachable region over three, and
                                // the ledger should not grow to cover
                                // formatting.
                                let queued =
                                    enqueue_after(&state, &job.org_id, repo_id, retry_after_secs);
                                if let Err(e) = queued {
                                    eprintln!("weft: import re-enqueue: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("weft: import failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(poll).await,
                Err(e) => {
                    eprintln!("weft: import claim: {e}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<ImportOutcome, String> {
    let Some(repo_id) = job.repo_id.clone() else {
        return Err("import job without repo".into());
    };
    let Some(repo) = registry::repo_by_id(&state.db, &job.org_id, &repo_id)? else {
        return Ok(ImportOutcome::NotNeeded);
    };
    // An import needs somewhere upstream to import *from*, and that is
    // the mirror's own origin — the same `owner/name` the repository is
    // already synchronising commits from.
    let (Some(full_name), Some(installation)) =
        (repo.origin_url.clone(), repo.origin_installation.clone())
    else {
        return Err(
            "this repository has no GitHub origin to import from — connect it as a \
             mirror first, so the import reads the same upstream the commits do"
                .into(),
        );
    };
    let Some(app) = state.sync.github_app() else {
        return Err("no GitHub App is configured on this deployment".into());
    };

    let mut issues = 0usize;
    let mut comments = 0usize;
    let budget = page_budget();

    // Phase 1 — labels. Small, and first because issues reference them.
    if imports::cursor(&state.db, &repo.id, "labels")?.is_none() {
        match app.labels_page(&installation, &full_name, 100, 1) {
            Ok(Page::Data { body, .. }) => {
                import_labels(state, &repo.id, &body)?;
                imports::set_cursor(&state.db, &repo.id, "labels", "done")?;
            }
            Ok(Page::RateLimited { retry_after_secs }) => {
                return requeue(state, job, retry_after_secs, issues, comments);
            }
            Ok(Page::Refused { status, body }) => return Err(refusal(status, &body)),
            Err(e) => return Err(e),
        }
    }

    // Phase 2 — milestones. Before issues, because issues point at them.
    if imports::cursor(&state.db, &repo.id, "milestones")?.is_none() {
        match app.milestones_page(&installation, &full_name, 100, 1) {
            Ok(Page::Data { body, .. }) => {
                import_milestones(state, &repo.id, &body)?;
                imports::set_cursor(&state.db, &repo.id, "milestones", "done")?;
            }
            Ok(Page::RateLimited { retry_after_secs }) => {
                return requeue(state, job, retry_after_secs, issues, comments);
            }
            Ok(Page::Refused { status, body }) => return Err(refusal(status, &body)),
            Err(e) => return Err(e),
        }
    }

    // Phase 3 — issues, **following `Link` rather than counting pages.**
    //
    // The cursor holds the next page's URL, not a number. Incrementing a
    // counter works until the last page is exactly full and then stops
    // one page early — silently, with a plausible-looking import that is
    // a hundred issues short. GitHub names the next page and nowhere
    // else does.
    let mut cursor = imports::cursor(&state.db, &repo.id, "issues")?;
    if cursor.as_deref() == Some("done") {
        return Ok(ImportOutcome::Done { issues, comments });
    }
    let mut walked = 0usize;
    loop {
        if walked >= budget {
            return requeue(state, job, 0, issues, comments);
        }
        let got = match cursor.as_deref() {
            // A URL GitHub gave us, checked against the configured host
            // before it is dialled — an installation token must never
            // leave the host it was minted for.
            Some(url) => app.next_page(&installation, url)?,
            None => app.issues_page(&installation, &full_name, 100, 1)?,
        };
        match got {
            Page::Data { body, next } => {
                let (n, imported) = import_issues(state, &repo.id, &body)?;
                issues += n;
                for (issue_id, number) in imported {
                    comments += import_comments(
                        state,
                        &app,
                        &installation,
                        &full_name,
                        &repo.id,
                        &issue_id,
                        number,
                    )?;
                }
                walked += 1;
                match next {
                    Some(url) => {
                        imports::set_cursor(&state.db, &repo.id, "issues", &url)?;
                        cursor = Some(url);
                    }
                    None => {
                        imports::set_cursor(&state.db, &repo.id, "issues", "done")?;
                        return Ok(ImportOutcome::Done { issues, comments });
                    }
                }
            }
            Page::RateLimited { retry_after_secs } => {
                return requeue(state, job, retry_after_secs, issues, comments);
            }
            Page::Refused { status, body } => return Err(refusal(status, &body)),
        }
    }
}

/// GitHub's milestones. Titles are unique per repository on both sides,
/// so a re-run finds them already there and leaves them alone.
fn import_milestones(state: &SharedState, repo_id: &str, body: &str) -> Result<(), String> {
    let items: Vec<serde_json::Value> =
        serde_json::from_str(body).map_err(|e| format!("milestones: {e}"))?;
    for m in items {
        let (Some(number), Some(title)) = (m["number"].as_i64(), m["title"].as_str()) else {
            continue;
        };
        if let Err(e) = stratum_control::issues::put_milestone(
            &state.db,
            repo_id,
            number as i32,
            title,
            m["description"].as_str().unwrap_or_default(),
            if m["state"].as_str() == Some("closed") {
                "closed"
            } else {
                "open"
            },
            m["due_on"].as_str().map(|s| iso_ms(Some(s))),
        ) {
            eprintln!("weft: import milestone {title}: {e}");
        }
    }
    Ok(())
}

/// What a refusal from GitHub means, said in words an operator can act
/// on rather than as a status code.
fn refusal(status: u16, body: &str) -> String {
    if body.contains("Resource not accessible by integration") {
        return "the GitHub App installation cannot read issues on this repository. \
                It needs the `issues: read` permission — the import has stopped rather \
                than reporting an empty tracker, because those look identical from here"
            .into();
    }
    format!("GitHub refused the import ({status}): {body}")
}

/// Report that there is more to do, having recorded progress.
///
/// Rate limiting is **not** a failure: the work done so far is written
/// and the remainder is somebody's later. Failing the job would lose the
/// cursor's meaning and make a large import look broken every time it hit
/// the budget it was always going to hit.
///
/// This no longer enqueues the follow-up itself. The caller does, once
/// this job's row is no longer active — see [`spawn`], where the reason
/// that order is not a preference is written down.
fn requeue(
    _state: &SharedState,
    _job: &jobs::Job,
    retry_after_secs: u64,
    issues: usize,
    comments: usize,
) -> Result<ImportOutcome, String> {
    Ok(ImportOutcome::More {
        issues,
        comments,
        retry_after_secs,
    })
}

/// GitHub's labels, mapped onto ours.
///
/// The hex is kept as `origin_color` and the *rendered* colour is a
/// design-system token, because a stored hex is unfixable when the
/// palette changes. Everything imported takes `neutral`: guessing which
/// of seven tokens is "closest" to `#d73a4a` would be inventing a fact,
/// and the project's real colour is right there in the dot.
fn import_labels(state: &SharedState, repo_id: &str, body: &str) -> Result<(), String> {
    let items: Vec<serde_json::Value> =
        serde_json::from_str(body).map_err(|e| format!("labels: {e}"))?;
    for l in items {
        let Some(name) = l["name"].as_str() else {
            continue;
        };
        let hex = l["color"].as_str().unwrap_or_default();
        let desc = l["description"].as_str().unwrap_or_default();
        // Already there from an earlier run: leave it alone rather than
        // overwrite a colour somebody has since chosen here.
        let existing = stratum_control::issues::labels(&state.db, repo_id)?;
        if existing.iter().any(|e| e.name == name) {
            continue;
        }
        if let Err(e) =
            stratum_control::issues::create_label(&state.db, repo_id, name, "neutral", desc)
        {
            eprintln!("weft: import label {name}: {e}");
            continue;
        }
        if let Err(e) =
            stratum_control::issues::set_label_origin_color(&state.db, repo_id, name, hex)
        {
            eprintln!("weft: import label colour {name}: {e}");
        }
    }
    Ok(())
}

/// One page of issues. Returns how many were written and the ids of the
/// ones whose comments still need fetching.
fn import_issues(
    state: &SharedState,
    repo_id: &str,
    body: &str,
) -> Result<(usize, Vec<(String, i32)>), String> {
    let items: Vec<serde_json::Value> =
        serde_json::from_str(body).map_err(|e| format!("issues: {e}"))?;
    let mut written = 0;
    let mut with_comments = Vec::new();
    for it in items {
        // **Pull requests come back from this endpoint too**, marked
        // only by this object. They are imported as issues carrying
        // their origin URL — reconstructing patchsets from a PR whose
        // head branch was deleted is a research project, and the docs
        // say so rather than letting it be discovered.
        let number = match it["number"].as_i64() {
            Some(n) => n as i32,
            None => continue,
        };
        let issue = ImportedIssue {
            number,
            title: it["title"].as_str().unwrap_or_default().to_string(),
            body: it["body"].as_str().unwrap_or_default().to_string(),
            state: if it["state"].as_str() == Some("closed") {
                "closed".into()
            } else {
                "open".into()
            },
            // `user` is null for a deleted GitHub account. Not an error:
            // it is what the API sends, and unwrapping it here would
            // panic on somebody's real export.
            author_login: it["user"]["login"].as_str().map(str::to_string),
            created_at: iso_ms(it["created_at"].as_str()),
            updated_at: iso_ms(it["updated_at"].as_str()),
            closed_at: it["closed_at"].as_str().map(|s| iso_ms(Some(s))),
            origin_url: it["html_url"].as_str().unwrap_or_default().to_string(),
            labels: it["labels"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let id = imports::put_issue(&state.db, repo_id, &issue)?;
        // **The milestone link.** GitHub sends it as an object on the
        // issue, and the number is the only stable half of it — titles
        // are editable, ids are theirs. Milestones are imported in the
        // phase before this one, so the row it names is already here.
        //
        // A number that resolves to nothing is left unset rather than
        // failing the issue: a milestone deleted upstream still has
        // issues pointing at it, and refusing those would lose the issue
        // to keep the pointer. Nothing linked them at all until now,
        // which made every imported milestone an orphan row.
        if let Some(number) = it["milestone"]["number"].as_i64() {
            match stratum_control::issues::milestone_id_by_number(&state.db, repo_id, number as i32)
            {
                Ok(Some(mid)) => {
                    if let Err(e) = stratum_control::issues::set_issue_milestone(
                        &state.db,
                        repo_id,
                        issue.number,
                        Some(&mid),
                    ) {
                        eprintln!("weft: import milestone link #{}: {e}", issue.number);
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("weft: import milestone lookup #{}: {e}", issue.number),
            }
        }
        if !issue.labels.is_empty() {
            let _ = stratum_control::issues::set_labels(
                &state.db,
                repo_id,
                issue.number,
                &issue.labels,
            );
        }
        written += 1;
        if it["comments"].as_i64().unwrap_or(0) > 0 {
            with_comments.push((id, number));
        }
    }
    Ok((written, with_comments))
}

/// Every comment on one issue, in the order they were said.
///
/// Paged, and the pages are followed rather than counted. A conversation
/// longer than a hundred comments is exactly the kind a migrating project
/// most wants to keep, and it is also the one an importer that reads only
/// the first page silently truncates.
fn import_comments(
    state: &SharedState,
    app: &crate::mirror::origin::GithubApp,
    installation: &str,
    full_name: &str,
    repo_id: &str,
    issue_id: &str,
    number: i32,
) -> Result<usize, String> {
    let mut written = 0usize;
    let mut page: u32 = 1;
    loop {
        let got = app.comments_page(installation, full_name, number as i64, 100, page)?;
        let (body, next) = match got {
            Page::Data { body, next } => (body, next),
            // The caller's loop re-enqueues; a partial conversation is
            // completed on the next run because the issue's own row is
            // already written and the comment ids are stable.
            Page::RateLimited { .. } => return Ok(written),
            Page::Refused { status, body } => return Err(refusal(status, &body)),
        };
        let items: Vec<serde_json::Value> =
            serde_json::from_str(&body).map_err(|e| format!("comments: {e}"))?;
        for c in items {
            let comment = ImportedComment {
                body: c["body"].as_str().unwrap_or_default().to_string(),
                author_login: c["user"]["login"].as_str().map(str::to_string),
                created_at: iso_ms(c["created_at"].as_str()),
                updated_at: iso_ms(c["updated_at"].as_str()),
                origin_url: c["html_url"].as_str().unwrap_or_default().to_string(),
            };
            imports::put_comment(&state.db, repo_id, issue_id, number, &comment)?;
            written += 1;
        }
        match next {
            Some(_) => page += 1,
            None => return Ok(written),
        }
    }
}

/// An ISO-8601 timestamp as epoch milliseconds.
///
/// Deliberately tolerant: an unparseable or absent date becomes 0 rather
/// than failing the import. A wrong timestamp on one issue is a cosmetic
/// defect; a refused import over one malformed field is a project that
/// cannot move.
fn iso_ms(s: Option<&str>) -> i64 {
    let Some(s) = s else { return 0 };
    chrono_lite(s).unwrap_or(0)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` without a date library.
fn chrono_lite(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let days = stratum_control::contribs::days_from_civil(y as i32, mo as u32, d as u32) as i64;
    Some(((days * 86_400) + h * 3600 + mi * 60 + sec) * 1000)
}
