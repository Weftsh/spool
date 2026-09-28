//! A person's profile, and the addresses their commits are signed with.
//!
//! Two surfaces with opposite visibility rules live here on purpose,
//! because the thing that keeps them apart is one file's worth of
//! discipline rather than two modules' worth of hope:
//!
//! * The **profile** — name, bio, links — is read by anybody signed in
//!   to this server. Everything it returns is a fact its owner chose to
//!   share with the people they work with.
//! * The **addresses** are read by nobody but their owner. They are the
//!   authorship-linkage surface: the set that decides which commits
//!   count as this person's work. Sharing it would hand a spammer a
//!   mailbox and hand an impersonator the exact string to put in
//!   `git config user.email`.
//!
//! The rule that makes authorship mean anything is that **only a row
//! with `verified_at IS NOT NULL` may ever count** — [`user_for_author`]
//! is the only way to ask, and it enforces that in the query rather than
//! trusting each caller to remember. Anybody can write any address into
//! a commit; an unproved one is a claim.

use crate::db::ControlDb;
use crate::ids::{now_ms, token_secret, ulid};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// A verification link for an additional address. A day — long enough
/// to survive a mail queue and a night's sleep, short enough that a link
/// left in an old inbox is not a live credential.
pub const EMAIL_VERIFY_TTL_SECS: i64 = 24 * 3600;

/// Bounds on hostile input (I13). Every one of these is a string
/// somebody types into a form, so each is capped rather than trusted;
/// the caps are generous enough that nobody honest meets them.
pub const MAX_DISPLAY_NAME: usize = 100;
pub const MAX_BIO: usize = 600;
pub const MAX_LOCATION: usize = 100;
pub const MAX_COMPANY: usize = 100;
pub const MAX_PRONOUNS: usize = 40;
pub const MAX_LINK_LABEL: usize = 60;
pub const MAX_LINK_URL: usize = 300;
pub const MAX_LINKS: usize = 5;
/// Each address costs a mail we send on request, so the count is capped
/// as much to bound outbound mail as to bound the row set.
pub const MAX_EMAILS: usize = 10;

/// Whether an account is a person or a machine principal.
///
/// Carried on the profile because it is the thing a reader most needs to
/// know about an unfamiliar name, and because slice 4's rule — an
/// agent's commits never inflate a human's graph — has to be able to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    Human,
    Agent,
}

impl AccountKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountKind::Human => "human",
            AccountKind::Agent => "agent",
        }
    }

    /// Anything unrecognised reads as a human. The column has a CHECK
    /// constraint so this cannot happen from our own writes; the arm
    /// exists so a row written by a future migration cannot make a
    /// profile page 500.
    pub fn parse(s: &str) -> AccountKind {
        match s {
            "agent" => AccountKind::Agent,
            _ => AccountKind::Human,
        }
    }
}

/// Everything `/{handle}` renders, and nothing it must not.
///
/// Note what is absent: no email of any kind, no private repository, no
/// count that a private repository moves. A field added here is a field
/// published to the anonymous internet, which is why the struct is the
/// serialization shape rather than a row wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Profile {
    pub user_id: String,
    /// The namespace name — `/{handle}` — which is also the `orgs` row.
    pub handle: String,
    /// The account's own name, as it was made with.
    pub name: String,
    /// What they would rather be called, if anything.
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub company: Option<String>,
    pub pronouns: Option<String>,
    pub kind: AccountKind,
    /// The README-profile repository, GitHub's convention: a repo named
    /// for the handle, rendered on the page.
    pub profile_repo: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    pub label: Option<String>,
    pub url: String,
}

/// One address, as its owner sees it. Never leaves the account it
/// belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmailRow {
    pub address: String,
    /// `None` means the link is still unspent: the address is claimed
    /// and counts for nothing.
    pub verified_at: Option<i64>,
    pub private: bool,
    /// True for the address the account signs in with, which cannot be
    /// removed here — removing it would leave an account nobody can
    /// authenticate or reach.
    pub primary: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OrgProfile {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub website: Option<String>,
    pub contact_email: Option<String>,
}

/// A patch. `None` leaves the field alone; `Some(None)` clears it.
///
/// Two levels of option rather than a sentinel string, because a bio
/// somebody actually wants to be the word "null" is not a bug we should
/// have to think about.
#[derive(Debug, Clone, Default)]
pub struct ProfileUpdate {
    pub display_name: Option<Option<String>>,
    pub bio: Option<Option<String>>,
    pub location: Option<Option<String>>,
    pub company: Option<Option<String>>,
    pub pronouns: Option<Option<String>>,
    pub profile_repo: Option<Option<String>>,
    /// Replaces the whole list when present — a patch of one link out of
    /// five is a shape the UI never produces and a merge rule nobody
    /// could predict.
    pub links: Option<Vec<Link>>,
}

#[derive(Debug, Clone, Default)]
pub struct OrgProfileUpdate {
    pub display_name: Option<Option<String>>,
    pub description: Option<Option<String>>,
    pub location: Option<Option<String>>,
    pub website: Option<Option<String>>,
    pub contact_email: Option<Option<String>>,
}

/// Trim, cap, and treat an empty result as absent.
///
/// "Absent" and "the empty string" render identically and compare
/// differently, which is how a profile ends up with a bio that is one
/// space and a page that reserves room for it.
fn clean(what: &str, raw: Option<String>, max: usize) -> Result<Option<String>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.chars().count() > max {
        return Err(format!("{what} must be at most {max} characters"));
    }
    // Control characters would survive JSON and break a line of HTML.
    // A newline in a bio is legitimate; a NUL or an escape sequence is
    // not, and Postgres refuses NUL outright with a 500-shaped error.
    if trimmed.chars().any(|c| c.is_control() && c != '\n') {
        return Err(format!("{what} contains a control character"));
    }
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

/// A link a profile may publish.
///
/// Only `http`/`https`. `javascript:` is the obvious one — a profile
/// link is rendered as an anchor on a page other people load — but
/// `data:` and `mailto:` are refused for the same reason: the only
/// scheme this field promises is a website, and a field that accepts
/// more than it promises is a field somebody will find a use for.
fn clean_link(link: &Link) -> Result<Link, String> {
    let url = link.url.trim();
    if url.chars().count() > MAX_LINK_URL {
        return Err(format!("a link must be at most {MAX_LINK_URL} characters"));
    }
    let lower = url.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("a link must start with http:// or https://".into());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("a link contains whitespace".into());
    }
    Ok(Link {
        label: clean("a link label", link.label.clone(), MAX_LINK_LABEL)?,
        url: url.to_string(),
    })
}

fn row_to_profile(row: &postgres::Row) -> Profile {
    Profile {
        user_id: row.get("id"),
        handle: row.get("handle"),
        name: row.get("name"),
        display_name: row.get("display_name"),
        bio: row.get("bio"),
        location: row.get("location"),
        company: row.get("company"),
        pronouns: row.get("pronouns"),
        kind: AccountKind::parse(row.get::<_, String>("kind").as_str()),
        profile_repo: row.get("profile_repo"),
        created_at: row.get("created_at"),
    }
}

/// The person behind a namespace name, and the namespace's id.
///
/// Resolved through `orgs` rather than through `users.handle` because
/// `orgs` is where the name is *unique* — 0009 put the case-folded index
/// there and left `users.handle` as a back-reference that predates it
/// and is nullable. Asking the column that owns uniqueness is the only
/// way `Ada` and `ada` cannot resolve to two different answers.
///
/// A disabled account has no profile. That is deliberate rather than a
/// side effect: suspending somebody has to take their page down, or the
/// suspension is a sign-in inconvenience and nothing more. The
/// namespace itself stays claimed, so nobody can move in behind them.
pub fn by_handle(db: &ControlDb, handle: &str) -> Result<Option<(Profile, String)>, String> {
    if !crate::registry::valid_name(handle) {
        // A name that could never have been stored names nobody — the
        // hostile string never reaches the query.
        return Ok(None);
    }
    db.lock()
        .query_opt(
            "SELECT u.id, o.name AS handle, u.name, u.display_name, u.bio, u.location, \
             u.company, u.pronouns, u.kind, u.profile_repo, \
             u.created_at, o.id AS org_id \
             FROM orgs o JOIN users u ON u.id = o.owner_user_id \
             WHERE o.kind = 'personal' AND lower(o.name) = lower($1) \
               AND u.disabled_at IS NULL",
            &[&handle.to_string()],
        )
        .map_err(|e| format!("lookup profile: {e}"))
        .map(|r| r.map(|row| (row_to_profile(&row), row.get("org_id"))))
}

pub fn update(db: &ControlDb, user_id: &str, patch: &ProfileUpdate) -> Result<(), String> {
    let display_name = clean(
        "a display name",
        patch.display_name.clone().flatten(),
        MAX_DISPLAY_NAME,
    )?;
    let bio = clean("a bio", patch.bio.clone().flatten(), MAX_BIO)?;
    let location = clean("a location", patch.location.clone().flatten(), MAX_LOCATION)?;
    let company = clean("a company", patch.company.clone().flatten(), MAX_COMPANY)?;
    let pronouns = clean("pronouns", patch.pronouns.clone().flatten(), MAX_PRONOUNS)?;
    // A profile repo is a repository *name*, and it becomes a URL, so it
    // is held to the same shape every other repo name is.
    let profile_repo = match patch.profile_repo.clone().flatten() {
        Some(name) if !crate::registry::valid_name(name.trim()) => {
            return Err("profile_repo is not a valid repository name".into())
        }
        other => other.map(|n| n.trim().to_string()),
    };
    let links = match &patch.links {
        None => None,
        Some(links) if links.len() > MAX_LINKS => return Err(format!("at most {MAX_LINKS} links")),
        Some(links) => Some(
            links
                .iter()
                .map(clean_link)
                .collect::<Result<Vec<_>, String>>()?,
        ),
    };

    let uid = user_id.to_string();
    let sets = (
        patch.display_name.is_some(),
        patch.bio.is_some(),
        patch.location.is_some(),
        patch.company.is_some(),
        patch.pronouns.is_some(),
        patch.profile_repo.is_some(),
    );
    let now = now_ms();
    // One transaction: a profile whose links landed and whose bio did
    // not is a half-saved form, and the person who submitted it has no
    // way to tell which half.
    let n = db
        .lock()
        .transaction(move |tx| {
            let n = tx.execute(
                "UPDATE users SET \
                 display_name = CASE WHEN $2 THEN $3 ELSE display_name END, \
                 bio = CASE WHEN $4 THEN $5 ELSE bio END, \
                 location = CASE WHEN $6 THEN $7 ELSE location END, \
                 company = CASE WHEN $8 THEN $9 ELSE company END, \
                 pronouns = CASE WHEN $10 THEN $11 ELSE pronouns END, \
                 profile_repo = CASE WHEN $12 THEN $13 ELSE profile_repo END \
                 WHERE id = $1 AND disabled_at IS NULL",
                &[
                    &uid,
                    &sets.0,
                    &display_name,
                    &sets.1,
                    &bio,
                    &sets.2,
                    &location,
                    &sets.3,
                    &company,
                    &sets.4,
                    &pronouns,
                    &sets.5,
                    &profile_repo,
                ],
            )?;
            if let Some(links) = &links {
                tx.execute("DELETE FROM user_links WHERE user_id = $1", &[&uid])?;
                for (i, link) in links.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO user_links (id, user_id, label, url, position, created_at) \
                         VALUES ($1, $2, $3, $4, $5, $6)",
                        &[&ulid(), &uid, &link.label, &link.url, &(i as i32), &now],
                    )?;
                }
            }
            Ok(n)
        })
        .map_err(|e| format!("update profile: {e}"))?;
    if n == 0 {
        return Err("no such user".into());
    }
    Ok(())
}

pub fn links(db: &ControlDb, user_id: &str) -> Result<Vec<Link>, String> {
    db.lock()
        .query(
            "SELECT label, url FROM user_links WHERE user_id = $1 ORDER BY position, id",
            &[&user_id.to_string()],
        )
        .map_err(|e| format!("read links: {e}"))
        .map(|rows| {
            rows.iter()
                .map(|r| Link {
                    label: r.get("label"),
                    url: r.get("url"),
                })
                .collect()
        })
}

// ---------------------------------------------------------------------
// Addresses: the authorship-linkage surface.
// ---------------------------------------------------------------------

fn hash_secret(secret: &str) -> String {
    stratum_store::pack::hex(&Sha256::digest(secret.as_bytes()))
}

/// Every address on this account, proved or not, plus which one is the
/// sign-in credential.
pub fn list_emails(db: &ControlDb, user_id: &str) -> Result<Vec<EmailRow>, String> {
    db.lock()
        .query(
            "SELECT e.address, e.verified_at, e.private, e.created_at, \
             (e.address = u.email) AS is_primary \
             FROM user_emails e JOIN users u ON u.id = e.user_id \
             WHERE e.user_id = $1 \
             ORDER BY (e.address = u.email) DESC, e.created_at, e.address",
            &[&user_id.to_string()],
        )
        .map_err(|e| format!("list addresses: {e}"))
        .map(|rows| {
            rows.iter()
                .map(|r| EmailRow {
                    address: r.get("address"),
                    verified_at: r.get("verified_at"),
                    private: r.get("private"),
                    primary: r.get("is_primary"),
                    created_at: r.get("created_at"),
                })
                .collect()
        })
}

/// Why an address could not be added. Separate from a plain string so
/// the route can answer 409 for a taken address without the message
/// having to be pattern-matched — and, more importantly, so the taken
/// case cannot accidentally grow a "taken by …" detail: whose it is, is
/// exactly what a caller must not be able to ask.
#[derive(Debug, PartialEq, Eq)]
pub enum AddEmail {
    /// Added, with the secret to mail. The link is the only proof.
    Added(String),
    /// Already on this account. Re-issues nothing, says nothing new.
    AlreadyYours,
    /// Some account holds it. Which one is never said.
    Taken,
    Invalid(String),
}

/// Claim an address, returning the secret to mail its holder.
///
/// The claim is worth nothing on its own: the row lands with
/// `verified_at` NULL, and [`user_for_author`] cannot see it. Only the
/// link proves the mailbox, and the link is bound to this address —
/// see migration 0020 for why binding it matters.
pub fn add_email(db: &ControlDb, user_id: &str, address: &str) -> Result<AddEmail, String> {
    let address = crate::users::normalize_email(address);
    if !crate::users::valid_email(&address) {
        return Ok(AddEmail::Invalid("invalid email address".into()));
    }
    let existing = list_emails(db, user_id)?;
    if existing.iter().any(|e| e.address == address) {
        return Ok(AddEmail::AlreadyYours);
    }
    if existing.len() >= MAX_EMAILS {
        return Ok(AddEmail::Invalid(format!(
            "at most {MAX_EMAILS} addresses on an account"
        )));
    }
    let secret = token_secret();
    let now = now_ms();
    let n = db
        .lock()
        .execute(
            "INSERT INTO user_emails \
             (address, user_id, verified_at, private, created_at, verify_hash, verify_expires_at) \
             VALUES ($1, $2, NULL, true, $3, $4, $5) ON CONFLICT (address) DO NOTHING",
            &[
                &address,
                &user_id.to_string(),
                &now,
                &hash_secret(&secret),
                &(now + EMAIL_VERIFY_TTL_SECS * 1000),
            ],
        )
        .map_err(|e| format!("add address: {e}"))?;
    if n == 0 {
        // Somebody else's, or a concurrent add of the same string. Both
        // are the same answer: the address is spoken for, and by whom
        // is not ours to say.
        return Ok(AddEmail::Taken);
    }
    Ok(AddEmail::Added(format!("{address}:{secret}")))
}

/// Spend a verification link, returning the address it proved.
///
/// Verification and stamping are one statement, exactly as
/// [`crate::usertokens::redeem`] is and for the same reason: checking
/// first and stamping after leaves a window in which two requests both
/// see a live token.
///
/// The link must be spent by the account that claimed the address —
/// `user_id` is in the WHERE clause, not checked afterwards. It changes
/// nothing about which row is proved (the row already names its owner)
/// and everything about what a leaked link is worth to somebody else:
/// pasted into a stranger's session it is inert, rather than quietly
/// finishing a claim on an account they cannot see.
///
/// Every wrong shape — malformed, unknown address, wrong secret,
/// expired, already spent, somebody else's — is the same `None`, so a
/// link cannot be used to ask which addresses are registered here.
pub fn verify_email(
    db: &ControlDb,
    user_id: &str,
    presented: &str,
) -> Result<Option<String>, String> {
    let Some((address, secret)) = presented.split_once(':') else {
        return Ok(None);
    };
    let address = crate::users::normalize_email(address);
    if !crate::users::valid_email(&address) {
        return Ok(None);
    }
    let now = now_ms();
    let row = db
        .lock()
        .query_opt(
            "UPDATE user_emails SET verified_at = $4, verify_hash = NULL, \
             verify_expires_at = NULL \
             WHERE address = $1 AND user_id = $2 AND verify_hash = $3 \
               AND verified_at IS NULL AND verify_expires_at > $4 \
             RETURNING address",
            &[&address, &user_id.to_string(), &hash_secret(secret), &now],
        )
        .map_err(|e| format!("verify address: {e}"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let address: String = row.get("address");

    // A newly proved address changes the past, so the past has to be
    // re-read.
    //
    // The contribution graph counts a commit only when its author line
    // carries a **verified** address. Somebody mirroring a decade of
    // work here arrives with an empty graph, adds the address they have
    // committed under since 2014, and every one of those commits should
    // light up — that is the migration promise this whole feature
    // exists to make. Without this it stays empty, because the walk
    // already passed those commits and recorded that they counted for
    // nobody.
    //
    // Scoped to repositories in namespaces this person belongs to, not
    // to the platform. An account may prove `MAX_EMAILS` addresses, and
    // "re-walk everything, ten times" is a denial of service with a
    // friendly name.
    //
    // Failure is logged and swallowed: the address **is** verified, the
    // row is written, and telling somebody their confirmation failed
    // because a background walk could not be queued would be a lie
    // about the thing they actually did. A missed re-walk costs a stale
    // graph until the next push to that repository; a refused
    // confirmation costs them the account.
    match crate::contribs::repos_for_rewalk(db, user_id) {
        Ok(repos) if !repos.is_empty() => {
            if let Err(e) = crate::contribs::rewalk(db, &repos) {
                eprintln!("weft: re-walk after verifying {address}: {e}");
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("weft: repositories to re-walk for {address}: {e}"),
    }
    Ok(Some(address))
}

/// Drop an address. The sign-in credential is refused: an account with
/// no reachable address is one nobody can recover.
pub fn remove_email(db: &ControlDb, user_id: &str, address: &str) -> Result<bool, String> {
    let address = crate::users::normalize_email(address);
    if !crate::users::valid_email(&address) {
        return Ok(false);
    }
    db.lock()
        .execute(
            "DELETE FROM user_emails e USING users u \
             WHERE u.id = e.user_id AND e.user_id = $1 AND e.address = $2 \
               AND e.address <> u.email",
            &[&user_id.to_string(), &address],
        )
        .map(|n| n > 0)
        .map_err(|e| format!("remove address: {e}"))
}

/// Whose work is a commit authored by this address?
///
/// **The only way to ask**, and the reason it exists as a function
/// rather than as a query each caller writes: `verified_at IS NOT NULL`
/// is in the WHERE clause here, once. An unproved address answers
/// `None` — a string in a commit's author line is a claim anybody can
/// make, and a contribution graph that counted claims would be a graph
/// anybody could forge by setting `git config user.email` to a
/// stranger's address.
///
/// Slice 4's contribution walker is the consumer; the rule lands with
/// the table it protects rather than with the code that will lean on it.
pub fn user_for_author(db: &ControlDb, address: &str) -> Result<Option<String>, String> {
    let address = crate::users::normalize_email(address);
    if !crate::users::valid_email(&address) {
        return Ok(None);
    }
    db.lock()
        .query_opt(
            "SELECT e.user_id FROM user_emails e JOIN users u ON u.id = e.user_id \
             WHERE e.address = $1 AND e.verified_at IS NOT NULL AND u.disabled_at IS NULL",
            &[&address],
        )
        .map_err(|e| format!("author lookup: {e}"))
        .map(|r| r.map(|r| r.get("user_id")))
}

// ---------------------------------------------------------------------
// Org profiles.
// ---------------------------------------------------------------------

/// An org's profile. Absent is the same as empty — an org that has
/// never been edited renders exactly like one whose fields were cleared,
/// so there is no second "no profile yet" state for a page to handle.
pub fn org_profile(db: &ControlDb, org_id: &str) -> Result<OrgProfile, String> {
    let row = db
        .lock()
        .query_opt(
            "SELECT display_name, description, location, website, contact_email \
             FROM org_profiles WHERE org_id = $1",
            &[&org_id.to_string()],
        )
        .map_err(|e| format!("read org profile: {e}"))?;
    Ok(match row {
        None => OrgProfile::default(),
        Some(r) => OrgProfile {
            display_name: r.get("display_name"),
            description: r.get("description"),
            location: r.get("location"),
            website: r.get("website"),
            contact_email: r.get("contact_email"),
        },
    })
}

pub fn set_org_profile(
    db: &ControlDb,
    org_id: &str,
    patch: &OrgProfileUpdate,
) -> Result<(), String> {
    let display_name = clean(
        "a display name",
        patch.display_name.clone().flatten(),
        MAX_DISPLAY_NAME,
    )?;
    let description = clean(
        "a description",
        patch.description.clone().flatten(),
        MAX_BIO,
    )?;
    let location = clean("a location", patch.location.clone().flatten(), MAX_LOCATION)?;
    let website = match patch.website.clone().flatten() {
        None => None,
        Some(url) if url.trim().is_empty() => None,
        Some(url) => Some(clean_link(&Link { label: None, url })?.url),
    };
    let contact_email = match patch.contact_email.clone().flatten() {
        None => None,
        Some(e) if e.trim().is_empty() => None,
        Some(e) => {
            let e = crate::users::normalize_email(&e);
            if !crate::users::valid_email(&e) {
                return Err("invalid contact email address".into());
            }
            Some(e)
        }
    };
    // Upsert rather than insert-then-update: the row's existence is an
    // implementation detail of "has anybody ever edited this", and a
    // caller must not have to know which statement to send.
    db.lock()
        .execute(
            "INSERT INTO org_profiles \
             (org_id, display_name, description, location, website, contact_email, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (org_id) DO UPDATE SET \
             display_name = CASE WHEN $8 THEN EXCLUDED.display_name \
                                 ELSE org_profiles.display_name END, \
             description = CASE WHEN $9 THEN EXCLUDED.description \
                                ELSE org_profiles.description END, \
             location = CASE WHEN $10 THEN EXCLUDED.location ELSE org_profiles.location END, \
             website = CASE WHEN $11 THEN EXCLUDED.website ELSE org_profiles.website END, \
             contact_email = CASE WHEN $12 THEN EXCLUDED.contact_email \
                                  ELSE org_profiles.contact_email END, \
             updated_at = EXCLUDED.updated_at",
            &[
                &org_id.to_string(),
                &display_name,
                &description,
                &location,
                &website,
                &contact_email,
                &now_ms(),
                &patch.display_name.is_some(),
                &patch.description.is_some(),
                &patch.location.is_some(),
                &patch.website.is_some(),
                &patch.contact_email.is_some(),
            ],
        )
        .map(|_| ())
        .map_err(|e| format!("set org profile: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{self, NewRepo, RepoKind};

    fn db(hint: &str) -> ControlDb {
        ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap()
    }

    /// A person with their personal namespace and a proved address, as
    /// an invitation or `admin user-create` makes them.
    fn person(db: &ControlDb, handle: &str, email: &str) -> (String, String) {
        let u = crate::users::create(db, email, handle, Some("a long enough password")).unwrap();
        crate::usertokens::mark_verified(db, &u.id).unwrap();
        let ns = registry::create_personal_namespace(db, &u.id, handle, None).unwrap();
        (u.id, ns.id)
    }

    fn repo(db: &ControlDb, org_id: &str, name: &str) -> registry::Repo {
        registry::create_repo(
            db,
            org_id,
            &NewRepo {
                description: Some("a repository"),
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

    #[test]
    fn a_profile_reads_back_what_was_written_and_clears_what_was_cleared() {
        let db = db("profiles");
        let (user, _) = person(&db, "ada", "ada@example.com");

        // Absent before anything is set, and the account's own name is
        // there from the start.
        let (p, _) = by_handle(&db, "ada").unwrap().unwrap();
        assert_eq!(p.name, "ada");
        assert_eq!(p.display_name, None);
        assert_eq!(p.kind, AccountKind::Human);

        update(
            &db,
            &user,
            &ProfileUpdate {
                display_name: Some(Some("  Ada Lovelace  ".into())),
                bio: Some(Some("Analytical engines.".into())),
                location: Some(Some("London".into())),
                company: Some(Some("@analytical".into())),
                pronouns: Some(Some("she/her".into())),
                profile_repo: Some(Some("ada".into())),
                links: Some(vec![
                    Link {
                        label: Some("Home".into()),
                        url: "https://example.com".into(),
                    },
                    Link {
                        label: None,
                        url: "http://notes.example.com/x".into(),
                    },
                ]),
            },
        )
        .unwrap();

        let (p, _) = by_handle(&db, "ada").unwrap().unwrap();
        // Trimmed on the way in, so the page never reserves room for
        // whitespace somebody pasted by accident.
        assert_eq!(p.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(p.bio.as_deref(), Some("Analytical engines."));
        assert_eq!(p.location.as_deref(), Some("London"));
        assert_eq!(p.company.as_deref(), Some("@analytical"));
        assert_eq!(p.pronouns.as_deref(), Some("she/her"));
        assert_eq!(p.profile_repo.as_deref(), Some("ada"));
        let l = links(&db, &user).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].url, "https://example.com");
        assert_eq!(l[1].label, None);

        // The name resolves case-insensitively, because `orgs` folds it.
        assert!(by_handle(&db, "ADA").unwrap().is_some());

        // An untouched field survives a patch that does not name it.
        update(
            &db,
            &user,
            &ProfileUpdate {
                location: Some(Some("Kent".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let (p, _) = by_handle(&db, "ada").unwrap().unwrap();
        assert_eq!(p.location.as_deref(), Some("Kent"));
        assert_eq!(p.bio.as_deref(), Some("Analytical engines."));
        assert_eq!(links(&db, &user).unwrap().len(), 2, "links were not named");

        // Clearing is `Some(None)`, and an all-whitespace value clears
        // rather than storing a string that renders as nothing.
        update(
            &db,
            &user,
            &ProfileUpdate {
                bio: Some(None),
                company: Some(Some("   ".into())),
                links: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
        let (p, _) = by_handle(&db, "ada").unwrap().unwrap();
        assert_eq!(p.bio, None);
        assert_eq!(p.company, None);
        assert!(links(&db, &user).unwrap().is_empty());
    }

    #[test]
    fn a_profile_refuses_input_it_could_not_render() {
        let db = db("profiles-bounds");
        let (user, _) = person(&db, "bo", "bo@example.com");
        let patch = |p: ProfileUpdate| update(&db, &user, &p);

        assert!(patch(ProfileUpdate {
            bio: Some(Some("x".repeat(MAX_BIO + 1))),
            ..Default::default()
        })
        .is_err());
        assert!(patch(ProfileUpdate {
            display_name: Some(Some("x".repeat(MAX_DISPLAY_NAME + 1))),
            ..Default::default()
        })
        .is_err());
        assert!(patch(ProfileUpdate {
            pronouns: Some(Some("x".repeat(MAX_PRONOUNS + 1))),
            ..Default::default()
        })
        .is_err());
        // A NUL would reach Postgres as a 500 rather than as a refusal.
        assert!(patch(ProfileUpdate {
            bio: Some(Some("bad\0bio".into())),
            ..Default::default()
        })
        .is_err());
        // A newline in a bio is legitimate; nothing else control-ish is.
        assert!(patch(ProfileUpdate {
            bio: Some(Some("two\nlines".into())),
            ..Default::default()
        })
        .is_ok());
        assert!(patch(ProfileUpdate {
            location: Some(Some("a\u{1b}[31mred".into())),
            ..Default::default()
        })
        .is_err());
        assert!(patch(ProfileUpdate {
            profile_repo: Some(Some("not a repo name".into())),
            ..Default::default()
        })
        .is_err());
        assert!(patch(ProfileUpdate {
            links: Some(vec![
                Link {
                    label: None,
                    url: "https://a.example".into()
                };
                MAX_LINKS + 1
            ]),
            ..Default::default()
        })
        .is_err());

        // A link is capped as well as counted. The cap is checked before
        // the scheme is, so a megabyte pasted into the field is refused
        // without ever being lowercased or scanned character by
        // character.
        assert!(patch(ProfileUpdate {
            links: Some(vec![Link {
                label: None,
                url: format!("https://a.example/{}", "x".repeat(MAX_LINK_URL)),
            }]),
            ..Default::default()
        })
        .is_err());

        // The whole point of the link check: a profile link is rendered
        // as an anchor on a page other people load.
        for hostile in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>",
            "mailto:someone@example.com",
            "//example.com",
            "example.com",
            "https://exa mple.com",
        ] {
            assert!(
                patch(ProfileUpdate {
                    links: Some(vec![Link {
                        label: None,
                        url: hostile.into(),
                    }]),
                    ..Default::default()
                })
                .is_err(),
                "{hostile:?} was accepted as a profile link"
            );
        }
        // Nothing hostile landed, and the refusals left no half-state.
        assert!(links(&db, &user).unwrap().is_empty());

        // A profile nobody has cannot be patched.
        assert!(update(&db, "01zzzzzzzzzzzzzzzzzzzzzzzz", &ProfileUpdate::default()).is_err());
    }

    /// A disabled account has no public page, and its namespace stays
    /// claimed so nobody can move in behind them.
    #[test]
    fn a_disabled_account_has_no_profile() {
        let db = db("profiles-disabled");
        let (user, _) = person(&db, "cy", "cy@example.com");
        assert!(by_handle(&db, "cy").unwrap().is_some());
        crate::users::set_disabled(&db, &user, true).unwrap();
        assert!(by_handle(&db, "cy").unwrap().is_none());
        // …and cannot be edited into visibility.
        assert!(update(
            &db,
            &user,
            &ProfileUpdate {
                bio: Some(Some("still here".into())),
                ..Default::default()
            }
        )
        .is_err());
        assert!(registry::create_org(&db, "cy").is_err(), "namespace freed");
    }

    /// A name that could never have been stored names nobody, and the
    /// hostile string never reaches a query.
    #[test]
    fn an_impossible_handle_is_simply_absent() {
        let db = db("profiles-badhandle");
        person(&db, "de", "de@example.com");
        for bad in ["", "has space", "nul\0here", "a/b", &"x".repeat(300)] {
            assert!(by_handle(&db, bad).unwrap().is_none(), "{bad:?}");
        }
        assert!(by_handle(&db, "nobody").unwrap().is_none());
    }

    /// The rule the contribution graph rests on: a claimed address is
    /// not a proved one, and only a proved one may carry authorship.
    #[test]
    fn an_unverified_address_can_never_carry_authorship() {
        let db = db("profiles-authorship");

        // Before the confirmation link is clicked the account exists and
        // its credential address has proved nothing — so it carries no
        // authorship either. Signing up and pointing `git config
        // user.email` at somebody else's address must not be a way to
        // claim their history.
        let unproved =
            crate::users::create(&db, "unproved@example.com", "U", Some("a long password"))
                .unwrap();
        let rows = list_emails(&db, &unproved.id).unwrap();
        assert_eq!(rows.len(), 1, "the credential address is an owned address");
        assert!(rows[0].verified_at.is_none());
        assert_eq!(user_for_author(&db, "unproved@example.com").unwrap(), None);
        crate::usertokens::mark_verified(&db, &unproved.id).unwrap();
        assert_eq!(
            user_for_author(&db, "unproved@example.com").unwrap(),
            Some(unproved.id.clone())
        );

        let (user, _) = person(&db, "gil", "gil@example.com");

        // The credential address is an owned address, proved by the
        // account's own verification.
        let rows = list_emails(&db, &user).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].address, "gil@example.com");
        assert!(rows[0].primary);
        assert!(rows[0].verified_at.is_some());
        assert_eq!(
            user_for_author(&db, "gil@example.com").unwrap(),
            Some(user.clone())
        );

        // A claimed address counts for nothing until the link is spent.
        let AddEmail::Added(secret) = add_email(&db, &user, "  Gil@Work.example  ").unwrap() else {
            panic!("the address was not added");
        };
        assert_eq!(user_for_author(&db, "gil@work.example").unwrap(), None);
        let rows = list_emails(&db, &user).unwrap();
        let claimed = rows
            .iter()
            .find(|r| r.address == "gil@work.example")
            .unwrap();
        assert!(claimed.verified_at.is_none());
        assert!(
            claimed.private,
            "an address is private unless said otherwise"
        );
        assert!(!claimed.primary);

        // Spending it is what makes the address count — and it counts
        // under its normalized spelling, whichever case a commit used.
        assert_eq!(
            verify_email(&db, &user, &secret).unwrap().as_deref(),
            Some("gil@work.example")
        );
        assert_eq!(
            user_for_author(&db, "GIL@Work.Example").unwrap(),
            Some(user.clone())
        );

        // A disabled account carries no authorship either: the graph
        // must not keep crediting somebody who has been suspended.
        crate::users::set_disabled(&db, &user, true).unwrap();
        assert_eq!(user_for_author(&db, "gil@work.example").unwrap(), None);
        crate::users::set_disabled(&db, &user, false).unwrap();

        // An address nobody claimed, and one that could never be stored.
        assert_eq!(user_for_author(&db, "ghost@example.com").unwrap(), None);
        assert_eq!(user_for_author(&db, "not-an-address").unwrap(), None);
        assert_eq!(user_for_author(&db, "nul\0@example.com").unwrap(), None);
    }

    /// The verification link binds the address, so a link that arrived
    /// in an attacker's own inbox cannot be spent against somebody
    /// else's mailbox. This is the hole that using a plain
    /// `user_tokens` row would have left open.
    #[test]
    fn a_verification_link_proves_one_address_and_no_other() {
        let db = db("profiles-bind");
        let (attacker, _) = person(&db, "mallory", "mallory@example.com");
        let AddEmail::Added(mine) = add_email(&db, &attacker, "mallory@evil.example").unwrap()
        else {
            panic!("not added");
        };
        // A second address on the *same account*, whose link the
        // attacker never receives.
        let AddEmail::Added(_) = add_email(&db, &attacker, "victim@corp.example").unwrap() else {
            panic!("not added");
        };
        let (_, secret) = mine.split_once(':').unwrap();

        // The secret they hold, aimed at the address they want.
        assert_eq!(
            verify_email(&db, &attacker, &format!("victim@corp.example:{secret}")).unwrap(),
            None
        );
        assert_eq!(user_for_author(&db, "victim@corp.example").unwrap(), None);

        // Nor may another account spend it, even naming the right
        // address: a leaked link pasted into a stranger's session is
        // inert rather than quietly finishing somebody else's claim.
        let (bystander, _) = person(&db, "bystander", "bystander@example.com");
        assert_eq!(verify_email(&db, &bystander, &mine).unwrap(), None);

        // It still works for the address it was actually issued for,
        // which is what proves the refusals above were about the binding
        // and not about a broken secret.
        assert_eq!(
            verify_email(&db, &attacker, &mine).unwrap().as_deref(),
            Some("mallory@evil.example")
        );
        // And exactly once.
        assert_eq!(verify_email(&db, &attacker, &mine).unwrap(), None);
    }

    #[test]
    fn every_malformed_or_expired_link_is_refused_identically() {
        let db = db("profiles-links");
        let (user, _) = person(&db, "han", "han@example.com");
        let AddEmail::Added(good) = add_email(&db, &user, "han@work.example").unwrap() else {
            panic!("not added");
        };
        let (_, secret) = good.split_once(':').unwrap();
        for bad in [
            String::new(),
            "nonsense".into(),
            "han@work.example".into(),
            "han@work.example:".into(),
            format!("han@work.example:{}", "x".repeat(52)),
            format!("ghost@nowhere.example:{secret}"),
            format!("not-an-address:{secret}"),
            format!("nul\0@x.example:{secret}"),
            format!("{good}x"),
        ] {
            assert!(verify_email(&db, &user, &bad).unwrap().is_none(), "{bad:?}");
        }

        // An expired link is dead even though nothing has spent it.
        // Aged past the API on purpose: the issuing path has no way to
        // mint one that is already dead.
        db.lock()
            .execute(
                "UPDATE user_emails SET verify_expires_at = $2 WHERE address = $1",
                &[&"han@work.example".to_string(), &(now_ms() - 1)],
            )
            .unwrap();
        assert!(verify_email(&db, &user, &good).unwrap().is_none());
        assert_eq!(user_for_author(&db, "han@work.example").unwrap(), None);
    }

    /// An address belongs to at most one account, and the refusal says
    /// nothing about whose it is.
    #[test]
    fn an_address_is_claimed_once_and_the_refusal_names_nobody() {
        let db = db("profiles-collide");
        let (first, _) = person(&db, "ivy", "ivy@example.com");
        let (second, _) = person(&db, "jo", "jo@example.com");

        assert!(matches!(
            add_email(&db, &first, "shared@example.com").unwrap(),
            AddEmail::Added(_)
        ));
        // Even unverified, the claim holds the string: otherwise two
        // accounts could each be told to prove the same mailbox and the
        // loser would find their row vanished.
        assert_eq!(
            add_email(&db, &second, "shared@example.com").unwrap(),
            AddEmail::Taken
        );
        // Including somebody else's *credential* address, which is the
        // one an impersonator would reach for.
        assert_eq!(
            add_email(&db, &second, "ivy@example.com").unwrap(),
            AddEmail::Taken
        );
        // `Taken` carries no owner. The enum is the guarantee — there is
        // nowhere for a "taken by" detail to be added by accident.
        assert_eq!(
            format!("{:?}", AddEmail::Taken),
            "Taken",
            "the taken answer grew a field naming whose address it is"
        );

        // Adding your own again is not a conflict, and mints nothing.
        assert_eq!(
            add_email(&db, &first, "SHARED@example.com").unwrap(),
            AddEmail::AlreadyYours
        );
        assert!(matches!(
            add_email(&db, &first, "not-an-address").unwrap(),
            AddEmail::Invalid(_)
        ));
        assert_eq!(list_emails(&db, &first).unwrap().len(), 2);
    }

    #[test]
    fn the_sign_in_address_cannot_be_removed_and_the_cap_holds() {
        let db = db("profiles-remove");
        let (user, _) = person(&db, "kit", "kit@example.com");
        let AddEmail::Added(_) = add_email(&db, &user, "kit@work.example").unwrap() else {
            panic!("not added");
        };
        assert!(remove_email(&db, &user, "KIT@Work.example").unwrap());
        assert!(!remove_email(&db, &user, "kit@work.example").unwrap());
        // The credential address stays: an account with no reachable
        // address is one nobody can recover.
        assert!(!remove_email(&db, &user, "kit@example.com").unwrap());
        assert!(!remove_email(&db, &user, "not-an-address").unwrap());
        assert_eq!(list_emails(&db, &user).unwrap().len(), 1);

        // Nor may somebody remove an address off another account.
        let (other, _) = person(&db, "lu", "lu@example.com");
        assert!(!remove_email(&db, &other, "kit@example.com").unwrap());

        for i in 0..(MAX_EMAILS - 1) {
            assert!(matches!(
                add_email(&db, &user, &format!("kit+{i}@work.example")).unwrap(),
                AddEmail::Added(_)
            ));
        }
        assert!(matches!(
            add_email(&db, &user, "kit+over@work.example").unwrap(),
            AddEmail::Invalid(_)
        ));
        assert_eq!(list_emails(&db, &user).unwrap().len(), MAX_EMAILS);
    }

    #[test]
    fn an_org_profile_is_empty_until_edited_and_patches_field_by_field() {
        let db = db("profiles-org");
        let org = registry::create_org(&db, "acme").unwrap();
        assert_eq!(org_profile(&db, &org.id).unwrap(), OrgProfile::default());

        set_org_profile(
            &db,
            &org.id,
            &OrgProfileUpdate {
                display_name: Some(Some("Acme Corp".into())),
                description: Some(Some("We make things.".into())),
                website: Some(Some("https://acme.example".into())),
                contact_email: Some(Some("  Hello@Acme.Example ".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let p = org_profile(&db, &org.id).unwrap();
        assert_eq!(p.display_name.as_deref(), Some("Acme Corp"));
        assert_eq!(p.website.as_deref(), Some("https://acme.example"));
        assert_eq!(p.contact_email.as_deref(), Some("hello@acme.example"));
        assert_eq!(p.location, None);

        // A second patch touches only what it names — the upsert's
        // conflict arm has to be a field-by-field merge, not a replace.
        set_org_profile(
            &db,
            &org.id,
            &OrgProfileUpdate {
                location: Some(Some("Bath".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let p = org_profile(&db, &org.id).unwrap();
        assert_eq!(p.location.as_deref(), Some("Bath"));
        assert_eq!(p.display_name.as_deref(), Some("Acme Corp"));

        set_org_profile(
            &db,
            &org.id,
            &OrgProfileUpdate {
                description: Some(None),
                website: Some(Some("  ".into())),
                contact_email: Some(Some("".into())),
                ..Default::default()
            },
        )
        .unwrap();
        let p = org_profile(&db, &org.id).unwrap();
        assert_eq!(p.description, None);
        assert_eq!(p.website, None);
        assert_eq!(p.contact_email, None);

        // The same refusals the personal profile makes, because an org
        // page is rendered to the same anonymous internet.
        assert!(set_org_profile(
            &db,
            &org.id,
            &OrgProfileUpdate {
                website: Some(Some("javascript:alert(1)".into())),
                ..Default::default()
            }
        )
        .is_err());
        assert!(set_org_profile(
            &db,
            &org.id,
            &OrgProfileUpdate {
                contact_email: Some(Some("not-an-address".into())),
                ..Default::default()
            }
        )
        .is_err());
        assert!(set_org_profile(
            &db,
            &org.id,
            &OrgProfileUpdate {
                display_name: Some(Some("x".repeat(MAX_DISPLAY_NAME + 1))),
                ..Default::default()
            }
        )
        .is_err());
        // None of the refusals landed.
        assert_eq!(org_profile(&db, &org.id).unwrap().website, None);
    }

    #[test]
    fn an_unrecognised_account_kind_reads_as_a_human() {
        assert_eq!(AccountKind::parse("human"), AccountKind::Human);
        assert_eq!(AccountKind::parse("agent"), AccountKind::Agent);
        assert_eq!(AccountKind::parse("martian"), AccountKind::Human);
        assert_eq!(AccountKind::Agent.as_str(), "agent");
        assert_eq!(AccountKind::Human.as_str(), "human");
    }

    /// Proving an address re-reads the past, which is the migration
    /// promise in one sentence.
    ///
    /// The contribution graph counts a commit only when its author line
    /// carries a **verified** address. So the flow that matters is:
    /// mirror a decade of work, see an empty graph, add the address you
    /// have committed under since 2014 — and watch it fill in. Without
    /// the re-walk it stays empty forever, because the walk already
    /// passed those commits and recorded that they counted for nobody.
    ///
    /// Asserted through the cursor and the queued job rather than
    /// through squares, because what `verify_email` owes is "go and
    /// look again"; whether looking again finds anything is the
    /// walker's job and is tested there.
    #[test]
    fn proving_an_address_sends_the_walker_back_over_the_past() {
        let db = db("profiles-verify-rewalk");
        let (user, org) = person(&db, "ada", "ada@example.com");
        let r = repo(&db, &org, "widget");

        // A walk has already happened and left a frontier behind.
        crate::contribs::apply(
            &db,
            &r.id,
            &[],
            &[crate::contribs::Advance {
                reference: "refs/heads/main".into(),
                from: None,
                to: "a".repeat(40),
            }],
        )
        .unwrap();
        assert!(
            !crate::contribs::cursors(&db, &r.id).unwrap().is_empty(),
            "the fixture did not establish a frontier, so this test proves nothing"
        );

        // `Added` already carries `address:secret` — the whole payload
        // the link is built from, not a bare secret.
        let link = match add_email(&db, &user, "ada@oldjob.example").unwrap() {
            AddEmail::Added(s) => s,
            other => panic!("add_email: {other:?}"),
        };

        // The wrong secret proves nothing and must move nothing: a
        // failed confirmation that still re-walked would let anybody
        // with an address make us re-read every repository they can see.
        assert!(verify_email(&db, &user, "ada@oldjob.example:wrong")
            .unwrap()
            .is_none());
        assert!(
            !crate::contribs::cursors(&db, &r.id).unwrap().is_empty(),
            "a refused confirmation reset the frontier"
        );

        let proved = verify_email(&db, &user, &link)
            .unwrap()
            .expect("the address is proved");
        assert_eq!(proved, "ada@oldjob.example");

        assert!(
            crate::contribs::cursors(&db, &r.id).unwrap().is_empty(),
            "proving an address left the old frontier in place, so the walker \
             would skip exactly the commits it now knows count"
        );
    }
}
