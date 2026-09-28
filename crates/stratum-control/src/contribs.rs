//! Who wrote what: one row per (person, repository, day), derived from
//! commit data we already store, never from activity on this platform —
//! a person counts as a contributor because a commit exists, not because
//! somebody used the website.
//!
//! **Raw rows, never a rendered total.** A repository's contributor list
//! is summed from them at read time, so a rewalk after an address is
//! verified or an account is disabled needs no second bookkeeping.
//!
//! The frontier that makes the walk incremental lives in
//! `contrib_cursor`, and [`apply`] advances it by compare-and-swap for
//! the reason I9 does the same thing to a manifest: two workers walking
//! one repository at once must not both be believed. The loser of the
//! race writes nothing at all, and its job is re-enqueued.

use crate::db::ControlDb;
use crate::ids::now_ms;
use serde::Serialize;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------
// Days. Pure arithmetic, no dependency, unit-tested below.
// ---------------------------------------------------------------------

/// Days since the epoch → `(year, month, day)`.
///
/// Howard Hinnant's civil-from-days, shifted to a March-based year so
/// the leap day is the last day of the internal year and needs no
/// special case. Verified against the boundaries a date bug actually
/// lands on: epoch, leap days, and century years.
pub fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `(year, month, day)` → days since the epoch. The exact inverse of
/// [`civil_from_days`] for every date this code can produce.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i32 - 719_468
}

/// `YYYY-MM-DD` for a day number.
pub fn iso_of_day(day: i32) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A day number for `YYYY-MM-DD`, or a sentence naming what was wrong.
///
/// Strict about shape *and* about range: `2024-02-30` parses as digits
/// and is not a date, and accepting it would render a square on the 1st
/// of March that nobody can explain. Round-tripping through
/// [`civil_from_days`] is the check, because it is the same arithmetic
/// the rest of the module trusts.
pub fn day_from_iso(s: &str) -> Result<i32, String> {
    let bad = || format!("{s:?} is not a date — use YYYY-MM-DD");
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return Err(bad());
    }
    if !b
        .iter()
        .enumerate()
        .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return Err(bad());
    }
    let y: i32 = s[0..4].parse().map_err(|_| bad())?;
    let m: u32 = s[5..7].parse().map_err(|_| bad())?;
    let d: u32 = s[8..10].parse().map_err(|_| bad())?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    let day = days_from_civil(y, m, d);
    if civil_from_days(day) != (y, m, d) {
        return Err(bad());
    }
    Ok(day)
}

// ---------------------------------------------------------------------
// The frontier.
// ---------------------------------------------------------------------

/// Every ref cursor this repository has, `ref` → the oid the walker
/// last read down to.
pub fn cursors(db: &ControlDb, repo_id: &str) -> Result<BTreeMap<String, String>, String> {
    db.lock()
        .query(
            "SELECT ref, last_seen FROM contrib_cursor WHERE repo_id = $1",
            &[&repo_id.to_string()],
        )
        .map_err(|e| format!("read contrib cursor: {e}"))
        .map(|rows| rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// One day's worth of one person's work, as the walker counted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counted {
    pub user_id: String,
    pub day: i32,
    pub count: i32,
}

/// A cursor move: the ref, what it said when the walk started (`None`
/// when there was no row), and where it should point now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    pub reference: String,
    pub from: Option<String>,
    pub to: String,
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Counts written, frontier advanced.
    Committed,
    /// Somebody else moved the frontier while this walk was reading the
    /// object store. Nothing was written — not the counts, not the
    /// cursor — and the caller re-enqueues.
    LostRace,
}

/// Write a completed walk: the counts and the frontier move, together,
/// or neither.
///
/// Two workers can hold two jobs for one repository — the partial unique
/// index that deduplicates `compact` does not cover this job kind, and
/// two pushes by two people are not interchangeable jobs anyway — so
/// both will happily walk the same commits from the same frontier.
/// Adding both results would double every square, and a contribution
/// count nobody can explain is worse than a missing one.
///
/// So the write is a compare-and-swap on the frontier, for the same
/// reason I9 makes a manifest move by CAS: the loser must write
/// **nothing**, not most of it. Two mechanisms, and both are needed:
///
/// * the repository's own row is locked first, so two `apply` calls for
///   one repository cannot interleave at all — every check below is then
///   still true when the writes happen a few statements later;
/// * every cursor is checked against the value the walk started from
///   *before* any row is written, so a loser returns having issued
///   nothing but `SELECT`s.
///
/// Checking as it went and undoing on failure would be the obvious
/// shape and the wrong one: a partly-advanced frontier strands every
/// commit between the two tips, uncounted, forever — the walk will never
/// look there again.
pub fn apply(
    db: &ControlDb,
    repo_id: &str,
    counts: &[Counted],
    advances: &[Advance],
) -> Result<Applied, String> {
    let repo = repo_id.to_string();
    let counts = counts.to_vec();
    let advances = advances.to_vec();
    let now = now_ms();
    db.lock()
        .transaction(move |tx| {
            // Serialize per repository. `repos` always has this row —
            // the job would not exist otherwise — where `contrib_cursor`
            // may have none at all on a first walk, which is precisely
            // the case two workers race on.
            let Some(row) = tx.query_opt(
                "SELECT fork_parent_id FROM repos WHERE id = $1 FOR UPDATE",
                &[&repo],
            )?
            else {
                // Deleted and purged since the walk began. Nothing to
                // attribute it to.
                return Ok(Applied::LostRace);
            };
            // **A fork credits nobody.**
            //
            // `contributions` is keyed `(user_id, repo_id, day)`, so the
            // same commit counted in two repositories is two rows and
            // the graph sums them. A fork starts life holding the whole
            // of its parent's history — so walking one re-credited every
            // author of that history, a second time, for work they did
            // once in a repository they may never have heard of. Fork a
            // project with ten thousand commits and every contributor's
            // graph gains ten thousand squares; fork it again and they
            // gain them again. Nobody has to be malicious for that to
            // happen, and anybody could do it deliberately.
            //
            // So a fork's walk advances its cursors and writes no
            // counts. The cursors still move, because the alternative is
            // a job that finds the same frontier for ever; and the
            // contribution a fork exists to carry is counted the moment
            // it **lands upstream**, in the upstream repository, which is
            // exactly where the credit belongs and is what
            // `a_change_landed_from_a_fork_credits_the_contributor…`
            // pins.
            let is_fork: Option<String> = row.get(0);
            let counts: &[Counted] = if is_fork.is_some() { &[] } else { &counts };
            for a in &advances {
                let held: Option<String> = tx
                    .query_opt(
                        "SELECT last_seen FROM contrib_cursor WHERE repo_id = $1 AND ref = $2",
                        &[&repo, &a.reference],
                    )?
                    .map(|r| r.get(0));
                if held != a.from {
                    return Ok(Applied::LostRace);
                }
            }
            for a in &advances {
                tx.execute(
                    "INSERT INTO contrib_cursor (repo_id, ref, last_seen, updated_at) \
                     VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (repo_id, ref) \
                     DO UPDATE SET last_seen = EXCLUDED.last_seen, \
                                   updated_at = EXCLUDED.updated_at",
                    &[&repo, &a.reference, &a.to, &now],
                )?;
            }
            for c in counts {
                tx.execute(
                    // `public` is a column the hosted edition reads;
                    // nothing here is public, so it is always false.
                    "INSERT INTO contributions (user_id, repo_id, day, count, public) \
                     VALUES ($1, $2, $3, $4, FALSE) \
                     ON CONFLICT (user_id, repo_id, day) \
                     DO UPDATE SET count = contributions.count + EXCLUDED.count",
                    &[&c.user_id, &repo, &c.day, &c.count],
                )?;
            }
            Ok(Applied::Committed)
        })
        .map_err(|e| format!("apply contributions: {e}"))
}

/// One account a commit address resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub user_id: String,
    /// Whether the account is a machine principal (`users.kind`).
    ///
    /// This is how an agent is identified once agent accounts can be
    /// created: an address that belongs to an `agent` account names an
    /// agent, wherever it appears. Until then the worker's own list of
    /// known agent addresses carries it, and this is the seam that
    /// replaces the list rather than a second mechanism beside it.
    pub agent: bool,
}

/// Which of these commit addresses belong to an account.
///
/// **Only a proved address answers.** `verified_at IS NOT NULL` is in
/// the query rather than in a caller's memory, for the same reason
/// [`crate::profiles::user_for_author`] puts it there: anybody can write
/// any string into `git config user.email`, so an unproved address is a
/// claim and a claim must colour no squares. This is the batch form of
/// that lookup — a walk resolves every distinct address once instead of
/// once per commit — and it is deliberately the *same predicate*, index
/// and all (`user_emails_verified`).
pub fn resolve_authors(
    db: &ControlDb,
    addresses: &[String],
) -> Result<BTreeMap<String, Author>, String> {
    if addresses.is_empty() {
        return Ok(BTreeMap::new());
    }
    let addrs: Vec<String> = addresses
        .iter()
        .map(|a| crate::users::normalize_email(a))
        .collect();
    db.lock()
        .query(
            "SELECT e.address, e.user_id, (u.kind = 'agent') AS agent \
             FROM user_emails e JOIN users u ON u.id = e.user_id \
             WHERE e.address = ANY($1) AND e.verified_at IS NOT NULL \
               AND u.disabled_at IS NULL",
            &[&addrs],
        )
        .map_err(|e| format!("resolve authors: {e}"))
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    (
                        r.get("address"),
                        Author {
                            user_id: r.get("user_id"),
                            agent: r.get("agent"),
                        },
                    )
                })
                .collect()
        })
}

/// Enqueue an authorship walk for a repository, remembering who pushed.
///
/// Not `jobs::enqueue_unique`: the partial unique index behind it covers
/// only the sweep kinds, and — more to the point — two contribution jobs
/// are not interchangeable the way two compactions are. Each carries the
/// principal that pushed it, which is what lets a commit whose author
/// address nobody has proved still count for the person who pushed it.
/// Collapsing two pushes by two people into one job would silently
/// credit one of them with the other's work.
///
/// What *is* collapsed is a repeat push by the same person while their
/// job is still waiting: the walk reads the repository's current state
/// when it runs, so a second identical row is pure waste. The check and
/// the insert are one statement — two nodes can still both insert, and
/// that is safe, because [`apply`] refuses the loser.
pub fn enqueue(
    db: &ControlDb,
    org_id: &str,
    repo_id: &str,
    pushed_by: Option<&str>,
) -> Result<bool, String> {
    let payload = pushed_by.map(|u| format!("{{\"pushed_by\":{}}}", json_string(u)));
    let inserted = db
        .lock()
        .execute(
            // `IS NOT DISTINCT FROM`, never `=`: a mirror sync has no
            // pusher, so the payload is NULL, and `NULL = NULL` is NULL
            // rather than true. With `=` the dedup would silently never
            // fire on exactly the path a migrating user's whole history
            // arrives on, which is the one that enqueues most often.
            "INSERT INTO jobs (id, org_id, repo_id, kind, payload, created_at, updated_at) \
             SELECT $1, $2, $3, 'contrib', $4, $5, $5 \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM jobs WHERE repo_id = $3 AND kind = 'contrib' \
                   AND state = 'queued' AND payload IS NOT DISTINCT FROM $4)",
            &[
                &crate::ids::ulid(),
                &org_id.to_string(),
                &repo_id.to_string(),
                &payload,
                &now_ms(),
            ],
        )
        .map_err(|e| format!("enqueue contribution walk: {e}"))?;
    Ok(inserted == 1)
}

/// A JSON string literal. The payload is two fixed keys and one opaque
/// id, so building it by hand is honest — but an id is still a value
/// from outside this function, and a hand-built JSON document that does
/// not escape is how a value becomes syntax.
fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Forget everything the walker has read for these repositories and
/// queue the walks again.
///
/// **This exists because proving an address is retroactive news.** The
/// walker attributes at walk time — `contributions` carries no address,
/// so a read has nothing to re-check — which means somebody who mirrors
/// twenty repositories, sees an empty graph, and *then* adds the address
/// they have committed with for a decade would watch it stay empty. That
/// is the exact flow this feature exists to serve, so verifying an
/// address has to send the walker back over the history.
///
/// **Deleting the rows is not optional, and it is the whole reason this
/// is one function rather than two calls.** [`apply`] adds to a day's
/// count. Resetting a cursor without clearing what that cursor's walk
/// already counted would re-walk the same commits and add them a second
/// time — every square doubled, by a repair. Both happen in one
/// transaction per repository, so a failure halfway leaves a repository
/// either fully re-queued or entirely untouched, never counted twice.
///
/// Bounded by the caller's choice of repositories rather than by a
/// platform-wide sweep; see [`repos_for_rewalk`] for the set that covers
/// the migration case without letting one address verification re-walk
/// every repository on the installation.
///
/// Returns how many repositories were queued.
pub fn rewalk(db: &ControlDb, repo_ids: &[String]) -> Result<usize, String> {
    let mut queued = 0;
    for repo_id in repo_ids {
        let repo = repo_id.clone();
        let org_id: Option<String> = db
            .lock()
            .query_opt("SELECT org_id FROM repos WHERE id = $1", &[&repo])
            .map_err(|e| format!("rewalk lookup: {e}"))?
            .map(|r| r.get(0));
        let Some(org_id) = org_id else {
            continue; // Deleted since the caller listed it.
        };
        let repo = repo_id.clone();
        db.lock()
            .transaction(move |tx| {
                tx.execute("DELETE FROM contributions WHERE repo_id = $1", &[&repo])?;
                tx.execute("DELETE FROM contrib_cursor WHERE repo_id = $1", &[&repo])?;
                Ok(())
            })
            .map_err(|e| format!("rewalk reset: {e}"))?;
        // Enqueued after the reset commits, and with no pusher: a
        // re-walk is not somebody's push, so only proved addresses may
        // claim any of it — which is the whole point of doing it.
        if enqueue(db, &org_id, repo_id, None)? {
            queued += 1;
        }
    }
    Ok(queued)
}

/// The repositories to re-walk when this person proves a new address.
///
/// Every active repository in a namespace they belong to — which is
/// where a migrating maintainer's history actually lands, since they
/// mirror their projects into their own namespace or their org's.
///
/// **Not every repository on the installation**, though that is the only
/// strictly complete answer: an address can appear in a commit anywhere,
/// and we cannot know where without walking. Two things make the
/// complete answer the wrong one. It is O(all history) per verification,
/// and an account may prove up to `profiles::MAX_EMAILS` addresses — so
/// the honest description of a platform-wide sweep is "any user can
/// re-walk the whole installation ten times", which is a denial of
/// service with a friendly name. The narrow set covers the case the
/// feature is for; a wider one is an operator's decision, not a
/// side-effect of somebody clicking a link in their mail.
pub fn repos_for_rewalk(db: &ControlDb, user_id: &str) -> Result<Vec<String>, String> {
    let orgs = crate::members::orgs_of(db, user_id)?;
    if orgs.is_empty() {
        return Ok(Vec::new());
    }
    db.lock()
        .query(
            // Forks excluded, for the reason `apply` gives at length: a
            // fork holds its parent's whole history and crediting it
            // would count that history a second time. `apply` refuses
            // them too — this is the cheaper half, so a rewalk does not
            // enqueue work that is going to write nothing.
            "SELECT id FROM repos WHERE org_id = ANY($1) AND state = 'active' \
             AND fork_parent_id IS NULL ORDER BY id",
            &[&orgs],
        )
        .map_err(|e| format!("rewalk repos: {e}"))
        .map(|rows| rows.iter().map(|r| r.get(0)).collect())
}

// ---------------------------------------------------------------------
// Reading.
// ---------------------------------------------------------------------

/// One person's share of one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contributor {
    pub user_id: String,
    pub handle: String,
    pub commits: i64,
    /// Epoch ms of this person's most recent contributing day here.
    ///
    /// Derived from the day number, so it is midnight at the start of
    /// that date — the date the author would say they wrote it on, not
    /// an instant anything happened at. A client renders it as a date;
    /// rendering it as a time would invent a precision the walker never
    /// had, because `contributions` stores a day and nothing finer.
    pub last_at: i64,
}

/// The most people one request may ask for (I13).
///
/// A repository with five thousand contributors is a page nobody can
/// render, and the rail this feeds shows a couple of dozen faces at
/// most. A hundred is comfortably past anything a UI displays and
/// comfortably short of a query that hurts.
pub const MAX_CONTRIBUTORS: i32 = 100;

/// Who has contributed to this repository, most first.
///
/// **This makes no visibility decision, and must not start.** The caller
/// has already resolved `repo_id` — through the same authorization it
/// uses for every other repo-scoped read — and by the time we are here
/// the answer to "may this reader see this repository" is yes. If a
/// future caller needs a check, it belongs where the repository is
/// resolved, next to every other one, not hidden in an aggregate.
///
/// `limit` is clamped rather than refused: a large `limit` is a caller
/// asking a perfectly good question and wanting more of the answer than
/// we serve.
/// Handing them the first hundred is what they wanted, near enough, and
/// a 400 on `?limit=500` would break an avatar rail over a number nobody
/// typed. The floor is 1 for a different reason: `?limit=0` reaches
/// Postgres as `LIMIT 0`, which answers "nobody has contributed here" —
/// a wrong fact, indistinguishable from a true one.
///
/// The handle comes from `orgs`, not `users.handle`, for the reason
/// [`crate::profiles::by_handle`] gives: `orgs` is where the name is
/// unique and case-folded, and `users.handle` is a nullable
/// back-reference that predates it.
///
/// Both joins are inner, and that is the answer to the account that is
/// no longer there. Disabling is this platform's delete (see
/// [`crate::users::set_disabled`]) and it takes the person's page down,
/// so listing them would render a face linking to a 404; a hard
/// delete takes the contribution rows with it by cascade and never
/// reaches this query at all. Either way the row is *dropped*, never
/// coalesced to an empty handle — an unnamed avatar is a link nobody can
/// follow and a total nobody can attribute, which is worse than a total
/// that is honestly one contributor short.
pub fn contributors(db: &ControlDb, repo_id: &str, limit: i32) -> Result<Vec<Contributor>, String> {
    let limit = limit.clamp(1, MAX_CONTRIBUTORS);
    db.lock()
        .query(
            // `lower(o.name)` is the tiebreak, not `o.name`: two people
            // on the same count must come back in the same order every
            // time, and a raw-name sort under a C collation would put
            // `Zeb` before `ada`. Case-folded is both friendlier and
            // still a total order, because `orgs_name_folded` makes two
            // handles differing only in case impossible.
            "SELECT c.user_id, o.name AS handle, \
                    SUM(c.count)::BIGINT AS commits, \
                    MAX(c.day)::INT AS last_day \
             FROM contributions c \
             JOIN users u ON u.id = c.user_id AND u.disabled_at IS NULL \
             JOIN orgs o ON o.owner_user_id = u.id AND o.kind = 'personal' \
             WHERE c.repo_id = $1 \
             GROUP BY c.user_id, o.name \
             ORDER BY commits DESC, lower(o.name) ASC \
             LIMIT $2",
            &[&repo_id.to_string(), &i64::from(limit)],
        )
        .map_err(|e| format!("read contributors: {e}"))
        .map(|rows| {
            rows.iter()
                .map(|r| Contributor {
                    user_id: r.get("user_id"),
                    handle: r.get("handle"),
                    commits: r.get("commits"),
                    // Widened before the multiply: a day number is an
                    // i32 and 86_400_000 is not, so doing this in i32
                    // would overflow about 24 days after the epoch.
                    last_at: i64::from(r.get::<_, i32>("last_day")) * 86_400_000,
                })
                .collect()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    fn person(db: &ControlDb, handle: &str, email: &str) -> (String, String) {
        let u = crate::users::create(db, email, handle, Some("a long enough password")).unwrap();
        crate::usertokens::mark_verified(db, &u.id).unwrap();
        let ns = registry::create_personal_namespace(db, &u.id, handle, None).unwrap();
        (u.id, ns.id)
    }

    /// Every commit counted against this repository, whoever wrote it.
    fn total(db: &ControlDb, repo_id: &str) -> i64 {
        contributors(db, repo_id, MAX_CONTRIBUTORS)
            .unwrap()
            .iter()
            .map(|c| c.commits)
            .sum()
    }

    fn repo(db: &ControlDb, org_id: &str, name: &str) -> registry::Repo {
        registry::create_repo(
            db,
            org_id,
            &NewRepo {
                description: None,
                name,
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap()
    }

    /// An account with a namespace, and so a name to be listed under.
    /// The cheap form of [`person`]: no password, because hashing one a
    /// hundred and one times for the clamp test below is a minute of CPU
    /// spent proving nothing this module claims.
    fn named(db: &ControlDb, handle: &str) -> String {
        let u = crate::users::create(db, &format!("{handle}@example.com"), handle, None).unwrap();
        registry::create_personal_namespace(db, &u.id, handle, None).unwrap();
        u.id
    }

    /// Write these counts against a repository, from an empty frontier.
    /// The ref name is unique per call so a second write in one test is
    /// a fresh CAS rather than a lost race.
    fn write(db: &ControlDb, repo_id: &str, reference: &str, counts: &[Counted]) {
        assert_eq!(
            apply(
                db,
                repo_id,
                counts,
                &[Advance {
                    reference: reference.into(),
                    from: None,
                    to: "aa".into(),
                }],
            )
            .unwrap(),
            Applied::Committed
        );
    }

    fn handles(got: &[Contributor]) -> Vec<&str> {
        got.iter().map(|c| c.handle.as_str()).collect()
    }

    fn counted(user_id: &str, day: i32, count: i32) -> Counted {
        Counted {
            user_id: user_id.into(),
            day,
            count,
        }
    }

    /// The anti-gaming rule, at the layer that enforces it.
    #[test]
    fn only_a_proved_address_resolves_to_an_account() {
        let db = db("contribs_resolve");
        let (uid, _) = person(&db, "ada", "ada@example.com");
        // A second address, claimed but not proved.
        assert!(matches!(
            crate::profiles::add_email(&db, &uid, "ada@old.example").unwrap(),
            crate::profiles::AddEmail::Added(_)
        ));

        let got = resolve_authors(
            &db,
            &[
                "ada@example.com".into(),
                "ada@old.example".into(),
                "nobody@example.invalid".into(),
            ],
        )
        .unwrap();
        assert_eq!(got.get("ada@example.com").map(|a| &a.user_id), Some(&uid));
        assert!(
            !got["ada@example.com"].agent,
            "a person is not a machine principal"
        );
        assert_eq!(
            got.get("ada@old.example"),
            None,
            "an address nobody has proved resolved to an account — \
             anybody can write that string into a commit"
        );
        assert_eq!(got.get("nobody@example.invalid"), None);
        // Case and surrounding space are the same address, because a
        // commit is hand-written text.
        let same = resolve_authors(&db, &["  Ada@Example.COM ".into()]).unwrap();
        assert_eq!(same.get("ada@example.com").map(|a| &a.user_id), Some(&uid));
        // Nothing in, nothing out — and no query.
        assert!(resolve_authors(&db, &[]).unwrap().is_empty());

        // An account marked as a machine principal answers so, which is
        // the seam that will replace the worker's hard-coded list of
        // agent addresses the moment `users.kind` becomes settable.
        // Written here rather than through a route because there is no
        // route yet — that gap is a finding, not a reason to leave the
        // predicate untested.
        db.lock()
            .execute("UPDATE users SET kind = 'agent' WHERE id = $1", &[&uid])
            .unwrap();
        assert!(
            resolve_authors(&db, &["ada@example.com".into()]).unwrap()["ada@example.com"].agent
        );
    }

    /// The pusherless path is the migration path, and it is the one a
    /// NULL parameter breaks.
    #[test]
    fn a_walk_is_enqueued_with_or_without_a_pusher() {
        let db = db("contribs_enqueue");
        let (uid, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");

        // No pusher: a mirror sync, which is how a decade of somebody
        // else's history arrives. The payload is NULL, and a statement
        // that cannot type a NULL fails only here.
        assert!(enqueue(&db, &org, &r.id, None).unwrap());
        // The same again is waste, and is collapsed.
        assert!(!enqueue(&db, &org, &r.id, None).unwrap());
        // A different pusher is a different job: collapsing them would
        // credit one person with another's work.
        assert!(enqueue(&db, &org, &r.id, Some(&uid)).unwrap());
        assert!(!enqueue(&db, &org, &r.id, Some(&uid)).unwrap());

        let jobs = db
            .lock()
            .query(
                "SELECT payload FROM jobs WHERE repo_id = $1 AND kind = 'contrib' \
                 ORDER BY payload NULLS FIRST",
                &[&r.id],
            )
            .unwrap();
        assert_eq!(jobs.len(), 2, "two distinct pushes, two jobs");
        assert_eq!(jobs[0].get::<_, Option<String>>(0), None);
        assert_eq!(
            jobs[1].get::<_, Option<String>>(0),
            Some(format!("{{\"pushed_by\":\"{uid}\"}}"))
        );
    }

    /// Two workers, one repository, one frontier.
    #[test]
    fn the_loser_of_a_frontier_race_writes_nothing_at_all() {
        let db = db("contribs_cas");
        let (uid, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let head = "refs/heads/main".to_string();

        assert!(cursors(&db, &r.id).unwrap().is_empty());
        // Two workers both plan from an empty frontier.
        let plan = |to: &str| Advance {
            reference: head.clone(),
            from: None,
            to: to.into(),
        };
        assert_eq!(
            apply(&db, &r.id, &[counted(&uid, 100, 3)], &[plan("aa")]).unwrap(),
            Applied::Committed
        );
        // The second one walked the same commits and must write neither
        // its counts nor its cursor: adding both would double every
        // square, which is a number nobody could ever explain.
        assert_eq!(
            apply(&db, &r.id, &[counted(&uid, 100, 3)], &[plan("aa")]).unwrap(),
            Applied::LostRace
        );
        assert_eq!(total(&db, &r.id), 3, "the loser's counts landed anyway");
        assert_eq!(
            cursors(&db, &r.id).unwrap().get(&head).map(String::as_str),
            Some("aa")
        );

        // A stale `from` is the same refusal, and a partial advance is
        // never left behind: the second ref below must not move either.
        let side = "refs/heads/side".to_string();
        let out = apply(
            &db,
            &r.id,
            &[counted(&uid, 100, 9)],
            // The order matters, and it is this way round on purpose:
            // the ref that *would* advance comes first, so an
            // implementation that checked and wrote as it went would
            // have already moved `side` by the time it discovered
            // `head` was stale.
            &[
                Advance {
                    reference: side.clone(),
                    from: None,
                    to: "cc".into(),
                },
                Advance {
                    reference: head.clone(),
                    from: Some("stale".into()),
                    to: "bb".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(out, Applied::LostRace);
        let after = cursors(&db, &r.id).unwrap();
        assert_eq!(after.get(&head).map(String::as_str), Some("aa"));
        assert!(
            !after.contains_key(&side),
            "a losing walk advanced one ref and not the other, stranding \
             every commit between the two tips: {after:?}"
        );
        assert_eq!(total(&db, &r.id), 3);

        // A real second walk, from the frontier it actually read, adds.
        assert_eq!(
            apply(
                &db,
                &r.id,
                &[counted(&uid, 100, 2)],
                &[Advance {
                    reference: head.clone(),
                    from: Some("aa".into()),
                    to: "bb".into(),
                }],
            )
            .unwrap(),
            Applied::Committed
        );
        assert_eq!(total(&db, &r.id), 5);
    }

    /// The repair must not be worse than the thing it repairs.
    #[test]
    fn a_rewalk_clears_what_it_will_recount_rather_than_doubling_it() {
        let db = db("contribs_rewalk");
        let (uid, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let head = "refs/heads/main".to_string();
        apply(
            &db,
            &r.id,
            &[counted(&uid, 400, 3)],
            &[Advance {
                reference: head.clone(),
                from: None,
                to: "aa".into(),
            }],
        )
        .unwrap();
        assert_eq!(total(&db, &r.id), 3);

        // The set a newly proved address sends the walker back over.
        let repos = repos_for_rewalk(&db, &uid).unwrap();
        assert_eq!(repos, vec![r.id.clone()], "her own namespace's repos");
        assert_eq!(rewalk(&db, &repos).unwrap(), 1);

        // Both halves, and the first is the one that matters: the rows
        // are gone, so the re-walk that follows adds to nothing rather
        // than adding to what it is about to recount.
        assert_eq!(
            total(&db, &r.id),
            0,
            "a re-walk left the old counts in place, so recounting the \
             same commits would double every square"
        );
        assert!(
            cursors(&db, &r.id).unwrap().is_empty(),
            "the frontier survived, so the re-walk would read nothing"
        );

        // Replaying the walk lands the same number it started with —
        // the repair is idempotent, which is what makes it safe to run
        // on every address verification.
        apply(
            &db,
            &r.id,
            &[counted(&uid, 400, 3)],
            &[Advance {
                reference: head,
                from: None,
                to: "aa".into(),
            }],
        )
        .unwrap();
        assert_eq!(total(&db, &r.id), 3);

        // Somebody with no namespaces has nothing to re-walk, and a
        // repository deleted between the listing and the reset is
        // skipped rather than failing the repair.
        assert!(repos_for_rewalk(&db, "u_nobody").unwrap().is_empty());
        assert_eq!(rewalk(&db, &["r_gone".to_string()]).unwrap(), 0);
        assert_eq!(rewalk(&db, &[]).unwrap(), 0);
    }

    /// The rail the About panel renders: everyone who worked here, most
    /// first, summed over every day and no other repository.
    #[test]
    fn contributors_are_ranked_by_their_total_across_every_day() {
        let db = db("contribs_contributors");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let widget = repo(&db, &org, "widget");
        let other = repo(&db, &org, "gadget");
        let bob = named(&db, "bob");
        let cid = named(&db, "cid");

        // The counts climb with both the creation order and the
        // alphabet, so the answer is the reverse of either — an
        // unordered aggregate cannot pass this by luck.
        write(
            &db,
            &widget.id,
            "refs/heads/main",
            &[
                counted(&ada, 500, 1),
                counted(&bob, 501, 4),
                counted(&cid, 502, 5),
                counted(&cid, 509, 4),
            ],
        );
        // A neighbouring repository must not add to this one's totals —
        // the whole point of the per-repo query is that it is per repo.
        write(&db, &other.id, "refs/heads/main", &[counted(&ada, 503, 99)]);

        let got = contributors(&db, &widget.id, 10).unwrap();
        assert_eq!(handles(&got), ["cid", "bob", "ada"], "{got:?}");
        assert_eq!(got[0].commits, 9, "days were not summed: {got:?}");
        assert_eq!(got[1].commits, 4);
        assert_eq!(got[2].user_id, ada);
        assert_eq!(got[2].commits, 1, "another repo's work leaked in: {got:?}");

        // `last_at` is the most recent day, and it is a day: midnight at
        // the start of the date, not the first day he worked and not
        // anything derived from the sum.
        assert_eq!(got[0].last_at, 509 * 86_400_000);
        assert_eq!(got[1].last_at, 501 * 86_400_000);
        assert_eq!(iso_of_day(509), "1971-05-25");

        // Asked about a repository nobody has touched, the answer is an
        // empty rail rather than an error.
        let empty = repo(&db, &org, "quiet");
        assert!(contributors(&db, &empty.id, 10).unwrap().is_empty());
    }

    /// Two people on the same count must not swap between page loads.
    #[test]
    fn an_equal_count_is_broken_by_handle_so_the_order_never_moves() {
        let db = db("contribs_contributors_tiebreak");
        let (_, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        // Created in reverse, so anything that leans on insertion order
        // — or on whatever order the aggregate happens to emit — comes
        // back the wrong way round.
        let counts: Vec<Counted> = ["zoe", "mia", "bob", "abe"]
            .iter()
            .map(|h| counted(&named(&db, h), 600, 7))
            .collect();
        write(&db, &r.id, "refs/heads/main", &counts);

        for _ in 0..3 {
            let got = contributors(&db, &r.id, 10).unwrap();
            assert_eq!(
                handles(&got),
                ["abe", "bob", "mia", "zoe"],
                "four identical counts came back in an order the next \
                 page load need not repeat: {got:?}"
            );
        }
    }

    /// The bound, and why it is a clamp.
    #[test]
    fn a_request_for_everybody_is_clamped_rather_than_refused() {
        let db = db("contribs_contributors_clamp");
        let (_, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        // One more than the cap, so the cap is the thing being measured
        // and not the size of the fixture.
        let counts: Vec<Counted> = (0..=MAX_CONTRIBUTORS)
            .map(|i| counted(&named(&db, &format!("u{i:03}")), 700, 1))
            .collect();
        write(&db, &r.id, "refs/heads/main", &counts);

        let n = MAX_CONTRIBUTORS as usize;
        assert_eq!(contributors(&db, &r.id, i32::MAX).unwrap().len(), n);
        assert_eq!(
            contributors(&db, &r.id, MAX_CONTRIBUTORS + 1)
                .unwrap()
                .len(),
            n
        );
        assert_eq!(contributors(&db, &r.id, 3).unwrap().len(), 3);
        // The floor. `LIMIT 0` would answer "nobody has contributed
        // here", which is a false fact that reads exactly like a true
        // one, and a negative limit is an error from Postgres rather
        // than from us.
        assert_eq!(contributors(&db, &r.id, 0).unwrap().len(), 1);
        assert_eq!(contributors(&db, &r.id, -5).unwrap().len(), 1);
    }

    /// An account that is no longer there leaves the rail entirely.
    #[test]
    fn an_account_that_is_gone_is_dropped_rather_than_listed_nameless() {
        let db = db("contribs_contributors_gone");
        let (ada, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");
        let bob = named(&db, "bob");
        // An account with no personal namespace has no name to be
        // listed under. It is a real state, not a hypothetical: an
        // account made by an older `user-create` has none until
        // `admin repair-identities` gives it one.
        let ghost = crate::users::create(&db, "ghost@example.com", "ghost", None)
            .unwrap()
            .id;
        write(
            &db,
            &r.id,
            "refs/heads/main",
            &[
                counted(&ada, 800, 5),
                counted(&bob, 800, 3),
                counted(&ghost, 800, 4),
            ],
        );

        let got = contributors(&db, &r.id, 10).unwrap();
        assert_eq!(
            handles(&got),
            ["ada", "bob"],
            "somebody with no namespace was listed under a name that \
             does not exist: {got:?}"
        );
        assert!(
            !got.iter().any(|c| c.handle.is_empty()),
            "a nameless avatar links nowhere and attributes nothing: {got:?}"
        );

        // Disabling is this platform's delete, and it takes the public
        // profile down — so a listed face would link to a 404.
        crate::users::set_disabled(&db, &bob, true).unwrap();
        assert_eq!(handles(&contributors(&db, &r.id, 10).unwrap()), ["ada"]);

        // A hard delete never reaches this query at all: the rows go
        // with the account by cascade, which is why nothing here has to
        // defend against a `user_id` pointing at nobody. It takes two
        // statements to demonstrate because the schema will not let a
        // person be deleted while an address still names them —
        // `user_emails` references `users` with no cascade at all. That
        // is the same decision `set_disabled` documents, enforced by the
        // database: an account is disabled, not erased.
        db.lock()
            .execute("DELETE FROM user_emails WHERE user_id = $1", &[&ghost])
            .unwrap();
        db.lock()
            .execute("DELETE FROM users WHERE id = $1", &[&ghost])
            .unwrap();
        let left: i64 = db
            .lock()
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM contributions WHERE user_id = $1",
                &[&ghost],
            )
            .unwrap()
            .get(0);
        assert_eq!(left, 0, "a deleted account's contribution rows outlived it");
    }

    #[test]
    fn day_numbers_and_dates_are_the_same_fact() {
        // The boundaries a date bug actually lands on.
        for (day, iso) in [
            (0, "1970-01-01"),
            (-1, "1969-12-31"),
            (11_016, "2000-02-29"), // a leap year that is also a century
            (19_782, "2024-02-29"),
            (19_783, "2024-03-01"),
            (2_678, "1977-05-02"),
        ] {
            assert_eq!(iso_of_day(day), iso, "day {day}");
            assert_eq!(day_from_iso(iso), Ok(day), "iso {iso}");
        }
        // And the round trip holds over a long stretch, which is what
        // catches an off-by-one that only bites in one month.
        for day in -40_000..40_000 {
            assert_eq!(day_from_iso(&iso_of_day(day)), Ok(day), "day {day}");
        }
    }

    #[test]
    fn a_date_that_is_not_a_date_is_refused_by_name() {
        for s in [
            "2024-02-30", // parses, is not a day
            "1900-02-29", // a century that is not a leap year
            "2024-13-01",
            "2024-00-10",
            "2024-01-00",
            "2024-1-01", // not padded
            "20240101",
            "2024-01-01T00:00:00Z",
            "",
            "abcd-ef-gh",
            "2024-01-0x",
        ] {
            let err = day_from_iso(s).expect_err(s);
            assert!(err.contains("YYYY-MM-DD"), "{s}: {err}");
        }
        // 1900 is the century-not-leap case in the other direction.
        assert_eq!(day_from_iso("1900-03-01"), Ok(days_from_civil(1900, 3, 1)));
    }
}
