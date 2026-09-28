//! Signing in through the company's identity provider, on the control
//! plane's side.
//!
//! The server has already verified the provider's ID token by the time
//! anything here runs; this module only decides **which account** the
//! person is, and makes one when they are nobody yet. Anybody the
//! company's provider signs in gets an account: that is the operator's
//! decision to make by configuring the provider, and the provider — not
//! this server — is where a person is admitted or turned away.
//!
//! # Which account, in this order
//!
//! 1. **The link we already hold**, keyed on the provider's issuer and
//!    its `sub` — never on an address or a username, both of which the
//!    provider lets people change. A person whose address changed at the
//!    provider still lands on their own account.
//! 2. **The address**, when the server trusts it (see the server's
//!    `oidc` module: the provider marked it verified, or its domain is one
//!    the operator said this provider speaks for). An account that signs
//!    in with that address, or has proved it as one of its own, is
//!    linked — this is how the operator who bootstrapped the server gets
//!    in once SSO is switched on.
//! 3. **Nobody**: a new account, made whole in one transaction — the
//!    account with its address proved, the address as an owned address
//!    (so their commits are theirs), a handle and personal namespace,
//!    membership of the organization everybody arriving by SSO joins,
//!    and the link.
//!
//! A person already known is not put back into that organization on
//! every sign-in: an administrator who took somebody out of it meant it.

use crate::audit::{self, AuditCtx};
use crate::db::{is_unique_violation, ControlDb};
use crate::ids::{now_ms, ulid};
use crate::members::Role;

/// The spelling of an identity provider in the `identities` table: the
/// issuer itself, so that a server moved to a different provider can
/// never confuse one provider's `sub` for another's.
pub fn provider_key(issuer: &str) -> String {
    format!("oidc:{issuer}")
}

/// Who the provider says arrived.
#[derive(Debug, Clone)]
pub struct Arrival {
    /// The issuer, exactly as configured and as the token carried it.
    pub issuer: String,
    /// The provider's own immutable id for the person.
    pub subject: String,
    /// An address the server trusts the provider for, lowercased; `None`
    /// when the provider vouched for none. Only needed when the person
    /// is not already linked.
    pub email: Option<String>,
    /// What to call them, if the provider said.
    pub name: Option<String>,
    /// The provider's username for them, used only to suggest a handle.
    pub preferred_username: Option<String>,
}

/// Where the person landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing {
    /// Linked already.
    Known(String),
    /// An existing account, linked just now by its address.
    Linked(String),
    /// A new account.
    Created(String),
}

impl Landing {
    pub fn user_id(&self) -> &str {
        match self {
            Landing::Known(u) | Landing::Linked(u) | Landing::Created(u) => u,
        }
    }
}

/// Why nobody was signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not linked, and the provider gave no address the server trusts, so
    /// there is nothing to link by and nothing to make an account with.
    NoEmail,
    /// No handle near the one the person's name or address suggests is
    /// free. Vanishingly rare; reported rather than retried forever.
    NoHandle,
}

/// Resolve an arrival to an account, making one if need be.
///
/// `org_id` is the organization everybody arriving by SSO joins, and
/// `role` the role they join it at. A lost race — two first sign-ins of
/// the same person at once, or a handle claimed in between — is retried
/// once, and the retry finds what the winner made.
pub fn resolve(
    db: &ControlDb,
    a: &Arrival,
    org_id: &str,
    role: Role,
) -> Result<Result<Landing, Refusal>, String> {
    match resolve_once(db, a, org_id, role) {
        Err(Race) => match resolve_once(db, a, org_id, role) {
            Err(Race) => Err("sso sign-in: lost the same race twice".into()),
            Ok(r) => r,
        },
        Ok(r) => r,
    }
}

/// A unique violation somebody else's transaction caused.
struct Race;

type Once = Result<Result<Result<Landing, Refusal>, String>, Race>;

fn resolve_once(db: &ControlDb, a: &Arrival, org_id: &str, role: Role) -> Once {
    let provider = provider_key(&a.issuer);
    match crate::identities::user_for(db, &provider, &a.subject) {
        Ok(Some(user_id)) => return Ok(Ok(Ok(Landing::Known(user_id)))),
        Ok(None) => {}
        Err(e) => return Ok(Err(e)),
    }
    let Some(email) = a.email.clone() else {
        return Ok(Ok(Err(Refusal::NoEmail)));
    };
    let email = crate::users::normalize_email(&email);

    // An account that signs in with this address, or has proved it as
    // one of its own.
    let holder = match crate::users::by_email(db, &email) {
        Ok(Some(u)) => Some(u.id),
        Ok(None) => match crate::profiles::user_for_author(db, &email) {
            Ok(u) => u,
            Err(e) => return Ok(Err(e)),
        },
        Err(e) => return Ok(Err(e)),
    };
    if let Some(user_id) = holder {
        return link_existing(db, &provider, a, &user_id, &email, org_id, role)
            .map(|r| r.map(|()| Ok(Landing::Linked(user_id.clone()))));
    }

    let seed = a
        .preferred_username
        .as_deref()
        .map(|u| u.split('@').next().unwrap_or(u).to_string())
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| email.split('@').next().unwrap_or(&email).to_string());
    let handle = match crate::registry::free_handle_near(db, &seed) {
        Ok(Some(h)) => h,
        Ok(None) => return Ok(Ok(Err(Refusal::NoHandle))),
        Err(e) => return Ok(Err(e)),
    };
    create(db, &provider, a, &email, &handle, org_id, role)
        .map(|r| r.map(|user_id| Ok(Landing::Created(user_id))))
}

/// Link an existing account, prove its address, and put it in the SSO
/// organization if it is not there already — at the SSO role, never
/// lowering one it already holds.
fn link_existing(
    db: &ControlDb,
    provider: &str,
    a: &Arrival,
    user_id: &str,
    email: &str,
    org_id: &str,
    role: Role,
) -> Result<Result<(), String>, Race> {
    let (provider, subject) = (provider.to_string(), a.subject.clone());
    let (user_id, email, org_id) = (user_id.to_string(), email.to_string(), org_id.to_string());
    let now = now_ms();
    let out = db.lock().transaction(move |tx| {
        tx.execute(
            "INSERT INTO identities (id, user_id, provider, provider_user_id, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
            &[&ulid(), &user_id, &provider, &subject, &now],
        )?;
        // The provider vouched for the address, so it is proved — on the
        // account's sign-in address and on the owned-address row alike.
        tx.execute(
            "UPDATE users SET verified_at = $2 WHERE id = $1 AND verified_at IS NULL \
               AND email = $3",
            &[&user_id, &now, &email],
        )?;
        tx.execute(
            "UPDATE user_emails SET verified_at = $3, verify_hash = NULL, \
             verify_expires_at = NULL \
             WHERE user_id = $1 AND address = $2 AND verified_at IS NULL",
            &[&user_id, &email, &now],
        )?;
        tx.execute(
            "INSERT INTO org_members (org_id, user_id, role, created_at) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (org_id, user_id) DO NOTHING",
            &[&org_id, &user_id, &role.as_str(), &now],
        )?;
        let ctx = AuditCtx {
            principal: format!("user:{user_id}"),
            user_id: Some(user_id.clone()),
            org_id: org_id.clone(),
        };
        audit::record_tx(
            tx,
            &ctx,
            None,
            "sso.link",
            Some(&serde_json::json!({ "provider": provider })),
        )?;
        Ok(())
    });
    match out {
        Ok(()) => Ok(Ok(())),
        Err(e) if is_unique_violation(&e) => Err(Race),
        Err(e) => Ok(Err(format!("sso link: {e}"))),
    }
}

/// A new account, whole, in one transaction.
fn create(
    db: &ControlDb,
    provider: &str,
    a: &Arrival,
    email: &str,
    handle: &str,
    org_id: &str,
    role: Role,
) -> Result<Result<String, String>, Race> {
    let user_id = ulid();
    let name = a
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(200).collect::<String>())
        .unwrap_or_else(|| handle.to_string());
    let namespace = crate::registry::Org {
        id: ulid(),
        name: handle.to_string(),
        created_at: now_ms(),
    };
    let (provider, subject) = (provider.to_string(), a.subject.clone());
    let (tx_user, email, org_id) = (user_id.clone(), email.to_string(), org_id.to_string());
    let now = now_ms();
    let out = db.lock().transaction(move |tx| {
        tx.execute(
            "INSERT INTO users (id, email, name, password_hash, created_at, verified_at) \
             VALUES ($1, $2, $3, NULL, $4, $4)",
            &[&tx_user, &email, &name, &now],
        )?;
        // An unproved claim on the address by somebody else holds nothing
        // — only a mailed link proves an address, and the provider has
        // just proved this one belongs to the person arriving.
        tx.execute(
            "DELETE FROM user_emails WHERE address = $1 AND verified_at IS NULL",
            &[&email],
        )?;
        tx.execute(
            "INSERT INTO user_emails (address, user_id, verified_at, private, created_at) \
             VALUES ($1, $2, $3, true, $3)",
            &[&email, &tx_user, &now],
        )?;
        crate::registry::claim_personal_namespace_tx(tx, &namespace, &tx_user)?;
        tx.execute(
            "INSERT INTO org_members (org_id, user_id, role, created_at) \
             VALUES ($1, $2, $3, $4)",
            &[&org_id, &tx_user, &role.as_str(), &now],
        )?;
        tx.execute(
            "INSERT INTO identities (id, user_id, provider, provider_user_id, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
            &[&ulid(), &tx_user, &provider, &subject, &now],
        )?;
        let ctx = AuditCtx {
            principal: format!("user:{tx_user}"),
            user_id: Some(tx_user.clone()),
            org_id: org_id.clone(),
        };
        audit::record_tx(
            tx,
            &ctx,
            None,
            "sso.provision",
            Some(&serde_json::json!({
                "provider": provider, "handle": namespace.name, "role": role.as_str(),
            })),
        )?;
        Ok(())
    });
    match out {
        Ok(()) => Ok(Ok(user_id)),
        Err(e) if is_unique_violation(&e) => Err(Race),
        Err(e) => Ok(Err(format!("sso provision: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{members, registry, users};

    const ISS: &str = "https://idp.acme.test";

    fn setup(hint: &str) -> (ControlDb, String) {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url(hint)).unwrap();
        let org = registry::create_org(&db, "acme").unwrap();
        (db, org.id)
    }

    fn arrival(sub: &str, email: Option<&str>) -> Arrival {
        Arrival {
            issuer: ISS.into(),
            subject: sub.into(),
            email: email.map(str::to_string),
            name: Some("Dana Scully".into()),
            preferred_username: None,
        }
    }

    /// A newcomer is a whole account the moment they arrive: proved
    /// address, owned address, handle, namespace, membership and link.
    #[test]
    fn a_newcomer_is_made_whole_in_the_sso_organization() {
        let (db, org) = setup("sso-new");
        let got = resolve(
            &db,
            &arrival("s-1", Some("Dana.Scully@acme.test")),
            &org,
            Role::Member,
        )
        .unwrap()
        .unwrap();
        let Landing::Created(uid) = got else {
            panic!("{got:?}")
        };
        let u = users::by_id(&db, &uid).unwrap().unwrap();
        assert_eq!(u.email, "dana.scully@acme.test");
        assert_eq!(u.name, "Dana Scully");
        assert!(u.verified_at.is_some());
        assert_eq!(u.handle.as_deref(), Some("dana-scully"));
        assert!(u.handle.is_some() && users::without_handle(&db).unwrap().is_empty());
        assert_eq!(
            members::role_of(&db, &org, &uid).unwrap(),
            Some(Role::Member)
        );
        assert_eq!(
            crate::profiles::user_for_author(&db, "dana.scully@acme.test").unwrap(),
            Some(uid.clone()),
            "their commits must be theirs"
        );
        // …and the second sign-in finds them, by the link, not the address.
        let again = resolve(&db, &arrival("s-1", None), &org, Role::Admin)
            .unwrap()
            .unwrap();
        assert_eq!(again, Landing::Known(uid.clone()));
        // Known people are not re-added or re-roled on every sign-in.
        assert_eq!(
            members::role_of(&db, &org, &uid).unwrap(),
            Some(Role::Member)
        );
    }

    /// The address changing at the provider does not move the account;
    /// a different person arriving with an old address does not reach it
    /// through the link.
    #[test]
    fn the_link_is_the_subject_not_the_address() {
        let (db, org) = setup("sso-sub");
        let first = resolve(
            &db,
            &arrival("s-1", Some("a@acme.test")),
            &org,
            Role::Member,
        )
        .unwrap()
        .unwrap();
        let moved = resolve(
            &db,
            &arrival("s-1", Some("renamed@acme.test")),
            &org,
            Role::Member,
        )
        .unwrap()
        .unwrap();
        assert_eq!(moved, Landing::Known(first.user_id().to_string()));
        // The same subject at another provider is somebody else entirely.
        let mut other = arrival("s-1", Some("b@acme.test"));
        other.issuer = "https://other-idp.test".into();
        let elsewhere = resolve(&db, &other, &org, Role::Member).unwrap().unwrap();
        assert!(matches!(elsewhere, Landing::Created(ref u) if u != first.user_id()));
    }

    /// An account made before SSO — the bootstrapped owner — is linked by
    /// its address, keeps its role, and is proved.
    #[test]
    fn an_existing_account_is_linked_by_its_address_and_keeps_its_role() {
        let (db, org) = setup("sso-link");
        let owner =
            users::create(&db, "owner@acme.test", "Owner", Some("a long enough pw")).unwrap();
        members::add(&db, &org, &owner.id, Role::Owner, None).unwrap();
        let got = resolve(
            &db,
            &arrival("s-owner", Some("OWNER@acme.test")),
            &org,
            Role::Member,
        )
        .unwrap()
        .unwrap();
        assert_eq!(got, Landing::Linked(owner.id.clone()));
        assert_eq!(
            members::role_of(&db, &org, &owner.id).unwrap(),
            Some(Role::Owner),
            "never lowered"
        );
        assert!(users::by_id(&db, &owner.id)
            .unwrap()
            .unwrap()
            .verified_at
            .is_some());
        // A person with an account elsewhere is put into the SSO org.
        let other = registry::create_org(&db, "elsewhere").unwrap();
        let outsider =
            users::create(&db, "out@acme.test", "Out", Some("a long enough pw")).unwrap();
        members::add(&db, &other.id, &outsider.id, Role::Owner, None).unwrap();
        resolve(
            &db,
            &arrival("s-out", Some("out@acme.test")),
            &org,
            Role::Viewer,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            members::role_of(&db, &org, &outsider.id).unwrap(),
            Some(Role::Viewer)
        );
    }

    /// With no link and no trusted address there is nothing to go on.
    #[test]
    fn no_link_and_no_address_is_refused_and_writes_nothing() {
        let (db, org) = setup("sso-noemail");
        assert_eq!(
            resolve(&db, &arrival("s-1", None), &org, Role::Member).unwrap(),
            Err(Refusal::NoEmail)
        );
        let n: i64 = db
            .lock()
            .query_one("SELECT count(*) FROM users", &[])
            .unwrap()
            .get(0);
        assert_eq!(n, 0);
    }

    /// Somebody else's *unproved* claim on the address does not stop the
    /// person who owns it arriving; a *proved* one links to its owner.
    #[test]
    fn claims_on_the_address_are_settled_by_who_proved_it() {
        let (db, org) = setup("sso-claims");
        let squatter = users::create(&db, "sq@acme.test", "Sq", Some("a long enough pw")).unwrap();
        let crate::profiles::AddEmail::Added(_) =
            crate::profiles::add_email(&db, &squatter.id, "dana@acme.test").unwrap()
        else {
            panic!("claim refused")
        };
        let got = resolve(
            &db,
            &arrival("s-dana", Some("dana@acme.test")),
            &org,
            Role::Member,
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(got, Landing::Created(ref u) if *u != squatter.id),
            "{got:?}"
        );
        assert_eq!(
            crate::profiles::user_for_author(&db, "dana@acme.test").unwrap(),
            Some(got.user_id().to_string())
        );
    }
}
