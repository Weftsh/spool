//! Smart-HTTP transport for the git wire protocol, wrapping stratum-proto.
//!
//! The engine is blocking by design; every protocol call runs on the
//! blocking pool behind a semaphore, writing into a channel-backed `Write`
//! adapter that becomes the streaming response body. The first chunk is
//! awaited before the response status is chosen: an error before any byte
//! means a clean HTTP error, an error mid-stream is already reported
//! in-band (sideband channel 3 / pkt-line ERR), exactly like the research
//! serving path.

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use std::io::Write;
use std::sync::Arc;
use stratum_store::{LatencyModel, ObjectStore};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

/// Work-class permits: a clone storm queues here instead of exhausting the
/// blocking pool.
pub struct GitPermits {
    pub serve: Arc<tokio::sync::Semaphore>,
    pub receive: Arc<tokio::sync::Semaphore>,
}

impl Default for GitPermits {
    fn default() -> Self {
        GitPermits {
            serve: Arc::new(tokio::sync::Semaphore::new(64)),
            receive: Arc::new(tokio::sync::Semaphore::new(16)),
        }
    }
}

/// What an accepted push moved, or `None` when nothing landed. The
/// server's post-push side-effects key off this: the workflow trigger
/// needs the refs and the commits, the rest only need to know it landed.
pub type Accepted = Option<Vec<stratum_proto::receive::Update>>;

/// Everything the wire handlers need to serve one repo.
#[derive(Clone)]
pub struct RepoCtx {
    /// Object-store base URL (bucket).
    pub store_url: String,
    /// Full layout prefix for this repo (`o/<org>/r/<repo>/<layout>`).
    pub prefix: String,
}

impl RepoCtx {
    pub fn store(&self) -> ObjectStore {
        ObjectStore::new(&self.store_url, LatencyModel::None)
    }

    pub fn load_manifest(&self) -> Result<stratum_store::Manifest, String> {
        let (repo, layout) = self.prefix.rsplit_once('/').ok_or("bad repo prefix")?;
        stratum_store::load_manifest(&self.store(), repo, layout)
    }
}

pub fn err_response(err: String) -> Response {
    let status = if stratum_engine::errclass::is_absent(&err) {
        StatusCode::NOT_FOUND
    } else if err.contains("protocol v2 required") || is_malformed_body(&err) {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, format!("weft: {err}\n")).into_response()
}

/// The request body could not be read as pkt-lines: a length that is not
/// hex, a length the body does not reach, a 2- or 3-byte length. Those are
/// the client's error and answer 400. They used to fall through to 500,
/// so a hand-rolled or truncated upload-pack read as the server failing.
/// The strings are `stratum_proto::pktline`'s own plus `read_exact`'s.
fn is_malformed_body(err: &str) -> bool {
    err.contains("bad pkt len")
        || err.contains("invalid pkt length")
        || err.contains("truncated pkt-line length")
        || err.contains("failed to fill whole buffer")
}

/// GET `/…/info/refs?service=git-upload-pack` — v2 capability advert.
/// The v2 gate lives here: a v0/v1 client would misparse the advert.
pub async fn upload_pack_advert(
    headers: &HeaderMap,
    cdn: Option<&stratum_proto::serve::CdnPack>,
) -> Response {
    let proto = headers
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !proto.split(':').any(|t| t == "version=2") {
        return err_response(format!(
            "protocol v2 required (client sent Git-Protocol: {proto:?})"
        ));
    }
    let mut buf = Vec::new();
    if let Err(e) = stratum_proto::serve::advertise(&mut buf, cdn) {
        return err_response(e);
    }
    advert_ok(buf, stratum_proto::UPLOAD_PACK_ADVERT_TYPE)
}

/// GET `/…/info/refs?service=git-receive-pack` — v0 ref advert.
pub async fn receive_pack_advert(ctx: RepoCtx, permits: Arc<GitPermits>) -> Response {
    let _p = permits.serve.acquire().await;
    let out = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let store = ctx.store();
        let manifest = ctx.load_manifest()?;
        let mut buf = Vec::new();
        stratum_proto::receive::advertise(&store, &manifest, &mut buf)?;
        Ok(buf)
    })
    .await
    .unwrap_or_else(|e| Err(format!("task join: {e}")));
    match out {
        Ok(buf) => advert_ok(buf, stratum_proto::RECEIVE_PACK_ADVERT_TYPE),
        Err(e) => err_response(e),
    }
}

/// A receive-pack advertisement whose only content is an in-band ERR pkt:
/// stock git surfaces it as "remote error: <msg>" — the actionable
/// read-only rejection the mirror product wants (M4).
pub fn advert_error(msg: &str) -> Response {
    let mut buf = Vec::new();
    let _ = stratum_proto::pktline::write_text(&mut buf, "# service=git-receive-pack");
    let _ = stratum_proto::pktline::write_flush(&mut buf);
    let _ = stratum_proto::pktline::write_text(&mut buf, &format!("ERR {msg}"));
    let _ = stratum_proto::pktline::write_flush(&mut buf);
    advert_ok(buf, stratum_proto::RECEIVE_PACK_ADVERT_TYPE)
}

fn advert_ok(buf: Vec<u8>, ctype: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        buf,
    )
        .into_response()
}

/// POST `/…/git-upload-pack` — ls-refs / fetch, streamed.
pub async fn upload_pack(
    ctx: RepoCtx,
    permits: Arc<GitPermits>,
    body: Bytes,
    cdn: Option<stratum_proto::serve::CdnPack>,
) -> Response {
    let permit = match permits.serve.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return err_response("shutting down".into()),
    };
    let (tx, mut rx) = mpsc::channel::<Bytes>(16);
    let task = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let store = ctx.store();
        let manifest = ctx.load_manifest()?;
        let mut w = ChannelWriter::new(tx);
        stratum_proto::serve::upload_pack(&store, &manifest, &body, &mut w, cdn.as_ref())?;
        w.finish().map_err(|e| e.to_string())
    });

    match rx.recv().await {
        None => {
            // Nothing was written: surface the error as a clean HTTP status.
            drop(permit);
            let res = task
                .await
                .unwrap_or_else(|e| Err(format!("task join: {e}")));
            err_response(res.err().unwrap_or_else(|| "empty response".into()))
        }
        Some(first) => {
            // Bytes are flowing: status is 200; a later failure is already
            // reported in-band and the stream simply ends. Hold the permit
            // until the blocking task finishes.
            tokio::spawn(async move {
                if let Ok(Err(e)) = task.await {
                    eprintln!("weft: upload-pack stream error: {e}");
                }
                drop(permit);
            });
            let stream = tokio_stream::once(Ok::<_, std::io::Error>(first))
                .chain(ReceiverStream::new(rx).map(Ok));
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, stratum_proto::UPLOAD_PACK_RESULT_TYPE),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                Body::from_stream(stream),
            )
                .into_response()
        }
    }
}

/// POST `/…/git-upload-pack` for a **changeset workspace** — the same
/// streaming shape, against a repository that exists only in memory.
///
/// Separate from [`upload_pack`] rather than generic over the two,
/// because what they share is the transport and what differs is
/// everything else: there is no object store to open, no manifest to
/// load, no CDN to consult, no mirror freshness contract and nothing to
/// meter. Threading a workspace through the repository path as a special
/// case would put five `if synthetic` branches on the hot path that
/// serves every clone on the fleet.
pub async fn upload_pack_workspace(
    ws: Arc<stratum_proto::workspace::Workspace>,
    permits: Arc<GitPermits>,
    body: Bytes,
) -> Response {
    let permit = match permits.serve.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return err_response("shutting down".into()),
    };
    let (tx, mut rx) = mpsc::channel::<Bytes>(16);
    let task = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut w = ChannelWriter::new(tx);
        stratum_proto::workspace::upload_pack(&ws, &body, &mut w)?;
        w.finish().map_err(|e| e.to_string())
    });
    match rx.recv().await {
        None => {
            drop(permit);
            let res = task
                .await
                .unwrap_or_else(|e| Err(format!("task join: {e}")));
            err_response(res.err().unwrap_or_else(|| "empty response".into()))
        }
        Some(first) => {
            tokio::spawn(async move {
                if let Ok(Err(e)) = task.await {
                    eprintln!("weft: workspace upload-pack stream error: {e}");
                }
                drop(permit);
            });
            let stream = tokio_stream::once(Ok::<_, std::io::Error>(first))
                .chain(ReceiverStream::new(rx).map(Ok));
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, stratum_proto::UPLOAD_PACK_RESULT_TYPE),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                Body::from_stream(stream),
            )
                .into_response()
        }
    }
}

/// POST `/…/git-receive-pack` — the push path. Reports are tiny; buffered.
/// Returns (response, accepted): accepted=true means the WAL entry and
/// manifest swap landed and the caller should audit the push.
/// `protected` names branches only the land queue may move; the engine
/// answers updates to them with an in-band `ng`. `room` is how many
/// bytes the org's private storage may still take (`None`: uncapped);
/// a larger pack gets the same in-band `ng`, before anything is stored.
///
/// The push itself is landed (or forwarded) by [`crate::push::receive`];
/// this turns its answer into the HTTP response. The updates come back
/// with the report: the caller triggers workflows per ref, and it must
/// learn what moved from the push itself, not from a manifest read that
/// a second push could have overtaken.
pub fn receive_pack_response(out: Result<(Vec<u8>, Accepted), String>) -> (Response, Accepted) {
    match out {
        Ok((buf, accepted)) => (
            (
                StatusCode::OK,
                [
                    (
                        header::CONTENT_TYPE,
                        stratum_proto::RECEIVE_PACK_RESULT_TYPE,
                    ),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                buf,
            )
                .into_response(),
            accepted,
        ),
        Err(e) => (err_response(e), None),
    }
}

/// Blocking `Write` that ships chunks into an async stream — the HTTP
/// response body here, the SSH channel in `ssh.rs`.
pub(crate) struct ChannelWriter {
    tx: mpsc::Sender<Bytes>,
    buf: Vec<u8>,
}

const CHUNK: usize = 64 * 1024;

impl ChannelWriter {
    pub(crate) fn new(tx: mpsc::Sender<Bytes>) -> Self {
        ChannelWriter {
            tx,
            buf: Vec::with_capacity(CHUNK),
        }
    }

    fn send_buf(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
        self.tx
            .blocking_send(chunk)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client went away"))
    }

    pub(crate) fn finish(mut self) -> std::io::Result<()> {
        self.send_buf()
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.send_buf()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send_buf()
    }
}
