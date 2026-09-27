//! The authorship walker: which commits are whose, worked out in the
//! background and never on the push path.
//!
//! **Never inline.** A first push of a ten-thousand-commit history would
//! otherwise be paid for by the person pushing it, in a request that has
//! already done the only work it owes them. The push path's standing
//! discipline is that nothing optional happens inside it, so this
//! enqueues beside `compactor::enqueue` at every fan-out site and the
//! pusher goes home.
//!
//! **Bounded, and honest about the bound (I13).** A job visits at most
//! [`max_visits`] commits. History past that is *dropped, not carried*,
//! and the job's result says `truncated`. That is the deliberate choice
//! between the three available failure modes:
//!
//! * carrying a partial walk across jobs means resuming from a frontier,
//!   and a merge whose parents straddle the boundary is then walked
//!   twice — a square that is greener than the person's work, which
//!   nobody can explain and nobody can correct;
//! * refusing to advance means the same over-large repository is walked
//!   from scratch on every job, forever, and never counts anything;
//! * dropping the tail undercounts one enormous first import, visibly,
//!   with a knob (`STRATUM_CONTRIB_MAX_VISITS`) to raise for a migration
//!   that needs it.
//!
//! Undercounting an import that says so is the only one of those three a
//! person can act on.
//!
//! The pure parts — [`plan_walk`], [`classify`], [`author_email`],
//! [`author_day`] — are separated from the impure ones on purpose, and
//! not only for the coverage gate. Nearly every rule worth arguing about
//! (whose commit is this, on what day, does an unproved address count)
//! is decidable from a string and a lookup result, and a rule you can
//! only test by pushing to a running server is a rule that gets tested
//! once.

use crate::app::SharedState;
use crate::review::change_id::trailers;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;
use stratum_control::contribs::{self, Advance, Applied, Counted};
use stratum_control::jobs;
use stratum_engine::objwrite::{parse_commit, parse_tag_target, OBJ_COMMIT, OBJ_TAG};

/// The default per-job visit ceiling. Twenty thousand commits is more
/// than the entire history of most repositories and a few minutes of
/// object reads at worst.
pub const DEFAULT_MAX_VISITS: usize = 20_000;

/// Addresses that name an agent rather than a person.
///
/// **A list we will have to grow, and it is the right shape anyway.**
/// The durable answer is `users.kind = 'agent'` — an address belonging
/// to an agent account names an agent wherever it appears — and
/// [`stratum_control::contribs::Author::agent`] already carries that.
/// Nothing in the product can *set* that column yet, so until it can,
/// this list carries the one address that matters in practice. Both
/// sources feed the same decision; neither is a second mechanism.
///
/// Lowercased, because [`author_email`] lowercases what it extracts.
const AGENT_ADDRESSES: &[&str] = &["noreply@anthropic.com"];

/// A day number a commit could plausibly claim. Author timestamps are
/// attacker-controlled text: without a bound, one commit dated year
/// 500000 becomes an `i32` overflow or a square on a page nobody can
/// scroll to. Roughly ±8000 years around the epoch.
const DAY_BOUND: i64 = 3_000_000;

fn max_visits() -> usize {
    std::env::var("STRATUM_CONTRIB_MAX_VISITS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_VISITS)
}

/// Enqueue an authorship walk after an accepted write, remembering who
/// pushed it — that is what `pushed-by` attribution is derived from
/// later, and it is knowable only here.
pub fn enqueue(state: &SharedState, org_id: &str, repo_id: &str, pushed_by: Option<&str>) {
    if let Err(e) = contribs::enqueue(&state.db, org_id, repo_id, pushed_by) {
        eprintln!("weft: contrib enqueue: {e}");
    }
}

pub fn spawn(state: SharedState) {
    let poll = super::env_secs("STRATUM_CONTRIB_POLL_SECS", 5);
    if poll == 0 {
        return;
    }
    // From the environment, never a literal. A hard-coded lease is
    // exactly how long a crashed node's work stays stuck, and it is not
    // a number this file can know: it depends on the size of the
    // histories this deployment holds.
    let lease_ms = super::lease_ms("STRATUM_CONTRIB_LEASE_SECS", 600);
    tokio::spawn(async move {
        loop {
            let claimed = {
                let db = state.db.clone();
                tokio::task::spawn_blocking(move || jobs::claim(&db, "contrib", lease_ms))
                    .await
                    .unwrap_or_else(|e| Err(format!("join: {e}")))
            };
            match claimed {
                Ok(Some(job)) => match run_one(&state, &job).await {
                    Ok(o) => {
                        let _ = jobs::complete(&state.db, &job.id, Some(&o.describe()));
                        // A lost race means somebody else moved the
                        // frontier under us and we wrote nothing. Their
                        // walk did not know who pushed *this* time, so
                        // the work is not done — try again.
                        if o.applied == Applied::LostRace {
                            if let Some(repo_id) = &job.repo_id {
                                enqueue(&state, &job.org_id, repo_id, pushed_by(&job).as_deref());
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("weft: contribution walk failed: {e}");
                        let _ = jobs::fail(&state.db, &job.id, &e);
                    }
                },
                Ok(None) => tokio::time::sleep(Duration::from_secs(poll)).await,
                Err(e) => {
                    eprintln!("weft: contrib claim: {e}");
                    tokio::time::sleep(Duration::from_secs(poll)).await;
                }
            }
        }
    });
}

/// The principal that pushed, out of the job payload. A payload we
/// cannot read is the same as no pusher: attribution falls back to
/// proved addresses, which is the safe direction.
pub fn pushed_by(job: &jobs::Job) -> Option<String> {
    let payload = job.payload.as_deref()?;
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    v.get("pushed_by")?.as_str().map(str::to_string)
}

// ---------------------------------------------------------------------
// The pure half.
// ---------------------------------------------------------------------

/// What a walk should do, worked out from the refs and the frontier
/// alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// The oids to walk from, in ref order, deduped.
    pub starts: Vec<String>,
    /// Where each ref's cursor should end up if the walk completes.
    pub advances: Vec<Advance>,
    /// Every oid the walk must stop at.
    pub stops: BTreeSet<String>,
}

/// Plan a walk: which tips are new, where to stop, and what the cursor
/// should say afterwards.
///
/// The stop set is the union of **every** ref's recorded frontier, not
/// just the frontier of the ref being walked, and that is the one
/// subtlety in this function. Branches share history: walking `release`
/// down to its own old tip would re-count every commit it inherited from
/// `main`, which was counted when `main` was walked. Stopping at all
/// known tips is `git rev-list new ^every-old-tip`, and it is what makes
/// "incremental" mean the same thing as "each commit once".
///
/// A ref that has vanished keeps its cursor row. The row costs nothing
/// and its oid is still a true statement about history we have already
/// counted — dropping it would make a deleted-and-recreated branch
/// re-count its whole history.
pub fn plan_walk(tips: &[(String, String)], seen: &BTreeMap<String, String>) -> Plan {
    let mut plan = Plan {
        stops: seen.values().cloned().collect(),
        ..Plan::default()
    };
    let mut started: HashSet<&str> = HashSet::new();
    for (reference, oid) in tips {
        let from = seen.get(reference);
        if from == Some(oid) {
            continue; // Already read down to this tip.
        }
        if started.insert(oid.as_str()) {
            plan.starts.push(oid.clone());
        }
        plan.advances.push(Advance {
            reference: reference.clone(),
            from: from.cloned(),
            to: oid.clone(),
        });
    }
    plan
}

/// The accounts a commit could be credited to, already looked up.
///
/// A struct of resolved answers rather than a database handle, so the
/// rule below is decidable from data and testable without one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Candidates<'a> {
    /// One of the commit's co-authors is an **agent**, identified by
    /// its address.
    ///
    /// By address and never by the presence of the trailer.
    /// `Co-authored-by` is GitHub's standard pair-programming trailer:
    /// their UI writes it, `git commit --trailer` writes it, and every
    /// mob-programming team on earth uses it. Treating the trailer
    /// itself as an agent signal would take the collaborative work a
    /// maintainer is proudest of straight off their graph, silently, on
    /// the day they migrate. See [`AGENT_ADDRESSES`].
    pub agent_assisted: bool,
    /// The account whose **verified** address the author line carries.
    pub author: Option<&'a str>,
    /// The person who pushed these commits here, if a person did.
    pub pusher: Option<&'a str>,
}

/// Who a commit counts for, and on what grounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    /// An agent had a hand in it, so it is nobody's square. Kept apart
    /// from [`Attribution::Unattributed`] because the two are the same
    /// outcome for different reasons, and a job that has to be debugged
    /// six months from now needs to be able to tell them apart.
    AgentAssisted,
    /// The author address is a proved address on this account.
    EmailVerified(String),
    /// Nobody has proved the author address, but we watched this person
    /// push the commit, which is a fact about our own server.
    PushedBy(String),
    /// Counts for nothing at all.
    Unattributed,
}

impl Attribution {
    /// The account to credit, or `None`.
    pub fn user_id(&self) -> Option<&str> {
        match self {
            Attribution::EmailVerified(u) | Attribution::PushedBy(u) => Some(u),
            Attribution::AgentAssisted | Attribution::Unattributed => None,
        }
    }
}

/// Decide who a commit counts for.
///
/// This is the anti-gaming rule, and it is short on purpose. Anybody can
/// put anybody's address in `git config user.email` and push the result
/// to a repository they own; if an unproved match coloured a square,
/// every contribution graph on the platform would be worth exactly
/// nothing. So there are two grounds and no others:
///
/// * **email-verified** — the author address is one the account holder
///   proved they can read;
/// * **pushed-by** — we do not know the address, but the credential that
///   pushed the commit was this person's, which is a fact about our own
///   server rather than a claim in a file.
///
/// Everything else counts for nothing. Commit signatures (GPG, SSH) are
/// deliberately out of scope: a signature proves a key held a commit,
/// not that the key's holder is the account, and half-implementing that
/// would publish a "verified" badge that means less than it looks like.
///
/// An agent co-author outranks both, and the outcome is that the commit
/// counts for nobody: a graph that goes green because somebody let an
/// agent run overnight is measuring the agent, and it is the person the
/// square is about. Crediting the agent instead would need an account to
/// credit it to, and the address in one of these trailers is usually a
/// no-reply mailbox nobody here owns.
///
/// Note what this is **not**: the presence of a `Co-authored-by`
/// trailer. That trailer is how two humans record pair programming —
/// GitHub's own UI writes it — and reading it as an agent signal would
/// erase exactly the collaborative work a maintainer is proudest of, on
/// the day they migrate, with no error and no failing test. Only a
/// co-author whose *address* we recognise counts here, and an
/// unrecognised one leaves the commit its author's.
pub fn classify(c: &Candidates) -> Attribution {
    if c.agent_assisted {
        return Attribution::AgentAssisted;
    }
    if let Some(author) = c.author {
        return Attribution::EmailVerified(author.to_string());
    }
    match c.pusher {
        Some(p) => Attribution::PushedBy(p.to_string()),
        None => Attribution::Unattributed,
    }
}

/// The address out of a git identity line, lowercased.
///
/// `A U Thor <a@b.c> 1700000000 +0100`. The angle brackets are the only
/// reliable delimiter — a name may contain spaces, quotes, more or less
/// anything — so the address is what lies between the *first* `<` and
/// the next `>`, and a line without both is not an identity line.
pub fn author_email(line: &str) -> Option<String> {
    let start = line.find('<')? + 1;
    let end = line[start..].find('>')? + start;
    let addr = line[start..end].trim();
    (!addr.is_empty()).then(|| addr.to_lowercase())
}

/// The day a commit's author would say they wrote it on.
///
/// **In the author's own timezone**, which is the whole point: somebody
/// committing at 23:30 in Auckland wrote it on that day, and rendering
/// it on the previous one in UTC puts their work on a square they did
/// not work on. The trailing `+HHMM` on the identity line is exactly
/// that offset, so the arithmetic is `(seconds + offset) / 86400` —
/// floor division, because a negative day number is a real date.
///
/// Bounded: the timestamp is text a stranger wrote, and an absurd one is
/// refused rather than folded into a square nobody can reach.
pub fn author_day(line: &str) -> Option<i32> {
    let end = line.rfind('>')?;
    let mut fields = line[end + 1..].split_whitespace();
    let secs: i64 = fields.next()?.parse().ok()?;
    let tz = fields.next().unwrap_or("+0000");
    let offset = tz_offset_secs(tz)?;
    let day = secs.checked_add(offset)?.div_euclid(86_400);
    (-DAY_BOUND..=DAY_BOUND)
        .contains(&day)
        .then_some(day as i32)
}

/// `+HHMM` / `-HHMM` as seconds. Anything else is not an offset.
fn tz_offset_secs(tz: &str) -> Option<i64> {
    let b = tz.as_bytes();
    if b.len() != 5 || !b[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let sign = match b[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hours: i64 = tz[1..3].parse().ok()?;
    let mins: i64 = tz[3..5].parse().ok()?;
    // A real offset is under a day and its minutes are minutes.
    (hours < 24 && mins < 60).then_some(sign * (hours * 3600 + mins * 60))
}

/// Every co-author address a commit names, lowercased.
///
/// **All of them.** A commit may carry four co-authors — that is what
/// the trailer is for — and judging it on whichever one is last would
/// make the verdict depend on the order somebody wrote the lines in.
///
/// Reuses [`trailers`] rather than growing a second trailer grammar,
/// which also means the key match is case-insensitive: GitHub writes
/// `Co-authored-by:`, and an exact match would find none of it.
pub fn coauthor_emails(message: &str) -> Vec<String> {
    trailers(message, "Co-authored-by")
        .into_iter()
        .filter_map(author_email)
        .collect()
}

/// Whether an address names an agent rather than a person.
///
/// The safe direction is the whole point. Under-detecting an agent
/// costs a slightly generous square; over-detecting erases somebody's
/// real work, and those are not comparable. So this answers `true` only
/// for an address we actually recognise, and everything else is a
/// person.
pub fn is_agent_address(address: &str) -> bool {
    AGENT_ADDRESSES.contains(&address)
}

/// Whether an agent had a hand in a commit, from its co-authors.
///
/// Pure, and called by the walker rather than reimplemented beside it —
/// which is the difference between a test that covers this rule and a
/// test that covers a copy of it. Two sources feed one decision: the
/// hard-coded [`AGENT_ADDRESSES`], and the addresses of accounts marked
/// `users.kind = 'agent'`, which the caller has already resolved.
///
/// **No co-author it recognises means no agent**, and the commit stays
/// its author's. A `Co-authored-by` trailer on its own says two people
/// worked together, which is the opposite of a reason to discard the
/// work.
pub fn agent_assisted(coauthors: &[String], agent_accounts: &BTreeSet<String>) -> bool {
    coauthors
        .iter()
        .any(|a| is_agent_address(a) || agent_accounts.contains(a))
}

// ---------------------------------------------------------------------
// The impure half.
// ---------------------------------------------------------------------

/// What one job did, for the job's `result` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Commits read, which is what the bound counts. Not the same as
    /// the number that produced a row: a commit whose author timestamp
    /// is unparseable is visited, costs budget, and counts for nothing.
    pub visited: usize,
    pub counted: i32,
    /// The bound was hit and the tail of history was dropped. Recorded
    /// rather than logged, because the person who has to decide whether
    /// to raise the knob is reading the job row.
    pub truncated: bool,
    pub applied: Applied,
}

impl Outcome {
    fn describe(&self) -> String {
        format!(
            "{:?} visited={} counted={}{}",
            self.applied,
            self.visited,
            self.counted,
            if self.truncated { " TRUNCATED" } else { "" }
        )
    }
}

/// One commit as the walk saw it, before anything is resolved.
struct Raw {
    author: Option<String>,
    /// Every co-author, kept raw so the agent decision can be made
    /// against resolved accounts as well as the known-address list.
    coauthors: Vec<String>,
    day: i32,
}

pub async fn run_one(state: &SharedState, job: &jobs::Job) -> Result<Outcome, String> {
    let Some(repo_id) = job.repo_id.clone() else {
        return Err("contribution job without repo".into());
    };
    let Some(repo) = stratum_control::registry::repo_by_id(&state.db, &job.org_id, &repo_id)?
    else {
        // Deleted since. Its rows went with it.
        return Ok(Outcome {
            visited: 0,
            counted: 0,
            truncated: false,
            applied: Applied::Committed,
        });
    };
    let seen = contribs::cursors(&state.db, &repo.id)?;
    let cap = max_visits();
    let prefix = repo.prefix().as_str().to_string();

    // The walk itself touches no database: it reads objects and returns
    // strings. Every lookup happens after it, once per distinct address
    // rather than once per commit.
    let (raws, advances, truncated, visits) =
        crate::api::reads::with_reader(state, prefix, move |reader| {
            let tips = reader.all_refs()?;
            let Plan {
                starts,
                advances,
                stops,
            } = plan_walk(&tips, &seen);
            let mut pending: Vec<String> = starts;
            let mut visited: HashSet<String> = HashSet::new();
            let mut raws: Vec<Raw> = Vec::new();
            let mut truncated = false;
            while let Some(oid) = pending.pop() {
                if stops.contains(&oid) || visited.contains(&oid) {
                    continue;
                }
                if visited.len() >= cap {
                    truncated = true;
                    break;
                }
                visited.insert(oid.clone());
                let (kind, data) = reader.object(&oid)?;
                // A ref may point at an annotated tag, not a commit —
                // git/git has a thousand of them — and reading a tag as
                // a commit ended the whole walk with "commit without
                // tree". Step through to what the tag names; anything
                // else a ref can point at (a tree, a blob) has no
                // authorship and is simply not walked.
                match kind {
                    OBJ_COMMIT => {}
                    OBJ_TAG => {
                        if let Some(target) = parse_tag_target(&data) {
                            pending.push(target);
                        }
                        continue;
                    }
                    _ => continue,
                }
                let commit = parse_commit(&data)?;
                if let Some(day) = author_day(&commit.author) {
                    raws.push(Raw {
                        author: author_email(&commit.author),
                        coauthors: coauthor_emails(&commit.message),
                        day,
                    });
                }
                pending.extend(commit.parents);
            }
            Ok((raws, advances, truncated, visited.len()))
        })
        .await?;

    // One lookup for every distinct address in the whole walk.
    let mut addresses: BTreeSet<String> = BTreeSet::new();
    for r in &raws {
        addresses.extend(r.author.iter().cloned());
        // Co-authors are looked up too, so an address belonging to an
        // `agent` account is recognised even when it is not on the
        // hard-coded list.
        addresses.extend(r.coauthors.iter().cloned());
    }
    let resolved =
        contribs::resolve_authors(&state.db, &addresses.into_iter().collect::<Vec<_>>())?;
    let pusher = pushed_by(job);
    // The addresses of accounts the platform itself marks as agents.
    let agent_accounts: BTreeSet<String> = resolved
        .iter()
        .filter(|(_, a)| a.agent)
        .map(|(address, _)| address.clone())
        .collect();

    let mut totals: HashMap<(String, i32), i32> = HashMap::new();
    for r in &raws {
        let author = r.author.as_deref().and_then(|a| resolved.get(a));
        let who = classify(&Candidates {
            agent_assisted: agent_assisted(&r.coauthors, &agent_accounts),
            author: author.map(|a| a.user_id.as_str()),
            pusher: pusher.as_deref(),
        });
        if let Some(user_id) = who.user_id() {
            *totals.entry((user_id.to_string(), r.day)).or_default() += 1;
        }
    }

    let counts: Vec<Counted> = totals
        .into_iter()
        .map(|((user_id, day), count)| Counted {
            user_id,
            day,
            count,
        })
        .collect();
    let counted = counts.iter().map(|c| c.count).sum();
    let applied = contribs::apply(&state.db, &repo.id, repo.public, &counts, &advances)?;
    Ok(Outcome {
        visited: visits,
        counted,
        truncated,
        applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tips(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(r, o)| (r.to_string(), o.to_string()))
            .collect()
    }

    fn seen(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(r, o)| (r.to_string(), o.to_string()))
            .collect()
    }

    #[test]
    fn a_first_walk_starts_at_every_tip_and_stops_nowhere() {
        let plan = plan_walk(
            &tips(&[("refs/heads/main", "aa"), ("refs/tags/v1", "bb")]),
            &BTreeMap::new(),
        );
        assert_eq!(plan.starts, vec!["aa".to_string(), "bb".to_string()]);
        assert!(plan.stops.is_empty());
        assert_eq!(
            plan.advances,
            vec![
                Advance {
                    reference: "refs/heads/main".into(),
                    from: None,
                    to: "aa".into()
                },
                Advance {
                    reference: "refs/tags/v1".into(),
                    from: None,
                    to: "bb".into()
                },
            ]
        );
    }

    #[test]
    fn a_tip_that_has_not_moved_is_not_walked_again() {
        let plan = plan_walk(
            &tips(&[("refs/heads/main", "aa"), ("refs/heads/dev", "cc")]),
            &seen(&[("refs/heads/main", "aa"), ("refs/heads/dev", "bb")]),
        );
        assert_eq!(plan.starts, vec!["cc".to_string()]);
        assert_eq!(
            plan.advances,
            vec![Advance {
                reference: "refs/heads/dev".into(),
                from: Some("bb".into()),
                to: "cc".into()
            }]
        );
    }

    /// The rule that makes "incremental" mean "each commit once".
    #[test]
    fn a_branch_stops_at_every_other_branchs_frontier_not_only_its_own() {
        let plan = plan_walk(
            &tips(&[("refs/heads/main", "m2"), ("refs/heads/rel", "r2")]),
            &seen(&[("refs/heads/main", "m1"), ("refs/heads/rel", "r1")]),
        );
        // Walking `rel` must stop at `m1` as well as `r1`, or every
        // commit `rel` inherited from `main` is counted a second time.
        assert!(plan.stops.contains("m1"), "{:?}", plan.stops);
        assert!(plan.stops.contains("r1"), "{:?}", plan.stops);
    }

    #[test]
    fn a_vanished_ref_keeps_its_frontier() {
        // `dev` is gone from the tips but its cursor still stops the
        // walk: a branch deleted and recreated must not re-count its
        // history.
        let plan = plan_walk(
            &tips(&[("refs/heads/main", "m2")]),
            &seen(&[("refs/heads/main", "m1"), ("refs/heads/dev", "d9")]),
        );
        assert!(plan.stops.contains("d9"));
        assert_eq!(plan.advances.len(), 1);
    }

    #[test]
    fn two_refs_at_one_oid_are_walked_once_and_both_advance() {
        let plan = plan_walk(
            &tips(&[("refs/heads/main", "aa"), ("refs/heads/trunk", "aa")]),
            &BTreeMap::new(),
        );
        assert_eq!(plan.starts, vec!["aa".to_string()]);
        assert_eq!(plan.advances.len(), 2);
    }

    #[test]
    fn an_unproved_address_counts_for_nothing() {
        // The whole anti-gaming rule in one assertion: no proved
        // address, nobody pushed it here, nothing counts.
        assert_eq!(
            classify(&Candidates::default()),
            Attribution::Unattributed,
            "an unproved address must colour no square"
        );
        assert_eq!(classify(&Candidates::default()).user_id(), None);
    }

    #[test]
    fn a_proved_address_beats_the_pusher_and_the_pusher_beats_nothing() {
        assert_eq!(
            classify(&Candidates {
                author: Some("u_author"),
                pusher: Some("u_pusher"),
                ..Candidates::default()
            }),
            Attribution::EmailVerified("u_author".into())
        );
        assert_eq!(
            classify(&Candidates {
                pusher: Some("u_pusher"),
                ..Candidates::default()
            }),
            Attribution::PushedBy("u_pusher".into())
        );
    }

    #[test]
    fn an_agents_commit_never_inflates_a_humans_graph() {
        // Every ground a commit could otherwise count on, and it still
        // counts for nobody — which is the point: a person who let an
        // agent run overnight has not had a productive night.
        let out = classify(&Candidates {
            agent_assisted: true,
            author: Some("u_human"),
            pusher: Some("u_human"),
        });
        assert_eq!(out, Attribution::AgentAssisted);
        assert_eq!(out.user_id(), None);
        // And the same commit without the trailer is ordinary work.
        assert_eq!(
            classify(&Candidates {
                agent_assisted: false,
                author: Some("u_human"),
                pusher: Some("u_human"),
            })
            .user_id(),
            Some("u_human")
        );
    }

    #[test]
    fn an_address_is_what_the_angle_brackets_hold() {
        assert_eq!(
            author_email("A U Thor <A@B.Co> 1700000000 +0100").as_deref(),
            Some("a@b.co"),
            "lowercased, so two spellings are one address"
        );
        // Names are hostile text: spaces, quotes, brackets in the name.
        assert_eq!(
            author_email("\"Thor, A <fake@evil>\" <real@b.co> 1 +0000").as_deref(),
            Some("fake@evil"),
            "the first bracket pair wins — and it is the name's, which \
             is why the address is proved against user_emails and never \
             trusted on its own"
        );
        assert_eq!(author_email("nobody 1700000000 +0000"), None);
        assert_eq!(author_email("nobody <unclosed 1 +0000"), None);
        assert_eq!(author_email("nobody <> 1 +0000"), None);
    }

    #[test]
    fn a_day_is_the_authors_own_day_not_utcs() {
        // 1699961400 is 2023-11-14T11:30Z. The same instant is already
        // 00:30 on the 15th in Auckland, and still the 14th in Hawaii —
        // three answers for one timestamp, and the author's is the one
        // that belongs on their graph.
        assert_eq!(author_day("A <a@b.c> 1699961400 +0000"), Some(19_675));
        assert_eq!(
            author_day("A <a@b.c> 1699961400 +1300"),
            Some(19_676),
            "somebody committing after midnight in Auckland wrote it on \
             that day, not on the previous one in UTC"
        );
        assert_eq!(author_day("A <a@b.c> 1699961400 -1000"), Some(19_675));
        // Before the epoch is a real date, not an underflow: floor
        // division, so one second before is the day before.
        assert_eq!(author_day("A <a@b.c> -1 +0000"), Some(-1));
        assert_eq!(author_day("A <a@b.c> 0 +0000"), Some(0));
    }

    #[test]
    fn a_hostile_timestamp_is_refused_rather_than_rendered() {
        for line in [
            "A <a@b.c> 999999999999999 +0000", // year 31 million
            "A <a@b.c> -999999999999999 +0000",
            "A <a@b.c> notanumber +0000",
            "A <a@b.c>",
            "A <a@b.c> 1700000000 +9900", // 99 hours is not an offset
            "A <a@b.c> 1700000000 +0099", // 99 minutes is not an offset
            "A <a@b.c> 1700000000 Z",
            "A <a@b.c> 1700000000 0100", // no sign
            "no brackets at all 1 +0000",
        ] {
            assert_eq!(author_day(line), None, "{line:?}");
        }
        // A missing offset is UTC, which is what git writes when it has
        // none — that one is not hostile and must still count.
        assert_eq!(author_day("A <a@b.c> 1700000000"), Some(19_675));
    }

    #[test]
    fn every_co_author_is_read_in_every_spelling() {
        // The spelling GitHub's UI writes is the first one here, and it
        // is what will be in imported history.
        let msg = "fix it\n\nprose\n\nChange-Id: Iabc12345\n\
                   Co-authored-by: Ada <ada@x.test>\n\
                   Co-Authored-By: Claude <noreply@anthropic.com>\n\
                   co-authored-by: Bob <bob@x.test>\n";
        assert_eq!(
            coauthor_emails(msg),
            vec!["ada@x.test", "noreply@anthropic.com", "bob@x.test"],
            "a spelling was missed, or only the nearest trailer was read"
        );
        // Not in the trailer block: prose that mentions one is not one.
        let prose = "fix it\n\nCo-authored-by: X <x@y.z> was suggested\n\nno trailers here";
        assert!(coauthor_emails(prose).is_empty());
        assert!(coauthor_emails("fix it").is_empty());
        assert!(coauthor_emails("fix it\n\nCo-authored-by: nobody\n").is_empty());
    }

    /// The bug this exists for: `Co-authored-by` is GitHub's standard
    /// pair-programming trailer, and reading its *presence* as an agent
    /// signal takes a maintainer's collaborative work off their graph on
    /// the day they migrate — silently, with no error and no failing
    /// test.
    #[test]
    fn a_human_pair_programmed_commit_counts_for_its_author() {
        let msg = "mob session\n\nCo-authored-by: Bob <bob@x.test>\n\
                   Co-authored-by: Cleo <cleo@x.test>\n";
        let coauthors = coauthor_emails(msg);
        assert_eq!(coauthors.len(), 2);
        let agent = agent_assisted(&coauthors, &BTreeSet::new());
        assert!(
            !agent,
            "two humans were read as agents because their commit \
             carried the trailer at all"
        );
        assert_eq!(
            classify(&Candidates {
                agent_assisted: agent,
                author: Some("u_ada"),
                pusher: None,
            }),
            Attribution::EmailVerified("u_ada".into()),
            "a commit two humans wrote together counted for neither"
        );
    }

    #[test]
    fn an_agent_is_named_by_its_address_wherever_it_sits() {
        assert!(is_agent_address("noreply@anthropic.com"));
        assert!(!is_agent_address("bob@x.test"));
        assert!(!is_agent_address(""));
        // First, last, or in the middle of a list of humans — the
        // verdict must not depend on the order the lines were written,
        // which is exactly what reading only the nearest trailer did.
        for msg in [
            "t\n\nCo-authored-by: Claude <noreply@anthropic.com>\n\
             Co-authored-by: Bob <bob@x.test>\n",
            "t\n\nCo-authored-by: Bob <bob@x.test>\n\
             Co-authored-by: Claude <noreply@anthropic.com>\n",
            "t\n\nCo-authored-by: Bob <bob@x.test>\n\
             Co-Authored-By: Claude <noreply@anthropic.com>\n\
             Co-authored-by: Cleo <cleo@x.test>\n",
        ] {
            assert!(
                agent_assisted(&coauthor_emails(msg), &BTreeSet::new()),
                "{msg:?}"
            );
        }
        // The second source: an address belonging to an account the
        // platform marks as an agent, which is what will replace the
        // hard-coded list once `users.kind` can be set.
        let known: BTreeSet<String> = ["bot@acme.test".to_string()].into_iter().collect();
        let msg = "t\n\nCo-authored-by: Helper <bot@acme.test>\n";
        assert!(agent_assisted(&coauthor_emails(msg), &known));
        assert!(
            !agent_assisted(&coauthor_emails(msg), &BTreeSet::new()),
            "an unrecognised co-author must leave the commit its \
             author's — over-detecting erases real work"
        );
        // No co-authors at all is no agent.
        assert!(!agent_assisted(&[], &known));
    }

    #[test]
    fn the_pusher_comes_out_of_the_payload_or_does_not() {
        let job = |payload: Option<&str>| jobs::Job {
            id: "j".into(),
            org_id: "o".into(),
            repo_id: Some("r".into()),
            kind: "contrib".into(),
            state: "queued".into(),
            payload: payload.map(str::to_string),
            result: None,
            error: None,
            attempts: 0,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(
            pushed_by(&job(Some(r#"{"pushed_by":"u_1"}"#))).as_deref(),
            Some("u_1")
        );
        // Anything unreadable falls back to "no pusher", which loses a
        // square rather than crediting the wrong person.
        assert_eq!(pushed_by(&job(None)), None);
        assert_eq!(pushed_by(&job(Some("not json"))), None);
        assert_eq!(pushed_by(&job(Some(r#"{"pushed_by":7}"#))), None);
        assert_eq!(pushed_by(&job(Some(r#"{"other":"u_1"}"#))), None);
    }

    #[test]
    fn an_outcome_says_what_happened_including_the_bound() {
        let o = Outcome {
            visited: 3,
            counted: 2,
            truncated: false,
            applied: Applied::Committed,
        };
        assert_eq!(o.describe(), "Committed visited=3 counted=2");
        let o = Outcome {
            truncated: true,
            applied: Applied::LostRace,
            ..o
        };
        assert_eq!(o.describe(), "LostRace visited=3 counted=2 TRUNCATED");
    }
}
