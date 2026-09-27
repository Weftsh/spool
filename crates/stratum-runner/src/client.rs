//! The four calls a runner makes back to the control plane, and the one
//! distinction that matters while making them: *this attempt failed* versus
//! *this job is over*.
//!
//! **410 is not an error.** It is the control plane saying the job left
//! `running` — cancelled by a person, superseded by a newer push, or timed
//! out by the dispatcher's sweep — and the only correct response is to stop
//! doing work and exit 0. Treating it as a transport failure would mean a
//! cancelled job keeps compiling for another six hours, holding a Fargate
//! task nobody is watching, and eventually reporting a verdict for a run
//! that already settled.

use crate::spec::{parse_assignment, Assignment};
use std::io::Read;
use std::time::Duration;

/// Why a call did not return 200.
#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    /// 410 — the job is no longer running. Stop, quietly.
    Gone,
    /// The control plane understood us and said no (401/403/404, or any
    /// other 4xx). Retrying cannot change the answer.
    Refused(String),
    /// A 5xx or a transport failure. The same call may work next time.
    Unavailable(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Gone => write!(f, "job is no longer running"),
            CallError::Refused(m) | CallError::Unavailable(m) => write!(f, "{m}"),
        }
    }
}

/// Knobs the tests turn down and production leaves alone.
///
/// The retry base is a field rather than a constant because a test that
/// proves "five attempts with backoff" must not take eight seconds to do
/// it, and a test that sleeps for real is a test people start ignoring.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    pub retry_base_ms: u64,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}

impl Default for Tuning {
    fn default() -> Tuning {
        Tuning {
            retry_base_ms: 250,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
        }
    }
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
    job_id: String,
    token: String,
    tuning: Tuning,
}

impl Client {
    pub fn new(base: &str, job_id: &str, token: &str, tuning: Tuning) -> Client {
        Client {
            agent: ureq::builder()
                .timeout_connect(tuning.connect_timeout)
                .timeout_read(tuning.read_timeout)
                .build(),
            base: base.trim_end_matches('/').to_string(),
            job_id: job_id.to_string(),
            token: token.to_string(),
            tuning,
        }
    }

    fn url(&self, suffix: &str) -> String {
        format!("{}/v1/runner/jobs/{}{}", self.base, self.job_id, suffix)
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// `GET /v1/runner/jobs/:id`, retried five times because a control
    /// plane that is redeploying while a task starts is normal and losing
    /// the job over it is not.
    pub fn fetch(&self) -> Result<Assignment, CallError> {
        let body = self.retry(5, || {
            self.agent
                .get(&self.url(""))
                .set("Authorization", &self.bearer())
                .call()
                .map_err(classify)
                .and_then(|r| {
                    r.into_string()
                        .map_err(|e| CallError::Unavailable(e.to_string()))
                })
        })?;
        parse_assignment(&body).map_err(CallError::Refused)
    }

    /// One log chunk. `seq` is 1-based and monotonic; re-sending a seq the
    /// control plane already has is an overwrite, not an error, which is
    /// what makes the retry below safe.
    pub fn post_chunk(&self, seq: u32, text: &str) -> Result<(), CallError> {
        self.agent
            .post(&self.url("/log"))
            .set("Authorization", &self.bearer())
            .set("Content-Type", "application/json")
            // Serialised here rather than through `send_json`: that needs
            // ureq's `json` feature, and turning it on would change the
            // feature set every other crate in the workspace builds ureq
            // with for the sake of one call.
            .send_string(&serde_json::json!({ "seq": seq, "text": text }).to_string())
            .map_err(classify)
            .map(drop)
    }

    /// The idle heartbeat. A job whose steps are silent for minutes — a
    /// long link step, a test suite that only prints at the end — is still
    /// alive, and without this the dispatcher's lease expires and another
    /// runner claims the same attempt.
    pub fn renew(&self) -> Result<(), CallError> {
        self.agent
            .post(&self.url("/lease"))
            .set("Authorization", &self.bearer())
            .send_string("")
            .map_err(classify)
            .map(drop)
    }

    /// The authoritative complete log. Chunks are for watching; this is
    /// what the log endpoint serves afterwards, so a chunk that was dropped
    /// under load does not leave a hole in the record.
    pub fn put_log(&self, text: &str) -> Result<(), CallError> {
        self.retry(3, || {
            self.agent
                .put(&self.url("/log"))
                .set("Authorization", &self.bearer())
                .set("Content-Type", "text/plain")
                .send_string(text)
                .map_err(classify)
                .map(drop)
        })
    }

    /// The verdict. Retried, because a verdict that never lands leaves the
    /// job to be failed by the dispatcher's overdue sweep hours later.
    pub fn finish(
        &self,
        state: &str,
        error: Option<&str>,
        abuse: Option<&str>,
    ) -> Result<(), CallError> {
        let mut body = serde_json::json!({
            "state": state,
            "error": match error { Some(e) => serde_json::Value::String(e.to_string()), None => serde_json::Value::Null },
        });
        // Absent rather than null on an ordinary verdict: the field says
        // "this job was stopped for abuse", and every job that was not is
        // the overwhelming majority.
        if let Some(kind) = abuse {
            body["abuse"] = serde_json::Value::String(kind.to_string());
        }
        self.retry(3, || {
            self.agent
                .post(&self.url("/finish"))
                .set("Authorization", &self.bearer())
                .set("Content-Type", "application/json")
                .send_string(&body.to_string())
                .map_err(classify)
                .map(drop)
        })
    }

    /// Retry only what retrying can fix. `Gone` and `Refused` come back on
    /// the first attempt: five rounds of backoff against a 403 is five
    /// rounds of telling an operator nothing.
    fn retry<T>(
        &self,
        attempts: u32,
        mut f: impl FnMut() -> Result<T, CallError>,
    ) -> Result<T, CallError> {
        let mut last = None;
        for attempt in 0..attempts {
            match f() {
                Ok(v) => return Ok(v),
                Err(CallError::Unavailable(m)) => {
                    last = Some(CallError::Unavailable(m));
                    if attempt + 1 < attempts {
                        std::thread::sleep(Duration::from_millis(
                            self.tuning.retry_base_ms << attempt,
                        ));
                    }
                }
                Err(other) => return Err(other),
            }
        }
        Err(last.expect("attempts is never zero, so the loop set a last error"))
    }
}

/// Map ureq's answer onto the one distinction the runner acts on.
fn classify(e: ureq::Error) -> CallError {
    match e {
        ureq::Error::Status(410, _) => CallError::Gone,
        ureq::Error::Status(code, r) if (500..600).contains(&code) => {
            CallError::Unavailable(format!("{code}: {}", snippet(r)))
        }
        ureq::Error::Status(code, r) => CallError::Refused(format!("{code}: {}", snippet(r))),
        ureq::Error::Transport(t) => CallError::Unavailable(t.to_string()),
    }
}

/// The first 200 bytes of an error body, for the container log. The whole
/// body could be an HTML error page from a proxy nobody knew was there.
fn snippet(r: ureq::Response) -> String {
    let mut s = String::new();
    let _ = r.into_reader().take(200).read_to_string(&mut s);
    s.trim().replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakecp::{FakeCp, Reply};

    fn client(cp: &FakeCp) -> Client {
        Client::new(
            &cp.base_url(),
            "job1",
            "tok",
            Tuning {
                retry_base_ms: 1,
                ..Tuning::default()
            },
        )
    }

    #[test]
    fn a_fetch_returns_the_assignment_and_carries_the_bearer_token() {
        let cp = FakeCp::start();
        let a = client(&cp).fetch().expect("fetch");
        assert_eq!(a.id, "job1");
        assert_eq!(cp.state().auth_seen, vec!["Bearer tok".to_string()]);
    }

    #[test]
    fn a_fetch_retries_a_5xx_and_then_succeeds() {
        let cp = FakeCp::start();
        cp.script_spec(vec![Reply::status(500), Reply::status(503)]);
        let a = client(&cp).fetch().expect("fetch");
        assert_eq!(a.id, "job1");
        assert_eq!(cp.state().spec_calls, 3);
    }

    #[test]
    fn a_fetch_gives_up_after_five_attempts_and_names_the_last_answer() {
        let cp = FakeCp::start();
        cp.script_spec(vec![Reply::status(500); 9]);
        let err = client(&cp).fetch().expect_err("must give up");
        assert_eq!(cp.state().spec_calls, 5, "five attempts, not nine");
        assert!(
            matches!(&err, CallError::Unavailable(m) if m.starts_with("500:")),
            "{err:?}"
        );
    }

    #[test]
    fn a_refusal_is_not_retried_and_a_410_is_its_own_answer() {
        for status in [401, 403, 404] {
            let cp = FakeCp::start();
            cp.script_spec(vec![Reply::status(status); 4]);
            let err = client(&cp).fetch().expect_err("must refuse");
            assert_eq!(cp.state().spec_calls, 1, "{status} must not be retried");
            assert!(matches!(&err, CallError::Refused(m) if m.starts_with(&format!("{status}:"))));
        }
        let cp = FakeCp::start();
        cp.script_spec(vec![Reply::status(410)]);
        assert_eq!(client(&cp).fetch().expect_err("gone"), CallError::Gone);
        assert_eq!(cp.state().spec_calls, 1);
    }

    #[test]
    fn a_document_the_runner_cannot_read_is_a_refusal_not_a_retry() {
        let cp = FakeCp::start();
        cp.script_spec(vec![Reply::body(200, "{\"id\": 7}")]);
        let err = client(&cp).fetch().expect_err("must refuse");
        assert_eq!(
            err,
            CallError::Refused("field \"id\" is missing or not a string".into())
        );
        assert_eq!(cp.state().spec_calls, 1);
    }

    #[test]
    fn a_control_plane_that_is_not_listening_is_unavailable_not_refused() {
        // A port nobody is bound to: the transport arm, with no network.
        let cp = FakeCp::start();
        let dead = format!("http://127.0.0.1:{}", cp.free_port());
        let c = Client::new(
            &dead,
            "job1",
            "tok",
            Tuning {
                retry_base_ms: 1,
                connect_timeout: Duration::from_millis(200),
                ..Tuning::default()
            },
        );
        assert!(matches!(c.fetch(), Err(CallError::Unavailable(_))));
    }

    #[test]
    fn the_log_lease_and_finish_calls_land_where_the_control_plane_expects() {
        let cp = FakeCp::start();
        let c = client(&cp);
        c.post_chunk(1, "hello").expect("chunk");
        c.renew().expect("lease");
        c.put_log("hello\nworld\n").expect("put log");
        c.finish("failed", Some("step \"Test\" exited 1"), None)
            .expect("finish");
        let s = cp.state();
        assert_eq!(s.chunks, vec![(1, "hello".to_string())]);
        assert_eq!(s.leases, 1);
        assert_eq!(s.final_log.as_deref(), Some("hello\nworld\n"));
        assert_eq!(
            s.finish,
            Some(("failed".into(), Some("step \"Test\" exited 1".into())))
        );
    }

    #[test]
    fn a_passing_verdict_reports_a_null_error() {
        let cp = FakeCp::start();
        client(&cp).finish("passed", None, None).expect("finish");
        assert_eq!(cp.state().finish, Some(("passed".into(), None)));
    }

    #[test]
    fn a_410_on_any_call_is_gone_and_a_5xx_run_out_is_unavailable() {
        let cp = FakeCp::start();
        cp.script_other(vec![Reply::status(410)]);
        assert_eq!(client(&cp).post_chunk(1, "x"), Err(CallError::Gone));

        let cp = FakeCp::start();
        cp.script_other(vec![Reply::status(500); 5]);
        let err = client(&cp)
            .finish("passed", None, None)
            .expect_err("must give up");
        assert!(matches!(err, CallError::Unavailable(_)));
        assert_eq!(cp.state().finish_calls, 3, "three attempts, then give up");
    }

    #[test]
    fn call_errors_read_as_themselves_in_the_container_log() {
        assert_eq!(CallError::Gone.to_string(), "job is no longer running");
        assert_eq!(
            CallError::Refused("403: nope".into()).to_string(),
            "403: nope"
        );
        assert_eq!(CallError::Unavailable("boom".into()).to_string(), "boom");
        assert!(format!("{:?}", Tuning::default()).contains("retry_base_ms"));
    }

    #[test]
    fn an_error_body_is_trimmed_to_something_a_container_log_can_hold() {
        let cp = FakeCp::start();
        cp.script_spec(vec![Reply::body(
            403,
            &format!("<html>{}</html>", "x".repeat(4000)),
        )]);
        let m = client(&cp).fetch().expect_err("must refuse").to_string();
        assert!(m.len() < 250, "{} bytes", m.len());
        assert!(m.starts_with("403: <html>xxx"), "{m}");
    }
}
