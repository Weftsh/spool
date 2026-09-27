//! Teams end to end against a real server.
//!
//! The rules are the point here, and they are asserted the way a person
//! would find out: by making a request with a credential and seeing what
//! it is allowed to do. A team grant must *raise* someone's access on the
//! very next request, a grant naming them personally must beat it in
//! either direction, and deleting the team must take the access with it —
//! all observed through the API, not through the tables.

use stratum_testkit::adversarial::{percent_encode, INJECTIONS};
use stratum_testkit::{gitcli::Scratch, Minio, Server};

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("teams-e2e")
        .data_dir(scratch.path().join("data"))
        .start()
}

struct World {
    server: Server,
    /// An org-admin service token: what an operator holds.
    admin: String,
    /// Vic's session cookie. A person's authority is resolved on every
    /// request, so this is the credential that shows a grant landing —
    /// a token minted before the grant would answer the older question.
    vic: String,
    vic_id: String,
    repo: String,
}

fn make_user(server: &Server, admin: &str, email: &str, name: &str, role: &str) -> String {
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            email,
            "--name",
            name,
            "--password",
            PASSWORD,
            "--role",
            role,
        ])
        .unwrap_or_else(|e| panic!("user-create {email}: {e}"));
    let (st, members) = server.get("/v1/orgs/acme/members", admin);
    assert_eq!(st, 200, "{members}");
    members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == email)
        .unwrap_or_else(|| panic!("{email} is a member"))["user_id"]
        .as_str()
        .unwrap()
        .to_string()
}

const PASSWORD: &str = "a long enough password";

/// Sign in and keep the cookie.
fn sign_in(server: &Server, email: &str) -> String {
    let resp = ureq::post(&format!("{}/v1/auth/login", server.base))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({"email": email, "password": PASSWORD}).to_string())
        .unwrap_or_else(|e| panic!("login {email}: {e}"));
    resp.header("set-cookie")
        .and_then(|c| c.split(';').next())
        .expect("a session cookie")
        .to_string()
}

/// A request carrying a session cookie rather than a bearer token.
fn as_person(
    server: &Server,
    cookie: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut r = ureq::request(method, &format!("{}{path}", server.base)).set("Cookie", cookie);
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
    let text = resp.into_string().unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

/// An org with a repo, an owner, and a signed-in viewer.
fn world(server: Server) -> World {
    let admin = server.bootstrap_org("acme");
    let vic_id = make_user(&server, &admin, "vic@acme.test", "Vic Viewer", "viewer");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin,
                Some(serde_json::json!({"name": "app"}))
            )
            .0,
        201
    );
    let vic = sign_in(&server, "vic@acme.test");
    World {
        server,
        admin,
        vic,
        vic_id,
        repo: "app".to_string(),
    }
}

impl World {
    fn team(&self, name: &str) -> String {
        let (st, t) = self.server.post(
            "/v1/orgs/acme/teams",
            &self.admin,
            Some(serde_json::json!({"name": name})),
        );
        assert_eq!(st, 201, "{t}");
        t["id"].as_str().unwrap().to_string()
    }

    fn join(&self, team: &str, user: &str) {
        let (st, b) = self.server.req(
            "PUT",
            &format!("/v1/orgs/acme/teams/{team}/members/{user}"),
            &self.admin,
            None,
        );
        assert_eq!(st, 204, "{b}");
    }

    fn grant_team(&self, team: &str, role: &str) {
        let (st, b) = self.server.post(
            &format!("/v1/orgs/acme/repos/{}/grants", self.repo),
            &self.admin,
            Some(serde_json::json!({"team_id": team, "role": role})),
        );
        assert_eq!(st, 204, "{b}");
    }

    fn vic(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        as_person(&self.server, &self.vic, method, path, body)
    }

    /// What Vic can actually do on the repo, observed rather than read
    /// out of a table: can they write a commit to it?
    fn vic_can_write(&self) -> bool {
        self.vic(
            "POST",
            &format!("/v1/orgs/acme/repos/{}/commits", self.repo),
            Some(serde_json::json!({
                "message": "probe",
                "operations": [{"op": "put", "path": "probe.txt", "content": "x"}]
            })),
        )
        .0 == 201
    }

    fn vic_can_read(&self) -> bool {
        self.vic("GET", &format!("/v1/orgs/acme/repos/{}", self.repo), None)
            .0
            == 200
    }
}

#[test]
fn a_team_grant_reaches_a_personal_token_on_the_next_request() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-raise");
    let scratch = Scratch::new("teams-raise");
    let w = world(spawn_server(&bucket.base_url, &scratch));

    // A viewer reads and does not write, whatever their token says.
    assert!(w.vic_can_read());
    assert!(!w.vic_can_write(), "a viewer must not write");

    let t = w.team("payments");
    w.join(&t, &w.vic_id);
    // In the team, but the team has no grant yet: still a viewer.
    assert!(!w.vic_can_write(), "team membership alone grants nothing");

    w.grant_team(&t, "member");
    assert!(
        w.vic_can_write(),
        "a team grant must raise on the very next request"
    );

    // A second team granting less must not take it away.
    let low = w.team("readers");
    w.join(&low, &w.vic_id);
    w.grant_team(&low, "viewer");
    assert!(
        w.vic_can_write(),
        "two teams disagreeing must take the higher"
    );

    // Leaving the raising team drops it back, at once.
    let (st, _) = w.server.req(
        "DELETE",
        &format!("/v1/orgs/acme/teams/{t}/members/{}", w.vic_id),
        &w.admin,
        None,
    );
    assert_eq!(st, 204);
    assert!(!w.vic_can_write(), "leaving the team must withdraw it");
    assert!(w.vic_can_read(), "and must not take the org role with it");
    assert!(w.server.healthy());
}

#[test]
fn a_grant_naming_the_person_beats_the_team_either_way() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-direct");
    let scratch = Scratch::new("teams-direct");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let t = w.team("payments");
    w.join(&t, &w.vic_id);
    w.grant_team(&t, "admin");
    assert!(w.vic_can_write());

    // Named personally as a viewer: held down, despite the team.
    let (st, b) = w.server.post(
        "/v1/orgs/acme/repos/app/grants",
        &w.admin,
        Some(serde_json::json!({"user_id": w.vic_id, "role": "viewer"})),
    );
    assert_eq!(st, 204, "{b}");
    assert!(
        !w.vic_can_write(),
        "a grant naming the person must win over their team's"
    );
    assert!(w.vic_can_read());

    // Revoking the personal grant lets the team's raise apply again.
    let (st, _) = w.server.delete(
        &format!("/v1/orgs/acme/repos/app/grants/{}", w.vic_id),
        &w.admin,
    );
    assert_eq!(st, 204);
    assert!(w.vic_can_write());
    assert!(w.server.healthy());
}

#[test]
fn one_grant_call_can_name_several_people_at_once() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-multi");
    let scratch = Scratch::new("teams-multi");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    // A second person to grant alongside Vic.
    w.server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "bo@acme.test",
            "--name",
            "Bo",
            "--password",
            "a long enough password",
            "--role",
            "viewer",
        ])
        .unwrap();
    let (_, members) = w.server.get("/v1/orgs/acme/members", &w.admin);
    let bo = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == "bo@acme.test")
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();

    // One id in the list that is not a member: nothing may take effect.
    let (st, b) = w.server.post(
        "/v1/orgs/acme/repos/app/grants",
        &w.admin,
        Some(serde_json::json!({
            "user_ids": [w.vic_id, bo, "01nobodynobodynobodynobody"], "role": "member"
        })),
    );
    assert_eq!(st, 400, "{b}");
    assert!(
        !w.vic_can_write(),
        "a rejected batch must not have granted the ids that were valid"
    );

    // The same call without the typo applies to everyone in it.
    let (st, b) = w.server.post(
        "/v1/orgs/acme/repos/app/grants",
        &w.admin,
        Some(serde_json::json!({"user_ids": [w.vic_id, bo], "role": "member"})),
    );
    assert_eq!(st, 204, "{b}");
    assert!(w.vic_can_write());

    // People and a team in one body is a refusal, not a guess: they are
    // different rules and the caller has to say which they meant.
    let t = w.team("payments");
    let (st, b) = w.server.post(
        "/v1/orgs/acme/repos/app/grants",
        &w.admin,
        Some(serde_json::json!({"user_ids": [w.vic_id], "team_id": t, "role": "member"})),
    );
    assert_eq!(st, 400, "{b}");
    // And a body naming nobody at all.
    let (st, b) = w.server.post(
        "/v1/orgs/acme/repos/app/grants",
        &w.admin,
        Some(serde_json::json!({"role": "member"})),
    );
    assert_eq!(st, 400, "{b}");
    assert!(w.server.healthy());
}

#[test]
fn deleting_a_team_withdraws_its_access_at_once() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-delete");
    let scratch = Scratch::new("teams-delete");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let t = w.team("payments");
    w.join(&t, &w.vic_id);
    w.grant_team(&t, "member");
    assert!(w.vic_can_write());

    let (st, _) = w
        .server
        .delete(&format!("/v1/orgs/acme/teams/{t}"), &w.admin);
    assert_eq!(st, 204);
    assert!(
        !w.vic_can_write(),
        "access must go with the team on the next request"
    );
    assert!(w.vic_can_read());
    // Gone, and gone twice is a 404 rather than an error.
    assert_eq!(
        w.server
            .delete(&format!("/v1/orgs/acme/teams/{t}"), &w.admin)
            .0,
        404
    );
    assert_eq!(
        w.server.get("/v1/orgs/acme/teams", &w.admin).1["teams"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // Revoking a team grant without deleting the team works too.
    let t2 = w.team("infra");
    w.join(&t2, &w.vic_id);
    w.grant_team(&t2, "member");
    assert!(w.vic_can_write());
    let (st, _) = w.server.delete(
        &format!("/v1/orgs/acme/repos/app/team-grants/{t2}"),
        &w.admin,
    );
    assert_eq!(st, 204);
    assert!(!w.vic_can_write());
    assert_eq!(
        w.server
            .delete(
                &format!("/v1/orgs/acme/repos/app/team-grants/{t2}"),
                &w.admin
            )
            .0,
        404
    );
    assert!(w.server.healthy());
}

#[test]
fn a_team_cannot_reach_out_of_its_org() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-orgs");
    let scratch = Scratch::new("teams-orgs");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let other = w.server.bootstrap_org("other");
    assert_eq!(
        w.server
            .post(
                "/v1/orgs/other/repos",
                &other,
                Some(serde_json::json!({"name": "theirs"}))
            )
            .0,
        201
    );
    let t = w.team("payments");

    // The other org's admin cannot see, rename, delete or grant our team.
    assert_eq!(
        w.server
            .get(&format!("/v1/orgs/other/teams/{t}/members"), &other)
            .0,
        404
    );
    assert_eq!(
        w.server
            .req(
                "PATCH",
                &format!("/v1/orgs/other/teams/{t}"),
                &other,
                Some(serde_json::json!({"name": "theirs"}))
            )
            .0,
        404
    );
    assert_eq!(
        w.server
            .delete(&format!("/v1/orgs/other/teams/{t}"), &other)
            .0,
        404
    );
    // The one mistake this table makes possible: our team on their repo.
    let (st, b) = w.server.post(
        "/v1/orgs/other/repos/theirs/grants",
        &other,
        Some(serde_json::json!({"team_id": t, "role": "member"})),
    );
    assert_eq!(
        st, 400,
        "one org's team must not be granted on another's repo: {b}"
    );

    // And our own admin token cannot reach their org at all.
    assert_eq!(w.server.get("/v1/orgs/other/teams", &w.admin).0, 404);
    assert!(w.server.healthy());
}

#[test]
fn only_an_admin_may_shape_teams_or_read_the_access_map() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-authz");
    let scratch = Scratch::new("teams-authz");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let t = w.team("payments");

    // A viewer may see that teams exist — they have to, to ask to join.
    let (st, list) = w.vic("GET", "/v1/orgs/acme/teams", None);
    assert_eq!(st, 200, "{list}");
    assert_eq!(list["teams"][0]["name"], "payments");
    assert_eq!(
        w.vic("GET", &format!("/v1/orgs/acme/teams/{t}/members"), None)
            .0,
        200
    );

    // …and may change nothing.
    for (method, path, body) in [
        (
            "POST",
            "/v1/orgs/acme/teams".to_string(),
            Some(serde_json::json!({"name": "mine"})),
        ),
        (
            "PATCH",
            format!("/v1/orgs/acme/teams/{t}"),
            Some(serde_json::json!({"name": "mine"})),
        ),
        ("DELETE", format!("/v1/orgs/acme/teams/{t}"), None),
        (
            "PUT",
            format!("/v1/orgs/acme/teams/{t}/members/{}", w.vic_id),
            None,
        ),
        (
            "POST",
            "/v1/orgs/acme/repos/app/grants".to_string(),
            Some(serde_json::json!({"team_id": t, "role": "admin"})),
        ),
    ] {
        let (st, b) = w.vic(method, &path, body);
        // Masked, not forbidden: `authx::require` answers 404 when the
        // authority is short, so an org's shape is not readable by
        // probing it. Same answer a stranger gets.
        assert_eq!(st, 404, "{method} {path} answered {st}: {b}");
    }

    // The access map is the org's roster crossed with its repos, so it
    // is administrative — a member must not enumerate people through it.
    assert_eq!(w.vic("GET", "/v1/orgs/acme/repos/app/access", None).0, 404);
    // Anonymous reaches none of it.
    for path in [
        "/v1/orgs/acme/teams",
        "/v1/orgs/acme/repos/app/access",
        &format!("/v1/orgs/acme/teams/{t}/members"),
    ] {
        assert_eq!(w.server.get(path, "").0, 401, "{path}");
    }
    assert!(w.server.healthy());
}

#[test]
fn the_access_map_says_where_each_persons_access_came_from() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-access");
    let scratch = Scratch::new("teams-access");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let t = w.team("payments");
    w.join(&t, &w.vic_id);
    w.grant_team(&t, "member");

    let (st, map) = w.server.get("/v1/orgs/acme/repos/app/access", &w.admin);
    assert_eq!(st, 200, "{map}");
    let vic = map["people"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["user_id"] == w.vic_id.as_str())
        .expect("the viewer reaches this repo");
    assert_eq!(vic["role"], "member");
    assert_eq!(vic["source"], "team");
    assert_eq!(vic["team_name"], "payments");
    assert_eq!(map["teams"][0]["team_name"], "payments");
    assert_eq!(map["teams"][0]["member_count"], 1);

    // Naming them personally changes what the map says decided it.
    let (st, _) = w.server.post(
        "/v1/orgs/acme/repos/app/grants",
        &w.admin,
        Some(serde_json::json!({"user_id": w.vic_id, "role": "viewer"})),
    );
    assert_eq!(st, 204);
    let (_, map) = w.server.get("/v1/orgs/acme/repos/app/access", &w.admin);
    let vic = map["people"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["user_id"] == w.vic_id.as_str())
        .unwrap();
    assert_eq!(
        (&vic["role"], &vic["source"]),
        (&"viewer".into(), &"direct_grant".into())
    );
    assert!(w.server.healthy());
}

#[test]
fn team_names_are_validated_unique_and_inert_to_hostile_input() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-names");
    let scratch = Scratch::new("teams-names");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    w.team("payments");
    assert_eq!(
        w.server
            .post(
                "/v1/orgs/acme/teams",
                &w.admin,
                Some(serde_json::json!({"name": "payments"}))
            )
            .0,
        400
    );
    assert_eq!(
        w.server
            .post(
                "/v1/orgs/acme/teams",
                &w.admin,
                Some(serde_json::json!({"name": "Payments"}))
            )
            .0,
        400,
        "case must not make a second team of the same name"
    );
    // A body with no name at all.
    assert_eq!(
        w.server
            .post("/v1/orgs/acme/teams", &w.admin, Some(serde_json::json!({})))
            .0,
        400
    );

    // Hostile names are refused; hostile ids in the path name nothing.
    for inj in INJECTIONS {
        let (st, b) = w.server.post(
            "/v1/orgs/acme/teams",
            &w.admin,
            Some(serde_json::json!({"name": inj})),
        );
        assert_eq!(st, 400, "team name {inj:?} was accepted: {b}");

        let id = percent_encode(inj);
        for (method, path) in [
            ("GET", format!("/v1/orgs/acme/teams/{id}/members")),
            ("DELETE", format!("/v1/orgs/acme/teams/{id}")),
            (
                "DELETE",
                format!("/v1/orgs/acme/repos/app/team-grants/{id}"),
            ),
        ] {
            let (st, b) = w.server.req(method, &path, &w.admin, None);
            assert!(st == 404 || st == 400, "{method} {path} answered {st}: {b}");
        }
        let (st, b) = w.server.post(
            "/v1/orgs/acme/repos/app/grants",
            &w.admin,
            Some(serde_json::json!({"team_id": inj, "role": "member"})),
        );
        assert_eq!(st, 400, "team_id {inj:?} answered {st}: {b}");
    }

    // An unknown role is refused before anything is written.
    assert_eq!(
        w.server
            .post(
                "/v1/orgs/acme/repos/app/grants",
                &w.admin,
                Some(serde_json::json!({"user_id": w.vic_id, "role": "root"}))
            )
            .0,
        400
    );
    assert!(w.server.healthy());
}

#[test]
fn a_personal_tokens_ceiling_follows_a_team_granted_raise() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-ceiling");
    let scratch = Scratch::new("teams-ceiling");
    let w = world(spawn_server(&bucket.base_url, &scratch));

    // What a viewer is offered to mint with, and what they are not.
    let (st, mine) = w.vic("GET", "/v1/orgs/acme/tokens", None);
    assert_eq!(st, 200, "{mine}");
    let offered: Vec<String> = mine["mintable_scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    assert!(!offered.contains(&"repo:write".to_string()), "{offered:?}");

    // Asking for it anyway is refused outright rather than quietly
    // narrowed: a credential that does less than it says is worse than
    // no credential.
    let (st, refused) = w.vic(
        "POST",
        "/v1/orgs/acme/tokens",
        Some(serde_json::json!({"scopes": ["repo:write"], "label": "own"})),
    );
    assert_eq!(st, 400, "a viewer must not mint write: {refused}");

    // A team grant lifts the ceiling, and now the same request works.
    let t = w.team("payments");
    w.join(&t, &w.vic_id);
    w.grant_team(&t, "member");
    let (st, mine) = w.vic("GET", "/v1/orgs/acme/tokens", None);
    assert_eq!(st, 200, "{mine}");
    assert!(
        mine["mintable_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "repo:write"),
        "a team-granted raise must reach the mint ceiling: {mine}"
    );
    let (st, minted) = w.vic(
        "POST",
        "/v1/orgs/acme/tokens",
        Some(serde_json::json!({"scopes": ["repo:write"], "label": "own"})),
    );
    assert_eq!(st, 201, "{minted}");
    let raised = minted["token"].as_str().expect("a token was minted");

    // And the token really writes — on the granted repo.
    assert_eq!(
        w.server
            .post(
                "/v1/orgs/acme/repos/app/commits",
                raised,
                Some(serde_json::json!({
                    "message": "probe",
                    "operations": [{"op": "put", "path": "p.txt", "content": "x"}]
                }))
            )
            .0,
        201,
        "someone whose only write access is a team's must be able to \
         mint a token that writes"
    );

    // …and only there. A repo the team was never granted stays read-only
    // even though the token carries repo:write, because the ceiling is
    // not the answer — the role on the repo is.
    assert_eq!(
        w.server
            .post(
                "/v1/orgs/acme/repos",
                &w.admin,
                Some(serde_json::json!({"name": "secret"}))
            )
            .0,
        201
    );
    assert_ne!(
        w.server
            .post(
                "/v1/orgs/acme/repos/secret/commits",
                raised,
                Some(serde_json::json!({
                    "message": "probe",
                    "operations": [{"op": "put", "path": "p.txt", "content": "x"}]
                }))
            )
            .0,
        201,
        "a team grant on one repo must not carry to another"
    );
    assert!(w.server.healthy());
}

/// The rest of the surface: the successful shapes the other tests do not
/// reach, and the refusals every route owes an unknown org or a caller
/// with no credential at all.
#[test]
fn every_team_route_answers_an_unknown_org_and_an_unknown_subject() {
    let minio = Minio::shared();
    let bucket = minio.bucket("teams-surface");
    let scratch = Scratch::new("teams-surface");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let t = w.team("payments");
    w.join(&t, &w.vic_id);

    // Renaming, and changing a description without touching the name.
    let (st, b) = w.server.req(
        "PATCH",
        &format!("/v1/orgs/acme/teams/{t}"),
        &w.admin,
        Some(serde_json::json!({"name": "platform"})),
    );
    assert_eq!(st, 204, "{b}");
    let (_, list) = w.server.get("/v1/orgs/acme/teams", &w.admin);
    assert_eq!(list["teams"][0]["name"], "platform");
    let (st, _) = w.server.req(
        "PATCH",
        &format!("/v1/orgs/acme/teams/{t}"),
        &w.admin,
        Some(serde_json::json!({"description": "keeps the lights on"})),
    );
    assert_eq!(st, 204);
    let (_, list) = w.server.get("/v1/orgs/acme/teams", &w.admin);
    assert_eq!(
        list["teams"][0]["name"], "platform",
        "a description change must not rename it"
    );
    assert_eq!(list["teams"][0]["description"], "keeps the lights on");
    assert_eq!(list["teams"][0]["member_count"], 1);

    // Renaming onto a name already taken is refused, and leaves the
    // team as it was rather than half-applying.
    w.team("infra");
    let (st, b) = w.server.req(
        "PATCH",
        &format!("/v1/orgs/acme/teams/{t}"),
        &w.admin,
        Some(serde_json::json!({"name": "infra"})),
    );
    assert_eq!(st, 400, "{b}");
    // …as is a shape no name may have.
    let (st, b) = w.server.req(
        "PATCH",
        &format!("/v1/orgs/acme/teams/{t}"),
        &w.admin,
        Some(serde_json::json!({"name": "has space"})),
    );
    assert_eq!(st, 400, "{b}");
    let (_, list) = w.server.get("/v1/orgs/acme/teams", &w.admin);
    let names: Vec<&str> = list["teams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["infra", "platform"]);

    // The roster carries the fields the UI draws with.
    let (st, roster) = w
        .server
        .get(&format!("/v1/orgs/acme/teams/{t}/members"), &w.admin);
    assert_eq!(st, 200, "{roster}");
    assert_eq!(roster["members"][0]["email"], "vic@acme.test");
    assert_eq!(roster["members"][0]["name"], "Vic Viewer");
    assert_eq!(roster["members"][0]["user_id"], w.vic_id.as_str());
    assert!(roster["members"][0]["created_at"].is_i64());

    // Somebody who is not in the org cannot be put in one of its teams.
    let other = w.server.bootstrap_org("other");
    w.server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "other",
            "--email",
            "stranger@other.test",
            "--name",
            "Stranger",
            "--password",
            PASSWORD,
            "--role",
            "member",
        ])
        .unwrap();
    let (_, theirs) = w.server.get("/v1/orgs/other/members", &other);
    let stranger = theirs["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == "stranger@other.test")
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, b) = w.server.req(
        "PUT",
        &format!("/v1/orgs/acme/teams/{t}/members/{stranger}"),
        &w.admin,
        None,
    );
    assert_eq!(st, 400, "a stranger must not join an org's team: {b}");

    // Taking out somebody who was never in is a 404, not a success.
    assert_eq!(
        w.server
            .delete(
                &format!("/v1/orgs/acme/teams/{t}/members/{stranger}"),
                &w.admin
            )
            .0,
        404
    );

    // A grant naming more people than any real org has is refused
    // outright rather than walked one row at a time.
    let many: Vec<String> = (0..201).map(|_| w.vic_id.clone()).collect();
    let (st, b) = w.server.post(
        "/v1/orgs/acme/repos/app/grants",
        &w.admin,
        Some(serde_json::json!({"user_ids": many, "role": "member"})),
    );
    assert_eq!(st, 400, "{b}");

    // Every route answers an org that does not exist the same way it
    // answers one you may not see: 404, with nothing to learn from it.
    for (method, path, body) in [
        ("GET", "/v1/orgs/ghost/teams".to_string(), None),
        (
            "POST",
            "/v1/orgs/ghost/teams".to_string(),
            Some(serde_json::json!({"name": "x"})),
        ),
        (
            "PATCH",
            format!("/v1/orgs/ghost/teams/{t}"),
            Some(serde_json::json!({"name": "x"})),
        ),
        ("DELETE", format!("/v1/orgs/ghost/teams/{t}"), None),
        ("GET", format!("/v1/orgs/ghost/teams/{t}/members"), None),
        (
            "PUT",
            format!("/v1/orgs/ghost/teams/{t}/members/{}", w.vic_id),
            None,
        ),
        (
            "DELETE",
            format!("/v1/orgs/ghost/teams/{t}/members/{}", w.vic_id),
            None,
        ),
        ("GET", "/v1/orgs/ghost/repos/app/access".to_string(), None),
        (
            "POST",
            "/v1/orgs/ghost/repos/app/grants".to_string(),
            Some(serde_json::json!({"team_id": t, "role": "member"})),
        ),
        (
            "DELETE",
            format!("/v1/orgs/ghost/repos/app/team-grants/{t}"),
            None,
        ),
    ] {
        let (st, b) = w.server.req(method, &path, &w.admin, body.clone());
        assert_eq!(st, 404, "{method} {path} answered {st}: {b}");
        // The same route on an org that *does* exist asks for a
        // credential rather than answering. (An unknown org answers 404
        // even anonymously, because the org is resolved before the
        // credential is — the org-level twin of the repo existence
        // oracle, which is a separate piece of work.)
        let real = path.replace("/orgs/ghost/", "/orgs/acme/");
        let (st, b) = w.server.req(method, &real, "", body);
        assert_eq!(st, 401, "anonymous {method} {real} answered {st}: {b}");
    }

    // A repo that does not exist, on an org that does.
    for (method, path, body) in [
        ("GET", "/v1/orgs/acme/repos/ghost/access".to_string(), None),
        (
            "POST",
            "/v1/orgs/acme/repos/ghost/grants".to_string(),
            Some(serde_json::json!({"team_id": t, "role": "member"})),
        ),
        (
            "DELETE",
            format!("/v1/orgs/acme/repos/ghost/team-grants/{t}"),
            None,
        ),
    ] {
        let (st, b) = w.server.req(method, &path, &w.admin, body);
        assert_eq!(st, 404, "{method} {path} answered {st}: {b}");
    }
    assert!(w.server.healthy());
}

/// Checking an origin before creating a mirror against it.
///
/// This endpoint fetches a URL a stranger supplies, so most of what is
/// asserted here is what it *refuses*: anything but https, anything
/// resolving inside the network, and anyone below `org:admin`.
#[test]
fn an_origin_is_checked_before_a_mirror_is_created_against_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("origin-probe");
    let scratch = Scratch::new("origin-probe");
    let w = world(spawn_server(&bucket.base_url, &scratch));
    let probe = |origin: &str| -> (u16, serde_json::Value) {
        w.server.post(
            "/v1/orgs/acme/origins/probe",
            &w.admin,
            Some(serde_json::json!({ "origin": origin })),
        )
    };

    // Nothing inside the network, however the URL is dressed up. The
    // answer is 200 with `reachable: false` — the probe worked, and its
    // finding is the body.
    for hostile in [
        "https://127.0.0.1/acme/widget",
        "https://localhost/acme/widget",
        "https://169.254.169.254/latest/meta-data",
        "https://[::1]/acme/widget",
        "https://10.0.0.1/a/b",
        "https://192.168.1.1/a/b",
        "file:///etc/passwd",
        "git://github.com/acme/widget",
        "ftp://example.com/a/b",
        "https://user:hunter2@github.com/acme/widget",
    ] {
        let (st, body) = probe(hostile);
        assert_eq!(st, 200, "{hostile} → {st}: {body}");
        assert_eq!(body["reachable"], false, "{hostile} was probed: {body}");
        assert!(
            body["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "{hostile} refused without saying why: {body}"
        );
        // A refusal must not read as "private" — that would send someone
        // down the connect-GitHub path for a URL that will never work.
        assert_eq!(body["private"], false, "{hostile}: {body}");
    }

    // Hostile bytes in the field are a refusal, not a 500.
    for inj in INJECTIONS {
        let (st, body) = probe(inj);
        assert_eq!(st, 200, "{inj:?} → {st}: {body}");
        assert_eq!(body["reachable"], false, "{inj:?}: {body}");
    }

    // A host that does not exist is reported as such, not as private.
    let (st, body) = probe("https://no-such-host.invalid/acme/widget");
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["reachable"], false);
    assert_eq!(body["private"], false);

    // Only an admin may make the server open connections. A viewer is
    // masked the same way every other admin route masks them.
    assert_eq!(
        w.vic(
            "POST",
            "/v1/orgs/acme/origins/probe",
            Some(serde_json::json!({"origin": "github.com/acme/widget"}))
        )
        .0,
        404
    );
    // …and anonymous does not reach it at all.
    assert_eq!(
        w.server
            .post(
                "/v1/orgs/acme/origins/probe",
                "",
                Some(serde_json::json!({"origin": "github.com/acme/widget"}))
            )
            .0,
        401
    );
    // An org that does not exist answers like one you may not see.
    assert_eq!(
        w.server
            .post(
                "/v1/orgs/ghost/origins/probe",
                &w.admin,
                Some(serde_json::json!({"origin": "github.com/acme/widget"}))
            )
            .0,
        404
    );

    assert!(w.server.healthy());
}

/// The probe cannot be turned into a scanner.
///
/// Its own server, because the counter is per process and the refusal
/// test above spends part of a window's budget — a rate-limit test that
/// shares a budget with another test measures the other test.
#[test]
fn the_probe_is_rate_limited_per_org() {
    let minio = Minio::shared();
    let bucket = minio.bucket("origin-ratelimit");
    let scratch = Scratch::new("origin-ratelimit");
    let w = world(spawn_server(&bucket.base_url, &scratch));

    let mut seen = Vec::new();
    for _ in 0..60 {
        let (st, _) = w.server.post(
            "/v1/orgs/acme/origins/probe",
            &w.admin,
            Some(serde_json::json!({"origin": "https://no-such-host.invalid/a/b"})),
        );
        seen.push(st);
        if st == 429 {
            break;
        }
    }
    assert_eq!(
        seen.last().copied(),
        Some(429),
        "the probe endpoint is not rate-limited: {seen:?}"
    );
    // …and it refuses for a reason a person can act on, not an empty 429.
    let (st, body) = w.server.post(
        "/v1/orgs/acme/origins/probe",
        &w.admin,
        Some(serde_json::json!({"origin": "https://no-such-host.invalid/a/b"})),
    );
    assert_eq!(st, 429);
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains("wait")),
        "{body}"
    );

    // Another org is unaffected: the limit is per org, not per server.
    let other = w.server.bootstrap_org("other");
    let (st, body) = w.server.post(
        "/v1/orgs/other/origins/probe",
        &other,
        Some(serde_json::json!({"origin": "https://no-such-host.invalid/a/b"})),
    );
    assert_eq!(
        st, 200,
        "one org's probing must not spend another's: {body}"
    );
    assert!(w.server.healthy());
}
