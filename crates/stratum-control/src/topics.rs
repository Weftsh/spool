//! Topics: the words a project chooses to be found by.
//!
//! One table, `repo_topics`, and one rule that the schema and this
//! module state twice on purpose: **a topic is lowercase.** The column
//! carries `CHECK (topic = lower(topic))`, so a write of `"Rust"` is
//! refused by PostgreSQL — which surfaces to a user as a 500 about our
//! schema rather than as anything they can act on. Normalising here,
//! before the write, is what makes the CHECK a backstop instead of the
//! error path. `normalize` and the CHECK say the same thing, and the
//! tests prove the pair agree rather than trusting either alone.
//!
//! The set is replaced whole rather than added to one at a time. That
//! is GitHub's shape (`PUT /topics` takes the list) and it is also the
//! only shape a form can implement honestly: an editor holds the whole
//! list on screen, so sending the whole list is what it actually did.
//!
//! Bounds are refusals, never truncation (I13). Twenty topics of
//! thirty-five characters is more than any project has ever needed, and
//! silently dropping the twenty-first would tell somebody their word was
//! saved when it was not.

use crate::db::ControlDb;
use crate::ids::now_ms;

/// How many topics one repository may carry.
///
/// GitHub's limit as well, and for the same reason: a topic list is a
/// handful of words a reader scans, and a repository claiming forty is
/// not describing itself, it is spamming a search index.
pub const MAX_TOPICS: usize = 20;

/// The longest a single topic may be. GitHub allows fifty; thirty-five
/// is what fits a pill in the About rail without ellipsising, and a
/// topic nobody can read is not a topic.
pub const MAX_TOPIC_LEN: usize = 35;

/// A topic as it will be stored, or the reason it cannot be.
///
/// Case is the only thing this *fixes*; everything else it refuses.
/// Lowercasing is safe because `Rust` and `rust` are unambiguously the
/// same word and treating them as two would split every listing. A
/// space or a slash is not a case difference — the caller meant
/// something we cannot guess, so they are told rather than guessed at.
///
/// The shape is deliberately the same one the URL has to survive:
/// a topic ends up in `/explore?topic=…` and in a `WHERE topic = $1`,
/// so ASCII alphanumerics and interior hyphens are the whole alphabet.
/// Refusing everything else is what keeps a topic from ever needing
/// escaping anywhere downstream.
///
/// Rejected, with the reason, rather than dropped: empty, too long,
/// non-ASCII, and anything that starts or ends with a hyphen. The
/// leading-hyphen rule is not cosmetic — `-foo` reads as a flag to
/// every command line a topic might be pasted into.
pub fn normalize(raw: &str) -> Result<String, String> {
    let t = raw.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Err("a topic cannot be empty".to_string());
    }
    if t.chars().count() > MAX_TOPIC_LEN {
        return Err(format!(
            "topic {raw:?} is longer than {MAX_TOPIC_LEN} characters"
        ));
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "topic {raw:?} may hold only letters, digits and hyphens"
        ));
    }
    if t.starts_with('-') || t.ends_with('-') {
        return Err(format!("topic {raw:?} may not start or end with a hyphen"));
    }
    Ok(t)
}

/// Every topic on a repository, alphabetical.
///
/// Alphabetical rather than insertion-ordered because the list is read
/// far more often than it is written, and a set of pills that reorders
/// itself whenever somebody edits one of them is a set nobody can scan
/// twice. `created_at` is still stored; nothing reads it yet, and that
/// is fine — it is the column that lets "recently tagged" exist later
/// without a migration.
pub fn list(db: &ControlDb, repo_id: &str) -> Result<Vec<String>, String> {
    let rows = db
        .lock()
        .query(
            "SELECT topic FROM repo_topics WHERE repo_id = $1 ORDER BY topic",
            &[&repo_id],
        )
        .map_err(|e| format!("topics: {e}"))?;
    Ok(rows.iter().map(|r| r.get("topic")).collect())
}

/// Normalise a caller's list into the set that would be stored.
///
/// Split out from [`set`] because it is the whole of the validation and
/// none of the database: a request can be refused with a 400 before a
/// transaction is opened, and the rules can be tested without a
/// PostgreSQL cluster.
///
/// Duplicates collapse rather than being refused — `["Rust", "rust"]`
/// is somebody typing the same word twice, not an error, and the
/// primary key would collapse them anyway. The count is checked
/// **after** the collapse for exactly that reason: refusing twenty-one
/// entries that are nineteen distinct topics would be a refusal the
/// user cannot see the cause of.
pub fn normalize_all(raw: &[String]) -> Result<Vec<String>, String> {
    // Bound the input before doing per-item work on it. A list of a
    // million strings is refused for being a million strings, not after
    // a million lowercase allocations.
    if raw.len() > MAX_TOPICS * 4 {
        return Err(format!("at most {MAX_TOPICS} topics"));
    }
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        let t = normalize(r)?;
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.len() > MAX_TOPICS {
        return Err(format!(
            "at most {MAX_TOPICS} topics; that list holds {}",
            out.len()
        ));
    }
    out.sort();
    Ok(out)
}

/// Replace a repository's topics with exactly this set.
///
/// Delete-then-insert inside one transaction, so a reader never sees a
/// repository with half its topics. `created_at` is refreshed for a
/// topic that was already there, which is a small lie the alternative
/// does not justify: keeping it would mean a per-row diff to decide
/// which writes to skip, for a column nothing reads.
///
/// Audited, because the topics are a public claim the project makes
/// about itself and "who changed this" is a question that gets asked
/// about public claims.
pub fn set(
    db: &ControlDb,
    repo_id: &str,
    topics: &[String],
    audit: &crate::audit::AuditCtx,
) -> Result<Vec<String>, String> {
    let wanted = normalize_all(topics)?;
    let stored = wanted.clone();
    let now = now_ms();
    db.lock()
        .transaction(move |tx| {
            tx.execute("DELETE FROM repo_topics WHERE repo_id = $1", &[&repo_id])?;
            for t in &stored {
                tx.execute(
                    "INSERT INTO repo_topics (repo_id, topic, created_at) VALUES ($1, $2, $3)",
                    &[&repo_id, t, &now],
                )?;
            }
            let blob = serde_json::json!({ "topics": stored });
            crate::audit::record_tx(tx, audit, Some(repo_id), "repo.topics", Some(&blob))?;
            Ok(())
        })
        .map_err(|e| format!("set topics: {e}"))?;
    Ok(wanted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditCtx;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    /// A repository, and an audit context belonging to a real person —
    /// `audit_log.user_id` references `users(id)`, so a made-up id would
    /// fail the write and take the whole transaction with it.
    fn fixture(db: &ControlDb, hint: &str) -> (registry::Repo, AuditCtx) {
        let u = crate::users::create(
            db,
            &format!("{hint}@example.com"),
            hint,
            Some("a long enough password"),
        )
        .unwrap();
        crate::usertokens::mark_verified(db, &u.id).unwrap();
        let ns = registry::create_personal_namespace(db, &u.id, hint, None).unwrap();
        let repo = registry::create_repo(
            db,
            &ns.id,
            &NewRepo {
                description: Some("a repository"),
                name: "widget",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        let ctx = AuditCtx {
            principal: format!("user:{}", u.id),
            user_id: Some(u.id),
            org_id: ns.id,
        };
        (repo, ctx)
    }

    #[test]
    fn case_is_the_only_thing_normalisation_fixes() {
        assert_eq!(normalize("Rust").unwrap(), "rust");
        assert_eq!(normalize("  GIT  ").unwrap(), "git");
        assert_eq!(normalize("Machine-Learning").unwrap(), "machine-learning");
        assert_eq!(normalize("c99").unwrap(), "c99");
    }

    /// Everything that is not a case difference is refused, with the
    /// reason, rather than mangled into something the caller did not ask
    /// for. A space is not a hyphen and `rust/lang` is not `rustlang`.
    #[test]
    fn anything_that_is_not_a_case_difference_is_refused() {
        for bad in [
            "",
            "   ",
            "two words",
            "rust/lang",
            "rust_lang",
            "rust.lang",
            "rust!",
            "café",
            "日本語",
            "rust\0lang",
            "-rust",
            "rust-",
            "--",
            "rust\nlang",
        ] {
            assert!(
                normalize(bad).is_err(),
                "{bad:?} was accepted as a topic name"
            );
        }
    }

    /// Bounded, and refused rather than truncated (I13). A topic that
    /// came back shorter than it was sent would tell somebody their word
    /// was saved when a different word was.
    #[test]
    fn a_long_topic_is_refused_rather_than_cut_down() {
        let ok = "a".repeat(MAX_TOPIC_LEN);
        assert_eq!(normalize(&ok).unwrap(), ok);
        let too_long = "a".repeat(MAX_TOPIC_LEN + 1);
        let e = normalize(&too_long).unwrap_err();
        assert!(e.contains("longer than"), "{e}");
    }

    #[test]
    fn duplicates_collapse_and_the_count_is_checked_after_they_do() {
        // Nineteen distinct words sent as twenty-two entries is not a
        // list of twenty-two topics, and refusing it would be a refusal
        // the user cannot see the cause of.
        let mut raw: Vec<String> = (0..19).map(|i| format!("topic-{i}")).collect();
        raw.push("Topic-0".into());
        raw.push("TOPIC-1".into());
        raw.push("topic-2".into());
        let out = normalize_all(&raw).unwrap();
        assert_eq!(out.len(), 19, "{out:?}");

        let twenty_one: Vec<String> = (0..MAX_TOPICS + 1).map(|i| format!("t{i}")).collect();
        let e = normalize_all(&twenty_one).unwrap_err();
        assert!(e.contains("at most"), "{e}");

        // And a list too large to be worth normalising at all is refused
        // on its length, before any per-item work.
        let huge: Vec<String> = (0..MAX_TOPICS * 4 + 1).map(|i| format!("t{i}")).collect();
        assert!(normalize_all(&huge).is_err());
    }

    #[test]
    fn one_bad_entry_refuses_the_whole_list() {
        // Not "the good ones were saved". A partial write of a set the
        // caller sent as a whole is the worst of both answers.
        let e = normalize_all(&["rust".into(), "two words".into()]).unwrap_err();
        assert!(e.contains("letters, digits and hyphens"), "{e}");
    }

    /// The schema's CHECK and `normalize` say the same thing, and this
    /// is what proves it rather than assuming it: an uppercase topic put
    /// straight into the table is refused by PostgreSQL, and the same
    /// word through `set` is stored lowercase.
    #[test]
    fn the_column_refuses_what_normalisation_prevents() {
        let db = db("topics_check");
        let (repo, ctx) = fixture(&db, "ada");
        let raw = db.lock().execute(
            "INSERT INTO repo_topics (repo_id, topic, created_at) VALUES ($1, $2, $3)",
            &[&repo.id, &"Rust", &now_ms()],
        );
        assert!(
            raw.is_err(),
            "the CHECK let an uppercase topic in; normalisation is now the only guard"
        );
        let out = set(&db, &repo.id, &["Rust".into()], &ctx).unwrap();
        assert_eq!(out, vec!["rust".to_string()]);
        assert_eq!(list(&db, &repo.id).unwrap(), vec!["rust".to_string()]);
    }

    #[test]
    fn setting_replaces_the_whole_set_and_is_audited() {
        let db = db("topics_set");
        let (repo, ctx) = fixture(&db, "ada");
        set(
            &db,
            &repo.id,
            &["storage".into(), "git".into(), "rust".into()],
            &ctx,
        )
        .unwrap();
        assert_eq!(list(&db, &repo.id).unwrap(), ["git", "rust", "storage"]);

        // Replaced, not merged: "storage" is gone because the caller did
        // not send it.
        set(&db, &repo.id, &["rust".into(), "s3".into()], &ctx).unwrap();
        assert_eq!(list(&db, &repo.id).unwrap(), ["rust", "s3"]);

        // An empty list clears them, which is how a form removes the
        // last pill.
        set(&db, &repo.id, &[], &ctx).unwrap();
        assert!(list(&db, &repo.id).unwrap().is_empty());

        let entries = crate::audit::query(
            &db,
            &ctx.org_id,
            &crate::audit::AuditQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
        let topic_entries = entries.iter().filter(|e| e.action == "repo.topics").count();
        assert_eq!(topic_entries, 3, "every set is an audited act: {entries:?}");
    }

    /// A refused list leaves the stored set exactly as it was. The
    /// validation happens before the transaction opens, so this is
    /// structural rather than lucky — and it is worth pinning, because
    /// the obvious implementation (delete, then insert, then fail on the
    /// bad one) would leave a repository with no topics at all.
    #[test]
    fn a_refused_write_does_not_disturb_what_is_there() {
        let db = db("topics_refuse");
        let (repo, ctx) = fixture(&db, "ada");
        set(&db, &repo.id, &["rust".into(), "git".into()], &ctx).unwrap();
        let e = set(&db, &repo.id, &["ok".into(), "not ok".into()], &ctx).unwrap_err();
        assert!(e.contains("letters, digits and hyphens"), "{e}");
        assert_eq!(list(&db, &repo.id).unwrap(), ["git", "rust"]);
    }

    /// `ON DELETE CASCADE`, asserted rather than assumed: an orphaned
    /// topic row would be indexed by `repo_topics_topic` and would put a
    /// deleted repository into a discovery listing.
    #[test]
    fn deleting_a_repository_takes_its_topics_with_it() {
        let db = db("topics_cascade");
        let (repo, ctx) = fixture(&db, "ada");
        set(&db, &repo.id, &["rust".into()], &ctx).unwrap();
        db.lock()
            .execute("DELETE FROM repos WHERE id = $1", &[&repo.id])
            .unwrap();
        let n: i64 = db
            .lock()
            .query_one(
                "SELECT COUNT(*) FROM repo_topics WHERE repo_id = $1",
                &[&repo.id],
            )
            .unwrap()
            .get(0);
        assert_eq!(n, 0);
    }

    /// One repository's topics are not another's. The primary key is
    /// `(repo_id, topic)`, so two projects may both be `rust` — and a
    /// `set` on one must not clear the other.
    #[test]
    fn two_repositories_may_share_a_topic() {
        let db = db("topics_shared");
        let (a, ctx) = fixture(&db, "ada");
        let b = registry::create_repo(
            &db,
            &ctx.org_id,
            &NewRepo {
                description: None,
                name: "gadget",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        set(&db, &a.id, &["rust".into()], &ctx).unwrap();
        set(&db, &b.id, &["rust".into(), "cli".into()], &ctx).unwrap();
        assert_eq!(list(&db, &a.id).unwrap(), ["rust"]);
        assert_eq!(list(&db, &b.id).unwrap(), ["cli", "rust"]);
    }
}
