//! stratum-cat smoke: the debug read CLI resolves a real object out of an
//! ingested layout, verifies its hash, and reports timing JSON; bad
//! invocations die with usage.

use std::process::Command;
use stratum_engine::ingest::{publish, PublishMode};
use stratum_engine::{build_locator, ingest, IngestConfig};
use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::gitcli::{self, Scratch};
use stratum_testkit::minio::{ROOT_PASSWORD, ROOT_USER};
use stratum_testkit::Minio;

#[test]
fn cat_reads_an_object_and_rejects_bad_usage() {
    let minio = Minio::shared();
    let bucket = minio.bucket("proto-cat");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);

    let scratch = Scratch::new("cat");
    let repo = scratch.path().join("fixture");
    let tip = gitcli::fixture_repo(&repo, 8);
    let prefix = "o/testorg/r/catrepo/prod";
    let staging = scratch.path().join("staging");
    let mut out = ingest(&repo, prefix, "main", &IngestConfig::default(), &staging).unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap();

    let cat = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_stratum-cat"))
            .env("STRATUM_STORE_URL", &bucket.base_url)
            .env("STRATUM_LAYOUT", "prod")
            .env("AWS_ACCESS_KEY_ID", ROOT_USER)
            .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
            .env("AWS_REGION", "us-east-1")
            .args(args)
            .output()
            .expect("run stratum-cat")
    };

    // The tip commit reads back: JSON report line + raw object body.
    let out = cat(&["o/testorg/r/catrepo", &tip]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value =
        serde_json::from_str(text.lines().next().unwrap()).expect("json report line");
    assert_eq!(report["oid"].as_str(), Some(tip.as_str()));
    assert_eq!(report["type"], "commit");
    assert!(report["gets"].as_u64().unwrap() >= 1);
    assert!(text.contains("tree "), "commit body follows the report");

    // --quiet suppresses the body but keeps the report.
    let out = cat(&["o/testorg/r/catrepo", &tip, "--quiet"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);

    // Bad usage and unknown objects fail loudly, not silently.
    let out = cat(&["o/testorg/r/catrepo", "nothex"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage"));
    let out = cat(&["o/testorg/r/catrepo", &"0".repeat(40)]);
    assert!(!out.status.success());
}

#[test]
fn parse_fetch_wants_extracts_wants() {
    let mut body = Vec::new();
    for line in ["command=fetch\n", "object-format=sha1\n"] {
        body.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
    }
    body.extend_from_slice(b"0001");
    for line in [
        "ofs-delta\n",
        &format!("want {}\n", "a".repeat(40)),
        &format!("want {}\n", "b".repeat(40)),
        &format!("have {}\n", "c".repeat(40)),
        "done\n",
    ] {
        body.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
    }
    body.extend_from_slice(b"0000");
    let wants = stratum_proto::serve::parse_fetch_wants(&body).expect("parses");
    assert_eq!(wants, vec!["a".repeat(40), "b".repeat(40)]);
    assert!(stratum_proto::serve::parse_fetch_wants(b"garbage").is_none());
}

#[test]
fn kv_tier_flag_is_inert_in_product_builds() {
    let minio = Minio::shared();
    let bucket = minio.bucket("proto-catkv");
    let store = ObjectStore::new(&bucket.base_url, LatencyModel::None);
    let scratch = Scratch::new("catkv");
    let repo = scratch.path().join("fixture");
    let tip = gitcli::fixture_repo(&repo, 4);
    let prefix = "o/testorg/r/kvrepo/prod";
    let staging = scratch.path().join("staging");
    let mut out = ingest(&repo, prefix, "main", &IngestConfig::default(), &staging).unwrap();
    let hdr = build_locator(&repo, &mut out, prefix, 0).unwrap();
    publish(&store, prefix, &out, &hdr, PublishMode::Create).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_stratum-cat"))
        .env("STRATUM_STORE_URL", &bucket.base_url)
        .env("STRATUM_LAYOUT", "prod")
        .env("STRATUM_LOCATOR_TIER", "kv") // bench models compile out: inert
        .env("AWS_ACCESS_KEY_ID", ROOT_USER)
        .env("AWS_SECRET_ACCESS_KEY", ROOT_PASSWORD)
        .env("AWS_REGION", "us-east-1")
        .args(["o/testorg/r/kvrepo", &tip, "--quiet"])
        .output()
        .expect("run stratum-cat");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn pktline_write_text_handles_preterminated_lines() {
    let mut a = Vec::new();
    let mut b = Vec::new();
    stratum_proto::pktline::write_text(&mut a, "hello\n").unwrap();
    stratum_proto::pktline::write_text(&mut b, "hello").unwrap();
    assert_eq!(a, b, "trailing newline is normalized, not doubled");
}
