//! Request metering (M6): every git-serving response is measured — count,
//! duration, bytes on the wire — and batched into per-minute rows by one
//! writer task. Streamed bodies meter via a drop guard that fires when the
//! stream ends (or the client goes away), so bytes are what actually left.
//!
//! The kind a request is recorded under is what the metrics pages split
//! traffic by — see `stratum_control::metrics::Event::kind` for the
//! vocabulary. Two decisions are made here rather than at the doors, so
//! HTTP and SSH cannot drift: [`egress_kind`] names a fetch, and
//! [`is_runner`] says whether the credential behind it is a runner's
//! job token, so a build fetching its own repository is not counted as
//! a person cloning it.

use axum::body::Body;
use axum::response::Response;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use stratum_control::auth::Principal;
use stratum_control::metrics::Event;
use stratum_control::ControlDb;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

/// Process-wide counters for the Prometheus endpoint (M7 hooks).
pub static REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static BYTES_OUT_TOTAL: AtomicU64 = AtomicU64::new(0);

/// The label prefix a runner's job token is minted with
/// (`api::runners_api`): `ci:{job.id}`. The label is the only mark the
/// token carries, and it is enough — nothing else mints one.
pub const RUNNER_LABEL_PREFIX: &str = "ci:";

/// Whether this credential is a runner fetching the repository it is
/// about to build: a token whose label begins `ci:`.
///
/// Decided from the token's label rather than a field on `Principal`,
/// because the answer is needed on git-serving requests only and the
/// lookup is one indexed read; every other request would carry the
/// field for nothing. A person, a session, an SSH key without a token
/// and an anonymous clone are never runners. A lookup that fails reads
/// as "not a runner": the runner's own clone is then counted as an
/// ordinary one, which is the side a metric can afford to be wrong on.
pub fn is_runner(db: &ControlDb, principal: Option<&Principal>) -> bool {
    principal.is_some_and(|p| {
        matches!(
            stratum_control::auth::label_of(db, &p.token_id),
            Ok(Some(label)) if label.starts_with(RUNNER_LABEL_PREFIX)
        )
    })
}

/// The kind a fetch is recorded under: a clone (no `have`s) or an
/// incremental fetch, offloaded to the CDN or served inline, by a runner
/// or by anybody else.
pub fn egress_kind(clone: bool, offloaded: bool, runner: bool) -> &'static str {
    match (runner, clone, offloaded) {
        (true, true, _) => "runner_clone",
        (true, false, _) => "runner_fetch",
        (false, true, true) => "cdn_clone",
        (false, true, false) => "clone",
        (false, false, _) => "fetch",
    }
}

/// The kind the CDN pack an offloaded clone was sent to fetch is
/// recorded under.
pub fn pack_kind(runner: bool) -> &'static str {
    if runner {
        "runner_pack"
    } else {
        "cdn_pack"
    }
}

impl MeterSink {
    /// Record the bulk pack an offloaded clone was sent to the edge for.
    /// The edge never reports back, so the pack's stored size is what
    /// left, recorded the moment the client opts in; a clone served
    /// inline records nothing here — its bytes are on the stream.
    pub fn record_offload(&self, repo_id: &str, offloaded: bool, runner: bool, pack_size: u64) {
        if offloaded {
            self.record(repo_id, pack_kind(runner), pack_size, None);
        }
    }
}

#[derive(Clone)]
pub struct MeterSink {
    tx: mpsc::UnboundedSender<Event>,
}

impl MeterSink {
    pub fn record(&self, repo_id: &str, kind: &'static str, bytes: u64, ms: Option<u64>) {
        REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);
        BYTES_OUT_TOTAL.fetch_add(bytes, Ordering::Relaxed);
        let _ = self.tx.send(Event {
            repo_id: repo_id.to_string(),
            kind,
            count: 1,
            bytes,
            ms,
        });
    }
}

/// Spawn the single writer task; returns the sink handlers use.
pub fn spawn_writer(db: ControlDb) -> MeterSink {
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let db = db.clone();
            let out =
                tokio::task::spawn_blocking(move || stratum_control::metrics::record(&db, &ev))
                    .await;
            if let Ok(Err(e)) = out {
                eprintln!("weft: metrics write failed: {e}");
            }
        }
    });
    MeterSink { tx }
}

struct StreamGuard {
    sink: MeterSink,
    repo_id: String,
    kind: &'static str,
    start: Instant,
    bytes: Arc<AtomicU64>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.sink.record(
            &self.repo_id,
            self.kind,
            self.bytes.load(Ordering::Relaxed),
            Some(self.start.elapsed().as_millis() as u64),
        );
    }
}

/// Wrap a streaming response so its bytes/duration are recorded when the
/// body finishes (however it finishes). `start` should be the moment the
/// request began, so queue time counts — that's what the client felt.
pub fn meter_response(
    resp: Response,
    sink: &MeterSink,
    repo_id: &str,
    kind: &'static str,
    start: Instant,
) -> Response {
    let (parts, body) = resp.into_parts();
    let bytes = Arc::new(AtomicU64::new(0));
    let guard = StreamGuard {
        sink: sink.clone(),
        repo_id: repo_id.to_string(),
        kind,
        start,
        bytes: bytes.clone(),
    };
    let stream = body.into_data_stream().map(move |chunk| {
        // The guard lives inside this closure; it drops with the stream.
        let _hold = &guard;
        if let Ok(c) = &chunk {
            bytes.fetch_add(c.len() as u64, Ordering::Relaxed);
        }
        chunk
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Prometheus exposition (process-level).
pub fn prometheus_text() -> String {
    format!(
        "# TYPE stratum_requests_total counter\nstratum_requests_total {}\n\
         # TYPE stratum_bytes_out_total counter\nstratum_bytes_out_total {}\n",
        REQUESTS_TOTAL.load(Ordering::Relaxed),
        BYTES_OUT_TOTAL.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_control::auth::{self, Mint, Scope};

    /// A runner's traffic is always a `runner_` kind and nobody else's
    /// ever is, so the metrics can split the two.
    #[test]
    fn a_runners_kinds_are_its_own() {
        for clone in [true, false] {
            for offloaded in [true, false] {
                assert!(!egress_kind(clone, offloaded, false).starts_with("runner_"));
                assert!(egress_kind(clone, offloaded, true).starts_with("runner_"));
            }
        }
        assert_eq!(egress_kind(true, true, false), "cdn_clone");
        assert_eq!(egress_kind(true, false, false), "clone");
        assert_eq!(egress_kind(false, true, false), "fetch");
        assert_eq!(egress_kind(false, false, true), "runner_fetch");
        assert_eq!(pack_kind(false), "cdn_pack");
        assert_eq!(pack_kind(true), "runner_pack");
    }

    /// A runner is a token labelled `ci:…` and nothing else: not a
    /// person's token with another label, not an unlabelled one, not
    /// nobody, and not a token id the database has never seen.
    #[test]
    fn a_runner_is_a_token_labelled_ci() {
        let db = ControlDb::open(&stratum_testkit::pg::test_db_url("meter-runner")).unwrap();
        let org = stratum_control::registry::create_org(&db, "o").unwrap();
        let mint = |label: Option<&str>| -> Principal {
            let t = auth::mint_for(
                &db,
                &org.id,
                &[Scope::RepoRead],
                Mint {
                    repo_id: None,
                    label,
                    user_id: None,
                    expires_at: None,
                },
                None,
            )
            .unwrap();
            auth::principal_for_token_id(&db, &t.id).unwrap().unwrap()
        };
        assert!(is_runner(&db, Some(&mint(Some("ci:job-1")))));
        assert!(!is_runner(&db, Some(&mint(Some("ci runner")))));
        assert!(!is_runner(&db, Some(&mint(Some("laptop")))));
        assert!(!is_runner(&db, Some(&mint(None))));
        assert!(!is_runner(&db, None));
        let mut ghost = mint(Some("ci:job-2"));
        ghost.token_id = "no-such-token".into();
        assert!(!is_runner(&db, Some(&ghost)));
    }
}
