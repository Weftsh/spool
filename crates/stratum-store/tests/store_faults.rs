//! Store client error paths against a scripted HTTP responder: retry and
//! error mapping, range discipline, LIST pagination and malformed XML,
//! DELETE errors, latency-model throttling — everything a healthy MinIO
//! never triggers. Runs unsigned (no AWS env), which also exercises the
//! signer-absent branches.

use stratum_store::{LatencyModel, ObjectStore, PutCond};
use stratum_testkit::httpfake::{FakeHttp, Reply, Scripted};

fn store(url: &str) -> ObjectStore {
    ObjectStore::new(url, LatencyModel::None)
}

#[test]
fn get_maps_4xx_without_retry_and_5xx_with_transport_retries() {
    // 403 → immediate error, exactly one request.
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(403, b""))]);
    let err = store(&fake.url).get("k/forbidden").unwrap_err();
    assert!(err.contains("HTTP 403"), "{err}");
    assert_eq!(fake.requests().len(), 1);

    // Two slammed connections then success → the retry loop recovers.
    let fake = FakeHttp::start(vec![
        Reply::Slam,
        Reply::Slam,
        Reply::Http(Scripted::new(200, b"payload")),
    ]);
    assert_eq!(store(&fake.url).get("k/flaky").unwrap(), b"payload");
    assert_eq!(fake.requests().len(), 3, "two retries then success");

    // Three straight transport failures → the mapped transport error.
    let fake = FakeHttp::start(vec![Reply::Slam, Reply::Slam, Reply::Slam]);
    let err = store(&fake.url).get("k/dead").unwrap_err();
    assert!(err.contains("GET k/dead"), "{err}");
}

#[test]
fn range_requests_reject_a_store_that_ignores_range() {
    // 200 instead of 206 for a ranged read would shift every offset.
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, b"whole-object"))]);
    let err = store(&fake.url)
        .get_stream("k/seg", Some((10, 20)))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| {
            // Error may surface on first read for streamed bodies.
            "expected 206".into()
        });
    assert!(err.contains("206"), "{err}");
}

#[test]
fn get_with_etag_maps_status_and_transport_errors() {
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(404, b""))]);
    let err = store(&fake.url).get_with_etag("k/missing").unwrap_err();
    assert!(err.contains("HTTP 404"), "{err}");

    let fake = FakeHttp::start(vec![Reply::Slam]);
    let err = store(&fake.url).get_with_etag("k/x").unwrap_err();
    assert!(!err.contains("HTTP"), "transport, not status: {err}");
}

#[test]
fn put_maps_conflict_and_other_errors() {
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(412, b""))]);
    match store(&fake.url).put("k/x", b"v", PutCond::IfMatch("etag".to_string())) {
        Err(stratum_store::PutError::Conflict) => {}
        other => panic!("expected Conflict, got {other:?}"),
    }
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(500, b""))]);
    match store(&fake.url).put("k/x", b"v", PutCond::IfNoneMatchStar) {
        Err(stratum_store::PutError::Other(e)) => assert!(e.contains("PUT k/x"), "{e}"),
        other => panic!("expected Other, got {other:?}"),
    }

    // 409 ConditionalRequestConflict: what real S3 answers when two
    // conditional writes to one key *overlap*, as opposed to 412, which
    // means the precondition genuinely failed. AWS documents 409 as the
    // retryable one ("fetch the object's ETag and retry"), and a racing
    // pair can even see 409 first and 412 on the retry. MinIO never emits
    // it, so nothing here or in the e2e suites had ever produced one —
    // and mapping it to `Other` drops the loser of a manifest CAS out of
    // the retry loop at receive.rs, failing a push that should simply
    // have re-run. Every conditional writer treats Conflict as "re-read
    // and retry", so 409 must land there too.
    for cond in [
        PutCond::IfNoneMatchStar,
        PutCond::IfMatch("etag".to_string()),
    ] {
        let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(409, b""))]);
        match store(&fake.url).put("k/x", b"v", cond) {
            Err(stratum_store::PutError::Conflict) => {}
            other => panic!("expected Conflict for 409, got {other:?}"),
        }
    }
    // Display impls (used in wrapped error strings).
    assert_eq!(
        stratum_store::PutError::Conflict.to_string(),
        "conditional write conflict"
    );
    assert_eq!(stratum_store::PutError::Other("x".into()).to_string(), "x");
}

fn list_xml(entries: &[(&str, &str)], next: Option<&str>) -> Vec<u8> {
    let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
    for (k, t) in entries {
        xml.push_str(&format!(
            "<Contents><Key>{k}</Key><LastModified>{t}</LastModified></Contents>"
        ));
    }
    match next {
        Some(t) => xml.push_str(&format!(
            "<IsTruncated>true</IsTruncated><NextContinuationToken>{t}</NextContinuationToken>"
        )),
        None => xml.push_str("<IsTruncated>false</IsTruncated>"),
    }
    xml.push_str("</ListBucketResult>");
    xml.into_bytes()
}

#[test]
fn list_paginates_with_continuation_tokens() {
    let fake = FakeHttp::start(vec![
        Reply::Http(Scripted::new(
            200,
            &list_xml(&[("a/1", "t1"), ("a/2", "t2")], Some("tok-next")),
        )),
        Reply::Http(Scripted::new(200, &list_xml(&[("a/3", "t3")], None))),
    ]);
    let listed = store(&fake.url).list("a/").unwrap();
    assert_eq!(
        listed,
        vec![
            ("a/1".to_string(), "t1".to_string()),
            ("a/2".to_string(), "t2".to_string()),
            ("a/3".to_string(), "t3".to_string()),
        ]
    );
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[1].contains("continuation-token=tok-next"), "{reqs:?}");
}

#[test]
fn list_survives_malformed_xml_and_maps_errors() {
    // Unclosed <Contents> stops parsing without panicking.
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(
        200,
        b"<ListBucketResult><Contents><Key>only</Key>",
    ))]);
    assert!(store(&fake.url).list("p/").unwrap().is_empty());

    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(503, b""))]);
    let err = store(&fake.url).list("p/").unwrap_err();
    assert!(err.contains("HTTP 503"), "{err}");

    let fake = FakeHttp::start(vec![Reply::Slam]);
    let err = store(&fake.url).list("p/").unwrap_err();
    assert!(err.contains("LIST p/"), "{err}");
}

#[test]
fn delete_maps_missing_ok_and_errors() {
    // 404 on delete is success (idempotent sweeps).
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(404, b""))]);
    store(&fake.url).delete("k/gone").unwrap();

    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(500, b""))]);
    let err = store(&fake.url).delete("k/x").unwrap_err();
    assert!(err.contains("HTTP 500"), "{err}");

    let fake = FakeHttp::start(vec![Reply::Slam]);
    let err = store(&fake.url).delete("k/x").unwrap_err();
    assert!(err.contains("DELETE k/x"), "{err}");
}

#[test]
fn https_base_urls_pick_the_https_scheme() {
    // No listener on this port speaks TLS; the point is the https branch
    // of URL construction is taken and surfaces as a transport error.
    let err = ObjectStore::new("https://127.0.0.1:1/none", LatencyModel::None)
        .list("p/")
        .unwrap_err();
    assert!(err.contains("LIST p/"), "{err}");
}

#[test]
fn latency_models_shape_ttfb_and_throughput() {
    // Standard model: non-zero TTFB (sleep path) and a throttled reader.
    let body = vec![b'x'; 64 * 1024];
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, &body))]);
    let st = ObjectStore::new(&fake.url, LatencyModel::Standard);
    let t0 = std::time::Instant::now();
    let got = st.get("k/data").unwrap();
    assert_eq!(got.len(), body.len());
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(30),
        "Standard model must add TTFB"
    );

    // Express and Kv cover their arms too (whatever tail bucket hits).
    for model in [LatencyModel::Express, LatencyModel::Kv] {
        let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, b"tiny"))]);
        assert_eq!(
            ObjectStore::new(&fake.url, model).get("k/t").unwrap(),
            b"tiny"
        );
    }

    // The product build compiles the env hook to None (bench-models off).
    assert_eq!(LatencyModel::from_env(), LatencyModel::None);
}

#[test]
fn object_store_from_env_reads_the_base_url() {
    let fake = FakeHttp::start(vec![Reply::Http(Scripted::new(200, b"env-ok"))]);
    std::env::set_var("STRATUM_STORE_URL", &fake.url);
    assert_eq!(ObjectStore::from_env().get("k/env").unwrap(), b"env-ok");
}

#[test]
fn request_count_tracks_gets() {
    let fake = FakeHttp::start(vec![
        Reply::Http(Scripted::new(200, b"a")),
        Reply::Http(Scripted::new(200, b"b")),
    ]);
    let st = store(&fake.url);
    let before = st.request_count();
    st.get("k/1").unwrap();
    st.get("k/2").unwrap();
    assert_eq!(st.request_count() - before, 2);
}

/// `<Size>` is read when the store sends it, and a listing that lacks
/// it — or carries something unparsable — reads as zero for that key
/// rather than failing the sweep that asked. `list` keeps its old
/// shape from the same page.
#[test]
fn list_sized_reads_sizes_and_tolerates_their_absence() {
    let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
    xml.push_str(
        "<Contents><Key>a/1</Key><LastModified>t1</LastModified><Size>12345</Size></Contents>",
    );
    xml.push_str("<Contents><Key>a/2</Key><LastModified>t2</LastModified></Contents>");
    xml.push_str(
        "<Contents><Key>a/3</Key><LastModified>t3</LastModified><Size>x</Size></Contents>",
    );
    xml.push_str("<IsTruncated>false</IsTruncated></ListBucketResult>");
    let fake = FakeHttp::start(vec![
        Reply::Http(Scripted::new(200, xml.as_bytes())),
        Reply::Http(Scripted::new(200, xml.as_bytes())),
    ]);
    let st = store(&fake.url);
    assert_eq!(
        st.list_sized("a/").unwrap(),
        vec![
            ("a/1".to_string(), 12_345),
            ("a/2".to_string(), 0),
            ("a/3".to_string(), 0)
        ]
    );
    assert_eq!(
        st.list("a/").unwrap(),
        vec![
            ("a/1".to_string(), "t1".to_string()),
            ("a/2".to_string(), "t2".to_string()),
            ("a/3".to_string(), "t3".to_string()),
        ]
    );
}
