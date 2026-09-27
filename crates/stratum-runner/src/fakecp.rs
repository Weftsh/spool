//! A scripted control plane on a real socket, for the runner's own tests.
//!
//! Raw TCP rather than a framework, and in-crate rather than in
//! `stratum-testkit`, for the same reason the fake GitHub is raw TCP: the
//! thing under test is `ureq` talking HTTP to something that answers 410
//! halfway through a job, and a mock of the client would prove only that
//! the mock agrees with itself.
//!
//! It records what it was sent, and it can be told to answer badly: a run
//! of 5xx, a refusal, a 410 after the Nth chunk. Everything the runner
//! must survive is expressible as a script rather than as a special case
//! in the runner.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One canned answer.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    /// The `Content-Length` to *claim*, when it is not the length of the
    /// body. A proxy that drops a long poll halfway through is the shape
    /// of failure the claim loop has to survive, and it is not reachable
    /// any other way: the status line arrives fine and the read fails
    /// afterwards.
    claimed_len: Option<usize>,
}

impl Reply {
    pub fn status(status: u16) -> Reply {
        Reply {
            status,
            body: format!("{{\"error\":\"scripted {status}\"}}"),
            claimed_len: None,
        }
    }

    pub fn body(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.to_string(),
            claimed_len: None,
        }
    }

    /// An answer that promises `claimed` bytes and then hangs up.
    pub fn truncated(status: u16, body: &str, claimed: usize) -> Reply {
        Reply {
            status,
            body: body.to_string(),
            claimed_len: Some(claimed),
        }
    }
}

/// What the runner sent us, in order.
#[derive(Debug, Clone, Default)]
pub struct Record {
    pub spec_calls: u32,
    pub finish_calls: u32,
    pub chunks: Vec<(u32, String)>,
    pub leases: u32,
    pub final_log: Option<String>,
    pub finish: Option<(String, Option<String>)>,
    /// The verdict's `abuse` field, which is absent on every job that was
    /// not stopped for it. Recorded separately so a test can tell "not
    /// sent" from "sent as null".
    pub abuse: Option<String>,
    pub auth_seen: Vec<String>,
    /// The body of the last `POST /v1/runners/register`, so a test can
    /// assert what the runner told the server about itself.
    pub registered: Option<serde_json::Value>,
    pub register_calls: u32,
    pub claim_calls: u32,
}

impl Record {
    /// The chunks the runner streamed, in sequence order, concatenated —
    /// what a person watching the job live would have seen.
    pub fn streamed(&self) -> String {
        let mut c = self.chunks.clone();
        c.sort_by_key(|(seq, _)| *seq);
        c.into_iter().map(|(_, t)| t).collect()
    }
}

#[derive(Default)]
struct Plan {
    spec_body: Option<String>,
    spec_script: VecDeque<Reply>,
    other_script: VecDeque<Reply>,
    register_script: VecDeque<Reply>,
    claim_script: VecDeque<Reply>,
    /// How long a claim takes to answer, so a test can raise a stop while
    /// one is genuinely in flight — the server long-polls, and letting go
    /// of a claim that has not answered is the property that keeps Ctrl-C
    /// from taking twenty-five seconds.
    claim_delay: Duration,
    gone_after_chunks: Option<u32>,
    gone_on_finish: bool,
}

#[derive(Default)]
struct Shared {
    plan: Plan,
    rec: Record,
    /// The claim count again, as something a watcher thread can hold
    /// without holding the fake: a test that waits for the Nth claim
    /// outlives the borrow it would otherwise need.
    claims: Arc<AtomicU32>,
    chunk_count: Arc<AtomicU32>,
}

pub struct FakeCp {
    addr: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

/// The default job document: two trivially fast steps, so a test that only
/// cares about the transport does not have to describe a workflow.
pub fn spec_json(steps: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "job1", "run_id": "run1", "attempt": 1,
        "key": "test", "job": "test",
        "clone_url": "", "fetch_ref": "refs/heads/main",
        "commit_sha": "0000000000000000000000000000000000000000",
        "ref_name": "main", "event": "push", "change_key": serde_json::Value::Null,
        "image": "default", "timeout_minutes": 5,
        "env": {}, "matrix": {},
        "steps": steps,
    })
}

impl FakeCp {
    pub fn start() -> FakeCp {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake control plane");
        let addr = listener.local_addr().expect("addr");
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st) = (Arc::clone(&shared), Arc::clone(&stop));
        std::thread::spawn(move || {
            // `flatten` because a failed accept on a loopback listener is
            // not a case worth a branch: the next one either works or the
            // listener is closed and the loop ends.
            for conn in listener.incoming().flatten() {
                if st.load(Ordering::SeqCst) {
                    return;
                }
                handle(conn, &s);
            }
        });
        FakeCp { addr, shared, stop }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A port with nothing on it — bound and released, so it is one the
    /// kernel just handed out and nobody else is using.
    pub fn free_port(&self) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    }

    pub fn state(&self) -> Record {
        self.shared.lock().expect("lock").rec.clone()
    }

    /// Answers for `GET …/jobs/:id`, consumed in order; once the script is
    /// empty the real document is served.
    pub fn script_spec(&self, replies: Vec<Reply>) {
        self.shared.lock().expect("lock").plan.spec_script = replies.into();
    }

    /// Answers for every other route, consumed in order.
    pub fn script_other(&self, replies: Vec<Reply>) {
        self.shared.lock().expect("lock").plan.other_script = replies.into();
    }

    /// Serve this job document instead of the default one.
    pub fn set_spec(&self, body: &serde_json::Value) {
        self.shared.lock().expect("lock").plan.spec_body = Some(body.to_string());
    }

    /// Answer 410 to everything once `n` chunks have been accepted — the
    /// shape a cancellation takes from the runner's side.
    pub fn gone_after_chunks(&self, n: u32) {
        self.shared.lock().expect("lock").plan.gone_after_chunks = Some(n);
    }

    /// Answer 410 to the finish call and nothing else — a run settling
    /// while its last step was drawing to a close.
    pub fn gone_on_finish(&self) {
        self.shared.lock().expect("lock").plan.gone_on_finish = true;
    }

    /// Answers for `POST /v1/runners/register`, consumed in order; once
    /// the script is empty a successful registration is served.
    pub fn script_register(&self, replies: Vec<Reply>) {
        self.shared.lock().expect("lock").plan.register_script = replies.into();
    }

    /// Answers for `POST /v1/runners/claim`, consumed in order; once the
    /// script is empty the answer is 204 — nothing to run — which is what
    /// an idle fleet sees all day.
    pub fn script_claim(&self, replies: Vec<Reply>) {
        self.shared.lock().expect("lock").plan.claim_script = replies.into();
    }

    pub fn delay_claims(&self, d: Duration) {
        self.shared.lock().expect("lock").plan.claim_delay = d;
    }

    /// A counter a watcher thread can own. `state()` needs the fake to
    /// still be borrowable; a thread that waits for the Nth claim does
    /// not.
    pub fn claim_counter(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.shared.lock().expect("lock").claims)
    }

    /// The same, for log chunks: "the step has printed something" is how
    /// a test knows a job is genuinely running rather than still cloning.
    pub fn shared_chunks(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.shared.lock().expect("lock").chunk_count)
    }
}

impl Drop for FakeCp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the thread observes the flag.
        let _ = TcpStream::connect(self.addr);
    }
}

fn handle(mut c: TcpStream, shared: &Arc<Mutex<Shared>>) {
    let Some((method, path, headers, body)) = read_request(&mut c) else {
        return;
    };
    let mut s = shared.lock().expect("lock");
    if let Some(a) = headers
        .iter()
        .find(|(k, _)| k == "authorization")
        .map(|(_, v)| v.clone())
    {
        s.rec.auth_seen.push(a);
    }
    // The two routes a self-hosted runner uses before it has a job. They
    // are matched on the whole path rather than its last segment: `claim`
    // and `register` are org-level, not per-job, and folding them into the
    // per-job routing below is how a typo in either would quietly be
    // answered by the job endpoints instead.
    if path.ends_with("/v1/runners/register") {
        s.rec.register_calls += 1;
        s.rec.registered = serde_json::from_str(&body).ok();
        let r = s.plan.register_script.pop_front().unwrap_or_else(|| {
            Reply::body(
                201,
                "{\"runner_id\":\"rnr1\",\"credential\":\"weftr_secret\",\"org\":\"acme\",\
                 \"group\":\"default\",\
                 \"labels\":[\"self-hosted\",\"linux\",\"x64\",\"gpu\"]}",
            )
        });
        drop(s);
        return respond(&mut c, &r);
    }
    if path.ends_with("/v1/runners/claim") {
        s.rec.claim_calls += 1;
        s.claims.fetch_add(1, Ordering::SeqCst);
        let delay = s.plan.claim_delay;
        let r = s
            .plan
            .claim_script
            .pop_front()
            .unwrap_or_else(|| Reply::body(204, ""));
        drop(s);
        std::thread::sleep(delay);
        return respond(&mut c, &r);
    }

    let is_spec = method == "GET";
    if is_spec {
        s.rec.spec_calls += 1;
        if let Some(r) = s.plan.spec_script.pop_front() {
            drop(s);
            return respond(&mut c, &r);
        }
        let body = s
            .plan
            .spec_body
            .clone()
            .unwrap_or_else(|| spec_json(serde_json::json!([{"run": "true"}])).to_string());
        drop(s);
        return respond(&mut c, &Reply::body(200, &body));
    }

    // Record first, then decide the answer: a 410 the runner is about to
    // be told still describes a call it really made.
    if path.ends_with("/log") && method == "POST" {
        let v: serde_json::Value = serde_json::from_str(&body).expect("chunk is JSON");
        let seq = v["seq"].as_u64().expect("seq") as u32;
        let text = v["text"].as_str().expect("text").to_string();
        s.rec.chunks.push((seq, text));
        s.chunk_count.fetch_add(1, Ordering::SeqCst);
    } else if path.ends_with("/log") {
        s.rec.final_log = Some(body.clone());
    } else if path.ends_with("/lease") {
        s.rec.leases += 1;
    } else if path.ends_with("/finish") {
        s.rec.finish_calls += 1;
        let v: serde_json::Value = serde_json::from_str(&body).expect("finish is JSON");
        s.rec.finish = Some((
            v["state"].as_str().expect("state").to_string(),
            v["error"].as_str().map(str::to_string),
        ));
        s.rec.abuse = v["abuse"].as_str().map(str::to_string);
    }

    if s.plan.gone_on_finish && path.ends_with("/finish") {
        drop(s);
        return respond(&mut c, &Reply::status(410));
    }
    if let Some(n) = s.plan.gone_after_chunks {
        if s.rec.chunks.len() as u32 > n {
            drop(s);
            return respond(&mut c, &Reply::status(410));
        }
    }
    let scripted = s.plan.other_script.pop_front();
    drop(s);
    match scripted {
        Some(r) => respond(&mut c, &r),
        None => respond(&mut c, &Reply::body(200, "{\"lease_until\":1}")),
    }
}

/// Method, path, lower-cased headers, body. `None` when the peer hung up
/// before sending a request — which is exactly what `Drop`'s wake-up
/// connection does.
type Request = (String, String, Vec<(String, String)>, String);

fn read_request(c: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    // Deliberately small. A 4 KiB buffer would swallow every request this
    // fake ever sees in one read, and the reassembly below — the part that
    // matters when a runner posts a 200 KiB chunk — would be a path
    // nothing exercised.
    let mut byte = [0u8; 64];
    let head_end = loop {
        let n = c.read(&mut byte).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&byte[..n]);
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    if body.len() < len {
        // Read exactly what Content-Length promised. A `take` rather than
        // a loop with an EOF arm: the short-read case is the same answer
        // as the complete one, and a branch nothing can reach is a line
        // the coverage gate would rightly ask about.
        let remaining = (len - body.len()) as u64;
        c.take(remaining).read_to_end(&mut body).ok()?;
    }
    Some((
        method,
        path,
        headers,
        String::from_utf8_lossy(&body).to_string(),
    ))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `Connection: close` on every answer, so `ureq` never pools a socket
/// this fake is about to drop — the failure mode there looks like a flaky
/// network rather than a fixture choice.
fn respond(c: &mut TcpStream, r: &Reply) {
    let _ = write!(
        c,
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        r.status,
        r.claimed_len.unwrap_or(r.body.len()),
        r.body
    );
    let _ = c.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrouted_path_still_answers_so_a_typo_is_not_a_hang() {
        let cp = FakeCp::start();
        let r = ureq::post(&format!("{}/v1/runner/jobs/job1/nonsense", cp.base_url()))
            .send_string("{}")
            .expect("call");
        assert_eq!(r.status(), 200);
        assert_eq!(cp.state().chunks.len(), 0);
    }

    #[test]
    fn a_peer_that_hangs_up_without_a_request_is_not_a_panic() {
        let cp = FakeCp::start();
        // Exactly what Drop's wake-up connection does.
        drop(TcpStream::connect(cp.addr).expect("connect"));
        // The server is still serving afterwards.
        assert_eq!(
            ureq::get(&format!("{}/v1/runner/jobs/job1", cp.base_url()))
                .call()
                .expect("call")
                .status(),
            200
        );
    }

    #[test]
    fn a_body_larger_than_one_read_is_reassembled() {
        let cp = FakeCp::start();
        let big = "x".repeat(200_000);
        ureq::post(&format!("{}/v1/runner/jobs/job1/log", cp.base_url()))
            .send_string(&serde_json::json!({"seq": 1, "text": big}).to_string())
            .expect("call");
        assert_eq!(cp.state().chunks[0].1.len(), 200_000);
    }

    #[test]
    fn the_recorded_chunks_read_back_in_sequence_order() {
        let r = Record {
            chunks: vec![(2, "b".into()), (1, "a".into())],
            ..Record::default()
        };
        assert_eq!(r.streamed(), "ab");
        assert!(format!("{:?}", Reply::status(500)).contains("500"));
    }
}
