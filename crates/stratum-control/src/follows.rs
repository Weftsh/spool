//! Following a person.
//!
//! No reciprocal row, no acceptance step, no block list: following is a
//! public act about public activity, not a relationship both parties
//! negotiate. That keeps the whole module to two writes and two reads,
//! and it is why the table can be a bare join with a primary key on the
//! pair — a second follow of the same person is the state you are
//! already in, not an error and not a second row.
//!
//! Two rules are load-bearing enough to be enforced here rather than
//! left to callers:
//!
//! * **Following yourself is refused in a sentence.** The database has
//!   `CHECK (follower_id <> followed_id)` and would refuse it too, but a
//!   check violation reaches a caller as a 500 and a paragraph of
//!   PostgreSQL. The check stays as the backstop; this is the answer.
//! * **A follow is a person's act.** There is no principal here that is
//!   not somebody with an account, so this layer only ever sees user
//!   ids and the API layer refuses a service token — a token following
//!   somebody is a number that means nothing.

use crate::db::ControlDb;
use crate::ids::now_ms;
use serde::Serialize;

/// How many names one listing may return (I13). A follower list is a
/// public page and the count is unbounded, so the page is bounded.
pub const MAX_LIST: i64 = 100;

/// The counts on a profile, plus the asking reader's own state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FollowState {
    pub followers: i64,
    pub following: i64,
    /// Whether the person asking follows this one. `false` for a
    /// stranger, which is also what a signed-out reader sees.
    pub you_follow: bool,
}

/// A name in a follower or following list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Person {
    pub handle: String,
    pub display_name: Option<String>,
}

/// Counts for one person, and the asker's own edge if there is an asker.
pub fn state(db: &ControlDb, user_id: &str, viewer: Option<&str>) -> Result<FollowState, String> {
    let uid = user_id.to_string();
    let mut c = db.lock();
    let row = c
        .query_one(
            "SELECT (SELECT count(*) FROM follows WHERE followed_id = $1) AS followers, \
                    (SELECT count(*) FROM follows WHERE follower_id = $1) AS following",
            &[&uid],
        )
        .map_err(|e| format!("read follow counts: {e}"))?;
    let you_follow = match viewer {
        Some(v) => c
            .query_opt(
                "SELECT 1 FROM follows WHERE follower_id = $1 AND followed_id = $2",
                &[&v.to_string(), &uid],
            )
            .map_err(|e| format!("read follow: {e}"))?
            .is_some(),
        None => false,
    };
    Ok(FollowState {
        followers: row.get("followers"),
        following: row.get("following"),
        you_follow,
    })
}

/// The sentence a person gets for the one refusal they can act on.
/// Spelled once, here beside the rule, so the route cannot drift from
/// it.
pub const SELF_FOLLOW: &str = "you cannot follow yourself";

/// Why a follow was refused. A separate type rather than a string so
/// the route answers 400 for that case without pattern-matching a
/// message — a sentence somebody improves later must not silently
/// become a 500.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowError {
    /// You cannot follow yourself. See [`SELF_FOLLOW`].
    Yourself,
    Failed(String),
}

/// Follow somebody. Following twice is following once.
///
/// The self-follow is caught here, before the statement, because the
/// answer a person deserves is a sentence about what they asked for.
/// The table's `CHECK (follower_id <> followed_id)` stays as the
/// backstop — it is what makes this a rule rather than a habit — but a
/// check violation surfaces as a 500 and a paragraph of PostgreSQL,
/// which reads as a broken site.
pub fn follow(db: &ControlDb, follower: &str, followed: &str) -> Result<FollowState, FollowError> {
    if follower == followed {
        return Err(FollowError::Yourself);
    }
    db.lock()
        .execute(
            "INSERT INTO follows (follower_id, followed_id, created_at) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            &[&follower.to_string(), &followed.to_string(), &now_ms()],
        )
        .map_err(|e| FollowError::Failed(format!("follow: {e}")))?;
    state(db, followed, Some(follower)).map_err(FollowError::Failed)
}

/// Unfollow. Unfollowing somebody you never followed is not an error;
/// it is a request for a state you are already in.
pub fn unfollow(db: &ControlDb, follower: &str, followed: &str) -> Result<FollowState, String> {
    db.lock()
        .execute(
            "DELETE FROM follows WHERE follower_id = $1 AND followed_id = $2",
            &[&follower.to_string(), &followed.to_string()],
        )
        .map_err(|e| format!("unfollow: {e}"))?;
    state(db, followed, Some(follower))
}

/// The people following this one, newest first, bounded.
pub fn followers(db: &ControlDb, user_id: &str) -> Result<Vec<Person>, String> {
    people(
        db,
        "SELECT o.name AS handle, u.display_name FROM follows f \
         JOIN users u ON u.id = f.follower_id \
         JOIN orgs o ON o.owner_user_id = u.id AND o.kind = 'personal' \
         WHERE f.followed_id = $1 AND u.disabled_at IS NULL \
         ORDER BY f.created_at DESC LIMIT $2",
        user_id,
    )
}

/// The people this one follows, newest first, bounded.
pub fn following(db: &ControlDb, user_id: &str) -> Result<Vec<Person>, String> {
    people(
        db,
        "SELECT o.name AS handle, u.display_name FROM follows f \
         JOIN users u ON u.id = f.followed_id \
         JOIN orgs o ON o.owner_user_id = u.id AND o.kind = 'personal' \
         WHERE f.follower_id = $1 AND u.disabled_at IS NULL \
         ORDER BY f.created_at DESC LIMIT $2",
        user_id,
    )
}

/// Both listings are the same shape, and a suspended account is absent
/// from both: suspending somebody takes their public page down, so a
/// follower list that still names them is a page that links to a 404.
fn people(db: &ControlDb, sql: &str, user_id: &str) -> Result<Vec<Person>, String> {
    db.lock()
        .query(sql, &[&user_id.to_string(), &MAX_LIST])
        .map_err(|e| format!("list follows: {e}"))
        .map(|rows| {
            rows.iter()
                .map(|r| Person {
                    handle: r.get("handle"),
                    display_name: r.get("display_name"),
                })
                .collect()
        })
}
