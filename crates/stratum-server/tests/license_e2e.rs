//! The Weft license key, end to end against a real server: installed with
//! the operator CLI, checked with a stand-in for the license service, and
//! never — in any state — standing in the way of anything the server does.
//!
//! The service here is `stratum_testkit::license`, a belief about the
//! real one. The license service's own `spool_e2e` runs this binary's
//! `admin license-install` and `license-check` against the real thing.

use stratum_testkit::gitcli::Scratch;
use stratum_testkit::license::{self as weft, Answer};
use stratum_testkit::{Minio, Server};

const LID: &str = "lic_0123456789abcdefghjkmnpqrs";

fn spawn(store_url: &str, scratch: &Scratch, hint: &str, service: &weft::FakeLicense) -> Server {
    let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), store_url)
        .db_hint(hint)
        .data_dir(scratch.path().join("data"))
        .env("STRATUM_LICENSE_ENDPOINT", service.endpoint.clone())
        .env("STRATUM_LICENSE_RETRY_MS", "20")
        // The worker is tested on its own; here the CLI decides when.
        .env("STRATUM_LICENSE_TICK_SECS", "0");
    for (k, v) in weft::trust_env() {
        b = b.env(k, v);
    }
    b.start()
}

/// `stratum-server admin <args>` with the server's own environment:
/// (succeeded, the JSON line it printed, stderr).
fn admin(server: &Server, args: &[&str]) -> (bool, serde_json::Value, String) {
    let mut all = vec!["admin"];
    all.extend_from_slice(args);
    let out = server.admin_in_server_env(&all);
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let json = serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
    (
        out.status.success(),
        json,
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn person(server: &Server, email: &str) -> String {
    let made = server.admin_json(&[
        "admin",
        "user-create",
        "--org",
        "acme",
        "--email",
        email,
        "--no-password",
        "--role",
        "member",
    ]);
    made["user"]["id"].as_str().unwrap().to_string()
}

/// Only a key this build trusts is installed, and what a refusal says
/// names the reason; nothing is stored until one verifies.
#[test]
fn a_key_is_installed_only_when_this_build_trusts_it() {
    let minio = Minio::shared();
    let bucket = minio.bucket("license-install");
    let scratch = Scratch::new("license-install");
    let service = weft::spawn();
    let server = spawn(&bucket.base_url, &scratch, "license_install", &service);
    server.bootstrap_org("acme");

    let (ok, status, _) = admin(&server, &["license-status"]);
    assert!(ok);
    assert_eq!(status["installed"], false, "{status}");

    let untrusted = weft::sign_with_seed(&weft::payload(LID), 9);
    let mut unknown_kid = weft::payload(LID);
    unknown_kid["kid"] = "spool-2031-01".into();
    for (key, why) in [
        ("not a key", "malformed key"),
        (untrusted.as_str(), "signature does not match"),
        (
            weft::sign(&unknown_kid).as_str(),
            "spool-2031-01, a key this build of Spool does not trust",
        ),
    ] {
        let (ok, _, err) = admin(&server, &["license-install", key]);
        assert!(!ok, "{key} was installed");
        assert!(err.contains(why), "{why}: {err}");
    }
    let (ok, _, err) = admin(&server, &["license-install"]);
    assert!(!ok && err.contains("license-install KEY required"), "{err}");
    assert_eq!(admin(&server, &["license-status"]).1["installed"], false);

    // Two people, one switched off: the license counts one.
    let before = admin(&server, &["license-status"]).1["people"]
        .as_u64()
        .unwrap();
    person(&server, "ann@acme.test");
    let bo = person(&server, "bo@acme.test");
    server
        .admin(&["admin", "user-disable", "--email", "bo@acme.test"])
        .unwrap();
    let _ = bo;

    let key = weft::sign(&weft::payload(LID));
    let (ok, installed, err) = admin(&server, &["license-install", &format!(" {key}\n")]);
    assert!(ok, "{err}");
    assert_eq!(installed["installed"], true, "{installed}");
    assert_eq!(installed["trusted"], true);
    assert_eq!(installed["lid"], LID);
    assert_eq!(installed["entity"], "Acme GmbH");
    assert_eq!(installed["tier"], "team");
    assert_eq!(installed["mode"], "online");
    assert_eq!(installed["state"], "active");
    assert_eq!(installed["limit"], 20);
    assert_eq!(installed["people"].as_u64().unwrap(), before + 1);
    assert_eq!(installed["over_limit"], false);
    assert_eq!(installed["check"]["status"], serde_json::Value::Null);

    // A server that no longer trusts the key says so rather than
    // reading it: here, the same database under a build without dev keys.
    let out = server.admin(&["admin", "license-status"]).unwrap();
    let plain: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(plain["trusted"], false, "{plain}");
    assert!(plain["error"].as_str().unwrap().contains("does not trust"));

    assert_eq!(admin(&server, &["license-remove"]).1["removed"], true);
    assert_eq!(admin(&server, &["license-remove"]).1["removed"], false);
    assert!(server.healthy());
}

/// The check sends exactly three fields — and what the service answers
/// is recorded for the operator, and changes nothing else.
#[test]
fn the_check_sends_three_fields_and_the_answer_stops_nothing() {
    let minio = Minio::shared();
    let bucket = minio.bucket("license-check");
    let scratch = Scratch::new("license-check");
    let service = weft::spawn();
    let server = spawn(&bucket.base_url, &scratch, "license_check", &service);
    let token = server.bootstrap_org("acme");
    person(&server, "ann@acme.test");
    let people = admin(&server, &["license-status"]).1["people"].clone();

    let (ok, _, err) = admin(&server, &["license-check"]);
    assert!(!ok && err.contains("no license key is installed"), "{err}");
    assert!(service.received().is_empty());

    admin(
        &server,
        &["license-install", &weft::sign(&weft::payload(LID))],
    );
    service.answer(LID, Answer::status("active", None));
    let (ok, report, err) = admin(&server, &["license-check"]);
    assert!(ok, "{err}");
    assert_eq!(report["outcome"], "answered", "{report}");
    assert_eq!(report["calls"], 1);
    assert_eq!(
        service.received(),
        vec![serde_json::json!({
            "keyId": LID,
            "version": env!("CARGO_PKG_VERSION"),
            "people": people,
        })],
        "the check sends exactly these three fields"
    );
    let status = admin(&server, &["license-status"]).1;
    assert_eq!(status["check"]["status"], "active", "{status}");
    assert!(status["check"]["at"].is_i64());

    // Revoked, with a sentence: recorded and shown — and every door the
    // server has is exactly as open as it was.
    let sentence = "Weft has revoked this license. Spool keeps running; contact Weft about it.";
    service.answer(LID, Answer::status("revoked", Some(sentence)));
    let (ok, report, _) = admin(&server, &["license-check"]);
    assert!(ok);
    assert_eq!(report["status"], "revoked");
    let status = admin(&server, &["license-status"]).1;
    assert_eq!(status["check"]["status"], "revoked");
    assert_eq!(status["check"]["notice"], sentence);
    let made = ureq::post(&format!("{}/v1/orgs/acme/repos", server.base))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({ "name": "still-works" }).to_string());
    assert!(
        made.is_ok(),
        "a revoked license refused a request: {made:?}"
    );
    assert!(server.healthy());
}

/// A refusal is asked once; an outage up to three times; neither erases
/// what the service last said.
#[test]
fn a_refusal_is_not_retried_and_an_outage_keeps_the_last_answer() {
    let minio = Minio::shared();
    let bucket = minio.bucket("license-retry");
    let scratch = Scratch::new("license-retry");
    let service = weft::spawn();
    let server = spawn(&bucket.base_url, &scratch, "license_retry", &service);
    server.bootstrap_org("acme");
    admin(
        &server,
        &["license-install", &weft::sign(&weft::payload(LID))],
    );

    // Unknown to the service: a 404 says the same tomorrow.
    let (ok, report, err) = admin(&server, &["license-check"]);
    assert!(!ok, "a refused check succeeded");
    assert_eq!(report["outcome"], "refused", "{report}");
    assert_eq!(report["calls"], 1, "a refusal was retried");
    assert!(err.contains("404"), "{err}");

    service.answer(LID, Answer::status("active", None));
    assert!(admin(&server, &["license-check"]).0);
    service.answer(LID, Answer::Fails(503));
    let sent_before = service.received().len();
    let (ok, report, _) = admin(&server, &["license-check"]);
    assert!(!ok);
    assert_eq!(report["outcome"], "unanswered", "{report}");
    assert_eq!(report["calls"], 3);
    assert_eq!(service.received().len(), sent_before + 3);
    let status = admin(&server, &["license-status"]).1;
    assert_eq!(
        status["check"]["status"], "active",
        "the last answer was lost: {status}"
    );
    assert!(status["check"]["error"].as_str().unwrap().contains("503"));
    assert!(server.healthy());
}

/// An offline key never calls out, whatever is asked of it.
#[test]
fn an_offline_key_never_calls_out() {
    let minio = Minio::shared();
    let bucket = minio.bucket("license-offline");
    let scratch = Scratch::new("license-offline");
    let service = weft::spawn();
    let server = spawn(&bucket.base_url, &scratch, "license_offline", &service);
    server.bootstrap_org("acme");
    let mut p = weft::payload(LID);
    p["mode"] = "offline".into();
    p["tier"] = "enterprise".into();
    p["maxConcurrent"] = serde_json::Value::Null;
    let (ok, installed, err) = admin(&server, &["license-install", &weft::sign(&p)]);
    assert!(ok, "{err}");
    assert_eq!(
        (installed["mode"].as_str(), installed["limit"].is_null()),
        (Some("offline"), true)
    );
    let (ok, report, _) = admin(&server, &["license-check"]);
    assert!(ok);
    assert_eq!(report["outcome"], "offline");
    assert!(service.received().is_empty(), "an offline key called out");
}

/// The worker checks when a check is due, not on every tick — however
/// often it wakes, and on however many nodes.
#[test]
fn the_worker_checks_once_a_day_however_often_it_wakes() {
    let minio = Minio::shared();
    let bucket = minio.bucket("license-worker");
    let service = weft::spawn();
    service.answer(LID, Answer::status("active", None));
    let start = |hint: &str, db: Option<&str>| {
        let scratch = Scratch::new(hint);
        let mut b = Server::builder(env!("CARGO_BIN_EXE_stratum-server"), &bucket.base_url)
            .db_hint("license_worker")
            .data_dir(scratch.path().join("data"))
            .env("STRATUM_LICENSE_ENDPOINT", service.endpoint.clone())
            .env("STRATUM_LICENSE_TICK_SECS", "1");
        for (k, v) in weft::trust_env() {
            b = b.env(k, v);
        }
        if let Some(url) = db {
            b = b.db_url(url);
        }
        (b.start(), scratch)
    };
    let (one, _s1) = start("license-worker-1", None);
    one.bootstrap_org("acme");
    admin(&one, &["license-install", &weft::sign(&weft::payload(LID))]);
    // A second node on the same database.
    let (two, _s2) = start("license-worker-2", Some(&one.db_url));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while service.received().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(service.received().len(), 1, "no check within 20s");
    // Several more ticks, on both nodes.
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert_eq!(
        service.received().len(),
        1,
        "the worker checked again inside a day"
    );
    assert_eq!(
        admin(&one, &["license-status"]).1["check"]["status"],
        "active"
    );
    assert!(one.healthy() && two.healthy());
}

/// A configuration that would trust a key it should not, or send the
/// check in the clear, is refused at boot.
#[test]
fn a_dangerous_license_configuration_refuses_to_boot() {
    let minio = Minio::shared();
    let bucket = minio.bucket("license-boot");
    let dev_keys = weft::trust_env()
        .into_iter()
        .find(|(k, _)| *k == "STRATUM_DEV_LICENSE_PUBLIC_KEYS")
        .unwrap()
        .1;
    for (env, needle) in [
        (
            vec![("STRATUM_DEV_LICENSE_PUBLIC_KEYS", dev_keys.clone())],
            "STRATUM_DEV_MODE=1",
        ),
        (
            vec![
                ("STRATUM_DEV_MODE", "1".to_string()),
                (
                    "STRATUM_DEV_LICENSE_PUBLIC_KEYS",
                    "{\"k\":\"not a pem\"}".into(),
                ),
            ],
            "not a PEM public key",
        ),
        (
            vec![(
                "STRATUM_LICENSE_ENDPOINT",
                "http://license.example/v1/spool/check".to_string(),
            )],
            "must be https://",
        ),
    ] {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_stratum-server"));
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("AWS_ACCESS_KEY_ID", "minioadmin")
            .env("AWS_SECRET_ACCESS_KEY", "minioadmin")
            .env("AWS_REGION", "us-east-1")
            .env("STRATUM_STORE_URL", &bucket.base_url)
            .env(
                "STRATUM_DB_URL",
                stratum_testkit::pg::test_db_url("license-boot"),
            )
            .env("STRATUM_BIND", "127.0.0.1:0");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run server");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{env:?} booted: {err}");
        assert!(err.contains(needle), "{env:?}: {err}");
    }
}
