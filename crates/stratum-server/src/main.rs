//! The Stratum platform server: git smart HTTP + REST API + background
//! workers, one deployable binary. `stratum-server admin <cmd>` provides
//! operator bootstrap (create an org, mint tokens) against STRATUM_DB_URL.

// Handlers use `Result<T, Response>` so authorization failures short-
// circuit with the exact HTTP response; the "large Err variant" lint is
// noise for that idiom.
#![allow(clippy::result_large_err)]

mod api;
mod app;
mod authx;
mod cdn;
mod changeset_workspace;
mod git_http;
mod mail;
mod metering;
mod mirror;
mod oidc;
mod push;
mod review;
mod ssh;
mod storage;
mod webassets;
mod workers;
mod workflow;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("admin") {
        if let Err(e) = admin(&args[2..]) {
            eprintln!("stratum-server admin: {e}");
            std::process::exit(2);
        }
        return;
    }
    // Serving takes no positional arguments, so one that is not `admin`
    // is a mistake — and the mistake this catches is dropping the word
    // `admin` itself: `stratum-server user-create --email …` used to
    // fall through to here and *start a server*, which then sat in the
    // foreground listening. An operator sees a command that never
    // returns and creates no account; a test harness sees a subprocess
    // that never exits and a suite that hangs until CI's timeout, with
    // nothing anywhere saying why. Refusing costs one line and turns a
    // hang into a sentence.
    if let Some(stray) = args.get(1).filter(|a| !a.starts_with('-')) {
        eprintln!(
            "stratum-server: unexpected argument {stray:?} — \
             serving takes none, and operator commands go after `admin` \
             (try `stratum-server admin {stray}`)"
        );
        std::process::exit(2);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(async_main());
}

async fn async_main() {
    let state = match app::state_from_env() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stratum-server: {e}");
            std::process::exit(2);
        }
    };
    let bind = std::env::var("STRATUM_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let poll_secs = std::env::var("STRATUM_MIRROR_POLL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    mirror::poller::spawn(state.clone(), poll_secs);
    workers::spawn_all(&state);
    if let Err(e) = app::serve(state, &bind).await {
        eprintln!("stratum-server: {e}");
        std::process::exit(1);
    }
}

/// Operator commands, deliberately tiny:
///   admin bootstrap --org NAME              create org + org:admin token
///   admin mint --org NAME --scopes a,b [--repo NAME] [--label L]
fn admin(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let db = app::open_db_from_env()?;
    match cmd {
        "bootstrap" => {
            let org_name = flag("--org").ok_or("--org NAME required")?;
            let org = stratum_control::registry::create_org(&db, &org_name)?;
            let token = stratum_control::auth::mint(
                &db,
                &org.id,
                &[stratum_control::auth::Scope::OrgAdmin],
                None,
                Some("bootstrap-admin"),
            )?;
            println!(
                "{}",
                serde_json::json!({
                    "org": { "id": org.id, "name": org.name },
                    "admin_token": token.plaintext,
                })
            );
            Ok(())
        }
        "mint" => {
            let org_name = flag("--org").ok_or("--org NAME required")?;
            let org = stratum_control::registry::org_by_name(&db, &org_name)?
                .ok_or_else(|| format!("org {org_name:?} not found"))?;
            let scopes: Vec<stratum_control::auth::Scope> = flag("--scopes")
                .ok_or("--scopes a,b required")?
                .split(',')
                .map(|s| match s {
                    "org:admin" => Ok(stratum_control::auth::Scope::OrgAdmin),
                    "org:read" => Ok(stratum_control::auth::Scope::OrgRead),
                    "repo:read" => Ok(stratum_control::auth::Scope::RepoRead),
                    "repo:write" => Ok(stratum_control::auth::Scope::RepoWrite),
                    other => Err(format!("unknown scope {other:?}")),
                })
                .collect::<Result<_, _>>()?;
            let repo_id = match flag("--repo") {
                None => None,
                Some(name) => Some(
                    stratum_control::registry::repo_by_name(&db, &org.id, &name)?
                        .ok_or_else(|| format!("repo {name:?} not found"))?
                        .id,
                ),
            };
            let token = stratum_control::auth::mint(
                &db,
                &org.id,
                &scopes,
                repo_id.as_deref(),
                flag("--label").as_deref(),
            )?;
            println!(
                "{}",
                serde_json::json!({ "id": token.id, "token": token.plaintext })
            );
            Ok(())
        }
        "user-create" => {
            // How the first person gets in. There is no self-serve signup:
            // an org is provisioned by an operator, and everyone after
            // this arrives by invitation from someone already inside.
            let email = flag("--email").ok_or("--email ADDRESS required")?;
            let name = flag("--name").unwrap_or_else(|| email.clone());
            // One or the other, said out loud: an account with no password
            // is right on a server that signs in with SSO — the first owner
            // there would otherwise have to invent a secret nobody uses —
            // and a quiet way to make an account nobody can sign in to
            // anywhere else.
            let password = match (
                flag("--password"),
                args.iter().any(|a| a == "--no-password"),
            ) {
                (Some(p), false) => Some(p),
                (None, true) => None,
                (Some(_), true) => return Err("--password and --no-password together".into()),
                (None, false) => {
                    return Err("--password SECRET required \
                                (or --no-password, on a server that signs in with SSO)"
                        .into())
                }
            };
            let org_name = flag("--org").ok_or("--org NAME required")?;
            let role_str = flag("--role").unwrap_or_else(|| "owner".into());
            let role = stratum_control::members::Role::parse(&role_str)
                .ok_or_else(|| format!("unknown role {role_str:?}"))?;
            let org = stratum_control::registry::org_by_name(&db, &org_name)?
                .ok_or_else(|| format!("org {org_name:?} not found"))?;
            // An existing address joins the org rather than failing —
            // one person, several orgs, is the normal case.
            let user = match stratum_control::users::by_email(&db, &email)? {
                Some(u) => u,
                None => stratum_control::users::create(&db, &email, &name, password.as_deref())?,
            };
            // A handle and a personal namespace, exactly as an invitation mints
            // them — because an account without one is not a whole
            // account, and the halves that are missing do not announce
            // themselves.
            //
            // `users.handle` is how a person is *attributed*: an issue
            // filed by a handle-less account renders with no author at
            // all, since resolving an id with no handle correctly yields
            // nothing. Every account this command made was in that
            // state, so on a seeded stack every issue read "opened by
            // somebody". Nothing errored; there was simply no name.
            //
            // The namespace is how a person *owns* anything of their
            // own: forking with no target defaults to "wherever I
            // belong", and an account with nowhere to belong has no
            // answer. Both come from `create_personal_namespace`, which
            // claims the name and stamps the handle in one transaction —
            // so doing one without the other is not a state this can
            // reach.
            //
            // Derived from the address rather than asked for, so an
            // operator provisioning a team is not made to invent handles;
            // `--handle` overrides when the derived one is taken or ugly.
            // Deliberately outside both arms of the match above, so
            // re-running this command against an account that predates
            // the fix heals it rather than skipping it. (An earlier
            // version of this comment said it was absent when the
            // account already existed. That was wrong, and wrong in the
            // direction that would have made the repair command below
            // look unnecessary.)
            //
            // Skipped only when the name is already claimed: joining a
            // second org must not fail because somebody else holds the
            // namespace.
            if user.handle.is_none() {
                let derived = flag("--handle").unwrap_or_else(|| {
                    stratum_control::registry::handle_from(
                        email.split('@').next().unwrap_or(&email),
                    )
                });
                match stratum_control::registry::create_personal_namespace(
                    &db, &user.id, &derived, None,
                ) {
                    Ok(_) => {}
                    // Named, never silent: an operator who wanted a
                    // handle needs to know they did not get one, and the
                    // remedy is one flag away.
                    Err(e) => eprintln!(
                        "stratum-admin: no personal namespace for {email}: {e} \
                         (pass --handle NAME to choose another)"
                    ),
                }
            }
            // An operator running this command has vouched for the
            // address more directly than any confirmation link could,
            // and there is nowhere to send one to a person who is not
            // sitting at a browser. Marking it proved here is also what
            // keeps a bootstrap owner able to create the first repo.
            stratum_control::usertokens::mark_verified(&db, &user.id)?;
            // An operator acting from the command line is not a person
            // with a session; the trail says so rather than attributing
            // it to whoever happens to be created.
            let actx = stratum_control::audit::AuditCtx::system(&org.id, "admin-cli");
            stratum_control::members::add(&db, &org.id, &user.id, role, Some(&actx))?;
            println!(
                "{}",
                serde_json::json!({
                    "user": { "id": user.id, "email": user.email, "name": user.name },
                    "org": org.name,
                    "role": role.as_str(),
                })
            );
            Ok(())
        }
        "repair-identities" => {
            // The other half of the `user-create` fix.
            //
            // That command used to make accounts with no handle and no
            // personal namespace, and fixing it fixed the *door*: every
            // account made afterwards is whole, and every account made
            // before it is still half-made. Nothing heals them on their
            // own, and both symptoms are silent — issues filed by such
            // an account render with no author, and forking with no
            // target answers "no personal namespace to fork into".
            //
            // Not a migration, because this is not a schema change: it
            // claims globally unique namespace names, which can collide
            // with an existing org and must then be *reported* rather
            // than resolved by a rule nobody chose. A migration that
            // silently skipped some accounts would leave exactly the
            // half-fixed state this exists to end, and one that failed
            // on a collision would refuse to start the server.
            //
            // Idempotent: an account that already has a handle is not
            // touched, so running it twice is running it once.
            let dry = std::env::args().any(|a| a == "--dry-run");
            let pending = stratum_control::users::without_handle(&db)?;
            if pending.is_empty() {
                println!(
                    "{}",
                    serde_json::json!({ "repaired": 0, "skipped": [], "note": "nothing to do" })
                );
                return Ok(());
            }
            let mut repaired = Vec::new();
            let mut skipped = Vec::new();
            for u in pending {
                let derived = stratum_control::registry::handle_from(
                    u.email.split('@').next().unwrap_or(&u.email),
                );
                if dry {
                    repaired.push(serde_json::json!({ "email": u.email, "handle": derived }));
                    continue;
                }
                match stratum_control::registry::create_personal_namespace(
                    &db, &u.id, &derived, None,
                ) {
                    Ok(_) => {
                        repaired.push(serde_json::json!({ "email": u.email, "handle": derived }))
                    }
                    // Named individually, never a count. A collision is
                    // a decision an operator has to make — the address
                    // that could not be given its obvious handle is the
                    // one they need to see, and `user-create --handle`
                    // is how they resolve it.
                    Err(e) => skipped.push(serde_json::json!({
                        "email": u.email,
                        "wanted": derived,
                        "why": e,
                    })),
                }
            }
            println!(
                "{}",
                serde_json::json!({
                    "dry_run": dry,
                    "repaired": repaired.len(),
                    "accounts": repaired,
                    "skipped": skipped,
                })
            );
            Ok(())
        }
        "user-disable" | "user-enable" => {
            // Offboarding, and undoing it. Deliberately not a delete: the
            // audit trail names this person by id, and rows pointing at
            // somebody who vanished are worse than rows pointing at
            // somebody disabled.
            //
            // Nothing is revoked by hand. Every credential resolves the
            // account on use — tokens and sessions check `disabled_at`
            // directly, SSH keys through `members::role_of` — so one flag
            // stops all of them on the next request.
            let disable = cmd == "user-disable";
            let email = flag("--email").ok_or("--email ADDRESS required")?;
            let user = stratum_control::users::by_email(&db, &email)?
                .ok_or_else(|| format!("no account for {email:?}"))?;
            // The row is known to exist from the lookup above, and
            // accounts are never deleted — only disabled — so there is
            // no second "did it apply?" to check here.
            stratum_control::users::set_disabled(&db, &user.id, disable)?;
            // Filed against every org they belong to: "who lost access
            // here?" is an org-level question, asked from an org's page.
            for m in stratum_control::members::orgs_of(&db, &user.id)? {
                let actx = stratum_control::audit::AuditCtx::system(&m, "admin-cli");
                stratum_control::audit::record(
                    &db,
                    &actx,
                    None,
                    if disable {
                        "user.disable"
                    } else {
                        "user.enable"
                    },
                    Some(&serde_json::json!({ "user_id": user.id })),
                )?;
            }
            println!(
                "{}",
                serde_json::json!({
                    "user": { "id": user.id, "email": user.email },
                    "disabled": disable,
                })
            );
            Ok(())
        }
        "sso-check" => sso_check(&db),
        other => Err(format!(
            "unknown admin command {other:?} (bootstrap | mint | \
             user-create | repair-identities | user-disable | user-enable | sso-check)"
        )),
    }
}

/// `admin sso-check`: everything single sign-on needs that can be asked
/// without a person at a browser, asked of the real provider with this
/// server's own configuration and parsers — so an operator finds a
/// wrong issuer, secret or organization before the first person does.
///
/// The client credentials are checked by trading a made-up code: a
/// provider authenticates the client before it looks at the code, so
/// `invalid_grant` means the credentials were taken and `invalid_client`
/// that they were not.
fn sso_check(db: &stratum_control::ControlDb) -> Result<(), String> {
    use serde_json::json;
    let cfg = oidc::config_from(|k| std::env::var(k).ok())?.ok_or(
        "single sign-on is not configured: set STRATUM_OIDC_ISSUER, \
         STRATUM_OIDC_CLIENT_ID and STRATUM_OIDC_CLIENT_SECRET",
    )?;
    let sso_only = oidc::sso_only_from(|k| std::env::var(k).ok(), true)?;
    let bind = std::env::var("STRATUM_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let public = std::env::var("STRATUM_PUBLIC_URL").unwrap_or_else(|_| format!("http://{bind}"));
    let callback = format!("{}/v1/auth/sso/callback", public.trim_end_matches('/'));
    let issuer = cfg.issuer.clone();
    let sso = oidc::Oidc::new(cfg);
    let failed = |e: String| json!({ "ok": false, "error": e });

    let organization = match sso.org_id(db) {
        Ok(_) => json!({ "ok": true, "name": sso.cfg.org }),
        Err(e) => failed(e),
    };
    let (discovery, keys, client) = match sso.discovery() {
        Err(e) => {
            let skipped = || failed("not asked: discovery failed".into());
            (failed(e), skipped(), skipped())
        }
        Ok(d) => {
            let keys = match sso.key_count(&d) {
                Ok(n) => json!({ "ok": true, "count": n }),
                Err(e) => failed(e),
            };
            let verifier = stratum_control::ids::token_secret();
            let client = match sso.exchange(&d, "weft-sso-check-not-a-code", &callback, &verifier) {
                Err(oidc::Exchange::Refused(answer)) => json!({ "ok": true, "answer": answer }),
                Err(oidc::Exchange::Client(e)) => failed(format!(
                    "the provider refused this server's client credentials: {e}"
                )),
                Err(oidc::Exchange::Unanswered(e)) => failed(e),
                Ok(_) => failed("the provider accepted a made-up code".into()),
            };
            let discovery = json!({
                "ok": true,
                "authorization_endpoint": d.authorization_endpoint,
                "token_endpoint": d.token_endpoint,
                "userinfo_endpoint": d.userinfo_endpoint,
                "client_auth": if d.basic_auth { "basic" } else { "post" },
            });
            (discovery, keys, client)
        }
    };
    let ok = [&organization, &discovery, &keys, &client]
        .iter()
        .all(|v| v["ok"] == true);
    println!(
        "{}",
        json!({
            "ok": ok,
            "issuer": issuer,
            "callback": callback,
            "sso_only": sso_only,
            "organization": organization,
            "discovery": discovery,
            "keys": keys,
            "client": client,
        })
    );
    if ok {
        Ok(())
    } else {
        Err("sso-check: a check failed; the line above says which".into())
    }
}
