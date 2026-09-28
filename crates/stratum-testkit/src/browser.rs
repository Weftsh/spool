//! A signed-in browser: holds the session cookie and replays it.
//!
//! Sessions are what the dashboard uses, so any suite that exercises a
//! route the dashboard calls needs one of these. [`crate::Server::req`]
//! and friends carry a Bearer token instead, which is a different
//! principal with different scopes — using one to test the other is how
//! a test ends up proving something nobody does.

use crate::Server;

/// The password of every account [`Browser::person`] and
/// [`Browser::stranger`] make.
pub const PASSWORD: &str = "a long enough password";

pub struct Browser<'a> {
    server: &'a Server,
    /// Public so a suite can forge one — a session cookie is a bearer
    /// credential, and "what happens when somebody presents a made-up
    /// one" is a test that has to be able to make one up.
    pub cookie: Option<String>,
}

impl<'a> Browser<'a> {
    pub fn new(server: &'a Server) -> Browser<'a> {
        Browser {
            server,
            cookie: None,
        }
    }

    pub fn req(
        &mut self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let mut r = ureq::request(method, &format!("{}{path}", self.server.base));
        if let Some(c) = &self.cookie {
            r = r.set("Cookie", c);
        }
        if body.is_some() {
            r = r.set("Content-Type", "application/json");
        }
        let resp = match body {
            Some(b) => r.send_string(&b.to_string()),
            None => r.call(),
        };
        let resp = match resp {
            Ok(x) => x,
            Err(ureq::Error::Status(_, x)) => x,
            Err(e) => panic!("transport {method} {path}: {e}"),
        };
        let status = resp.status();
        // Keep whatever the server set, including a clearing cookie.
        if let Some(set) = resp.header("set-cookie") {
            self.cookie = set.split(';').next().map(str::to_string);
        }
        let text = resp.into_string().unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
        )
    }

    pub fn login(&mut self, email: &str, password: &str) -> u16 {
        self.req(
            "POST",
            "/v1/auth/login",
            Some(serde_json::json!({ "email": email, "password": password })),
        )
        .0
    }

    /// Sign in, asserting it worked. Most tests only care about the
    /// session that follows, and a silent 401 here surfaces as a
    /// confusing 404 several assertions later.
    pub fn signed_in(server: &'a Server, email: &str, password: &str) -> Browser<'a> {
        let mut b = Browser::new(server);
        let st = b.login(email, password);
        assert_eq!(st, 200, "sign in as {email}");
        b
    }

    /// Somebody made the way this server makes people — an operator's
    /// `admin user-create` at `role` in `org`, which must exist, with
    /// `handle` as their personal namespace — and signed in with
    /// [`PASSWORD`].
    ///
    /// There is no signing yourself up: an account comes from an
    /// operator or from an invitation, and this is the operator's door.
    /// For somebody who must have no role in the organisation under
    /// test, see [`Browser::stranger`].
    pub fn person(
        server: &'a Server,
        org: &str,
        role: &str,
        handle: &str,
        email: &str,
    ) -> Browser<'a> {
        server.admin_json(&[
            "admin",
            "user-create",
            "--org",
            org,
            "--role",
            role,
            "--email",
            email,
            "--name",
            handle,
            "--password",
            PASSWORD,
            "--handle",
            handle,
        ]);
        Browser::signed_in(server, email, PASSWORD)
    }

    /// A person with no role anywhere but their own: an organisation
    /// made for them, `{handle}-org`, and their personal namespace
    /// `handle`. The stranger a masked 404, a fork gate or a membership
    /// check is about.
    pub fn stranger(server: &'a Server, handle: &str, email: &str) -> Browser<'a> {
        let org = format!("{handle}-org");
        server.bootstrap_org(&org);
        Browser::person(server, &org, "owner", handle, email)
    }

    /// Invite `email` into `org` as `role` from this session, which must
    /// be an admin of `org`, and accept the invitation from a fresh
    /// browser. The link is the credential: a person who already has an
    /// account needs no password, and one who does not is made with
    /// [`PASSWORD`] and a handle made from their address.
    ///
    /// Every repository is private to its organisation, so this is how a
    /// second person comes to read one: to fork it, to open a change
    /// against it, to be the stranger a fork gate is about.
    pub fn invite_and_accept(&mut self, org: &str, email: &str, role: &str) {
        let (st, inv) = self.req(
            "POST",
            &format!("/v1/orgs/{org}/invites"),
            Some(serde_json::json!({ "email": email, "role": role })),
        );
        assert_eq!(st, 201, "invite {email} into {org}: {inv}");
        let link = inv["invite_link"]
            .as_str()
            .unwrap_or_else(|| panic!("the invite carries its link: {inv}"))
            .to_string();
        let mut them = Browser::new(self.server);
        let (st, out) = them.req(
            "POST",
            "/v1/auth/accept-invite",
            Some(serde_json::json!({ "invite": link, "name": email, "password": PASSWORD })),
        );
        assert_eq!(st, 201, "{email} accepting the invite into {org}: {out}");
    }
}
