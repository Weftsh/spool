//! Writing an imported project's issues into this tracker.
//!
//! Separate from `issues` because the rules are different, and the
//! differences are the interesting part:
//!
//! * **Numbers are preserved, not allocated.** `#4721` appears in commit
//!   messages, changelogs and other people's documentation. Renumbering
//!   moves the data and loses every reference to it, which is most of
//!   what a migration is for.
//! * **Authors are named, not guessed.** An imported issue whose author
//!   has no account here keeps `octocat (github)` as text. Attributing
//!   it to whoever holds that handle *here* would be worse than not
//!   importing the name at all.
//! * **Every write is idempotent on the upstream URL.** An import of ten
//!   thousand issues will be interrupted; re-running it must not produce
//!   two of anything.

use crate::ids::{now_ms, ulid, valid_id};
use crate::ControlDb;

/// An issue as it arrived from upstream.
#[derive(Debug, Clone)]
pub struct ImportedIssue {
    pub number: i32,
    pub title: String,
    pub body: String,
    /// `open` or `closed`, already normalised by the caller.
    pub state: String,
    /// The upstream login, or `None` for an account that has been
    /// deleted — which GitHub represents as a null `user`.
    pub author_login: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    pub origin_url: String,
    pub labels: Vec<String>,
}

/// A comment as it arrived from upstream.
#[derive(Debug, Clone)]
pub struct ImportedComment {
    pub body: String,
    pub author_login: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub origin_url: String,
}

/// How an upstream login is written when nobody here is that person.
///
/// The provider is named so the string cannot be mistaken for a local
/// handle: `octocat (github)` reads as somebody else's identity, where a
/// bare `octocat` reads as ours.
pub fn foreign_author(login: &str) -> String {
    format!("{login} (github)")
}

/// Whether this repository may be imported into at all.
///
/// **A tracker with issues in it is refused**, and this is the load-bearing
/// rule of the whole importer. Imported numbers are upstream's, so
/// importing into a repository that has allocated its own would either
/// collide or renumber — and renumbering breaks every `#4721` written
/// down anywhere. Refusing is the only answer that keeps the promise.
pub fn may_import(db: &ControlDb, repo_id: &str) -> Result<Result<(), String>, String> {
    if !valid_id(repo_id) {
        return Ok(Err("no such repository".into()));
    }
    let n: i64 = db
        .lock()
        .query_opt(
            "SELECT count(*) AS n FROM issues WHERE repo_id = $1",
            &[&repo_id],
        )
        .map_err(|e| format!("count issues: {}", crate::db::detail(&e)))?
        .map(|r| r.get("n"))
        .unwrap_or(0);
    if n > 0 {
        return Ok(Err(format!(
            "this repository already has {n} issue(s). An import keeps the \
             numbers it came with, so it can only go into an empty tracker — \
             importing here would either collide with those numbers or \
             renumber the imported ones, and every #reference to them \
             elsewhere would stop meaning what it says"
        )));
    }
    Ok(Ok(()))
}

/// Write one issue, keeping its number.
///
/// Idempotent on `origin_url`: a re-run after an interrupted import
/// updates the row it already wrote rather than failing on the unique
/// number or making a second one.
pub fn put_issue(db: &ControlDb, repo_id: &str, issue: &ImportedIssue) -> Result<String, String> {
    let id = ulid();
    let author_label = issue.author_login.as_deref().map(foreign_author);
    let row = db
        .lock()
        .query_one(
            "INSERT INTO issues \
             (id, repo_id, number, title, body, state, author_id, author_label, \
              created_at, updated_at, closed_at, origin_url) \
             VALUES ($1, $2, $3, $4, $5, $6, NULL, $7, $8, $9, $10, $11) \
             ON CONFLICT (repo_id, number) DO UPDATE SET \
               title = EXCLUDED.title, body = EXCLUDED.body, \
               state = EXCLUDED.state, author_label = EXCLUDED.author_label, \
               updated_at = EXCLUDED.updated_at, closed_at = EXCLUDED.closed_at, \
               origin_url = EXCLUDED.origin_url \
             RETURNING id",
            &[
                &id,
                &repo_id,
                &issue.number,
                &issue.title,
                &issue.body,
                &issue.state,
                &author_label,
                &issue.created_at,
                &issue.updated_at,
                &issue.closed_at,
                &issue.origin_url,
            ],
        )
        .map_err(|e| format!("import issue #{}: {}", issue.number, crate::db::detail(&e)))?;
    let issue_id: String = row.get("id");
    remember_url(db, repo_id, &issue.origin_url, "issue", issue.number)?;
    Ok(issue_id)
}

/// Write one comment, in the order it was said.
pub fn put_comment(
    db: &ControlDb,
    repo_id: &str,
    issue_id: &str,
    number: i32,
    c: &ImportedComment,
) -> Result<(), String> {
    let author_label = c.author_login.as_deref().map(foreign_author);
    db.lock()
        .execute(
            "INSERT INTO issue_comments \
             (id, issue_id, body, author_id, author_label, created_at, updated_at) \
             VALUES ($1, $2, $3, NULL, $4, $5, $6) \
             ON CONFLICT (id) DO NOTHING",
            &[
                // Derived from the upstream URL rather than random, so a
                // re-run after an interruption lands on the same row. A
                // ULID here would make every retry duplicate the whole
                // conversation.
                &comment_id(&c.origin_url),
                &issue_id,
                &c.body,
                &author_label,
                &c.created_at,
                &c.updated_at,
            ],
        )
        .map_err(|e| format!("import comment: {}", crate::db::detail(&e)))?;
    remember_url(db, repo_id, &c.origin_url, "comment", number)
}

/// A stable id for an imported comment, from the URL it came from.
///
/// Not a hash for secrecy — a hash for *stability*. The requirement is
/// only that the same upstream comment maps to the same row every time
/// the import runs.
fn comment_id(origin_url: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(origin_url.as_bytes());
    format!("i{}", hex(&d[..12]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Record where something came from, so the old link still resolves.
fn remember_url(
    db: &ControlDb,
    repo_id: &str,
    url: &str,
    kind: &str,
    number: i32,
) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO imported_urls (url, repo_id, kind, number, created_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (url) DO NOTHING",
            &[&url, &repo_id, &kind, &number, &now_ms()],
        )
        .map(|_| ())
        .map_err(|e| format!("remember {url}: {}", crate::db::detail(&e)))
}

/// What an old upstream URL now points at here.
pub fn resolve_url(db: &ControlDb, url: &str) -> Result<Option<(String, i32)>, String> {
    db.lock()
        .query_opt(
            "SELECT repo_id, number FROM imported_urls WHERE url = $1",
            &[&url],
        )
        .map(|r| r.map(|r| (r.get("repo_id"), r.get("number"))))
        .map_err(|e| format!("resolve {url}: {}", crate::db::detail(&e)))
}

/// Where the import has got to, per phase.
pub fn cursor(db: &ControlDb, repo_id: &str, phase: &str) -> Result<Option<String>, String> {
    db.lock()
        .query_opt(
            "SELECT cursor FROM import_cursor WHERE repo_id = $1 AND phase = $2",
            &[&repo_id, &phase],
        )
        .map(|r| r.map(|r| r.get("cursor")))
        .map_err(|e| format!("import cursor: {}", crate::db::detail(&e)))
}

pub fn set_cursor(db: &ControlDb, repo_id: &str, phase: &str, cursor: &str) -> Result<(), String> {
    db.lock()
        .execute(
            "INSERT INTO import_cursor (repo_id, phase, cursor, updated_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (repo_id, phase) DO UPDATE SET \
               cursor = EXCLUDED.cursor, updated_at = EXCLUDED.updated_at",
            &[&repo_id, &phase, &cursor, &now_ms()],
        )
        .map(|_| ())
        .map_err(|e| format!("set import cursor: {}", crate::db::detail(&e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    fn world(hint: &str) -> (ControlDb, String) {
        let db = db(hint);
        let u = crate::users::create(
            &db,
            "ada@example.com",
            "ada",
            Some("a long enough password"),
        )
        .unwrap();
        let ns = registry::create_personal_namespace(&db, &u.id, "ada", None).unwrap();
        let r = registry::create_repo(
            &db,
            &ns.id,
            &NewRepo {
                description: None,
                name: "widget",
                kind: RepoKind::Native,
                default_branch: "main",
                origin_url: None,
                origin_provider: None,
                origin_installation: None,
            },
        )
        .unwrap();
        (db, r.id)
    }

    fn issue(number: i32, url: &str) -> ImportedIssue {
        ImportedIssue {
            number,
            title: format!("issue {number}"),
            body: "body".into(),
            state: "open".into(),
            author_login: Some("octocat".into()),
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_001_000,
            closed_at: None,
            origin_url: url.into(),
            labels: vec![],
        }
    }

    /// The number is the migration. `#4721` is written down in commit
    /// messages and other people's documentation, and an import that
    /// renumbers has moved the data and lost every reference to it.
    #[test]
    fn an_imported_issue_keeps_the_number_it_came_with() {
        let (db, repo) = world("imports-numbers");
        for n in [4721, 3, 88] {
            put_issue(
                &db,
                &repo,
                &issue(n, &format!("https://github.com/a/b/issues/{n}")),
            )
            .unwrap();
        }
        let got = crate::issues::get(&db, &repo, 4721).unwrap().expect("4721");
        assert_eq!(got.number, 4721);
        assert_eq!(got.title, "issue 4721");
        // Not renumbered to 1, 2, 3 — which is what an allocator would
        // have done and what would silently break every link.
        assert!(crate::issues::get(&db, &repo, 1).unwrap().is_none());
    }

    /// An import of ten thousand issues **will** be interrupted. Running
    /// it again must land on the same rows rather than duplicating a
    /// project's entire tracker.
    #[test]
    fn re_running_an_import_updates_rather_than_duplicates() {
        let (db, repo) = world("imports-idempotent");
        let url = "https://github.com/a/b/issues/7";
        let id1 = put_issue(&db, &repo, &issue(7, url)).unwrap();

        let mut second = issue(7, url);
        second.title = "retitled upstream".into();
        second.state = "closed".into();
        let id2 = put_issue(&db, &repo, &second).unwrap();
        assert_eq!(id1, id2, "the second import made a different row");

        let got = crate::issues::get(&db, &repo, 7).unwrap().unwrap();
        assert_eq!(got.title, "retitled upstream");
        assert_eq!(got.state, "closed");

        // And the conversation does not double.
        let c = ImportedComment {
            body: "hello".into(),
            author_login: Some("octocat".into()),
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
            origin_url: "https://github.com/a/b/issues/7#issuecomment-1".into(),
        };
        put_comment(&db, &repo, &id1, 7, &c).unwrap();
        put_comment(&db, &repo, &id1, 7, &c).unwrap();
        assert_eq!(crate::issues::comments(&db, &id1).unwrap().len(), 1);
    }

    /// An upstream author with no account here is **named**, never
    /// attributed. Handing `octocat`'s issues to whoever holds that
    /// handle on this forge is worse than not importing the name.
    #[test]
    fn an_unmapped_author_is_named_as_somebody_elses() {
        let (db, repo) = world("imports-authors");
        let id = put_issue(&db, &repo, &issue(1, "https://github.com/a/b/issues/1")).unwrap();
        let got = crate::issues::get(&db, &repo, 1).unwrap().unwrap();
        assert_eq!(got.author, None, "an import must not claim a local account");
        assert_eq!(got.author_label.as_deref(), Some("octocat (github)"));
        assert_eq!(got.author_id, None);

        // A deleted upstream account has no login at all, and that is
        // not an error — it is what GitHub sends.
        let mut anon = issue(2, "https://github.com/a/b/issues/2");
        anon.author_login = None;
        put_issue(&db, &repo, &anon).unwrap();
        let got = crate::issues::get(&db, &repo, 2).unwrap().unwrap();
        assert_eq!(got.author_label, None);
        let _ = id;
    }

    /// The old links keep working, which is the point of keeping the
    /// numbers in the first place.
    #[test]
    fn an_old_upstream_url_resolves_to_what_it_became() {
        let (db, repo) = world("imports-urls");
        let url = "https://github.com/acme/widget/issues/4721";
        put_issue(&db, &repo, &issue(4721, url)).unwrap();
        let (got_repo, number) = resolve_url(&db, url).unwrap().expect("a redirect");
        assert_eq!(got_repo, repo);
        assert_eq!(number, 4721);
        assert!(resolve_url(&db, "https://github.com/a/b/issues/9")
            .unwrap()
            .is_none());
    }

    /// Importing into a tracker that already has issues is refused, with
    /// a sentence that says why rather than a code.
    #[test]
    fn a_tracker_with_issues_in_it_refuses_an_import() {
        let (db, repo) = world("imports-refuse");
        assert!(may_import(&db, &repo).unwrap().is_ok());

        let u = crate::users::create(
            &db,
            "bob@example.com",
            "bob",
            Some("a long enough password"),
        )
        .unwrap();
        crate::issues::open(&db, &repo, &u.id, "a native issue", "").unwrap();

        let refused = may_import(&db, &repo).unwrap().unwrap_err();
        assert!(refused.contains("already has 1 issue"), "{refused}");
        assert!(
            refused.contains("numbers"),
            "the refusal does not say why, so it reads as arbitrary: {refused}"
        );
        // A repository that does not exist is refused rather than
        // answering "yes, go ahead".
        assert!(may_import(&db, "not-an-id").unwrap().is_err());
    }

    #[test]
    fn the_cursor_remembers_where_a_phase_got_to() {
        let (db, repo) = world("imports-cursor");
        assert_eq!(cursor(&db, &repo, "issues").unwrap(), None);
        set_cursor(&db, &repo, "issues", "page=3").unwrap();
        set_cursor(&db, &repo, "comments", "issue=88").unwrap();
        assert_eq!(
            cursor(&db, &repo, "issues").unwrap().as_deref(),
            Some("page=3")
        );
        // Phases do not share a cursor: comments resume independently of
        // the issue walk that produced them.
        assert_eq!(
            cursor(&db, &repo, "comments").unwrap().as_deref(),
            Some("issue=88")
        );
        set_cursor(&db, &repo, "issues", "page=4").unwrap();
        assert_eq!(
            cursor(&db, &repo, "issues").unwrap().as_deref(),
            Some("page=4")
        );
    }
}
