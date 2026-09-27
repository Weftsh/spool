//! Git-over-SSH front door: `git clone ssh://git@host:port/org/repo`.
//!
//! A second transport onto the same engine paths the smart-HTTP door
//! uses — `stratum_proto` serving/receive are transport-agnostic by
//! design, so this module is exactly a bridge: russh channels in,
//! pkt-line streams out, with the same permits, metering, freshness
//! contract, and push side-effects as `app.rs`'s HTTP handlers.
//!
//! Authentication is publickey-only. A presented key is resolved by its
//! OpenSSH SHA-256 fingerprint to a registered `ssh_keys` row, which
//! binds it to a token — the key authenticates AS that token's
//! principal, so scopes and instant revocation are the token machinery.
//!
//! Protocol notes (the load-bearing details):
//! - upload-pack runs protocol v2 (the only version the engine serves).
//!   The client asks for it via the `GIT_PROTOCOL` env request; without
//!   it we answer with an in-band pkt ERR. The server speaks first: the
//!   capability advertisement body, then a loop of command requests
//!   (pkt frames to a flush-pkt), each answered by the engine. A lone
//!   flush-pkt or EOF ends the session.
//! - receive-pack is v0: ref advertisement first, then the client's
//!   commands+pack are buffered to channel EOF (verified empirically:
//!   git half-closes its write side after the pack), handed to the
//!   engine, and answered with the report-status.
//! - Failures are in-band pkt `ERR <msg>` — stock git prints
//!   "remote error: <msg>" — plus a non-zero exit status.

use crate::app::{SharedState, WireDeny};
use crate::git_http::{ChannelWriter, RepoCtx};
use crate::mirror::freshness;
use bytes::Bytes;
use russh::keys::HashAlg;
use russh::server::{Auth, Handle, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use std::collections::HashMap;
use std::sync::Arc;
use stratum_control::auth::{Principal, Scope};
use stratum_control::registry::{Repo, RepoKind};
use tokio::sync::{mpsc, watch};

/// The SSH front door, prepared (key decoded, socket bound) before the
/// serve loop starts so misconfiguration fails the boot, not the first
/// connection.
pub struct SshFront {
    state: SharedState,
    listener: tokio::net::TcpListener,
    config: Arc<russh::server::Config>,
}

pub async fn prepare(
    state: SharedState,
    bind: &str,
    host_key_pem: &str,
) -> Result<SshFront, String> {
    let key = russh::keys::decode_secret_key(host_key_pem, None)
        .map_err(|e| format!("STRATUM_SSH_HOST_KEY: {e}"))?;
    let config = Arc::new(russh::server::Config {
        keys: vec![key],
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        auth_rejection_time: std::time::Duration::from_millis(300),
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| format!("bind ssh {bind}: {e}"))?;
    eprintln!(
        "stratum-server ssh listening on {}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    Ok(SshFront {
        state,
        listener,
        config,
    })
}

impl SshFront {
    /// Accept until the shared stop signal fires (the same SIGINT/SIGTERM
    /// watcher that drains the HTTP door).
    pub async fn run(self, mut stop: watch::Receiver<bool>) -> Result<(), String> {
        let mut server = SshServer { state: self.state };
        let running = server.run_on_socket(self.config, &self.listener);
        let handle = running.handle();
        tokio::select! {
            r = running => r.map_err(|e| e.to_string()),
            _ = stop.changed() => {
                handle.shutdown("stratum-server is shutting down".into());
                Ok(())
            }
        }
    }
}

struct SshServer {
    state: SharedState,
}

impl Server for SshServer {
    type Handler = SshConn;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> SshConn {
        SshConn {
            state: self.state.clone(),
            identity: None,
            chans: HashMap::new(),
        }
    }
}

/// Handler-method failures abort the connection; a String is all the
/// diagnostics that path needs (read via {:?} when russh logs it).
#[derive(Debug)]
pub struct ConnError(#[allow(dead_code)] String);

impl From<russh::Error> for ConnError {
    fn from(e: russh::Error) -> Self {
        ConnError(e.to_string())
    }
}

/// One SSH connection: the authenticated principal plus per-channel
/// state (env before exec; stdin bridge to the worker after).
struct SshConn {
    state: SharedState,
    identity: Option<SshIdentity>,
    chans: HashMap<ChannelId, Chan>,
}

#[derive(Default)]
struct Chan {
    /// Value of the client's GIT_PROTOCOL env request, if any.
    git_protocol: Option<String>,
    /// Live stdin bridge to the channel's worker; dropped on EOF so the
    /// worker sees end-of-input.
    stdin: Option<mpsc::Sender<Vec<u8>>>,
}

/// Who a presented public key says you are — before any repository is
/// named.
///
/// A *personal* key names a person, and a person belongs to as many
/// namespaces as they have joined, so the namespace cannot be decided
/// here. It is decided in `exec_request`, where the client finally says
/// which one it wants. A *deploy* key names a token, which is org-bound
/// by construction and carries its namespace with it.
///
/// Getting this wrong is the defect this replaced: the key row carried
/// an org, so one laptop key reached exactly one namespace and the
/// second registration was refused as a duplicate fingerprint.
#[derive(Clone)]
enum SshIdentity {
    /// A person, by user id. Their role is resolved per namespace, on
    /// every request, so a membership change lands on the next one.
    Person(String),
    /// A deploy key's token, already resolved and already org-bound.
    Deploy(Principal),
}

/// fingerprint → active key row → whoever that row names.
///
/// Both lookups hit the database, so revoking the key, the token, the
/// membership or the account cuts access on the very next connection.
/// Any error fails closed.
fn identity_for_key(state: &SharedState, key: &russh::keys::PublicKey) -> Option<SshIdentity> {
    let fp = key.fingerprint(HashAlg::Sha256).to_string();
    let row = match stratum_control::sshkeys::lookup_active(&state.db, &fp) {
        Ok(r) => r?,
        Err(e) => {
            eprintln!("weft: ssh key lookup failed: {e}");
            return None;
        }
    };
    match (&row.user_id, &row.token_id, &row.org_id) {
        (Some(user_id), _, _) => Some(SshIdentity::Person(user_id.clone())),
        (None, Some(token_id), _) => {
            match stratum_control::auth::principal_for_token_id(&state.db, token_id) {
                Ok(p) => p.map(SshIdentity::Deploy),
                Err(e) => {
                    eprintln!("weft: ssh principal lookup failed: {e}");
                    None
                }
            }
        }
        // A key bound to neither cannot be produced by any write path;
        // refuse rather than invent an authority for it.
        (None, None, _) => None,
    }
}

/// The authority this identity has in one namespace, or `None` if it has
/// none there — which the caller must render as "not found", never as a
/// different answer from "no such namespace".
fn principal_in_org(
    state: &SharedState,
    identity: &SshIdentity,
    org_id: &str,
) -> Option<Principal> {
    match identity {
        SshIdentity::Person(user_id) => {
            match stratum_control::members::role_of(&state.db, org_id, user_id) {
                Ok(role) => role.map(|r| Principal::for_user(org_id, user_id, r.scopes())),
                Err(e) => {
                    eprintln!("weft: ssh role lookup failed: {e}");
                    None
                }
            }
        }
        SshIdentity::Deploy(p) if p.org_id == org_id => Some(p.clone()),
        SshIdentity::Deploy(_) => None,
    }
}

impl Handler for SshConn {
    type Error = ConnError;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        // Answer the probe honestly so clients with several keys don't
        // burn signatures on ones we'll refuse.
        Ok(if identity_for_key(&self.state, key).is_some() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        // Signature already verified by russh; possession is proven.
        match identity_for_key(&self.state, key) {
            Some(who) => {
                self.identity = Some(who);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.chans.insert(channel.id(), Chan::default());
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "GIT_PROTOCOL" {
            if let Some(c) = self.chans.get_mut(&channel) {
                c.git_protocol = Some(value.to_string());
            }
        }
        // Other variables are accepted and ignored — git only needs
        // GIT_PROTOCOL through.
        session.channel_success(channel)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data);
        let Ok((service, path)) = parse_git_command(&cmd) else {
            // Not a git command — this is a git host, nothing else runs.
            session.channel_failure(channel)?;
            return Ok(());
        };
        let Some(identity) = self.identity.clone() else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        let Some(chan) = self.chans.get_mut(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;

        let v2 = chan
            .git_protocol
            .as_deref()
            .map(|v| v.split(':').any(|t| t == "version=2"))
            .unwrap_or(false);
        let (tx, stdin_rx) = mpsc::channel::<Vec<u8>>(64);
        chan.stdin = Some(tx);
        let handle = session.handle();
        let state = self.state.clone();

        let need = match service {
            GitService::UploadPack => Scope::RepoRead,
            GitService::ReceivePack => Scope::RepoWrite,
        };
        // The namespace is known only now, from the command the client
        // sent — so this is where a person's key becomes an authority.
        // A namespace that does not exist and one this identity cannot
        // reach must be indistinguishable (R8).
        let principal = stratum_control::registry::org_by_name(&state.db, path.org())
            .ok()
            .flatten()
            .and_then(|o| principal_in_org(&state, &identity, &o.id));
        // Both forms resolve to "what may this identity serve here", and
        // both mask the same way — a changeset workspace is readable by
        // exactly whoever may read every one of its members, so a denial
        // there is a denial of the whole path.
        // A hosted runner's own fetch is never refused for transfer and
        // never billed — decided here, where the credential is in hand.
        let runner = crate::metering::is_runner(&state.db, principal.as_ref());
        let resolved = match &path {
            WirePath::Repo { org, repo } => Resolved::Repo(
                match &principal {
                    Some(p) => crate::app::wire_repo_for_principal(&state, p, org, repo, need),
                    // A key this server knows, under a namespace it has
                    // no role in: a public repository is still served,
                    // and a push to one is refused in words rather than
                    // masked — the key's owner can read it anyway.
                    None => crate::app::wire_repo_for_outsider(&state, org, repo, need),
                }
                .map(|(r, ctx)| {
                    Box::new(RepoTarget {
                        org: org.clone(),
                        name: repo.clone(),
                        repo: r,
                        ctx,
                    })
                }),
            ),
            WirePath::Changeset { org, key } => Resolved::Workspace(
                crate::changeset_workspace::wire_for_identity(&state, principal.as_ref(), org, key),
            ),
        };
        tokio::spawn(async move {
            let result = match resolved {
                Resolved::Repo(Ok(target)) => {
                    let RepoTarget {
                        org,
                        name,
                        repo,
                        ctx,
                    } = *target;
                    match service {
                        GitService::UploadPack => {
                            upload_session(
                                &state, &repo, &ctx, &org, &name, v2, runner, &handle, channel,
                                stdin_rx,
                            )
                            .await
                        }
                        GitService::ReceivePack => {
                            let actx = stratum_control::audit::AuditCtx::of(
                                &repo.org_id,
                                principal.as_ref(),
                            );
                            receive_session(&state, &repo, &ctx, &actx, &handle, channel, stdin_rx)
                                .await
                        }
                    }
                }
                Resolved::Workspace(Ok(built)) => match service {
                    GitService::UploadPack => {
                        workspace_session(&built.ws, v2, &handle, channel, stdin_rx).await
                    }
                    // The refusal is in-band, so git prints it: the
                    // combination cannot be pushed to, but every commit
                    // in it has a repository that can.
                    GitService::ReceivePack => {
                        Err(format!("weft: {}", crate::changeset_workspace::READ_ONLY))
                    }
                },
                // Existence masking (R8): denied and absent are the same
                // answer over every transport.
                Resolved::Repo(Err(d)) | Resolved::Workspace(Err(d)) => match d {
                    WireDeny::NotFound => Err("weft: repository not found".into()),
                    WireDeny::ReadOnly(msg) | WireDeny::Internal(msg) => {
                        Err(format!("weft: {msg}"))
                    }
                },
            };
            finish(&handle, channel, result).await;
        });
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(c) = self.chans.get(&channel) {
            if let Some(tx) = &c.stdin {
                // A gone worker means the op already ended (e.g. after an
                // in-band ERR); surplus client bytes are dropped.
                let _ = tx.send(data.to_vec()).await;
            }
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(c) = self.chans.get_mut(&channel) {
            // Dropping the sender is end-of-stdin for the worker.
            c.stdin = None;
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.chans.remove(&channel);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // No interactive anything: git exec requests only.
        session.channel_failure(channel)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }
}

/// A repository the identity may serve, with the names it was reached
/// by: carrying them here is what makes "resolved a repository, but the
/// path was a changeset's" unrepresentable rather than an arm nothing
/// can reach.
struct RepoTarget {
    org: String,
    name: String,
    repo: Repo,
    ctx: RepoCtx,
}

/// What the path resolved to, once the identity has been applied.
///
/// The repository half is boxed: a `Repo` row plus its `RepoCtx` is much
/// the larger of the two, and every changeset request would otherwise
/// carry that much stack for a variant it does not use.
enum Resolved {
    Repo(Result<Box<RepoTarget>, WireDeny>),
    Workspace(Result<crate::changeset_workspace::Built, WireDeny>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitService {
    UploadPack,
    ReceivePack,
}

/// What the client asked to talk to: a repository, or the synthetic
/// repository a changeset is cloned as.
///
/// The two are told apart by shape alone — `org/repo` against
/// `org/changesets/key` — which is why `changesets` is reserved as a
/// repository name: without that, a repository of that name would be
/// reachable at a path this parser reads as a changeset's.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WirePath {
    Repo { org: String, repo: String },
    Changeset { org: String, key: String },
}

impl WirePath {
    /// The namespace, which both forms have and which is where a key
    /// becomes an authority.
    fn org(&self) -> &str {
        match self {
            WirePath::Repo { org, .. } | WirePath::Changeset { org, .. } => org,
        }
    }
}

/// Parse the exec command stock git sends:
/// `git-upload-pack 'org/repo.git'` (also the `git upload-pack` spelling,
/// unquoted/double-quoted paths, optional leading `/` and `.git`), or
/// `git-upload-pack 'org/changesets/Ic5.git'` for a changeset workspace.
fn parse_git_command(cmd: &str) -> Result<(GitService, WirePath), String> {
    let cmd = cmd.trim();
    let rest = [
        "git-upload-pack",
        "git upload-pack",
        "git-receive-pack",
        "git receive-pack",
    ]
    .iter()
    .find_map(|p| cmd.strip_prefix(p).map(|r| (*p, r)));
    let Some((verb, rest)) = rest else {
        return Err(format!("not a git service command: {cmd:?}"));
    };
    let service = if verb.contains("upload") {
        GitService::UploadPack
    } else {
        GitService::ReceivePack
    };
    let path = rest.trim();
    if path.is_empty() {
        return Err("missing repository path".into());
    }
    let path = path
        .strip_prefix('\'')
        .and_then(|p| p.strip_suffix('\''))
        .or_else(|| path.strip_prefix('"').and_then(|p| p.strip_suffix('"')))
        .unwrap_or(path);
    let path = path.trim_start_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.contains("..") {
        return Err(format!("repository path must not traverse, got {path:?}"));
    }
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        // `org/changesets` with nothing after it is a *repository* named
        // `changesets`, and falls through to the two-segment arm. The
        // registry refuses to create one, so it resolves to not-found —
        // the same answer as any other repository that is not there.
        [org, "changesets", key] if !org.is_empty() && !key.is_empty() => Ok((
            service,
            WirePath::Changeset {
                org: (*org).to_string(),
                key: (*key).to_string(),
            },
        )),
        [org, repo] if !org.is_empty() && !repo.is_empty() => Ok((
            service,
            WirePath::Repo {
                org: (*org).to_string(),
                repo: (*repo).to_string(),
            },
        )),
        _ => Err(format!(
            "repository path must be org/repo or org/changesets/key, got {path:?}"
        )),
    }
}

/// Scan `buf` for one complete protocol-v2 command request — every pkt
/// frame up to and including a flush-pkt. Returns (bytes consumed,
/// request including the flush); None until the flush has arrived.
fn split_v2_request(buf: &[u8]) -> Result<Option<(usize, Vec<u8>)>, String> {
    let mut i = 0usize;
    loop {
        if buf.len() < i + 4 {
            return Ok(None);
        }
        let len = std::str::from_utf8(&buf[i..i + 4])
            .ok()
            .and_then(|s| usize::from_str_radix(s, 16).ok())
            .ok_or("bad pkt-line length")?;
        match len {
            0 => {
                let end = i + 4;
                return Ok(Some((end, buf[..end].to_vec())));
            }
            1 => i += 4, // delim-pkt
            2..=3 => return Err("invalid pkt-line length".into()),
            _ => {
                if buf.len() < i + len {
                    return Ok(None);
                }
                i += len;
            }
        }
    }
}

async fn send(handle: &Handle, chan: ChannelId, data: Vec<u8>) -> Result<(), String> {
    handle
        .data(chan, Bytes::from(data))
        .await
        .map_err(|_| "client went away".to_string())
}

/// End the channel: report any error as an in-band pkt ERR (git shows
/// "remote error: <msg>"), then exit status, EOF, close.
async fn finish(handle: &Handle, chan: ChannelId, result: Result<(), String>) {
    let code = match result {
        Ok(()) => 0,
        Err(msg) => {
            let mut buf = Vec::new();
            let _ = stratum_proto::pktline::write_text(&mut buf, &format!("ERR {msg}"));
            let _ = send(handle, chan, buf).await;
            1
        }
    };
    let _ = handle.exit_status_request(chan, code).await;
    let _ = handle.eof(chan).await;
    let _ = handle.close(chan).await;
}

/// git-upload-pack over SSH: advertise v2 capabilities, then answer
/// command requests until the client is done.
#[allow(clippy::too_many_arguments)]
async fn upload_session(
    state: &SharedState,
    repo: &Repo,
    ctx: &RepoCtx,
    org_name: &str,
    repo_name: &str,
    v2: bool,
    runner: bool,
    handle: &Handle,
    chan: ChannelId,
    mut stdin: mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
    if !v2 {
        return Err(
            "weft: git protocol v2 required (git ≥ 2.26 sends it by default; \
             set protocol.version=2)"
                .into(),
        );
    }
    let cdn = crate::cdn::resolve(state, org_name, repo_name, &ctx.prefix);
    let mut advert = Vec::new();
    stratum_proto::serve::advertise_body(&mut advert, cdn.as_ref().map(|o| &o.pack))
        .map_err(|e| format!("weft: {e}"))?;
    send(handle, chan, advert).await?;

    let mut pending: Vec<u8> = Vec::new();
    loop {
        while let Some((consumed, req)) =
            split_v2_request(&pending).map_err(|e| format!("weft: {e}"))?
        {
            pending.drain(..consumed);
            if req.len() <= 4 {
                // Lone flush-pkt: the client is done with this session.
                return Ok(());
            }
            serve_one(state, repo, ctx, handle, chan, req, cdn.as_ref(), runner).await?;
        }
        match stdin.recv().await {
            Some(chunk) => {
                pending.extend_from_slice(&chunk);
                if pending.len() > crate::app::BODY_LIMIT {
                    return Err("weft: request exceeds the 64 MiB limit".into());
                }
            }
            None if pending.is_empty() => return Ok(()),
            None => return Err("weft: truncated request".into()),
        }
    }
}

/// One v2 command request (ls-refs or fetch) through the engine, with
/// the mirror freshness contract and metering the HTTP door applies.
#[allow(clippy::too_many_arguments)]
async fn serve_one(
    state: &SharedState,
    repo: &Repo,
    ctx: &RepoCtx,
    handle: &Handle,
    chan: ChannelId,
    body: Vec<u8>,
    cdn: Option<&crate::cdn::CdnOffer>,
    runner: bool,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    let summary = stratum_proto::serve::parse_fetch_summary(&body);
    if repo.kind == RepoKind::Mirror {
        if let Some(s) = &summary {
            if !s.wants.is_empty() {
                // M2 over SSH: the explicit failure answers become in-band
                // ERR pkts. (The honest-staleness signal on success is an
                // HTTP-header concept; SSH has no equivalent slot.)
                freshness::ensure_wants_core(state, repo, &s.wants)
                    .await
                    .map_err(|d| match d {
                        freshness::Denied::NotFound { msg, .. } => format!("stratum-mirror: {msg}"),
                        freshness::Denied::Internal(e) => format!("weft: {e}"),
                    })?;
            }
        }
    }
    // Offloaded only when a pack was advertised *and* this client opted
    // in — that pair is what actually sends bytes to the edge.
    let offloaded = cdn.is_some() && summary.as_ref().is_some_and(|s| s.wants_packfile_uris);
    let permit = state
        .permits
        .serve
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "weft: shutting down".to_string())?;
    let (tx, mut rx) = mpsc::channel::<Bytes>(16);
    let ctx2 = ctx.clone();
    let pack_size = cdn.map(|o| o.size).unwrap_or(0);
    let cdn = cdn.map(|o| o.pack.clone());
    let task = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let store = ctx2.store();
        let manifest = ctx2.load_manifest()?;
        let mut w = ChannelWriter::new(tx);
        stratum_proto::serve::upload_pack(&store, &manifest, &body, &mut w, cdn.as_ref())?;
        w.finish().map_err(|e| e.to_string())
    });
    let mut bytes_out: u64 = 0;
    while let Some(chunk) = rx.recv().await {
        bytes_out += chunk.len() as u64;
        send(handle, chan, chunk.to_vec()).await?;
    }
    let res = task
        .await
        .unwrap_or_else(|e| Err(format!("task join: {e}")));
    drop(permit);
    res.map_err(|e| format!("weft: {e}"))?;
    match &summary {
        Some(s) => {
            let kind = crate::metering::egress_kind(s.haves == 0, offloaded, runner);
            state
                .meter
                .record_offload(&repo.id, offloaded, runner, pack_size);
            state.meter.record(
                &repo.id,
                kind,
                bytes_out,
                Some(start.elapsed().as_millis() as u64),
            );
        }
        // ls-refs counts as an absorbed request, like HTTP adverts.
        None => state.meter.record(&repo.id, "api", 0, None),
    }
    Ok(())
}

/// git-upload-pack over SSH for a changeset workspace.
///
/// The same session shape as [`upload_session`] — advertise, then answer
/// v2 requests until the client sends a lone flush — against a
/// repository that was built in memory before the first byte went out.
/// Nothing is streamed from storage and nothing is metered: the pack is
/// three objects, and there is no repository whose account it belongs to.
async fn workspace_session(
    ws: &stratum_proto::workspace::Workspace,
    v2: bool,
    handle: &Handle,
    chan: ChannelId,
    mut stdin: mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
    if !v2 {
        return Err(
            "weft: git protocol v2 required (git ≥ 2.26 sends it by default; \
             set protocol.version=2)"
                .into(),
        );
    }
    let mut advert = Vec::new();
    stratum_proto::serve::advertise_body(&mut advert, None).map_err(|e| format!("weft: {e}"))?;
    send(handle, chan, advert).await?;

    let mut pending: Vec<u8> = Vec::new();
    loop {
        while let Some((consumed, req)) =
            split_v2_request(&pending).map_err(|e| format!("weft: {e}"))?
        {
            pending.drain(..consumed);
            if req.len() <= 4 {
                return Ok(());
            }
            let mut out = Vec::new();
            stratum_proto::workspace::upload_pack(ws, &req, &mut out)
                .map_err(|e| format!("weft: {e}"))?;
            send(handle, chan, out).await?;
        }
        match stdin.recv().await {
            Some(chunk) => {
                pending.extend_from_slice(&chunk);
                if pending.len() > crate::app::BODY_LIMIT {
                    return Err("weft: request exceeds the 64 MiB limit".into());
                }
            }
            None if pending.is_empty() => return Ok(()),
            None => return Err("weft: truncated request".into()),
        }
    }
}

/// git-receive-pack over SSH: v0 ref advertisement, buffer the client's
/// commands+pack to EOF, run the engine receive, report, and fire the
/// same push side-effects as the HTTP door.
async fn receive_session(
    state: &SharedState,
    repo: &Repo,
    ctx: &RepoCtx,
    actx: &stratum_control::audit::AuditCtx,
    handle: &Handle,
    chan: ChannelId,
    mut stdin: mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
    // A mirror that has nothing that could push to its origin: same
    // words as HTTP's advert ERR.
    if let Some(msg) = crate::app::mirror_push_refusal(state, repo) {
        return Err(msg);
    }
    let advert = {
        let _p = state.permits.serve.acquire().await;
        let ctx2 = ctx.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
            let store = ctx2.store();
            let manifest = ctx2.load_manifest()?;
            let mut buf = Vec::new();
            stratum_proto::receive::advertise_body(&store, &manifest, &mut buf)?;
            Ok(buf)
        })
        .await
        .unwrap_or_else(|e| Err(format!("task join: {e}")))
        .map_err(|e| format!("weft: {e}"))?
    };
    send(handle, chan, advert).await?;

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stdin.recv().await {
        if body.len() + chunk.len() > crate::app::BODY_LIMIT {
            return Err("weft: push exceeds the 64 MiB request limit".into());
        }
        body.extend_from_slice(&chunk);
        // A delete-only push sends no pack and does not close its side:
        // it sends the commands and waits for the report. Reading to EOF
        // here deadlocks against a client that is waiting for us, and the
        // user sees the connection drop minutes later. See
        // `receive::request_is_complete`.
        if stratum_proto::receive::request_is_complete(&body) {
            break;
        }
    }
    if body.len() <= 4 {
        // Nothing to push (everything up to date): flush-pkt or nothing.
        return Ok(());
    }

    // Same protection load as the HTTP door: both fronts answer a push
    // to a protected branch with the identical in-band sentence.
    let protected = stratum_control::protections::protected_branches(&state.db, &repo.id)
        .map_err(|e| format!("weft: {e}"))?;
    let start = std::time::Instant::now();
    let pushed = body.len() as u64;
    let (report, accepted) = crate::push::receive(state, repo, ctx, body.into(), protected)
        .await
        .map_err(|e| format!("weft: {e}"))?;
    send(handle, chan, report).await?;
    if let Some(updates) = accepted {
        crate::push::after_accept(state, repo, actx, &updates, "ssh", pushed, start).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parsing_accepts_stock_git_shapes() {
        for (cmd, svc) in [
            ("git-upload-pack 'acme/app.git'", GitService::UploadPack),
            ("git-upload-pack '/acme/app.git'", GitService::UploadPack),
            ("git-upload-pack acme/app", GitService::UploadPack),
            ("git upload-pack \"acme/app\"", GitService::UploadPack),
            ("git-receive-pack 'acme/app.git'", GitService::ReceivePack),
            ("git receive-pack 'acme/app'", GitService::ReceivePack),
        ] {
            let (s, path) = parse_git_command(cmd).unwrap();
            assert_eq!(s, svc, "{cmd}");
            assert_eq!(
                path,
                WirePath::Repo {
                    org: "acme".into(),
                    repo: "app".into()
                },
                "{cmd}"
            );
        }
    }

    /// A changeset's workspace is one segment longer, and the extra
    /// segment is a literal. Everything else three-deep stays rejected —
    /// the shape is the only thing separating the two, so it has to be
    /// exact.
    #[test]
    fn command_parsing_tells_a_changeset_workspace_from_a_repository() {
        for cmd in [
            "git-upload-pack 'acme/changesets/Ic5.git'",
            "git-upload-pack acme/changesets/Ic5",
            "git-upload-pack '/acme/changesets/Ic5.git'",
        ] {
            let (_, path) = parse_git_command(cmd).unwrap();
            assert_eq!(
                path,
                WirePath::Changeset {
                    org: "acme".into(),
                    key: "Ic5".into()
                },
                "{cmd}"
            );
        }
        // `org/changesets` with nothing after it is a repository by that
        // name, which the registry will not create — so it resolves to
        // not-found rather than to a changeset with an empty key.
        let (_, path) = parse_git_command("git-upload-pack 'acme/changesets.git'").unwrap();
        assert_eq!(
            path,
            WirePath::Repo {
                org: "acme".into(),
                repo: "changesets".into()
            }
        );
    }

    #[test]
    fn command_parsing_rejects_everything_else() {
        for cmd in [
            "",
            "scp -t /etc/passwd",
            "git-upload-pack",
            "git-upload-pack ''",
            "git-upload-pack 'no-slash'",
            "git-upload-pack 'a/b/c'",
            "git-upload-pack 'acme/changesets/Ic5/x'",
            "git-upload-pack 'acme/changesets/'",
            "git-upload-pack '../a/b'",
            "git-upload-pack 'a/../b'",
            "rm -rf /; git-upload-pack 'a/b'",
        ] {
            assert!(parse_git_command(cmd).is_err(), "{cmd:?}");
        }
    }

    #[test]
    fn v2_request_splitting() {
        // Incomplete: no flush yet.
        assert_eq!(split_v2_request(b"0014command=ls-refs").unwrap(), None);
        assert_eq!(split_v2_request(b"00").unwrap(), None);
        // A full request: command line, delim, arg, flush — then surplus.
        let mut req = Vec::new();
        stratum_proto::pktline::write_text(&mut req, "command=ls-refs").unwrap();
        stratum_proto::pktline::write_delim(&mut req).unwrap();
        stratum_proto::pktline::write_text(&mut req, "peel").unwrap();
        stratum_proto::pktline::write_flush(&mut req).unwrap();
        let mut buf = req.clone();
        buf.extend_from_slice(b"0009next!");
        let (consumed, got) = split_v2_request(&buf).unwrap().unwrap();
        assert_eq!(consumed, req.len());
        assert_eq!(got, req);
        // A lone flush is a complete (empty) request.
        let (consumed, got) = split_v2_request(b"0000").unwrap().unwrap();
        assert_eq!((consumed, got.as_slice()), (4, &b"0000"[..]));
        // Garbage lengths are errors, not hangs.
        assert!(split_v2_request(b"zzzz").is_err());
        assert!(split_v2_request(b"0002").is_err());
    }
}
