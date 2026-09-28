//! Users, roles and sessions end to end against a real server.
//!
//! The properties that matter here are mostly *refusals*, so most of this
//! file is negative: a viewer must not write, a member must not
//! administer, a session for one org must not reach another, a removed
//! member must lose access on their very next request, and none of the
//! new surface may be reachable without credentials.
//!
//! Throughout, Bearer tokens must keep working exactly as before — CI and
//! git depend on them, and sessions are an addition, not a replacement.

use stratum_testkit::adversarial::INJECTIONS;
use stratum_testkit::browser::Browser;
use stratum_testkit::{gitcli::Scratch, Minio, Server};

fn spawn_server(store_url: &str, scratch: &Scratch) -> Server {
    Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint("users-e2e")
        .data_dir(scratch.path().join("data"))
        .start()
}

/// Create an org with an owner account, returning (admin token, owner email).
fn org_with_owner(server: &Server, org: &str, email: &str) -> String {
    let token = server.bootstrap_org(org);
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            org,
            "--email",
            email,
            "--name",
            "Owner",
            "--password",
            "a long enough password",
            "--role",
            "owner",
        ])
        .unwrap_or_else(|e| panic!("user-create: {e}"));
    token
}

#[test]
fn the_repair_command_heals_an_account_that_predates_the_fix() {
    // `a319def` fixed the door and not the accounts already through it.
    // Every account `admin user-create` made before it has no handle and
    // no personal namespace, nothing heals them on their own, and both
    // symptoms are silent — no author on their issues, nowhere for a
    // fork to go. Reported by stratum-core-8c and core-32.
    //
    // The half-made state is produced here the only way that is honest:
    // by putting the account back into it. Creating one through the
    // fixed door and then clearing `handle` is what a pre-fix account
    // *is*, and it means this test keeps working when nothing in the
    // tree can produce one any more.
    let minio = Minio::shared();
    let bucket = minio.bucket("users-repair");
    let scratch = Scratch::new("users-repair");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "ada@acme.dev");

    let mut ada = Browser::new(&server);
    assert_eq!(
        ada.req(
            "POST",
            "/v1/auth/login",
            Some(serde_json::json!({
                "email": "ada@acme.dev",
                "password": "a long enough password",
            })),
        )
        .0,
        200
    );
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{body}");

    // Back to the pre-fix shape, straight against the control plane —
    // `server.db_url` is the same database the server is using.
    {
        let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
        db.execute(
            "UPDATE users SET handle = NULL WHERE email = 'ada@acme.dev'",
            &[],
        )
        .expect("clear handle");
        // Membership first: the namespace row is referenced by the
        // owner row that `create_personal_namespace` writes beside it.
        db.execute(
            "DELETE FROM org_members WHERE org_id IN (SELECT id FROM orgs WHERE name = 'ada')",
            &[],
        )
        .expect("drop membership");
        db.execute("DELETE FROM orgs WHERE name = 'ada'", &[])
            .expect("drop namespace");
    }

    // The symptom, reproduced: an issue with no author.
    let (st, filed) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "who filed this?" })),
    );
    assert_eq!(st, 201, "{filed}");
    assert!(
        filed["author"].is_null(),
        "the half-made state was not reproduced, so this test proves nothing: {filed}"
    );

    // `--dry-run` reports and changes nothing.
    let out = server.admin_json(&["admin", "repair-identities", "--dry-run"]);
    assert_eq!(out["repaired"], 1, "{out}");
    let (_, still) = ada.req("GET", "/v1/orgs/acme/repos/widget/issues/1", None);
    assert!(still["author"].is_null(), "a dry run wrote: {still}");

    let out = server.admin_json(&["admin", "repair-identities"]);
    assert_eq!(out["repaired"], 1, "{out}");
    assert_eq!(out["accounts"][0]["handle"], "ada", "{out}");
    assert!(
        out["skipped"].as_array().expect("skipped").is_empty(),
        "{out}"
    );

    // Both halves, asserted through what somebody sees rather than
    // through the column. The namespace exists…
    let (st, me) = ada.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    assert!(
        me["orgs"]
            .as_array()
            .expect("orgs")
            .iter()
            .any(|o| o["name"] == "ada"),
        "{me}"
    );
    // …and issues filed from now on carry a name.
    let (st, after) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "and this one?" })),
    );
    assert_eq!(st, 201, "{after}");
    assert_eq!(after["author"], "ada", "{after}");

    // Idempotent: running it again is running it once.
    let out = server.admin_json(&["admin", "repair-identities"]);
    assert_eq!(out["repaired"], 0, "{out}");

    // A collision, which is the whole reason this is a command and not
    // a migration. `bob@acme.dev` derives the handle `bob`, and an org
    // called `bob` already exists — so the repair cannot claim it, and
    // must **name the account** rather than fold it into a count. An
    // operator who is only told "1 skipped" has to go and find which.
    server.bootstrap_org("bob");
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "bob@acme.dev",
            "--name",
            "Bob",
            "--password",
            "a long enough password",
            "--role",
            "member",
            "--handle",
            "bob-alt",
        ])
        .expect("create bob");
    {
        let mut db = postgres::Client::connect(&server.db_url, postgres::NoTls).unwrap();
        db.execute(
            "DELETE FROM org_members WHERE org_id IN (SELECT id FROM orgs WHERE name = 'bob-alt')",
            &[],
        )
        .unwrap();
        db.execute("DELETE FROM orgs WHERE name = 'bob-alt'", &[])
            .unwrap();
        db.execute(
            "UPDATE users SET handle = NULL WHERE email = 'bob@acme.dev'",
            &[],
        )
        .unwrap();
    }

    let out = server.admin_json(&["admin", "repair-identities"]);
    assert_eq!(
        out["repaired"], 0,
        "a taken handle was claimed anyway: {out}"
    );
    let skipped = out["skipped"].as_array().expect("skipped");
    assert_eq!(skipped.len(), 1, "{out}");
    assert_eq!(skipped[0]["email"], "bob@acme.dev", "{out}");
    assert_eq!(skipped[0]["wanted"], "bob", "{out}");
    assert!(
        !skipped[0]["why"].as_str().unwrap_or_default().is_empty(),
        "a skipped account must say why: {out}"
    );

    // And the operator's remedy works: name a handle that is free.
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "bob@acme.dev",
            "--name",
            "Bob",
            "--password",
            "a long enough password",
            "--role",
            "member",
            "--handle",
            "bob-two",
        ])
        .expect("re-run with a free handle");
    let out = server.admin_json(&["admin", "repair-identities"]);
    assert_eq!(out["repaired"], 0, "{out}");
    assert!(
        out["skipped"].as_array().expect("skipped").is_empty(),
        "the collision was not resolved by naming a free handle: {out}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn an_operator_created_account_is_a_whole_account() {
    // `admin user-create` made accounts with **no handle and no personal
    // namespace**, and neither half announced itself.
    //
    // The handle is how a person is attributed. Resolving an id with no
    // handle correctly yields nothing, so every issue filed by an
    // operator-created account rendered with no author — on a seeded
    // stack, every row read "opened by somebody". Nothing errored.
    //
    // The namespace is how a person owns anything of their own: forking
    // with no target means "wherever I belong", and an account with
    // nowhere to belong has no answer.
    //
    // Asserted through what a *reader* sees rather than by reading the
    // column, because the column being null was never the complaint —
    // the missing name on somebody's bug report was.
    let minio = Minio::shared();
    let bucket = minio.bucket("users-whole");
    let scratch = Scratch::new("users-whole");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "ada@acme.dev");

    let mut ada = Browser::new(&server);
    let (st, body) = ada.req(
        "POST",
        "/v1/auth/login",
        Some(serde_json::json!({
            "email": "ada@acme.dev",
            "password": "a long enough password",
        })),
    );
    assert_eq!(st, 200, "{body}");

    // The namespace: an org named for the derived handle, which the
    // account owns.
    let (st, me) = ada.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    let orgs = me["orgs"].as_array().expect("orgs");
    assert!(
        orgs.iter().any(|o| o["name"] == "ada"),
        "an operator-created account has no personal namespace, so a fork \
         with no target has nowhere to go: {me}"
    );

    // The handle: an issue this person files carries their name.
    let (st, body) = ada.req(
        "POST",
        "/v1/orgs/acme/repos",
        Some(serde_json::json!({ "name": "widget" })),
    );
    assert_eq!(st, 201, "{body}");
    let (st, filed) = ada.req(
        "POST",
        "/v1/orgs/acme/repos/widget/issues",
        Some(serde_json::json!({ "title": "attribution, please" })),
    );
    assert_eq!(st, 201, "{filed}");
    assert_eq!(
        filed["author"], "ada",
        "an operator-created account filed an issue with no author, which \
         renders as 'opened by somebody': {filed}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_person_signs_in_and_sees_only_their_own_orgs() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-signin");
    let scratch = Scratch::new("users-signin");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");
    org_with_owner(&server, "other", "someone@other.test");

    let mut b = Browser::new(&server);
    // Wrong password, unknown address and a disabled-shaped address all
    // answer identically: sign-in must not reveal who has an account.
    assert_eq!(b.login("owner@acme.test", "wrong"), 401);
    assert_eq!(b.login("nobody@acme.test", "a long enough password"), 401);
    assert_eq!(b.login("not-an-email", "a long enough password"), 401);

    assert_eq!(b.login("owner@acme.test", "a long enough password"), 200);
    let (st, me) = b.req("GET", "/v1/auth/me", None);
    assert_eq!(st, 200, "{me}");
    assert_eq!(me["email"], "owner@acme.test");
    let orgs: Vec<String> = me["orgs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap().to_string())
        .collect();
    // Asserted as the property rather than as an exact list. The list
    // now also carries this person's own namespace, which they are
    // plainly "in" — so an exact match would be testing the fixture's
    // shape, not the rule. The rule is that a namespace you are not a
    // member of never appears, and it is asserted directly below and
    // again against `other` at the end of this test.
    assert!(
        orgs.contains(&"acme".to_string()),
        "must see the org they are in: {orgs:?}"
    );
    assert!(
        !orgs.contains(&"other".to_string()),
        "must not see an org they are not in: {orgs:?}"
    );
    let acme = me["orgs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == "acme")
        .expect("acme in orgs");
    assert_eq!(acme["role"], "owner");

    // The session reaches this org's API without any token at all.
    assert_eq!(b.req("GET", "/v1/orgs/acme/repos", None).0, 200);
    // …and is masked from the other org exactly as a foreign token is.
    assert_eq!(b.req("GET", "/v1/orgs/other/repos", None).0, 404);

    // Signing out is immediate.
    assert_eq!(b.req("POST", "/v1/auth/logout", None).0, 204);
    assert_eq!(b.req("GET", "/v1/auth/me", None).0, 401);
    assert_eq!(b.req("GET", "/v1/orgs/acme/repos", None).0, 401);
}

/// The whole point of roles: a viewer reads, a member writes, and only an
/// admin administers. Each denial is checked, not just each permission.
#[test]
fn roles_permit_and_refuse_exactly_what_they_say() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-roles");
    let scratch = Scratch::new("users-roles");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin_token,
                Some(serde_json::json!({"name": "app"}))
            )
            .0,
        201
    );

    for (email, role) in [
        ("viewer@acme.test", "viewer"),
        ("member@acme.test", "member"),
        ("admin@acme.test", "admin"),
    ] {
        server
            .admin(&[
                "admin",
                "user-create",
                "--org",
                "acme",
                "--email",
                email,
                "--name",
                role,
                "--password",
                "a long enough password",
                "--role",
                role,
            ])
            .unwrap();
    }

    let expectations = [
        //  role     read  write  administer
        ("viewer@acme.test", true, false, false),
        ("member@acme.test", true, true, false),
        ("admin@acme.test", true, true, true),
    ];
    for (email, can_read, can_write, can_admin) in expectations {
        let mut b = Browser::new(&server);
        assert_eq!(b.login(email, "a long enough password"), 200, "{email}");

        let read = b.req("GET", "/v1/orgs/acme/repos/app", None).0;
        assert_eq!(read == 200, can_read, "{email} read → {read}");

        let write = b
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/commits",
                Some(serde_json::json!({
                    "message": "from a role test",
                    "operations": [{"op": "put", "path": "r.txt", "content": "x"}]
                })),
            )
            .0;
        assert_eq!(
            write == 201 || write == 200,
            can_write,
            "{email} write → {write}"
        );

        // Administering: inviting someone is the canonical admin act.
        let administer = b
            .req(
                "POST",
                "/v1/orgs/acme/invites",
                Some(serde_json::json!({"email": "x@acme.test", "role": "viewer"})),
            )
            .0;
        assert_eq!(
            administer == 201,
            can_admin,
            "{email} invite → {administer}"
        );

        // Nobody below admin may mint an org token, whatever else they can do.
        let mint = b
            .req(
                "POST",
                "/v1/orgs/acme/tokens",
                Some(serde_json::json!({"scopes": ["org:admin"]})),
            )
            .0;
        assert_eq!(
            mint == 201 || mint == 200,
            can_admin,
            "{email} mint → {mint}"
        );
    }
}

/// A per-repo grant replaces the org role on that repo only — in both
/// directions. This is the contractor case and the sensitive-repo case,
/// and getting either backwards is a privilege bug.
#[test]
fn a_repo_grant_raises_and_lowers_access_on_that_repo_alone() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-grants");
    let scratch = Scratch::new("users-grants");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    for name in ["app", "secret"] {
        server.post(
            "/v1/orgs/acme/repos",
            &admin_token,
            Some(serde_json::json!({"name": name})),
        );
    }
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "v@acme.test",
            "--name",
            "V",
            "--password",
            "a long enough password",
            "--role",
            "viewer",
        ])
        .unwrap();

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    let (_, me) = {
        let mut v = Browser::new(&server);
        v.login("v@acme.test", "a long enough password");
        v.req("GET", "/v1/auth/me", None)
    };
    let viewer_id = me["id"].as_str().unwrap().to_string();

    let write = |b: &mut Browser, repo: &str| -> u16 {
        b.req(
            "POST",
            &format!("/v1/orgs/acme/repos/{repo}/commits"),
            Some(serde_json::json!({
                "message": "grant test",
                "operations": [{"op": "put", "path": "g.txt", "content": "x"}]
            })),
        )
        .0
    };

    let mut v = Browser::new(&server);
    v.login("v@acme.test", "a long enough password");
    assert_ne!(write(&mut v, "app"), 201, "a viewer must not write");

    // Raise this viewer to member on one repo.
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({"user_id": viewer_id, "role": "member"})),
            )
            .0,
        204
    );
    let mut v = Browser::new(&server);
    v.login("v@acme.test", "a long enough password");
    assert_eq!(write(&mut v, "app"), 201, "the grant should permit writing");
    assert_ne!(
        write(&mut v, "secret"),
        201,
        "the grant must not leak to another repo"
    );

    // And the reverse: hold an admin down to viewer on one repo.
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "a@acme.test",
            "--name",
            "A",
            "--password",
            "a long enough password",
            "--role",
            "admin",
        ])
        .unwrap();
    let admin_id = {
        let mut a = Browser::new(&server);
        a.login("a@acme.test", "a long enough password");
        a.req("GET", "/v1/auth/me", None).1["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/secret/grants",
                Some(serde_json::json!({"user_id": admin_id, "role": "viewer"})),
            )
            .0,
        204
    );
    let mut a = Browser::new(&server);
    a.login("a@acme.test", "a long enough password");
    assert_ne!(
        write(&mut a, "secret"),
        201,
        "a viewer grant must hold an admin down on that repo"
    );
    assert_eq!(write(&mut a, "app"), 201, "…and nowhere else");

    // A grant to somebody outside the org would grant nothing; refuse it
    // rather than storing a row that lies.
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({"user_id": "not-a-member", "role": "member"})),
            )
            .0,
        400
    );
    // Revoking restores the org role.
    assert_eq!(
        owner
            .req(
                "DELETE",
                &format!("/v1/orgs/acme/repos/secret/grants/{admin_id}"),
                None,
            )
            .0,
        204
    );
}

/// Offboarding has to be total and immediate: the next request, not the
/// next login, and every credential they hold, not just the browser.
#[test]
fn removing_a_member_ends_their_access_on_the_next_request() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-offboard");
    let scratch = Scratch::new("users-offboard");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "leaver@acme.test",
            "--name",
            "Leaver",
            "--password",
            "a long enough password",
            "--role",
            "member",
        ])
        .unwrap();

    let mut leaver = Browser::new(&server);
    leaver.login("leaver@acme.test", "a long enough password");
    assert_eq!(leaver.req("GET", "/v1/orgs/acme/repos", None).0, 200);
    let leaver_id = leaver.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    assert_eq!(
        owner
            .req(
                "DELETE",
                &format!("/v1/orgs/acme/members/{leaver_id}"),
                None
            )
            .0,
        204
    );

    // Their still-open session is now worthless, with no re-login needed
    // to notice — the role is resolved per request, not cached at sign-in.
    assert_eq!(leaver.req("GET", "/v1/orgs/acme/repos", None).0, 404);
    // They remain a valid person — just not one with access here.
    assert_eq!(leaver.req("GET", "/v1/auth/me", None).0, 200);
    // Not "no orgs at all": a removed person still owns their own
    // namespace, and taking that away would be deleting their account
    // rather than ending their membership. What must be gone is `acme`.
    let after: Vec<String> = leaver.req("GET", "/v1/auth/me", None).1["orgs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !after.contains(&"acme".to_string()),
        "a removed member still sees the org they were removed from: {after:?}"
    );
}

/// The org must never be left without an owner: there is no super-user to
/// repair it from outside.
#[test]
fn the_last_owner_cannot_remove_or_demote_themselves() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-lastowner");
    let scratch = Scratch::new("users-lastowner");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    let id = owner.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (st, body) = owner.req("DELETE", &format!("/v1/orgs/acme/members/{id}"), None);
    assert_eq!(st, 409, "{body}");
    let (st, body) = owner.req(
        "PATCH",
        &format!("/v1/orgs/acme/members/{id}"),
        Some(serde_json::json!({"role": "viewer"})),
    );
    assert_eq!(st, 409, "{body}");
    // Still an owner, still able to administer.
    assert_eq!(owner.req("GET", "/v1/orgs/acme/members", None).0, 200);
}

/// An invitation is a bearer credential. It must be single-use, bound to
/// the org that issued it, and useless once revoked or replayed.
#[test]
fn an_invitation_is_single_use_and_cannot_be_forged() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-invite");
    let scratch = Scratch::new("users-invite");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    let (st, inv) = owner.req(
        "POST",
        "/v1/orgs/acme/invites",
        Some(serde_json::json!({"email": "new@acme.test", "role": "member"})),
    );
    assert_eq!(st, 201, "{inv}");
    let link = inv["invite_link"].as_str().unwrap().to_string();

    // A forged or truncated link is refused.
    let mut anon = Browser::new(&server);
    for bad in ["", "nonsense", "stinv_", &format!("{link}x")] {
        let st = anon
            .req(
                "POST",
                "/v1/auth/accept-invite",
                Some(serde_json::json!({
                    "invite": bad, "name": "X", "password": "a long enough password"
                })),
            )
            .0;
        assert_eq!(st, 400, "forged invite {bad:?} was accepted");
    }

    // The genuine link works once, and signs the new person straight in.
    let mut newbie = Browser::new(&server);
    let (st, body) = newbie.req(
        "POST",
        "/v1/auth/accept-invite",
        Some(serde_json::json!({
            "invite": link, "name": "New Person", "password": "a long enough password"
        })),
    );
    assert_eq!(st, 201, "{body}");
    assert_eq!(body["email"], "new@acme.test");
    assert_eq!(newbie.req("GET", "/v1/orgs/acme/repos", None).0, 200);

    // Replaying it is refused.
    let mut replay = Browser::new(&server);
    assert_eq!(
        replay
            .req(
                "POST",
                "/v1/auth/accept-invite",
                Some(serde_json::json!({
                    "invite": link, "name": "Impostor", "password": "a long enough password"
                })),
            )
            .0,
        400
    );
}

/// Everything a caller with no credentials, the wrong credentials, or
/// hostile input might try against the new surface.
#[test]
fn the_new_surface_refuses_anonymous_foreign_and_hostile_callers() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-neg");
    let scratch = Scratch::new("users-neg");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");
    let other_token = org_with_owner(&server, "bravo", "owner@bravo.test");

    // Anonymous: every route is closed.
    let mut anon = Browser::new(&server);
    for (method, path) in [
        ("GET", "/v1/auth/me"),
        ("GET", "/v1/orgs/acme/members"),
        ("GET", "/v1/orgs/acme/invites"),
    ] {
        assert_eq!(anon.req(method, path, None).0, 401, "{method} {path}");
    }
    assert_eq!(
        anon.req(
            "POST",
            "/v1/orgs/acme/invites",
            Some(serde_json::json!({"email": "x@y.test", "role": "viewer"}))
        )
        .0,
        401
    );

    // Another org's admin token is masked, not merely refused.
    assert_eq!(server.get("/v1/orgs/acme/members", &other_token).0, 404);
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/invites",
                &other_token,
                Some(serde_json::json!({"email": "x@y.test", "role": "viewer"}))
            )
            .0,
        404
    );

    // A tampered session cookie is not a session.
    let mut forged = Browser::new(&server);
    forged.cookie = Some("stratum_session=stses_forged_nonsense".into());
    assert_eq!(forged.req("GET", "/v1/auth/me", None).0, 401);
    assert_eq!(forged.req("GET", "/v1/orgs/acme/repos", None).0, 401);

    // Hostile identifiers are inert everywhere they can be placed.
    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    for inj in INJECTIONS {
        let st = owner
            .req(
                "POST",
                "/v1/orgs/acme/invites",
                Some(serde_json::json!({"email": inj, "role": "member"})),
            )
            .0;
        assert!(
            st == 400 || st == 409,
            "injection {inj:?} as an email → {st}"
        );
        let st = owner
            .req(
                "PATCH",
                &format!("/v1/orgs/acme/members/{}", urlish(inj)),
                Some(serde_json::json!({"role": "viewer"})),
            )
            .0;
        assert!(
            st == 404 || st == 400,
            "injection {inj:?} as a user id → {st}"
        );
    }
    // An unknown role never becomes a permission.
    for role in ["root", "superuser", "", "OWNER", "owner "] {
        let st = owner
            .req(
                "POST",
                "/v1/orgs/acme/invites",
                Some(serde_json::json!({"email": "role@acme.test", "role": role})),
            )
            .0;
        assert_eq!(st, 400, "role {role:?} was accepted");
    }

    // Still healthy and still serving after all of it.
    assert!(server.healthy());
    assert_eq!(owner.req("GET", "/v1/orgs/acme/members", None).0, 200);
}

/// Percent-encode so an attack string survives being placed in a path.
fn urlish(s: &str) -> String {
    stratum_testkit::adversarial::percent_encode(s)
}

/// Sessions and tokens must not interfere. CI holds tokens; people hold
/// sessions; a developer often holds both in the same browser.
#[test]
fn bearer_tokens_keep_working_and_win_over_a_session() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-tokens");
    let scratch = Scratch::new("users-tokens");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    server.post(
        "/v1/orgs/acme/repos",
        &admin_token,
        Some(serde_json::json!({"name": "app"})),
    );
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "v@acme.test",
            "--name",
            "V",
            "--password",
            "a long enough password",
            "--role",
            "viewer",
        ])
        .unwrap();

    // The pre-existing token path is untouched.
    assert_eq!(server.get("/v1/orgs/acme/repos", &admin_token).0, 200);
    // A real git clone with the token still works — the thing CI does.
    let url = server.authed_url(&admin_token, "acme", "app");
    let dest = scratch.path().join("clone");
    stratum_testkit::gitcli::git(
        scratch.path(),
        &["clone", "-q", &url, dest.to_str().unwrap()],
    );

    // A viewer's session plus an admin token on the same request gets the
    // TOKEN's authority: a developer testing a credential must see what
    // that credential can do, not what they can do.
    let resp = ureq::post(&format!("{}/v1/orgs/acme/invites", server.base))
        .set("Cookie", &{
            let mut v = Browser::new(&server);
            v.login("v@acme.test", "a long enough password");
            v.cookie.clone().unwrap()
        })
        .set("Authorization", &format!("Bearer {admin_token}"))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({"email": "t@acme.test", "role": "viewer"}).to_string());
    let st = match resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("transport: {e}"),
    };
    assert_eq!(
        st, 201,
        "the token's authority should apply, not the viewer's"
    );
}

/// A password change must end the sessions an attacker might hold, while
/// leaving the tab the owner just used signed in.
#[test]
fn changing_a_password_revokes_other_sessions() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-pw");
    let scratch = Scratch::new("users-pw");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");

    let mut here = Browser::new(&server);
    here.login("owner@acme.test", "a long enough password");
    let mut elsewhere = Browser::new(&server);
    elsewhere.login("owner@acme.test", "a long enough password");
    assert_eq!(elsewhere.req("GET", "/v1/auth/me", None).0, 200);

    // The current password is required: a hijacked session must not be
    // able to lock the owner out.
    assert_eq!(
        here.req(
            "POST",
            "/v1/auth/password",
            Some(serde_json::json!({
                "current_password": "wrong", "new_password": "a brand new password"
            })),
        )
        .0,
        401
    );
    assert_eq!(
        here.req(
            "POST",
            "/v1/auth/password",
            Some(serde_json::json!({
                "current_password": "a long enough password",
                "new_password": "a brand new password"
            })),
        )
        .0,
        204
    );

    // The other session is dead; this one continues.
    assert_eq!(elsewhere.req("GET", "/v1/auth/me", None).0, 401);
    assert_eq!(here.req("GET", "/v1/auth/me", None).0, 200);

    // The old password no longer works, the new one does.
    let mut again = Browser::new(&server);
    assert_eq!(
        again.login("owner@acme.test", "a long enough password"),
        401
    );
    assert_eq!(again.login("owner@acme.test", "a brand new password"), 200);
    // A too-short new password is refused rather than stored.
    assert_eq!(
        here.req(
            "POST",
            "/v1/auth/password",
            Some(serde_json::json!({
                "current_password": "a brand new password", "new_password": "short"
            })),
        )
        .0,
        400
    );
}

/// The administrative surface an org owner actually uses: change someone's
/// role, list and revoke pending invitations, grant and revoke access to a
/// single repo. Each happy path is asserted beside the refusal that shares
/// its route, because a route that only ever gets tested through its
/// refusal can be broken in the direction that matters.
#[test]
fn an_admin_manages_members_invites_and_repo_grants() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-admin");
    let scratch = Scratch::new("users-admin");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin_token,
                Some(serde_json::json!({"name": "app"}))
            )
            .0,
        201
    );

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");

    // Invite two people; accept one, leave one pending.
    let mut link_for = |email: &str| -> String {
        let (st, inv) = owner.req(
            "POST",
            "/v1/orgs/acme/invites",
            Some(serde_json::json!({"email": email, "role": "member"})),
        );
        assert_eq!(st, 201, "{inv}");
        inv["invite_link"].as_str().unwrap().to_string()
    };
    let accepted_link = link_for("joiner@acme.test");
    let pending_link = link_for("pending@acme.test");

    let mut joiner = Browser::new(&server);
    assert_eq!(
        joiner
            .req(
                "POST",
                "/v1/auth/accept-invite",
                Some(serde_json::json!({
                    "invite": accepted_link, "name": "Joiner",
                    "password": "a long enough password"
                })),
            )
            .0,
        201
    );
    let joiner_id = joiner.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Listing shows both, and says which one is still outstanding.
    let (st, list) = owner.req("GET", "/v1/orgs/acme/invites", None);
    assert_eq!(st, 200, "{list}");
    let invites = list["invites"].as_array().unwrap();
    assert_eq!(invites.len(), 2, "{list}");
    let pending: Vec<&serde_json::Value> = invites
        .iter()
        .filter(|i| i["accepted_at"].is_null())
        .collect();
    assert_eq!(pending.len(), 1, "exactly one invitation is outstanding");
    assert_eq!(pending[0]["email"], "pending@acme.test");
    assert_eq!(pending[0]["role"], "member");
    let pending_id = pending[0]["id"].as_str().unwrap().to_string();

    // Revoking it is immediate, idempotent-by-404, and kills the link.
    assert_eq!(
        owner
            .req(
                "DELETE",
                &format!("/v1/orgs/acme/invites/{pending_id}"),
                None
            )
            .0,
        204
    );
    assert_eq!(
        owner
            .req(
                "DELETE",
                &format!("/v1/orgs/acme/invites/{pending_id}"),
                None
            )
            .0,
        404,
        "revoking twice is not a second revocation"
    );
    assert_eq!(
        owner
            .req(
                "DELETE",
                "/v1/orgs/acme/invites/00000000000000000000000000",
                None
            )
            .0,
        404
    );
    let mut late = Browser::new(&server);
    assert_eq!(
        late.req(
            "POST",
            "/v1/auth/accept-invite",
            Some(serde_json::json!({
                "invite": pending_link, "name": "Late",
                "password": "a long enough password"
            })),
        )
        .0,
        400,
        "a revoked invitation must not still be redeemable"
    );

    // Promote the joiner, then check the promotion actually took.
    assert_eq!(
        owner
            .req(
                "PATCH",
                &format!("/v1/orgs/acme/members/{joiner_id}"),
                Some(serde_json::json!({"role": "admin"})),
            )
            .0,
        204
    );
    let (_, roster) = owner.req("GET", "/v1/orgs/acme/members", None);
    let promoted = roster["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["user_id"] == joiner_id.as_str())
        .expect("the joiner is on the roster");
    assert_eq!(promoted["role"], "admin");
    assert_eq!(promoted["email"], "joiner@acme.test");

    // A role that does not exist never becomes a permission, and a
    // stranger's id is absent rather than an error.
    assert_eq!(
        owner
            .req(
                "PATCH",
                &format!("/v1/orgs/acme/members/{joiner_id}"),
                Some(serde_json::json!({"role": "superuser"})),
            )
            .0,
        400
    );
    for id in ["00000000000000000000000000", "not-an-id"] {
        assert_eq!(
            owner
                .req(
                    "PATCH",
                    &format!("/v1/orgs/acme/members/{id}"),
                    Some(serde_json::json!({"role": "viewer"})),
                )
                .0,
            404,
            "PATCH member {id}"
        );
        assert_eq!(
            owner
                .req("DELETE", &format!("/v1/orgs/acme/members/{id}"), None)
                .0,
            404,
            "DELETE member {id}"
        );
    }

    // Repo grants: the role has to be one a repo can carry, and the
    // person has to be in the org.
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({"user_id": joiner_id, "role": "wizard"})),
            )
            .0,
        400,
        "an unknown role"
    );
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({"user_id": joiner_id, "role": "owner"})),
            )
            .0,
        400,
        "owner is an org role, not a repo role"
    );
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({
                    "user_id": "00000000000000000000000000", "role": "member"
                })),
            )
            .0,
        400,
        "a grant to a non-member grants nothing"
    );
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({"user_id": joiner_id, "role": "viewer"})),
            )
            .0,
        204
    );
    // Revoking a grant that is not there is a 404, not a silent success.
    assert_eq!(
        owner
            .req(
                "DELETE",
                "/v1/orgs/acme/repos/app/grants/00000000000000000000000000",
                None
            )
            .0,
        404
    );
    assert_eq!(
        owner
            .req(
                "DELETE",
                &format!("/v1/orgs/acme/repos/app/grants/{joiner_id}"),
                None
            )
            .0,
        204
    );

    // The last owner still cannot demote themselves, whatever else an
    // admin may now do.
    let owner_id = owner.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        owner
            .req(
                "PATCH",
                &format!("/v1/orgs/acme/members/{owner_id}"),
                Some(serde_json::json!({"role": "member"})),
            )
            .0,
        409
    );
    assert!(server.healthy());
}

/// Every administrative route answers the same way to a caller who should
/// not learn the org exists: 404 for a foreign token and for an org that
/// is not there, 401 with no credentials at all. Checked route by route,
/// because a single unguarded handler is the whole leak.
#[test]
fn every_admin_route_masks_a_missing_org_and_a_foreign_caller() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-mask");
    let scratch = Scratch::new("users-mask");
    let server = spawn_server(&bucket.base_url, &scratch);
    let acme = org_with_owner(&server, "acme", "owner@acme.test");
    let foreign = org_with_owner(&server, "bravo", "owner@bravo.test");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &acme,
                Some(serde_json::json!({"name": "app"}))
            )
            .0,
        201
    );
    let stranger = "00000000000000000000000000";
    let routes: [(&str, &str, Option<serde_json::Value>); 10] = [
        ("GET", "/v1/orgs/{org}/members", None),
        ("GET", "/v1/orgs/{org}/tokens", None),
        ("GET", "/v1/orgs/{org}/ssh-keys", None),
        (
            "PATCH",
            "/v1/orgs/{org}/members/00000000000000000000000000",
            Some(serde_json::json!({"role": "viewer"})),
        ),
        (
            "DELETE",
            "/v1/orgs/{org}/members/00000000000000000000000000",
            None,
        ),
        ("GET", "/v1/orgs/{org}/invites", None),
        (
            "POST",
            "/v1/orgs/{org}/invites",
            Some(serde_json::json!({"email": "x@y.test", "role": "viewer"})),
        ),
        (
            "DELETE",
            "/v1/orgs/{org}/invites/00000000000000000000000000",
            None,
        ),
        (
            "POST",
            "/v1/orgs/{org}/repos/app/grants",
            Some(serde_json::json!({"user_id": stranger, "role": "member"})),
        ),
        (
            "DELETE",
            "/v1/orgs/{org}/repos/app/grants/00000000000000000000000000",
            None,
        ),
    ];
    for (method, template, body) in routes {
        // An org that does not exist.
        let absent = template.replace("{org}", "ghost-org");
        assert_eq!(
            server.req(method, &absent, &acme, body.clone()).0,
            404,
            "{method} {absent} with a valid token"
        );
        // An org that does, to somebody who is not in it.
        let real = template.replace("{org}", "acme");
        assert_eq!(
            server.req(method, &real, &foreign, body.clone()).0,
            404,
            "{method} {real} with a foreign token"
        );
        // And with no credentials at all.
        let mut anon = Browser::new(&server);
        assert_eq!(
            anon.req(method, &real, body.clone()).0,
            401,
            "{method} {real} anonymous"
        );
    }
    assert!(server.healthy());
}

/// Session-only routes when the session is missing, forged, or dead.
/// Signing out is the interesting one: it must clear the browser's cookie
/// even when the cookie it was given was never valid.
#[test]
fn account_routes_refuse_a_missing_or_forged_session() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-nosession");
    let scratch = Scratch::new("users-nosession");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");

    let change = serde_json::json!({
        "current_password": "a long enough password",
        "new_password": "a brand new password",
    });

    // No cookie at all.
    let mut anon = Browser::new(&server);
    assert_eq!(
        anon.req("POST", "/v1/auth/password", Some(change.clone()))
            .0,
        401
    );
    // Signing out without a session still answers, and does not error.
    assert_eq!(anon.req("POST", "/v1/auth/logout", None).0, 204);

    // A forged cookie is not a session — including for sign-out, which
    // must still clear it rather than refusing.
    for forged in [
        "stratum_session=stses_forged_nonsense",
        "stratum_session=stses_00000000000000000000000000_wrongsecret",
        "stratum_session=",
    ] {
        let mut b = Browser::new(&server);
        b.cookie = Some(forged.to_string());
        assert_eq!(
            b.req("POST", "/v1/auth/password", Some(change.clone())).0,
            401,
            "{forged}"
        );
        b.cookie = Some(forged.to_string());
        assert_eq!(b.req("POST", "/v1/auth/logout", None).0, 204, "{forged}");
        b.cookie = Some(forged.to_string());
        assert_eq!(b.req("GET", "/v1/auth/me", None).0, 401, "{forged}");
    }

    // A session that has been signed out is as dead as a forged one.
    let mut b = Browser::new(&server);
    b.login("owner@acme.test", "a long enough password");
    assert_eq!(b.req("GET", "/v1/auth/me", None).0, 200);
    let live = b.cookie.clone().expect("a session cookie");
    assert_eq!(b.req("POST", "/v1/auth/logout", None).0, 204);
    b.cookie = Some(live);
    assert_eq!(b.req("POST", "/v1/auth/password", Some(change)).0, 401);
    assert!(server.healthy());
}

/// `Secure` on the session cookie is not decoration: without it a browser
/// will send the session over plain HTTP. It is set exactly when the
/// deployment is HTTPS — and *not* on the local HTTP stack, where a
/// Secure cookie would simply never be stored and sign-in would appear to
/// succeed and then not work.
#[test]
fn the_session_cookie_is_secure_only_on_an_https_deployment() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-cookie");
    let scratch = Scratch::new("users-cookie");

    let plain = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&plain, "acme", "owner@acme.test");
    let http_cookie = set_cookie_on_login(&plain);
    assert!(
        !http_cookie.contains("Secure"),
        "plain HTTP must not set Secure: {http_cookie}"
    );

    let tls = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
        .db_url(&plain.db_url)
        .data_dir(scratch.path().join("data-tls"))
        .env("STRATUM_PUBLIC_URL", "https://stratum.example")
        .start();
    let https_cookie = set_cookie_on_login(&tls);
    assert!(
        https_cookie.contains("; Secure"),
        "an https deployment must set Secure: {https_cookie}"
    );

    // The rest of the flags are unconditional and equally load-bearing.
    for cookie in [&http_cookie, &https_cookie] {
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        assert!(
            !cookie.contains("a long enough password"),
            "the password must never appear in the cookie: {cookie}"
        );
    }
}

/// The raw `Set-Cookie` a successful sign-in returns.
fn set_cookie_on_login(server: &Server) -> String {
    let (st, _, headers) = server.req_full(
        "POST",
        "/v1/auth/login",
        "",
        Some(serde_json::json!({
            "email": "owner@acme.test", "password": "a long enough password"
        })),
    );
    assert_eq!(st, 200, "sign-in should succeed");
    headers
        .get("set-cookie")
        .cloned()
        .expect("sign-in must set a session cookie")
}

/// A signed-in person is not a member of every org. Against another org's
/// repositories their session must be masked exactly as a foreign token
/// is — 404, not 403 — for a read and for a write, and indistinguishably
/// from a name the org does not have.
///
/// This used to carry a public half: an outsider's session read a public
/// repository and was refused a push to it. Every repository is private
/// now, so the half that survives is the one that was always about
/// private repositories, made total — and the member beside the
/// outsider proves the masking is about the outsider, not the route.
#[test]
fn a_session_from_another_org_is_masked_from_private_repos() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-foreign");
    let scratch = Scratch::new("users-foreign");
    let server = spawn_server(&bucket.base_url, &scratch);
    let acme = org_with_owner(&server, "acme", "owner@acme.test");
    org_with_owner(&server, "bravo", "outsider@bravo.test");
    for name in ["private", "open"] {
        assert_eq!(
            server
                .post(
                    "/v1/orgs/acme/repos",
                    &acme,
                    Some(serde_json::json!({ "name": name }))
                )
                .0,
            201
        );
    }

    let mut outsider = Browser::new(&server);
    outsider.login("outsider@bravo.test", "a long enough password");
    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    let commit = serde_json::json!({
        "message": "not mine",
        "operations": [{"op": "put", "path": "x", "content": "x"}]
    });
    for name in ["private", "open", "never-was"] {
        let path = format!("/v1/orgs/acme/repos/{name}");
        assert_eq!(
            outsider.req("GET", &path, None).0,
            404,
            "acme/{name} must be masked from a signed-in outsider"
        );
        // Reading is not all they are refused: a write is masked the
        // same way, never a 403 that would confirm the name.
        assert_eq!(
            outsider
                .req("POST", &format!("{path}/commits"), Some(commit.clone()))
                .0,
            404,
            "a write to acme/{name} told an outsider something"
        );
        assert_eq!(
            server.req("GET", &path, "", None).0,
            401,
            "acme/{name} answered a caller with no credential"
        );
    }
    for name in ["private", "open"] {
        assert_eq!(
            owner
                .req("GET", &format!("/v1/orgs/acme/repos/{name}"), None)
                .0,
            200,
            "a member could not read acme/{name}"
        );
    }
    // Nothing the outsider sent landed.
    let (st, branches) = owner.req("GET", "/v1/orgs/acme/repos/open/branches", None);
    assert_eq!(st, 200, "{branches}");
    assert!(
        branches["branches"]
            .as_array()
            .is_some_and(|b| b.is_empty()),
        "{branches}"
    );
    assert!(server.healthy());
}

/// One person, several orgs, is the normal case — so `user-create` for an
/// address that already exists joins the new org rather than failing.
#[test]
fn the_cli_adds_an_existing_person_to_a_second_org() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-cli");
    let scratch = Scratch::new("users-cli");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");
    server.bootstrap_org("bravo");

    let out = server.admin_json(&[
        "admin",
        "user-create",
        "--org",
        "bravo",
        "--email",
        "owner@acme.test",
        "--name",
        "Ignored",
        "--password",
        "a different password",
        "--role",
        "member",
    ]);
    assert_eq!(out["user"]["email"], "owner@acme.test");
    assert_eq!(out["org"], "bravo");
    // The original account is what joined: the name and the password from
    // the second invocation are not applied to an existing person.
    assert_eq!(out["user"]["name"], "Owner");

    let mut b = Browser::new(&server);
    assert_eq!(b.login("owner@acme.test", "a different password"), 401);
    assert_eq!(b.login("owner@acme.test", "a long enough password"), 200);
    let orgs: Vec<String> = b.req("GET", "/v1/auth/me", None).1["orgs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap().to_string())
        .collect();
    // Both memberships, asserted as memberships. The count was standing
    // in for them and no longer can: this person also owns their own
    // namespace, which is a third entry and not a third membership.
    assert!(
        orgs.contains(&"acme".to_string()) && orgs.contains(&"bravo".to_string()),
        "the existing account did not join the second org: {orgs:?}"
    );
    // The second invocation must not have minted a *second* personal
    // namespace — the handle is claimed once, and an existing account
    // joining another org is not a new person.
    assert_eq!(
        orgs.iter().filter(|o| *o == "owner").count(),
        1,
        "joining a second org minted another personal namespace: {orgs:?}"
    );

    // Input the CLI must refuse rather than half-apply.
    for (org, email, role, why) in [
        ("bravo", "not-an-email", "member", "an unstorable address"),
        (
            "bravo",
            "x@acme.test",
            "superuser",
            "a role that does not exist",
        ),
        (
            "no-such-org",
            "x@acme.test",
            "member",
            "an org that does not exist",
        ),
    ] {
        let err = server.admin_expect_err(&[
            "admin",
            "user-create",
            "--org",
            org,
            "--email",
            email,
            "--name",
            "X",
            "--password",
            "a long enough password",
            "--role",
            role,
        ]);
        assert!(!err.trim().is_empty(), "{why} should explain itself");
    }
    // …and none of those attempts left an account behind.
    let mut ghost = Browser::new(&server);
    assert_eq!(ghost.login("x@acme.test", "a long enough password"), 401);
    assert!(server.healthy());
}

/// A member's own credentials: a personal access token and a personal SSH
/// key, created and ended by them without an administrator, visible only
/// to them, and bounded by their role at every moment rather than at the
/// moment they were issued.
#[test]
fn a_member_holds_their_own_token_and_ssh_key() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-creds");
    let scratch = Scratch::new("users-creds");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin_token,
                Some(serde_json::json!({"name": "app"}))
            )
            .0,
        201
    );
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "dev@acme.test",
            "--name",
            "Dev",
            "--password",
            "a long enough password",
            "--role",
            "member",
        ])
        .unwrap();

    let mut dev = Browser::new(&server);
    dev.login("dev@acme.test", "a long enough password");
    let dev_id = dev.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A token for themselves, no administrator involved.
    let (st, minted) = dev.req(
        "POST",
        "/v1/orgs/acme/tokens",
        Some(serde_json::json!({"scopes": ["repo:write"], "label": "laptop"})),
    );
    assert_eq!(st, 201, "{minted}");
    let personal = minted["token"].as_str().unwrap().to_string();
    let personal_id = minted["id"].as_str().unwrap().to_string();

    // It works, and it acts as *them* — including for a git push.
    assert_eq!(server.get("/v1/orgs/acme/repos/app", &personal).0, 200);
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos/app/commits",
                &personal,
                Some(serde_json::json!({
                    "message": "from a personal token",
                    "operations": [{"op": "put", "path": "p.txt", "content": "x"}]
                }))
            )
            .0,
        201
    );

    // A token can never be minted above the role holding it.
    let (st, refused) = dev.req(
        "POST",
        "/v1/orgs/acme/tokens",
        Some(serde_json::json!({"scopes": ["org:admin"]})),
    );
    assert_eq!(st, 400, "{refused}");

    // They see their own credentials and nobody else's; the owner sees
    // the whole inventory, including the org's service token.
    let mine = dev.req("GET", "/v1/orgs/acme/tokens", None).1;
    let ids: Vec<&str> = mine["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![personal_id.as_str()], "{mine}");
    let all = server.get("/v1/orgs/acme/tokens", &admin_token).1;
    assert!(
        all["tokens"].as_array().unwrap().len() >= 2,
        "an admin sees the org's tokens: {all}"
    );

    // A personal SSH key needs no token id — the defect this replaces.
    let (st, key) = dev.req(
        "POST",
        "/v1/orgs/acme/ssh-keys",
        Some(serde_json::json!({
            "public_key": DEV_PUBKEY, "label": "laptop"
        })),
    );
    assert_eq!(st, 201, "{key}");
    assert_eq!(key["user_id"].as_str().unwrap(), dev_id);
    assert!(key["token_id"].is_null());
    let key_id = key["id"].as_str().unwrap().to_string();
    let listed = dev.req("GET", "/v1/orgs/acme/ssh-keys", None).1;
    assert_eq!(listed["keys"].as_array().unwrap().len(), 1, "{listed}");

    // A deploy key hands out a token's authority outright, so a member
    // may not create one.
    assert_eq!(
        dev.req(
            "POST",
            "/v1/orgs/acme/ssh-keys",
            Some(serde_json::json!({
                "public_key": OTHER_PUBKEY,
                "token_id": Server::token_id(&admin_token)
            })),
        )
        .0,
        403
    );

    // Somebody else's credential is masked, not refused: a member must
    // not be able to probe which ids exist.
    let other_key = server
        .post(
            "/v1/orgs/acme/ssh-keys",
            &admin_token,
            Some(serde_json::json!({
                "public_key": OTHER_PUBKEY,
                "token_id": Server::token_id(&admin_token)
            })),
        )
        .1["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        dev.req(
            "DELETE",
            &format!("/v1/orgs/acme/ssh-keys/{other_key}"),
            None
        )
        .0,
        404
    );
    assert_eq!(
        dev.req(
            "DELETE",
            &format!("/v1/orgs/acme/tokens/{}", Server::token_id(&admin_token)),
            None
        )
        .0,
        404
    );

    // A demotion narrows the token they already hold, with nothing
    // reissued: it still reads and no longer writes.
    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    assert_eq!(
        owner
            .req(
                "PATCH",
                &format!("/v1/orgs/acme/members/{dev_id}"),
                Some(serde_json::json!({"role": "viewer"})),
            )
            .0,
        204
    );
    assert_eq!(server.get("/v1/orgs/acme/repos/app", &personal).0, 200);
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos/app/commits",
                &personal,
                Some(serde_json::json!({
                    "message": "no longer allowed",
                    "operations": [{"op": "put", "path": "q.txt", "content": "x"}]
                }))
            )
            .0,
        404,
        "a demotion must narrow the token already in their hands"
    );

    // Ending their own credentials needs no administrator either.
    assert_eq!(
        dev.req("DELETE", &format!("/v1/orgs/acme/ssh-keys/{key_id}"), None)
            .0,
        204
    );
    assert_eq!(
        dev.req(
            "DELETE",
            &format!("/v1/orgs/acme/tokens/{personal_id}"),
            None
        )
        .0,
        204
    );
    assert_eq!(
        server.get("/v1/orgs/acme/repos/app", &personal).0,
        401,
        "a revoked token is dead immediately"
    );

    // And offboarding ends what is left, without hunting for it.
    let (st, second) = {
        let mut again = Browser::new(&server);
        again.login("dev@acme.test", "a long enough password");
        again.req(
            "POST",
            "/v1/orgs/acme/tokens",
            Some(serde_json::json!({"scopes": ["repo:read"]})),
        )
    };
    assert_eq!(st, 201, "{second}");
    let survivor = second["token"].as_str().unwrap().to_string();
    assert_eq!(server.get("/v1/orgs/acme/repos/app", &survivor).0, 200);
    assert_eq!(
        owner
            .req("DELETE", &format!("/v1/orgs/acme/members/{dev_id}"), None)
            .0,
        204
    );
    assert_eq!(
        server.get("/v1/orgs/acme/repos/app", &survivor).0,
        401,
        "removing a member must end every credential they hold"
    );
    assert!(server.healthy());
}

/// Two real ed25519 public keys, fixed so the suite needs no `ssh-keygen`
/// subprocess; the transport itself is exercised in `ssh_e2e`.
const DEV_PUBKEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
const OTHER_PUBKEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBFcVeGJ2wZQqQjxRKz3aRSGnRhOZPBGqRLTgTPqEHzL";

/// Session fixation: a cookie the browser was carrying before sign-in
/// must not become a valid session by signing in.
///
/// The attack is planting a known session id on someone's browser — via
/// a subdomain, a proxy, a link — and waiting for them to authenticate
/// it for you. The defence is that sign-in mints a fresh session and
/// replaces whatever was there, never adopts it.
#[test]
fn signing_in_replaces_any_session_the_browser_was_carrying() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-fixation");
    let scratch = Scratch::new("users-fixation");
    let server = spawn_server(&bucket.base_url, &scratch);
    org_with_owner(&server, "acme", "owner@acme.test");

    let planted = "stses_00000000000000000000000000_plantedbyanattacker";
    let mut victim = Browser::new(&server);
    victim.cookie = Some(format!("stratum_session={planted}"));
    assert_eq!(
        victim.req("GET", "/v1/auth/me", None).0,
        401,
        "inert to start"
    );

    assert_eq!(
        victim.login("owner@acme.test", "a long enough password"),
        200
    );
    let issued = victim.cookie.clone().expect("sign-in issues a session");
    assert!(
        !issued.contains(planted),
        "sign-in adopted the planted session: {issued}"
    );
    assert_eq!(victim.req("GET", "/v1/auth/me", None).0, 200);

    // The attacker, still holding the planted value, has nothing.
    let mut attacker = Browser::new(&server);
    attacker.cookie = Some(format!("stratum_session={planted}"));
    assert_eq!(attacker.req("GET", "/v1/auth/me", None).0, 401);
    assert_eq!(attacker.req("GET", "/v1/orgs/acme/repos", None).0, 401);

    // Signing in again rotates it once more, so a session that leaked
    // from one sign-in is not the session of the next.
    let mut again = Browser::new(&server);
    again.login("owner@acme.test", "a long enough password");
    assert_ne!(
        again.cookie.clone().unwrap(),
        issued,
        "two sign-ins must not share a session"
    );
    // …and the first one is still live: signing in elsewhere does not
    // sign you out here.
    assert_eq!(victim.req("GET", "/v1/auth/me", None).0, 200);
}

/// A per-repo grant reaches the credential in a developer's hand, not
/// only the session in their browser.
///
/// This is the difference between a grant being useful and a grant being
/// paperwork: granting someone write on one repo has to make their
/// existing token push to it, with nothing reissued and nothing to wait
/// for. It must also stop exactly at the repo it names, and never carry
/// them past what their token was minted for.
/// A per-repository admin is told they are one.
///
/// The bug this fixes is invisible from the server side and was found
/// from the client. `repos::patch` and the protections routes call
/// `authx::require(.., Some(&repo.id), ..)`, which **refines** a
/// principal against `repo_grants` — so a member holding `admin` on one
/// repository may move that repository's default branch and change its
/// branch policy. But the only question the dashboard could ask was
/// `GET …/access`, which requires **org-wide** admin, so it answered no
/// and the settings surface was hidden from somebody entitled to it.
///
/// A client cannot fix that by probing harder: the two authorities are
/// genuinely different questions, and the honest answer is for the
/// server to say which one this caller has rather than let every surface
/// infer it from whichever endpoint happened to reply.
#[test]
fn a_per_repo_admin_is_told_they_may_administer_that_repo_and_no_other() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-viewer-admin");
    let scratch = Scratch::new("users-viewer-admin");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");

    for name in ["app", "other"] {
        assert_eq!(
            server
                .req(
                    "POST",
                    "/v1/orgs/acme/repos",
                    &admin_token,
                    Some(serde_json::json!({ "name": name })),
                )
                .0,
            201
        );
        // A trunk to point the default branch at: moving it is the
        // write only an administrator of the repository may make.
        let (st, out) = server.req(
            "POST",
            &format!("/v1/orgs/acme/repos/{name}/commits"),
            &admin_token,
            Some(serde_json::json!({
                "message": "seed",
                "operations": [{"op": "put", "path": "README", "content": "x"}],
            })),
        );
        assert_eq!(st, 201, "{out}");
    }
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "vic@acme.test",
            "--name",
            "Vic",
            "--password",
            "a long enough password",
            "--role",
            "viewer",
        ])
        .unwrap();

    let mut vic = Browser::new(&server);
    vic.login("vic@acme.test", "a long enough password");
    let vic_id = vic.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Before any grant: a viewer, and told so on both repositories.
    let (st, app) = vic.req("GET", "/v1/orgs/acme/repos/app", None);
    assert_eq!(st, 200, "{app}");
    assert_eq!(app["viewer_admin"], false, "{app}");

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({"user_id": vic_id, "role": "admin"})),
            )
            .0,
        204
    );

    // Now an admin on `app` — and the flag says so.
    let (st, app) = vic.req("GET", "/v1/orgs/acme/repos/app", None);
    assert_eq!(st, 200, "{app}");
    assert_eq!(
        app["viewer_admin"], true,
        "a per-repo admin was not told they may administer it: {app}"
    );

    // The flag is a claim about *this* repository. A grant on `app` must
    // not light up `other`, or the client would offer a settings surface
    // that every write behind it refuses.
    let (st, other) = vic.req("GET", "/v1/orgs/acme/repos/other", None);
    assert_eq!(st, 200, "{other}");
    assert_eq!(
        other["viewer_admin"], false,
        "a grant on one repository claimed authority over another: {other}"
    );

    // And the flag is not decoration: the write it predicts — moving
    // the default branch, which takes an administrator — is allowed on
    // `app` and refused on `other`, masked as every refusal of a
    // repository's administration is.
    let (st, out) = vic.req(
        "PATCH",
        "/v1/orgs/acme/repos/app",
        Some(serde_json::json!({ "default_branch": "main" })),
    );
    assert_eq!(
        st, 200,
        "viewer_admin said yes and the write was refused: {out}"
    );
    let (st, out) = vic.req(
        "PATCH",
        "/v1/orgs/acme/repos/other",
        Some(serde_json::json!({ "default_branch": "main" })),
    );
    assert_eq!(
        st, 404,
        "viewer_admin said no and the write was allowed: {out}"
    );

    // Nobody signed out is told anything at all — not `false`, not the
    // repository — because there is nothing here they may read.
    let (st, anon) = server.req("GET", "/v1/orgs/acme/repos/other", "", None);
    assert_eq!(st, 401, "{anon}");

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}

#[test]
fn a_repo_grant_reaches_a_personal_token_in_both_directions() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-granttoken");
    let scratch = Scratch::new("users-granttoken");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    for name in ["app", "secret"] {
        assert_eq!(
            server
                .post(
                    "/v1/orgs/acme/repos",
                    &admin_token,
                    Some(serde_json::json!({"name": name}))
                )
                .0,
            201
        );
    }
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "vic@acme.test",
            "--name",
            "Vic",
            "--password",
            "a long enough password",
            "--role",
            "viewer",
        ])
        .unwrap();

    let mut vic = Browser::new(&server);
    vic.login("vic@acme.test", "a long enough password");
    let vic_id = vic.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();
    // A viewer holding no grant cannot mint a token that claims write:
    // it would be a credential saying more than they can do anywhere.
    let (st, refused) = vic.req(
        "POST",
        "/v1/orgs/acme/tokens",
        Some(serde_json::json!({"scopes": ["org:read", "repo:write"], "label": "laptop"})),
    );
    assert_eq!(st, 400, "{refused}");

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({"user_id": vic_id, "role": "member"})),
            )
            .0,
        204
    );
    // With a grant somewhere, they can. The ceiling is what they can do
    // *anywhere* in the org; where it applies is decided per repo.
    let (st, minted) = vic.req(
        "POST",
        "/v1/orgs/acme/tokens",
        Some(serde_json::json!({"scopes": ["org:read", "repo:write"], "label": "laptop"})),
    );
    assert_eq!(st, 201, "{minted}");
    let token = minted["token"].as_str().unwrap().to_string();

    let write = |repo: &str| -> u16 {
        server
            .post(
                &format!("/v1/orgs/acme/repos/{repo}/commits"),
                &token,
                Some(serde_json::json!({
                    "message": "grant test",
                    "operations": [{"op": "put", "path": "g.txt", "content": "x"}]
                })),
            )
            .0
    };
    assert_eq!(server.get("/v1/orgs/acme/repos/app", &token).0, 200);
    assert_eq!(write("app"), 201, "the grant should reach the token");
    assert_ne!(write("secret"), 201, "and stop at the repo it names");

    // Every seam obeys the grant, not just the one that serves commits.
    // Repo delete and mirror sync authorize through a different path, and
    // a grant that lowered access there but not here would be a hole in
    // exactly the direction that matters.
    assert_eq!(
        server.get("/v1/orgs/acme/repos/secret", &token).0,
        200,
        "a viewer still reads the repos they are not granted on"
    );
    assert_eq!(
        server.delete("/v1/orgs/acme/repos/secret", &token).0,
        404,
        "a viewer must not delete a repo their grant does not cover"
    );

    // Revoking the grant takes it away again, just as immediately.
    assert_eq!(
        owner
            .req(
                "DELETE",
                &format!("/v1/orgs/acme/repos/app/grants/{vic_id}"),
                None
            )
            .0,
        204
    );
    assert_ne!(write("app"), 201, "revoking a grant must be immediate too");
    assert_eq!(
        server.delete("/v1/orgs/acme/repos/app", &token).0,
        404,
        "and the delete seam agrees"
    );
}

/// The lowering direction through the seam that serves repo delete.
///
/// An org admin holds `org:admin`, which grants everything. A per-repo
/// grant of `viewer` on one repo has to take that away *there* — on every
/// path, not only the one that happens to serve commits.
#[test]
fn a_viewer_grant_holds_an_admins_own_token_down_on_that_repo() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-grantdown");
    let scratch = Scratch::new("users-grantdown");
    let server = spawn_server(&bucket.base_url, &scratch);
    let bootstrap = org_with_owner(&server, "acme", "owner@acme.test");
    for name in ["app", "secret"] {
        assert_eq!(
            server
                .post(
                    "/v1/orgs/acme/repos",
                    &bootstrap,
                    Some(serde_json::json!({"name": name}))
                )
                .0,
            201
        );
    }
    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "amy@acme.test",
            "--name",
            "Amy",
            "--password",
            "a long enough password",
            "--role",
            "admin",
        ])
        .unwrap();

    let mut amy = Browser::new(&server);
    amy.login("amy@acme.test", "a long enough password");
    let amy_id = amy.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = amy
        .req(
            "POST",
            "/v1/orgs/acme/tokens",
            Some(serde_json::json!({"scopes": ["org:admin"], "label": "laptop"})),
        )
        .1["token"]
        .as_str()
        .unwrap()
        .to_string();

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/secret/grants",
                Some(serde_json::json!({"user_id": amy_id, "role": "viewer"})),
            )
            .0,
        204
    );

    assert_eq!(
        server.get("/v1/orgs/acme/repos/secret", &token).0,
        200,
        "a viewer grant still reads"
    );
    assert_eq!(
        server.delete("/v1/orgs/acme/repos/secret", &token).0,
        404,
        "an admin token held to viewer here must not delete this repo"
    );
    assert_eq!(
        server.delete("/v1/orgs/acme/repos/app", &token).0,
        204,
        "…and is untouched everywhere else"
    );
}

/// A service token belongs to nobody, so "your own credentials" is an
/// empty set for it — not the org's whole credential inventory.
///
/// The credential routes split on two questions: are you an
/// administrator, and are you a person? An `org:read` service token
/// answers no to both, and that corner is the one where a mistake hands
/// a read-only CI token the keys to the org.
#[test]
fn a_service_token_below_admin_sees_no_credentials_and_owns_none() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-svc");
    let scratch = Scratch::new("users-svc");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");

    // An org:read service token: no person behind it, and not an admin.
    let (st, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &admin_token,
        Some(serde_json::json!({"scopes": ["org:read"], "label": "ci-reader"})),
    );
    assert_eq!(st, 201, "{minted}");
    let reader = minted["token"].as_str().unwrap().to_string();

    // It can read the org, so it is past the scope check and squarely in
    // the branch under test.
    assert_eq!(server.get("/v1/orgs/acme/repos", &reader).0, 200);

    // Credentials: it owns none, so it sees none — the admin's tokens and
    // keys must not be listed to it.
    let (st, tokens) = server.get("/v1/orgs/acme/tokens", &reader);
    assert_eq!(st, 200, "{tokens}");
    assert!(
        tokens["tokens"].as_array().unwrap().is_empty(),
        "a service token must not see the org's credentials: {tokens}"
    );
    assert!(
        tokens["mintable_scopes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| {
                s == "org:admin"
                    || s == "org:read"
                    || s == "repo:read"
                    || s == "repo:write"
                    || s == "repo:cache"
            }),
        "{tokens}"
    );
    let (st, keys) = server.get("/v1/orgs/acme/ssh-keys", &reader);
    assert_eq!(st, 200, "{keys}");
    assert!(keys["keys"].as_array().unwrap().is_empty(), "{keys}");

    // Revoking somebody else's is masked, not merely refused.
    assert_eq!(
        server
            .delete(
                &format!("/v1/orgs/acme/tokens/{}", Server::token_id(&admin_token)),
                &reader
            )
            .0,
        404
    );
    assert_eq!(
        server
            .delete("/v1/orgs/acme/ssh-keys/00000000000000000000000000", &reader)
            .0,
        404
    );

    // Minting a token that belongs to nobody is an administrative act.
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/tokens",
                &reader,
                Some(serde_json::json!({"scopes": ["repo:read"]}))
            )
            .0,
        403
    );

    // …and there is no "yourself" to register a key for, so a key with no
    // token id is a bad request rather than a key belonging to no one.
    let (st, refused) = server.post(
        "/v1/orgs/acme/ssh-keys",
        &reader,
        Some(serde_json::json!({"public_key": DEV_PUBKEY, "label": "nobody"})),
    );
    assert_eq!(st, 400, "{refused}");
    assert!(server.healthy());
}

/// The trail answers "who did that?" for the things that move authority.
///
/// Adding a member, changing a role, inviting, granting a repo — none of
/// it was recorded at all before this. An audit log that covers repo
/// content but not the permissions guarding it answers the easy question
/// and not the one an incident asks.
#[test]
fn the_audit_trail_names_who_changed_who_could_do_what() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-audittrail");
    let scratch = Scratch::new("users-audittrail");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin_token,
                Some(serde_json::json!({"name": "app"}))
            )
            .0,
        201
    );

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    let owner_id = owner.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Invite, accept, promote, grant, revoke the grant, remove.
    let link = owner
        .req(
            "POST",
            "/v1/orgs/acme/invites",
            Some(serde_json::json!({"email": "new@acme.test", "role": "member"})),
        )
        .1["invite_link"]
        .as_str()
        .unwrap()
        .to_string();
    let mut newbie = Browser::new(&server);
    assert_eq!(
        newbie
            .req(
                "POST",
                "/v1/auth/accept-invite",
                Some(serde_json::json!({
                    "invite": link, "name": "New", "password": "a long enough password"
                })),
            )
            .0,
        201
    );
    let new_id = newbie.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();
    for (method, path, body) in [
        (
            "PATCH",
            format!("/v1/orgs/acme/members/{new_id}"),
            Some(serde_json::json!({"role": "admin"})),
        ),
        (
            "POST",
            "/v1/orgs/acme/repos/app/grants".to_string(),
            Some(serde_json::json!({"user_id": new_id, "role": "viewer"})),
        ),
        (
            "DELETE",
            format!("/v1/orgs/acme/repos/app/grants/{new_id}"),
            None,
        ),
        ("DELETE", format!("/v1/orgs/acme/members/{new_id}"), None),
    ] {
        let st = owner.req(method, &path, body).0;
        assert!(st == 204 || st == 201, "{method} {path} → {st}");
    }

    let (st, audit) = owner.req("GET", "/v1/orgs/acme/audit?limit=200", None);
    assert_eq!(st, 200, "{audit}");
    let entries = audit["entries"].as_array().unwrap();
    let by_action = |a: &str| -> Option<serde_json::Value> {
        entries.iter().find(|e| e["action"] == a).cloned()
    };

    // Every authority change is there, and each names the person who made
    // it — not whichever credential they happened to reach for.
    for action in [
        "invite.create",
        "invite.accept",
        "member.role",
        "repo.grant",
        "repo.grant.revoke",
        "member.remove",
    ] {
        let e = by_action(action).unwrap_or_else(|| panic!("no {action} entry in {audit}"));
        assert!(e["user_id"].as_str().is_some(), "{action}: {e}");
        assert!(
            e["principal"].as_str().unwrap().starts_with("user:"),
            "{action} names a credential, not a person: {e}"
        );
    }
    assert_eq!(by_action("member.role").unwrap()["user_id"], owner_id);
    assert_eq!(
        by_action("invite.accept").unwrap()["user_id"],
        new_id,
        "the acceptor accepted, not the inviter"
    );
    // The join gives a name to show, without copying it into the row.
    assert_eq!(
        by_action("repo.grant").unwrap()["user_email"],
        "owner@acme.test"
    );

    // Filtering by person is what the log is for.
    let (_, mine) = owner.req(
        "GET",
        &format!("/v1/orgs/acme/audit?user={new_id}&limit=100"),
        None,
    );
    let actions: Vec<&str> = mine["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, vec!["invite.accept"], "{mine}");

    // …and by what happened.
    let (_, grants) = owner.req(
        "GET",
        "/v1/orgs/acme/audit?action=repo.grant&limit=100",
        None,
    );
    assert_eq!(grants["entries"].as_array().unwrap().len(), 1, "{grants}");
    assert!(server.healthy());
}

/// `git log` shows a person, not a credential.
///
/// A REST commit used to be authored by
/// `token:01hx… <token:01hx…@stratum.local>` — not a person, and every
/// tool downstream that groups by author was reading it as one.
#[test]
fn a_rest_commit_is_authored_by_the_person_who_made_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-author");
    let scratch = Scratch::new("users-author");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin_token,
                Some(serde_json::json!({"name": "app"}))
            )
            .0,
        201
    );

    let commit = |b: &mut Browser, path: &str, author: Option<serde_json::Value>| -> String {
        let mut body = serde_json::json!({
            "branch": "main",
            "message": format!("add {path}"),
            "operations": [{"op": "put", "path": path, "content": "x\n"}],
        });
        if let Some(a) = author {
            body["author"] = a;
        }
        let (st, out) = b.req("POST", "/v1/orgs/acme/repos/app/commits", Some(body));
        assert_eq!(st, 201, "{out}");
        out["commit"].as_str().unwrap().to_string()
    };

    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    commit(&mut owner, "one.txt", None);

    // A personal token is the same person, so it authors the same way.
    let token = owner
        .req(
            "POST",
            "/v1/orgs/acme/tokens",
            Some(serde_json::json!({"scopes": ["org:read", "repo:write"]})),
        )
        .1["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos/app/commits",
                &token,
                Some(serde_json::json!({
                    "branch": "main",
                    "message": "from my laptop token",
                    "operations": [{"op": "put", "path": "two.txt", "content": "x\n"}],
                }))
            )
            .0,
        201
    );

    // An explicit author still wins: an agent committing on someone's
    // behalf must be able to say whose behalf.
    commit(
        &mut owner,
        "three.txt",
        Some(serde_json::json!({"name": "Agent 7", "email": "agent7@acme.test"})),
    );

    let (_, log) = server.req(
        "GET",
        "/v1/orgs/acme/repos/app/log?limit=10",
        &admin_token,
        None,
    );
    let authors: Vec<&str> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["author"].as_str().unwrap())
        .collect();
    // git idents carry a trailing "<epoch> <offset>", so match the ident.
    assert!(
        authors
            .iter()
            .any(|a| a.starts_with("Agent 7 <agent7@acme.test>")),
        "explicit author should win: {authors:?}"
    );
    let by_owner = authors
        .iter()
        .filter(|a| a.starts_with("Owner <owner@acme.test>"))
        .count();
    assert_eq!(
        by_owner, 2,
        "both the session and the personal token should author as the person: {authors:?}"
    );
    for a in &authors {
        assert!(
            !a.contains("token:") && !a.contains("stratum.local"),
            "a credential leaked into git history: {a}"
        );
    }
}

/// A service token — one acting for no person — signs its commits with
/// its **label**, keeping the `token:` prefix that marks it a machine
/// and its id in the address. It used to sign with the id alone, so the
/// forge (which renders any `token:` author as a machine) showed every
/// commit a labelled automation made as the one word "token": a
/// repository seeded by the bootstrap token had a history nobody had
/// written. Found reading a seeded repository's front page.
#[test]
fn a_service_token_signs_its_commits_with_its_label() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-token-author");
    let scratch = Scratch::new("users-token-author");
    let server = spawn_server(&bucket.base_url, &scratch);
    // `admin bootstrap` mints its token with the label `bootstrap-admin`.
    let admin_token = server.bootstrap_org("acme");
    assert_eq!(
        server
            .post(
                "/v1/orgs/acme/repos",
                &admin_token,
                Some(serde_json::json!({"name": "app"}))
            )
            .0,
        201
    );
    // A second service token, minted by the first, with a label of its
    // own — and a third with none.
    let mint = |label: Option<&str>| -> (String, String) {
        let mut body = serde_json::json!({"scopes": ["org:read", "repo:write"]});
        if let Some(l) = label {
            body["label"] = serde_json::Value::String(l.to_string());
        }
        let (st, out) = server.post("/v1/orgs/acme/tokens", &admin_token, Some(body));
        assert_eq!(st, 201, "{out}");
        (
            out["id"].as_str().unwrap().to_string(),
            out["token"].as_str().unwrap().to_string(),
        )
    };
    let (bot_id, bot) = mint(Some("release-bot"));
    let (bare_id, bare) = mint(None);
    for (token, path) in [
        (&admin_token, "one.txt"),
        (&bot, "two.txt"),
        (&bare, "three.txt"),
    ] {
        let (st, out) = server.post(
            "/v1/orgs/acme/repos/app/commits",
            token,
            Some(serde_json::json!({
                "branch": "main",
                "message": format!("add {path}"),
                "operations": [{"op": "put", "path": path, "content": "x\n"}],
            })),
        );
        assert_eq!(st, 201, "{out}");
    }

    let (_, log) = server.req(
        "GET",
        "/v1/orgs/acme/repos/app/log?limit=10",
        &admin_token,
        None,
    );
    let authors: Vec<&str> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["author"].as_str().unwrap())
        .collect();
    assert_eq!(authors.len(), 3, "{authors:?}");
    let starts = |prefix: &str| authors.iter().any(|a| a.starts_with(prefix));
    assert!(
        starts("token:bootstrap-admin <token:"),
        "the bootstrap token signs with its label: {authors:?}"
    );
    assert!(
        starts(&format!("token:release-bot <token:{bot_id}@stratum.local>")),
        "a labelled token signs with its label and keeps its id in the address: {authors:?}"
    );
    assert!(
        starts(&format!("token:{bare_id} <token:{bare_id}@stratum.local>")),
        "an unlabelled token still signs with its id: {authors:?}"
    );
    // Machines, all three: nothing here borrowed a human name.
    assert!(
        authors.iter().all(|a| a.starts_with("token:")),
        "{authors:?}"
    );
}

/// The audit endpoint asks for `org:read`, and a *repo-bound* token
/// deliberately satisfies org-level `org:read` — that is what lets a
/// per-repo CI token read its repo's metadata. Together those two facts
/// handed a token scoped to one repo the entire org's trail, including
/// every `token.mint` record. A repo-bound credential now sees its own
/// repo and nothing else.
#[test]
fn a_repo_bound_token_sees_only_its_own_repos_trail() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-auditscope");
    let scratch = Scratch::new("users-auditscope");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");
    for name in ["ci-repo", "secret"] {
        assert_eq!(
            server
                .post(
                    "/v1/orgs/acme/repos",
                    &admin_token,
                    Some(serde_json::json!({"name": name}))
                )
                .0,
            201
        );
        // Something to find in the trail for each.
        assert_eq!(
            server
                .post(
                    &format!("/v1/orgs/acme/repos/{name}/commits"),
                    &admin_token,
                    Some(serde_json::json!({
                        "message": format!("work on {name}"),
                        "operations": [{"op": "put", "path": "f.txt", "content": "x"}]
                    }))
                )
                .0,
            201
        );
    }
    let (st, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &admin_token,
        Some(serde_json::json!({
            "scopes": ["org:read", "repo:write"], "repo": "ci-repo", "label": "runner"
        })),
    );
    assert_eq!(st, 201, "{minted}");
    let runner = minted["token"].as_str().unwrap().to_string();

    // Its own repo: visible.
    let (st, mine) = server.get("/v1/orgs/acme/audit?limit=100", &runner);
    assert_eq!(st, 200, "{mine}");
    let entries = mine["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "should see its own repo's trail");
    assert!(
        entries.iter().all(|e| e["repo_id"].is_string()),
        "an org-level entry leaked to a repo-bound token: {mine}"
    );
    assert!(
        entries
            .iter()
            .all(|e| e["action"] != "token.mint" && e["action"] != "member.add"),
        "credential and membership records leaked: {mine}"
    );

    // Somebody else's repo: masked, whether asked for by name or not.
    let (st, other) = server.get("/v1/orgs/acme/audit?repo=secret&limit=100", &runner);
    assert_eq!(st, 404, "{other}");

    // The org admin still sees everything, including both repos.
    let (_, all) = server.get("/v1/orgs/acme/audit?limit=200", &admin_token);
    let all_entries = all["entries"].as_array().unwrap();
    assert!(all_entries.len() > entries.len(), "{all}");
    assert!(
        all_entries.iter().any(|e| e["action"] == "token.mint"),
        "an admin must still see credential records: {all}"
    );

    // Hostile filters are inert, not errors.
    for inj in INJECTIONS {
        for param in ["user", "action", "principal"] {
            let path = format!("/v1/orgs/acme/audit?{param}={}&limit=5", urlish(inj));
            let (st, body) = server.get(&path, &admin_token);
            assert_eq!(st, 200, "{param}={inj:?} → {st}: {body}");
            assert!(
                body["entries"].as_array().unwrap().is_empty(),
                "{param}={inj:?} matched something: {body}"
            );
        }
    }
    // A tampered pagination cursor returns nothing, not everything.
    for after in ["0", "-1", "999999999", "abc"] {
        let (st, body) = server.get(
            &format!("/v1/orgs/acme/audit?after={after}&limit=5"),
            &admin_token,
        );
        assert_eq!(st, 200, "after={after} → {st}");
        let n = body["entries"].as_array().unwrap().len();
        assert!(n <= 5, "after={after} returned {n}");
    }
    assert!(server.healthy());
}

/// The activity feed reads newest-first, pages backwards, and exports.
///
/// Order is the whole point of this view. The table is stored and shipped
/// oldest-first, so a feed that took the default would open on the
/// hundred oldest events the org ever recorded and never show what just
/// happened. Both directions are asserted here, along with the cursor
/// that goes with each.
#[test]
fn the_activity_feed_reads_newest_first_and_exports_as_csv() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-activity");
    let scratch = Scratch::new("users-activity");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin = org_with_owner(&server, "acme", "owner@acme.test");

    // A trail with a known order: first, second, third.
    for name in ["first", "second", "third"] {
        assert_eq!(
            server
                .post(
                    "/v1/orgs/acme/repos",
                    &admin,
                    Some(serde_json::json!({"name": name}))
                )
                .0,
            201
        );
    }
    // A record whose context blob is JSON — commas and quotes inside one
    // CSV cell, which is the case a naive exporter corrupts.
    let (st, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({
            "scopes": ["org:read", "repo:write"], "label": "ci"
        })),
    );
    assert_eq!(st, 201, "{minted}");

    let seqs = |body: &serde_json::Value| -> Vec<i64> {
        body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_i64().unwrap())
            .collect()
    };

    // Newest first, and the cursor names the older end.
    let (st, desc) = server.get("/v1/orgs/acme/audit?order=desc&limit=100", &admin);
    assert_eq!(st, 200, "{desc}");
    let d = seqs(&desc);
    assert!(d.len() >= 4, "{desc}");
    assert!(
        d.windows(2).all(|w| w[0] > w[1]),
        "order=desc must be strictly descending: {d:?}"
    );
    assert_eq!(desc["entries"][0]["action"], "token.mint", "{desc}");
    assert!(desc["next_after"].is_null(), "{desc}");
    assert_eq!(desc["next_before"].as_i64(), d.last().copied());

    // The default is unchanged — the shipper and every existing caller
    // read the table forwards.
    let (_, asc) = server.get("/v1/orgs/acme/audit?limit=100", &admin);
    let a = seqs(&asc);
    assert!(
        a.windows(2).all(|w| w[0] < w[1]),
        "the default must stay ascending: {a:?}"
    );
    assert!(asc["next_before"].is_null(), "{asc}");
    assert_eq!(asc["next_after"].as_i64(), a.last().copied());
    let mut reversed = a.clone();
    reversed.reverse();
    assert_eq!(reversed, d, "both orders must be the same rows: {a:?}");

    // Paging backwards reaches older rows and never repeats one.
    let (_, page1) = server.get("/v1/orgs/acme/audit?order=desc&limit=2", &admin);
    let p1 = seqs(&page1);
    assert_eq!(p1.len(), 2, "{page1}");
    let cursor = page1["next_before"].as_i64().unwrap();
    assert_eq!(cursor, p1[1]);
    let (_, page2) = server.get(
        &format!("/v1/orgs/acme/audit?order=desc&limit=2&before={cursor}"),
        &admin,
    );
    let p2 = seqs(&page2);
    assert!(!p2.is_empty(), "{page2}");
    assert!(
        p2.iter().all(|s| *s < cursor),
        "backwards paging returned a row it had already shown: {p2:?} vs {cursor}"
    );

    // The export: the same rows, quoted so a spreadsheet can read them.
    let (st, csv, headers) = server.req_full("GET", "/v1/orgs/acme/audit?format=csv", &admin, None);
    assert_eq!(st, 200);
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("text/csv"),
        "{headers:?}"
    );
    let text = csv.as_str().unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "seq,at,action,principal,user_email,repo_id,context"
    );
    assert_eq!(lines.len(), a.len() + 1, "one header plus every row");
    // The mint blob, cell-encoded: inner quotes doubled, the whole thing
    // wrapped, so its commas cannot split the row.
    let blob = asc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "token.mint")
        .map(|e| e["context"].to_string())
        .unwrap();
    assert!(
        blob.contains(',') && blob.contains('"'),
        "this assertion is only interesting if the blob has both: {blob}"
    );
    let cell = format!("\"{}\"", blob.replace('"', "\"\""));
    assert!(
        text.contains(&cell),
        "the JSON context must survive the export intact:\n{cell}\nnot found in\n{text}"
    );

    // A filter naming no repo answers in the shape it was asked in — a
    // CSV export must not quietly hand back a JSON body named `.csv`.
    let (st, empty, headers) = server.req_full(
        "GET",
        "/v1/orgs/acme/audit?format=csv&repo=ghost",
        &admin,
        None,
    );
    assert_eq!(st, 200);
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("text/csv"),
        "{headers:?}"
    );
    assert_eq!(
        empty.as_str().unwrap().lines().collect::<Vec<_>>(),
        vec!["seq,at,action,principal,user_email,repo_id,context"]
    );

    // The export obeys the same scoping as the view: a repo-bound
    // credential exports its own repo's trail and nothing else.
    let (st, minted) = server.post(
        "/v1/orgs/acme/tokens",
        &admin,
        Some(serde_json::json!({
            "scopes": ["org:read"], "repo": "first", "label": "runner"
        })),
    );
    assert_eq!(st, 201, "{minted}");
    let runner = minted["token"].as_str().unwrap().to_string();
    let (st, scoped, _) = server.req_full("GET", "/v1/orgs/acme/audit?format=csv", &runner, None);
    assert_eq!(st, 200);
    let scoped = scoped.as_str().unwrap();
    assert!(
        !scoped.contains("token.mint"),
        "credential records leaked into a repo-bound export:\n{scoped}"
    );
    assert!(
        scoped.lines().count() < lines.len(),
        "a repo-bound export must be narrower than the admin's:\n{scoped}"
    );
    assert!(server.healthy());
}

/// `viewer_write` says whether this caller may open a change whose
/// commits are already in this repository — the question the Changes tab
/// has to answer before it draws a form.
///
/// It exists because the dashboard had no way to ask it. `viewer_admin`
/// is a different and stricter question, so the tab used neither and
/// simply drew the form for everybody: a signed-out stranger reading a
/// public repository was offered "Start a review", filled it in, and was
/// then told that opening a change needs write access and that they
/// should pass a `source` field — a REST parameter with nothing on
/// screen corresponding to it.
///
/// So the flag is asserted against the endpoint it predicts, not on its
/// own. A flag that says "yes" where `changes::create` says 403 would be
/// the same bug with an extra field.
#[test]
fn viewer_write_predicts_whether_a_change_needs_a_fork() {
    let minio = Minio::shared();
    let bucket = minio.bucket("users-viewer-write");
    let scratch = Scratch::new("users-viewer-write");
    let server = spawn_server(&bucket.base_url, &scratch);
    let admin_token = org_with_owner(&server, "acme", "owner@acme.test");

    assert_eq!(
        server
            .req(
                "POST",
                "/v1/orgs/acme/repos",
                &admin_token,
                Some(serde_json::json!({ "name": "app", "public": true })),
            )
            .0,
        201
    );

    server
        .admin(&[
            "admin",
            "user-create",
            "--org",
            "acme",
            "--email",
            "vic@acme.test",
            "--name",
            "Vic",
            "--password",
            "a long enough password",
            "--role",
            "viewer",
        ])
        .unwrap();

    let mut vic = Browser::new(&server);
    vic.login("vic@acme.test", "a long enough password");
    let vic_id = vic.req("GET", "/v1/auth/me", None).1["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A viewer on a public repository: may read it, may not push to it.
    let (st, app) = vic.req("GET", "/v1/orgs/acme/repos/app", None);
    assert_eq!(st, 200, "{app}");
    assert_eq!(
        app["viewer_write"], false,
        "a viewer was told they may write: {app}"
    );

    // And the flag is not decoration — this is the refusal the Changes
    // tab was walking people into.
    let (st, body) = vic.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({ "from": "feature" })),
    );
    assert_eq!(
        st, 403,
        "viewer_write said no and the change was not refused: {body}"
    );

    // A stranger with no account at all is told `false` rather than
    // nothing, so a client has nothing to distinguish "no" from "not
    // told" and cannot end up guessing.
    let (st, anon) = server.req("GET", "/v1/orgs/acme/repos/app", "", None);
    assert_eq!(st, 200, "{anon}");
    assert_eq!(anon["viewer_write"], false, "{anon}");

    // Raise Vic to a writer on this repository only.
    let mut owner = Browser::new(&server);
    owner.login("owner@acme.test", "a long enough password");
    assert_eq!(
        owner
            .req(
                "POST",
                "/v1/orgs/acme/repos/app/grants",
                Some(serde_json::json!({"user_id": vic_id, "role": "member"})),
            )
            .0,
        204
    );

    let (st, app) = vic.req("GET", "/v1/orgs/acme/repos/app", None);
    assert_eq!(st, 200, "{app}");
    assert_eq!(
        app["viewer_write"], true,
        "a per-repo member was not told they may write: {app}"
    );

    // The same call is no longer refused *for lack of write access*. It
    // still fails, because `feature` does not exist — and that is the
    // point of asserting the status rather than success: the write gate
    // is what moved, and a test that demanded 201 here would be testing
    // whether a branch exists instead.
    let (st, body) = vic.req(
        "POST",
        "/v1/orgs/acme/repos/app/changes",
        Some(serde_json::json!({ "from": "feature" })),
    );
    assert_ne!(
        st, 403,
        "viewer_write said yes and the change was still refused for access: {body}"
    );

    // Writing here says nothing about administering it: the two flags
    // are different questions, and conflating them is how a member gets
    // offered a settings surface every write behind it refuses.
    assert_eq!(
        app["viewer_admin"], false,
        "a writer was told they may administer the repository: {app}"
    );

    assert_eq!(server.req("GET", "/healthz", "", None).0, 200);
}
