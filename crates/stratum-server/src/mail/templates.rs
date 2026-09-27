//! What the messages say.
//!
//! Kept in one place, as data, so the wording is reviewable and the
//! links are built by one function rather than by each call site. Plain
//! text, one idea, one link — a transactional mail that needs a layout
//! is a mail that has stopped being transactional.
//!
//! The link is always absolute and always built from the server's
//! configured public URL. A base taken from a request header would let
//! whoever triggered the mail choose where the recipient's credentials
//! get sent.

use super::Message;

/// The dashboard URL that accepts an invitation.
///
/// The token goes in the fragment, not the query: a fragment is not sent
/// to the server, does not reach access logs, and does not travel in a
/// `Referer` when the page later loads a font or an image.
pub fn invite_url(public_url: &str, token: &str) -> String {
    format!(
        "{}/dashboard/#invite={}",
        public_url.trim_end_matches('/'),
        urlencode(token)
    )
}

/// Percent-encode everything outside the URL-unreserved set.
///
/// The token's own alphabet is safe, but this also carries whatever a
/// future token shape uses, and an unencoded `#` or `&` here would
/// truncate the credential silently.
pub(crate) fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// "You have been invited to <org>."
pub fn invitation(to: &str, org: &str, role: &str, public_url: &str, token: &str) -> Message {
    let url = invite_url(public_url, token);
    Message {
        to: to.to_string(),
        subject: format!("You've been invited to {org} on Weft"),
        text: format!(
            "You've been invited to join {org} on Weft as {role}.\n\
             \n\
             Accept the invitation:\n\
             {url}\n\
             \n\
             The link works once and expires in seven days. If you weren't\n\
             expecting this, you can ignore it — nothing was created for you.\n"
        ),
    }
}

/// Where a mailed token is redeemed. Same reasoning as
/// [`invite_url`]: the fragment keeps the credential out of access logs
/// and out of any `Referer` the page later sends.
fn token_url(public_url: &str, key: &str, token: &str) -> String {
    format!(
        "{}/dashboard/#{key}={}",
        public_url.trim_end_matches('/'),
        urlencode(token)
    )
}

/// The link a verification mail carries — also what `admin verify-link`
/// prints, for an address the mail could not reach.
pub fn verification_url(public_url: &str, token: &str) -> String {
    token_url(public_url, "verify", token)
}

/// "Confirm your address."
pub fn verification(to: &str, public_url: &str, token: &str) -> Message {
    let url = verification_url(public_url, token);
    Message {
        to: to.to_string(),
        subject: "Confirm your email address".to_string(),
        text: format!(
            "Welcome to Weft. Confirm this address to finish setting up\n\
             your account:\n\
             \n\
             {url}\n\
             \n\
             The link works once and expires in 24 hours. Until you confirm,\n\
             you can sign in and look around but not create repositories.\n\
             \n\
             If you didn't sign up, ignore this — the account cannot be used\n\
             without the password whoever created it chose.\n"
        ),
    }
}

/// "Set a new password."
///
/// Says plainly that nothing has changed yet, because the message a
/// stranger receives when somebody else typed their address is the one
/// that decides whether they panic.
pub fn password_reset(to: &str, public_url: &str, token: &str) -> Message {
    let url = token_url(public_url, "reset", token);
    Message {
        to: to.to_string(),
        subject: "Reset your Weft password".to_string(),
        text: format!(
            "Somebody asked to reset the password for this address.\n\
             \n\
             {url}\n\
             \n\
             The link works once and expires in an hour. Using it signs out\n\
             every other session on the account.\n\
             \n\
             If this wasn't you, nothing has happened and nothing will —\n\
             your current password still works and no one has been told\n\
             whether this address has an account at all.\n"
        ),
    }
}

/// "You already have an account."
///
/// Sent when somebody signs up with an address that is already
/// registered. The API answers identically either way, so this message
/// is the only thing that tells anyone anything — and it goes to the
/// mailbox's owner, who is the one person entitled to know.
///
/// It is also the answer to a real dead end: without it, somebody who
/// forgot they had an account sees "check your email", receives nothing,
/// and has no idea why.
pub fn account_exists(to: &str, public_url: &str) -> Message {
    let base = public_url.trim_end_matches('/');
    Message {
        to: to.to_string(),
        subject: "You already have a Weft account".to_string(),
        text: format!(
            "Somebody just tried to sign up with this address, and it already\n\
             has an account here. Nothing was created and nothing changed.\n\
             \n\
             If that was you, sign in instead:\n\
             {base}/dashboard/\n\
             \n\
             If you have forgotten the password, ask for a reset link from\n\
             the same screen. If it wasn't you, you can ignore this — but it\n\
             is worth knowing somebody has your address.\n"
        ),
    }
}

/// "Confirm this address so your commits count."
///
/// Deliberately not [`verification`]: that message is about finishing an
/// account, and somebody who receives it for an address they never
/// signed up with has no idea what it refers to. This one names the
/// account that claimed the address and says what happens if it was not
/// them — which matters more here than at signup, because the claim is
/// made by whoever is already signed in somewhere else.
pub fn address_verification(to: &str, handle: &str, public_url: &str, token: &str) -> Message {
    let url = token_url(public_url, "verify-email", token);
    Message {
        to: to.to_string(),
        subject: "Confirm this email address on Weft".to_string(),
        text: format!(
            "The Weft account {handle} has added this address, so that\n\
             commits authored with it are credited to that account.\n\
             \n\
             Confirm it:\n\
             {url}\n\
             \n\
             The link works once, expires in 24 hours, and only works while\n\
             signed in as {handle}. Until it is used the address counts for\n\
             nothing and stays private.\n\
             \n\
             If this wasn't you, ignore it. Nothing has been credited to\n\
             anybody and the address cannot be added by anyone else while\n\
             the claim stands — tell us if you want it released.\n"
        ),
    }
}

/// "Somebody did something to a change you are involved in."
///
/// One template for all four events rather than four near-identical
/// ones. The subject carries the repository and the change, because that
/// is what a maintainer filters and threads on, and the body says who
/// did what in one line before the link — a notification that makes you
/// open it to find out whether it matters is a notification that trains
/// you to ignore the next one.
///
/// It says why *you* were told at the end. "You are a reviewer this
/// change requires" and "you commented on it" are different facts, and a
/// person who cannot tell which one applies to them cannot decide
/// whether to change their settings — so they turn the whole channel
/// off instead.
pub struct ChangeActivity<'a> {
    pub to: &'a str,
    pub public_url: &'a str,
    pub org: &'a str,
    pub repo: &'a str,
    pub change_key: &'a str,
    pub title: &'a str,
    pub actor: &'a str,
    pub event: &'a str,
}

pub fn change_activity(m: &ChangeActivity<'_>) -> Message {
    let ChangeActivity {
        to,
        public_url,
        org,
        repo,
        change_key,
        title,
        actor,
        event,
    } = *m;
    let did = match event {
        "opened" => "opened",
        "commented" => "commented on",
        "approved" => "approved",
        "landed" => "landed",
        "reviewed" => "reviewed",
        // The verb carries the whole message for this one: somebody
        // filtering by subject line must be able to tell "Bo reviewed"
        // from "Bo requested changes on" without opening it, because
        // only one of them is asking them to do something.
        "changes_requested" => "requested changes on",
        // A kind this build does not know: say so plainly rather than
        // inventing a verb. The worker refuses unknown events before it
        // gets here, so this is belt and braces.
        _ => "updated",
    };
    let url = format!(
        "{}/{org}/{repo}/changes/{change_key}",
        public_url.trim_end_matches('/')
    );
    Message {
        to: to.to_string(),
        subject: format!("[{org}/{repo}] {actor} {did}: {title}"),
        text: format!(
            "{actor} {did} a change in {org}/{repo}.\n\
             \n\
             {title}\n\
             {url}\n\
             \n\
             You are getting this because you opened this change, took part\n\
             in it, or the repository's OWNERS file names you as a reviewer\n\
             it needs. Change what you hear about from the Watch control on\n\
             the repository.\n"
        ),
    }
}

/// "Somebody did something to a changeset you are part of."
///
/// The sibling of [`change_activity`], and deliberately not a fifth
/// event on it: a changeset is one review over several repositories, so
/// the two facts a reader needs first — how many repositories, and
/// which — have nowhere to go in a message whose subject is one
/// `org/repo`. A maintainer who is told "a change landed" and has to
/// open a link to find out it was part of a four-repository landing has
/// been told the wrong thing.
///
/// The subject carries the org and the changeset key rather than a
/// repository, because that is the unit: filtering on one member's
/// repository would sort half a landing into one folder.
pub struct ChangesetActivity<'a> {
    pub to: &'a str,
    pub public_url: &'a str,
    pub org: &'a str,
    pub key: &'a str,
    pub title: &'a str,
    pub actor: &'a str,
    pub event: &'a str,
    /// Every member repository, in landing order. Named in full: "4
    /// repositories" is a number, and the reader's question is whether
    /// theirs is one of them.
    pub repos: &'a [String],
}

/// Where a changeset is read.
///
/// The dashboard, and that is a deliberate stopgap. The public forge
/// serves `/{org}/{repo}/changes/{key}` for a single change but has no
/// `/{org}/changesets/{key}` yet, and a notification pointing at a route
/// nothing serves is the worst thing this file can do: the person who
/// was told they are needed clicks, gets a 404, and concludes the
/// feature is broken. `/dashboard/changesets/{key}` is a real client
/// route (`web/dashboard/src/App.tsx` reads `changesets/<key>`), served
/// by the SPA fallback, and it works for every signed-in member — which
/// is everyone this mail goes to, since a recipient who cannot read
/// every member repository is not on the list at all.
///
/// **When the public forge grows that address, it supersedes this one**
/// and this function is where the change goes: it is the only place a
/// changeset URL is built.
///
/// The key is percent-encoded even though `valid_change_key` admits only
/// unreserved characters, for the same reason the token links are: the
/// guarantee is one validator away from this line, and a `#` reaching a
/// mail client silently truncates the link.
pub fn changeset_url(public_url: &str, key: &str) -> String {
    format!(
        "{}/dashboard/changesets/{}",
        public_url.trim_end_matches('/'),
        urlencode(key)
    )
}

pub fn changeset_activity(m: &ChangesetActivity<'_>) -> Message {
    let ChangesetActivity {
        to,
        public_url,
        org,
        key,
        title,
        actor,
        event,
        repos,
    } = *m;
    let did = match event {
        "composed" => "composed",
        "landed" => "landed",
        // A landing that did not land. Said as its own word rather than
        // as a failure of "landed", because the two are read at a glance
        // in a subject line and the reader's next action differs.
        "failed" => "could not land",
        // The worker refuses an unknown event before it gets here; this
        // is belt and braces, and says nothing it cannot support.
        _ => "updated",
    };
    let url = changeset_url(public_url, key);
    let n = repos.len();
    let plural = if n == 1 { "repository" } else { "repositories" };
    Message {
        to: to.to_string(),
        subject: format!("[{org}] {actor} {did} changeset {key}: {title}"),
        text: format!(
            "{actor} {did} a changeset in {org}, over {n} {plural}.\n\
             \n\
             {title}\n\
             {url}\n\
             \n\
             Members:\n\
             {members}\n\
             \n\
             A changeset lands as one unit or not at all, so every member\n\
             below is waiting on every other. You are getting this because\n\
             you took part in one of these changes, or one of the OWNERS\n\
             files names you as a reviewer it needs. Change what you hear\n\
             about from the Watch control on the repository.\n",
            members = repos
                .iter()
                .map(|r| format!("  {org}/{r}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The product is called Weft. It is called Stratum only inside this
    /// repository, and a reader of our mail has never heard that name.
    ///
    /// Three bodies said "Stratum" for months while the subject lines
    /// directly above them said "Weft" — including the very first
    /// message a new account receives, "Welcome to Stratum", under the
    /// subject "Confirm your email address". Nothing failed: no test
    /// asserted on a body's prose, and the person best placed to notice
    /// is a stranger who has just signed up and now doubts whether the
    /// mail is genuine.
    ///
    /// So this walks every message the module can produce rather than
    /// naming the three that were wrong. A template added later is
    /// covered the day it is written.
    #[test]
    fn no_message_calls_the_product_by_its_repository_name() {
        let repos = ["acme/one".to_string(), "acme/two".to_string()];
        let every: Vec<Message> = vec![
            invitation(
                "a@example.com",
                "acme",
                "member",
                "https://x.example",
                "t_1",
            ),
            verification("a@example.com", "https://x.example", "t_2"),
            password_reset("a@example.com", "https://x.example", "t_3"),
            account_exists("a@example.com", "https://x.example"),
            address_verification("a@example.com", "bo", "https://x.example", "t_4"),
            change_activity(&ChangeActivity {
                to: "a@example.com",
                public_url: "https://x.example",
                org: "acme",
                repo: "one",
                change_key: "c_1",
                title: "A title",
                actor: "bo",
                event: "commented",
            }),
            changeset_activity(&ChangesetActivity {
                to: "a@example.com",
                public_url: "https://x.example",
                org: "acme",
                key: "cs_1",
                title: "A title",
                actor: "bo",
                event: "landed",
                repos: &repos,
            }),
        ];

        assert_eq!(
            every.len(),
            7,
            "a template was added or removed; add it here so it is checked too"
        );

        for m in &every {
            for (part, text) in [("subject", &m.subject), ("body", &m.text)] {
                assert!(
                    !text.contains("Stratum") && !text.contains("stratum"),
                    "the {part} of the message to {} names the repository \
                     rather than the product:\n{text}",
                    m.to
                );
            }
        }
    }

    /// The claim message must name the account that made the claim, and
    /// must not read like the signup one — the recipient may have no
    /// account here at all.
    #[test]
    fn the_address_claim_names_the_account_and_one_working_link() {
        let m = address_verification(
            "work@example.com",
            "ada",
            "https://stratum.test/",
            "work@example.com:sekret",
        );
        assert_eq!(m.to, "work@example.com");
        assert!(m.text.contains("ada"), "{}", m.text);
        assert!(
            m.text
                .contains("https://stratum.test/dashboard/#verify-email="),
            "{}",
            m.text
        );
        // The token is percent-encoded: its `:` separator would
        // otherwise be a fragment character a mail client may clip.
        assert!(m.text.contains("work%40example.com%3Asekret"), "{}", m.text);
        assert!(!m.text.contains("finish setting up"), "{}", m.text);
        m.validate().unwrap();
    }

    #[test]
    fn the_invitation_names_the_org_the_role_and_one_working_link() {
        let m = invitation(
            "new@example.com",
            "acme",
            "member",
            "https://stratum.example.com/",
            "stinv_01hx_secret",
        );
        assert_eq!(m.to, "new@example.com");
        assert_eq!(m.subject, "You've been invited to acme on Weft");
        assert!(m.text.contains("as member"));
        assert!(
            m.text
                .contains("https://stratum.example.com/dashboard/#invite=stinv_01hx_secret"),
            "{}",
            m.text
        );
        // A trailing slash on the configured base must not double up.
        assert!(!m.text.contains("com//dashboard"));
        m.validate().unwrap();
    }

    /// Both mailed tokens land on the dashboard with the credential in
    /// the fragment, and each says what redeeming it costs: the
    /// verification link says what is blocked until you click, the reset
    /// link says it ends every other session.
    #[test]
    fn the_token_messages_say_what_the_link_does() {
        let v = verification("new@example.com", "https://x.example", "weftv_1_abc");
        assert_eq!(v.subject, "Confirm your email address");
        assert!(v
            .text
            .contains("https://x.example/dashboard/#verify=weftv_1_abc"));
        assert!(v.text.contains("not create repositories"), "{}", v.text);
        assert!(v.text.contains("24 hours"), "{}", v.text);
        v.validate().unwrap();

        let r = password_reset("known@example.com", "https://x.example/", "weftrs_2_def");
        assert_eq!(r.subject, "Reset your Weft password");
        assert!(r
            .text
            .contains("https://x.example/dashboard/#reset=weftrs_2_def"));
        assert!(!r.text.contains("com//dashboard"));
        assert!(r.text.contains("every other session"), "{}", r.text);
        // The message a stranger gets must not confirm that the address
        // has an account here.
        assert!(
            r.text.contains("whether this address has an account"),
            "{}",
            r.text
        );
        r.validate().unwrap();

        // Sent when the address is already taken: it must say plainly
        // that nothing happened, and point at the two ways forward.
        let e = account_exists("known@example.com", "https://x.example/");
        assert_eq!(e.subject, "You already have a Weft account");
        assert!(e.text.contains("Nothing was created"), "{}", e.text);
        assert!(e.text.contains("https://x.example/dashboard/"));
        assert!(!e.text.contains("com//dashboard"));
        assert!(e.text.contains("reset link"), "{}", e.text);
        e.validate().unwrap();

        // Both carry exactly one link, so a reader cannot click the
        // wrong one.
        for m in [&v, &r] {
            assert_eq!(m.text.matches("https://").count(), 1, "{}", m.text);
        }
    }

    /// What a changeset mail has to say before the reader opens it: the
    /// org, the key, the title, how many repositories and which — and
    /// exactly one link.
    ///
    /// The member list is the half a single-change template cannot
    /// carry. A reader told "a change landed" who is actually looking at
    /// a four-repository landing has been told the wrong thing, and the
    /// question they ask next is whether their repository is one of the
    /// four.
    #[test]
    fn the_changeset_mail_names_the_org_the_set_and_every_repository() {
        let repos = vec!["api".to_string(), "web".to_string(), "docs".to_string()];
        let m = changeset_activity(&ChangesetActivity {
            to: "cy@acme.test",
            public_url: "https://stratum.example.com/",
            org: "acme",
            key: "CS-42",
            title: "split the auth crate",
            actor: "Ada",
            event: "composed",
            repos: &repos,
        });
        assert_eq!(m.to, "cy@acme.test");
        assert_eq!(
            m.subject,
            "[acme] Ada composed changeset CS-42: split the auth crate"
        );
        assert!(m.text.contains("over 3 repositories"), "{}", m.text);
        for r in &repos {
            assert!(
                m.text.contains(&format!("acme/{r}")),
                "the mail does not name {r}: {}",
                m.text
            );
        }
        // One link, and one only: a transactional mail with two links is
        // one where the reader clicks the wrong one.
        assert_eq!(m.text.matches("https://").count(), 1, "{}", m.text);
        assert!(
            m.text
                .contains("https://stratum.example.com/dashboard/changesets/CS-42"),
            "{}",
            m.text
        );
        // A trailing slash on the configured base must not double up.
        assert!(!m.text.contains("com//dashboard"));
        m.validate().unwrap();

        // One member reads as one, not as "1 repositories".
        let one = vec!["api".to_string()];
        let m = changeset_activity(&ChangesetActivity {
            to: "cy@acme.test",
            public_url: "https://stratum.example.com",
            org: "acme",
            key: "CS-42",
            title: "split the auth crate",
            actor: "Ada",
            event: "landed",
            repos: &one,
        });
        assert!(m.text.contains("over 1 repository"), "{}", m.text);
        assert!(
            m.subject.contains("landed changeset CS-42"),
            "{}",
            m.subject
        );

        // A landing that did not land says so as its own word: the
        // reader's next action is different, and they decide it from the
        // subject line.
        let m = changeset_activity(&ChangesetActivity {
            to: "cy@acme.test",
            public_url: "https://stratum.example.com",
            org: "acme",
            key: "CS-42",
            title: "split the auth crate",
            actor: "the lander",
            event: "failed",
            repos: &one,
        });
        assert!(
            m.subject.contains("could not land changeset CS-42"),
            "{}",
            m.subject
        );
    }

    /// The address a changeset mail points at is built in one place, and
    /// it is the one the product serves today.
    ///
    /// `/{org}/changesets/{key}` on the public forge does not exist yet;
    /// `/dashboard/changesets/{key}` does. This is the unit half of that
    /// claim — that the URL has the dashboard's shape and never the
    /// forge's — and `changeset_notify_e2e` is the half that asks a
    /// running server for it.
    #[test]
    fn a_changeset_link_points_at_a_route_that_exists() {
        let url = changeset_url("https://x.example/", "CS-1");
        assert_eq!(url, "https://x.example/dashboard/changesets/CS-1");
        // Not the forge address, which would 404 for everybody.
        assert!(!url.contains("/acme/changesets/"), "{url}");
        // A key with anything the fragment or the path would eat is
        // encoded rather than truncating the link.
        assert_eq!(
            changeset_url("https://x.example", "a b#c/d"),
            "https://x.example/dashboard/changesets/a%20b%23c%2Fd"
        );
    }

    /// An event this build does not know is still a sendable message.
    ///
    /// The worker refuses an unknown event and fails the job, so nothing
    /// in the running product reaches the fallback arm — but "nothing
    /// reaches it" is a property of the *current* pair of files, and the
    /// pair is exactly what a rolling deploy separates: a newer node
    /// enqueues an event an older node has never heard of. What must not
    /// happen then is a panic or a subject line with a hole in it, and
    /// what this pins is that the fallback says something true and
    /// nothing it cannot support — no verb claiming the changeset
    /// landed, and a message that still passes the header check every
    /// transport applies.
    #[test]
    fn an_event_this_build_does_not_know_still_reads_as_a_sentence() {
        let repos = vec!["app".to_string(), "lib".to_string()];
        let m = changeset_activity(&ChangesetActivity {
            to: "bo@acme.test",
            public_url: "https://x.example",
            org: "acme",
            key: "CS-1",
            title: "split the auth crate",
            event: "teleported",
            actor: "Ada",
            repos: &repos,
        });
        assert_eq!(
            m.subject,
            "[acme] Ada updated changeset CS-1: split the auth crate"
        );
        // Never a word that claims an outcome: "updated" is the only
        // thing an unknown event supports, and reading "landed" out of
        // one would tell somebody their review is over when it is not.
        for claim in ["landed", "could not land", "composed"] {
            assert!(
                !m.subject.contains(claim),
                "an unknown event claimed {claim:?}: {}",
                m.subject
            );
        }
        assert!(m.text.contains("over 2 repositories"), "{}", m.text);
        assert!(
            m.text
                .contains("https://x.example/dashboard/changesets/CS-1"),
            "{}",
            m.text
        );
        m.validate().expect("the fallback still sends");
    }

    /// The token is the credential; anything in it that would end the
    /// fragment early has to be encoded.
    #[test]
    fn the_token_is_percent_encoded_into_the_fragment() {
        let url = invite_url("https://x.example", "a b&c#d/e+f%g");
        assert_eq!(
            url,
            "https://x.example/dashboard/#invite=a%20b%26c%23d%2Fe%2Bf%25g"
        );
        // The ordinary token shape passes through unchanged.
        assert_eq!(
            invite_url("https://x.example", "stinv_01HX_abc-def.ghi~jkl"),
            "https://x.example/dashboard/#invite=stinv_01HX_abc-def.ghi~jkl"
        );
    }
}
