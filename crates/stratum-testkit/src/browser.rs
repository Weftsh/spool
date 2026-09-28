//! A signed-in browser: holds the session cookie and replays it.
//!
//! Sessions are what the dashboard uses, so any suite that exercises a
//! route the dashboard calls needs one of these. [`crate::Server::req`]
//! and friends carry a Bearer token instead, which is a different
//! principal with different scopes — using one to test the other is how
//! a test ends up proving something nobody does.

use crate::Server;

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

    /// Invite `email` into `org` as `role` from this session, which must
    /// be an admin of `org`, and accept the invitation from a fresh
    /// browser — the link is the credential, so no password is needed
    /// for a person who already has an account.
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
            Some(serde_json::json!({ "invite": link, "name": email })),
        );
        assert_eq!(st, 201, "{email} accepting the invite into {org}: {out}");
    }
}
