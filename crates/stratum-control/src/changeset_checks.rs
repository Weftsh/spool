//! Verdicts a composed run reaches, kept where only the changeset can
//! read them.
//!
//! This is `check_runs` for changesets, and it is a second table rather
//! than more rows in that one for a reason that is easy to miss and
//! expensive to get wrong. A workflow file that says
//! `on: [change, changeset]` produces two runs at the same commit with
//! the *same* check name — `ci / test` from the change's own run, and
//! `ci / test` from the composed run of the changeset it belongs to.
//! Mirrored into `check_runs` they would collide on name, and the
//! per-change land gate — which reads by name — would take the composed
//! verdict as the member's own. A change would then be held, or
//! released, by a build of a combination it is only one part of.
//!
//! So these rows reach exactly one reader: [`crate::changesets::composed_gate`]
//! and the changeset view behind it. Nothing else joins to them.
//!
//! Rows are keyed by `external_id` — the job's id, or the run's id for a
//! run that settled without a job — so one job keeps one row however
//! many times it reports, the same identity `checks::upsert` uses.
//! `composition` is carried on every row rather than looked up through
//! the run, because the whole read is "the rows for the composition the
//! changeset is at *now*", and a run made against an older composition
//! must not answer it.

use crate::db::ControlDb;
use crate::ids::{ulid, valid_id};

/// One composed verdict, as the gate and the changeset view read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesetCheck {
    pub id: String,
    pub changeset_id: String,
    pub composition: String,
    /// The member repository whose run produced it — the "where to go"
    /// half of every sentence the gate writes.
    pub repo_id: String,
    pub run_id: String,
    pub external_id: String,
    /// `<workflow> / <cell>`, exactly as `check_runs` names a job.
    pub name: String,
    /// `queued`, `running`, `passing`, `failing`, `cancelled`, `skipped`.
    pub state: String,
    pub detail_url: Option<String>,
}

/// What a composed verdict is on the way in.
#[derive(Debug, Clone)]
pub struct NewChangesetCheck<'a> {
    pub changeset_id: &'a str,
    pub composition: &'a str,
    pub repo_id: &'a str,
    pub run_id: &'a str,
    pub external_id: &'a str,
    pub name: &'a str,
    pub state: &'a str,
    pub detail_url: Option<&'a str>,
}

const COLS: &str =
    "id, changeset_id, composition, repo_id, run_id, external_id, name, state, detail_url";

fn row_to_check(r: &postgres::Row) -> ChangesetCheck {
    ChangesetCheck {
        id: r.get("id"),
        changeset_id: r.get("changeset_id"),
        composition: r.get("composition"),
        repo_id: r.get("repo_id"),
        run_id: r.get("run_id"),
        external_id: r.get("external_id"),
        name: r.get("name"),
        state: r.get("state"),
        detail_url: r.get("detail_url"),
    }
}

/// Write a composed verdict, or move the one this job already has.
///
/// One statement, on the unique `external_id`, so a job reporting from
/// two places at once cannot leave two rows: the mirror is called from
/// every transition a job makes and several of those can overlap — a
/// cancellation racing a finish is the ordinary one.
///
/// Everything except the identity is overwritten, `composition`
/// included, because the run's own composition is the truth and a row
/// that disagreed with it would be a verdict filed under a combination
/// that never produced it.
pub fn upsert(db: &ControlDb, check: &NewChangesetCheck) -> Result<ChangesetCheck, String> {
    let id = ulid();
    let row = db
        .lock()
        .query_one(
            &format!(
                "INSERT INTO changeset_checks \
                   (id, changeset_id, composition, repo_id, run_id, external_id, \
                    name, state, detail_url) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
                 ON CONFLICT (external_id) DO UPDATE SET \
                   changeset_id = EXCLUDED.changeset_id, \
                   composition = EXCLUDED.composition, \
                   repo_id = EXCLUDED.repo_id, \
                   run_id = EXCLUDED.run_id, \
                   name = EXCLUDED.name, \
                   state = EXCLUDED.state, \
                   detail_url = EXCLUDED.detail_url, \
                   updated_at = now() \
                 RETURNING {COLS}"
            ),
            &[
                &id,
                &check.changeset_id,
                &check.composition,
                &check.repo_id,
                &check.run_id,
                &check.external_id,
                &check.name,
                &check.state,
                &check.detail_url,
            ],
        )
        .map_err(|e| format!("upsert changeset check: {e}"))?;
    Ok(row_to_check(&row))
}

/// The current verdict from each check for one composition, ordered the
/// way the changeset view prints them: by repository, then by name.
///
/// One row per `(repo_id, name)`, the newest — the same rule
/// [`crate::checks::latest_for_commit`] applies to `check_runs`, and for
/// the same reason plus one of its own. A composition can be built
/// twice: cancel it by moving a member, move the member back, and the
/// combination is current again with a *new* run against it. The old
/// run's row is still filed under that composition, and it is
/// `cancelled` — so a read that returned both would block the gate on
/// the history of a build that has already been replaced.
///
/// Ordering by `repo_id` rather than by the repository's name keeps this
/// module free of a join it has no other reason to make; the caller
/// holds the names and re-orders by them when it renders. What matters
/// here is that the order is *stable*, so two reads of an unchanged
/// changeset do not shuffle its checks.
pub fn for_composition(
    db: &ControlDb,
    changeset_id: &str,
    composition: &str,
) -> Result<Vec<ChangesetCheck>, String> {
    if !valid_id(changeset_id) {
        return Ok(Vec::new());
    }
    let rows = db
        .lock()
        .query(
            &format!(
                "SELECT DISTINCT ON (repo_id, name) {COLS} FROM changeset_checks \
                 WHERE changeset_id = $1 AND composition = $2 \
                 ORDER BY repo_id, name, created_at DESC, id DESC"
            ),
            &[&changeset_id, &composition],
        )
        .map_err(|e| format!("changeset checks for composition: {e}"))?;
    Ok(rows.iter().map(row_to_check).collect())
}

/// Remove one verdict by the id of the thing that produced it.
///
/// The mirror image of `checks::delete_external`, and it exists for the
/// same one caller: approving a fork member's workflows replaces the
/// `blocked` placeholder run with a real one, and the placeholder's row
/// — keyed on the **run**, where the real jobs are keyed on **jobs** —
/// would otherwise stay queued forever and hold the composed gate. The
/// row also goes by cascade when the run is deleted; this is for the
/// caller that wants to be sure it is gone before it triggers again.
pub fn delete_external(db: &ControlDb, external_id: &str) -> Result<bool, String> {
    db.lock()
        .execute(
            "DELETE FROM changeset_checks WHERE external_id = $1",
            &[&external_id],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("delete changeset check: {e}"))
}
