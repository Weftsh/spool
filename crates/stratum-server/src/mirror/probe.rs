//! Deciding whether an origin is worth accepting, before accepting it.
//!
//! Mirror creation used to answer 202 and walk away. A typo, a private
//! repo, an origin that is not a git repository at all — none of it was
//! caught at creation; it surfaced minutes later as `sync_error` on a
//! repo that looked broken. Paste-a-URL only works as a pitch if the
//! paste is checked while the person is still looking at the field.
//!
//! # This endpoint fetches a URL a stranger supplies
//!
//! That is the definition of SSRF, so the guards are the substance of
//! this module rather than a footnote:
//!
//! * **Scheme allowlist.** `https` only. Not `http` (a redirect to
//!   plaintext is a downgrade and there is no reason to mirror over it),
//!   not `file://`, `ssh://`, `git://`, `gopher://` or anything else. The
//!   `Generic` provider still accepts `file://` for tests and for
//!   operator-configured origins — that is a different trust level from
//!   a URL typed into a web form.
//! * **No redirects.** Following one re-opens every check against a
//!   destination the caller did not name. `git ls-remote` is invoked with
//!   redirects disabled.
//! * **Address literals refused, and every resolved address checked.** A
//!   name is resolved here, and if *any* address it resolves to is
//!   private, loopback, link-local, unique-local, multicast or otherwise
//!   not global unicast, the probe refuses. Checking the hostname alone
//!   is the classic bypass (`localtest.me`, a DNS record pointing at
//!   169.254.169.254).
//! * **A hard timeout**, so a slow origin cannot pin a worker.
//! * **Bounded output**, so a hostile server cannot answer with a
//!   gigabyte of refs.
//!
//! There remains a DNS-rebinding window between resolving and connecting:
//! the name could answer with a public address here and a private one to
//! git. Closing it properly means pinning the resolved address into the
//! connection, which `git` does not let us do. It is documented rather
//! than hidden, and the probe is rate-limited per org so it cannot become
//! a scanner even if the window is won.

use std::net::{IpAddr, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;

/// What a probe found. Deliberately small: this answers "should creation
/// accept this?", not "describe the repository".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Probe {
    pub reachable: bool,
    /// True when the origin exists but would not talk to us without
    /// credentials — the case that leads to the GitHub App install.
    pub private: bool,
    pub default_branch: Option<String>,
    /// How many refs it advertised. A rough size signal, and evidence
    /// that it really is a git repository.
    pub refs: usize,
    /// Why not, when `reachable` is false. Shown next to the field that
    /// caused it, so it is written for a person.
    pub reason: Option<String>,
}

impl Probe {
    /// A refusal with a reason a person can act on. Public because
    /// mirror creation makes one when the probe task itself dies, which
    /// is a refusal like any other from the caller's point of view.
    pub fn refused(reason: impl Into<String>) -> Probe {
        Probe {
            reachable: false,
            private: false,
            default_branch: None,
            refs: 0,
            reason: Some(reason.into()),
        }
    }
}

/// Normalise what a person actually pastes into a URL we can probe.
///
/// `github.com/owner/repo`, `owner/repo`, a browser URL with `/tree/main`
/// on the end, a `.git` suffix, an `ssh://` or `git@` form — they all
/// mean the same repository, and asking someone to canonicalise it by
/// hand is the friction this increment exists to remove.
pub fn normalize(input: &str) -> Result<String, String> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err("paste a repository URL".into());
    }
    if s.len() > 512 {
        return Err("that does not look like a repository URL".into());
    }

    // Work out the scheme first. Prefixing `https://` onto whatever was
    // typed would turn `file:///etc/passwd` into something that parses,
    // so an unrecognised scheme is refused rather than absorbed.
    let rest = if let Some((scheme, rest)) = s.split_once("://") {
        match scheme {
            "https" | "http" | "ssh" => rest,
            "git" => {
                return Err("git:// is unauthenticated and unencrypted — use the https URL".into())
            }
            other => return Err(format!("{other}:// origins cannot be mirrored")),
        }
    } else if let Some(rest) = s.strip_prefix("git@") {
        // scp-style: git@github.com:owner/repo(.git)
        &rest.replacen(':', "/", 1)
    } else if s.split('/').count() == 2 && !s.contains('.') {
        // Bare `owner/repo` means GitHub, the way every tool assumes.
        &format!("github.com/{s}")
    } else {
        s
    };

    // Everything below the scheme is host + path.
    let rest = rest.split(['#', '?']).next().unwrap_or(rest);
    let (authority, path) = match rest.split_once('/') {
        Some(x) => x,
        None => return Err("that URL names a host but not a repository".into()),
    };
    // `git@` in the ssh form is the protocol's own user, not a
    // credential. Anything else before the host is one, and quietly
    // storing a password because somebody pasted it is not a favour.
    let host = match authority.rsplit_once('@') {
        None => authority,
        Some(("git", host)) => host,
        Some(_) => {
            return Err("remove the credentials from the URL — connect the account instead".into())
        }
    };
    if host.is_empty() {
        return Err("that URL names a host but not a repository".into());
    }

    // Strip the browser furniture: /tree/main, /blob/…/README.md,
    // /pull/12, and a trailing .git.
    let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for marker in ["tree", "blob", "commits", "pull", "issues", "releases"] {
        // Only past the repository itself — a group called `issues` is a
        // legitimate path segment on a self-hosted forge.
        if let Some(i) = parts.iter().position(|p| *p == marker) {
            if i >= 2 {
                parts.truncate(i);
            }
        }
    }
    if parts.len() < 2 {
        return Err("that URL names a host but not a repository".into());
    }
    let last = parts.len() - 1;
    let tail = parts[last].trim_end_matches(".git");
    if tail.is_empty() {
        return Err("that URL names a host but not a repository".into());
    }
    let mut owned: Vec<String> = parts[..last].iter().map(|p| p.to_string()).collect();
    owned.push(tail.to_string());
    Ok(format!("https://{host}/{}", owned.join("/")))
}

/// The `owner/name` a GitHub origin means, whatever shape it was typed in.
///
/// Mirror creation stored the origin as typed, and the GitHub provider's
/// `fetch_url` treats that string as `owner/name`. The probe accepted
/// `github.com/weftsh/checkout` — it normalises — and the first sync then
/// fetched `https://github.com/github.com/weftsh/checkout.git`, which
/// GitHub answered "not found", and the dashboard reported the origin as
/// unreachable for a repository the probe had just found. Both paths now
/// go through [`normalize`]; the host must be github.com, and the path
/// must be exactly a repository, never a URL into one.
pub fn github_full_name(input: &str) -> Result<String, String> {
    let url = normalize(input)?;
    let rest = url
        .strip_prefix("https://")
        .ok_or("that URL names a host but not a repository")?;
    let (host, path) = rest
        .split_once('/')
        .ok_or("that URL names a host but not a repository")?;
    if !host.eq_ignore_ascii_case("github.com") && !host.eq_ignore_ascii_case("www.github.com") {
        return Err(format!(
            "{host} is not github.com — mirror it with the generic provider"
        ));
    }
    let mut parts = path.split('/');
    let owner = parts.next().filter(|s| !s.is_empty());
    let name = parts.next().filter(|s| !s.is_empty());
    match (owner, name, parts.next()) {
        (Some(o), Some(n), None) => Ok(format!("{o}/{n}")),
        _ => Err("a GitHub origin is owner/name".into()),
    }
}

/// Every reason an address is not somewhere we will fetch from.
///
/// Written against `IpAddr` rather than the hostname because the
/// hostname is the part an attacker controls: any name can be made to
/// resolve to 127.0.0.1 or to a cloud metadata endpoint.
fn is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // 100.64.0.0/10, carrier-grade NAT — reaches other
                // tenants on some clouds.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                // 192.0.0.0/24, IETF protocol assignments.
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0)
                // 198.18.0.0/15, benchmarking.
                || (v4.octets()[0] == 198 && (18..20).contains(&v4.octets()[1]))
                // 240.0.0.0/4, reserved.
                || v4.octets()[0] >= 240
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique-local, fe80::/10 link-local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // An IPv4-mapped address is an IPv4 address wearing a
                // hat; judge it as one.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_forbidden(IpAddr::V4(v4)))
        }
    }
}

/// Resolve a host and refuse if *any* address it answers with is one we
/// will not fetch from.
///
/// Any, not all: a name that answers with a public address and a private
/// one is a bypass attempt, and there is no legitimate origin that needs
/// this.
fn host_is_reachable(host: &str, port: u16) -> Result<(), String> {
    // A literal is refused outright. Nobody mirrors from an IP address,
    // and allowing it only widens what has to be checked.
    if host.parse::<IpAddr>().is_ok() {
        return Err("give the repository's hostname, not an IP address".into());
    }
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|_| format!("could not resolve {host}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("could not resolve {host}"));
    }
    if addrs.iter().any(|a| is_forbidden(a.ip())) {
        return Err(format!(
            "{host} resolves to an address inside a private network"
        ));
    }
    Ok(())
}

/// Everything the probe decides *before* touching the network.
///
/// Split out because it is the security boundary and it is pure enough
/// to test exhaustively — which is the point: a guard whose tests need
/// a network is a guard that gets tested once.
pub fn guard(url: &str) -> Result<String, Probe> {
    let normalized = normalize(url).map_err(Probe::refused)?;
    let Some((scheme, rest)) = normalized.split_once("://") else {
        return Err(Probe::refused("that is not a URL"));
    };
    if scheme != "https" {
        return Err(Probe::refused(
            "only https origins can be mirrored from here",
        ));
    }
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        // Not a port — an IPv6 literal, which is refused below anyway.
        Some((h, p)) => match p.parse() {
            Ok(n) => (h, n),
            Err(_) => (authority, 443),
        },
        None => (authority, 443),
    };
    host_is_reachable(host, port).map_err(Probe::refused)?;
    Ok(normalized)
}

/// Ask git what a URL advertises, with a deadline.
///
/// Separate from [`probe`] so the process plumbing — spawning, the
/// timeout, reading the output — can be tested against a real local
/// repository, which the guard above would (correctly) never allow.
pub fn ls_remote(url: &str, timeout: Duration) -> Result<std::process::Output, String> {
    let child = Command::new("git")
        // Redirects are off: following one reaches a destination that
        // never went through the guard.
        .args([
            "-c",
            "http.followRedirects=false",
            "-c",
            "credential.helper=",
            "ls-remote",
            "--symref",
            url,
        ])
        // No terminal, no credential prompt, no askpass: a private
        // origin must fail fast rather than block waiting for a password
        // nobody is there to type.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run git: {e}"))?;
    wait_bounded(child, timeout)
}

/// Does this failure mean "exists but needs credentials"?
///
/// GitHub answers "not found" for a private repository as well as a
/// missing one — deliberately, and we cannot tell them apart from
/// outside. Both lead to the same next step, so both are reported as
/// "this may be private" rather than guessing.
fn needs_credentials(stderr: &str) -> bool {
    let err = stderr.to_lowercase();
    // Matched against git's own phrasing, never against a bare number.
    // git quotes the URL back in its errors, so `403` on its own matches
    // a repository called `rfc-403` — and telling someone to connect
    // GitHub for a repository that simply does not exist sends them
    // somewhere that cannot help. Found by a test whose scratch path
    // happened to carry the process id 4018.
    [
        "authentication",
        "not found",
        "could not read username",
        "terminal prompts disabled",
        "access denied",
        "permission denied",
        "returned error: 403",
        "returned error: 401",
    ]
    .iter()
    .any(|m| err.contains(m))
}

/// The default branch and ref count from `ls-remote --symref` output.
fn parse_ls_remote(stdout: &str) -> (Option<String>, usize) {
    let default_branch = stdout.lines().find_map(|l| {
        l.strip_prefix("ref: refs/heads/")
            .and_then(|r| r.split_whitespace().next())
            .map(str::to_string)
    });
    let refs = stdout
        .lines()
        .filter(|l| !l.starts_with("ref:") && !l.trim().is_empty())
        .count();
    (default_branch, refs)
}

/// Ask an origin what it has, without side effects.
///
/// `git ls-remote` is the whole probe: one round trip, it proves the URL
/// really is a git repository, it names the default branch, and its
/// stderr distinguishes "private or absent" from "not a git repository".
pub fn probe(url: &str, timeout: Duration) -> Probe {
    // Three covered pieces, composed. Kept to one expression because
    // this composition is the only part a hermetic suite cannot reach —
    // it needs a public https origin — so it should be as small as the
    // thing it is standing in for.
    match guard(url) {
        Err(refusal) => refusal,
        Ok(url) => ls_remote(&url, timeout).map_or_else(Probe::refused, |o| from_output(&o)),
    }
}

/// Turn git's answer into the answer a person needs.
fn from_output(out: &std::process::Output) -> Probe {
    if !out.status.success() {
        let private = needs_credentials(&String::from_utf8_lossy(&out.stderr));
        return Probe {
            reachable: false,
            private,
            default_branch: None,
            refs: 0,
            reason: Some(if private {
                "this looks private — connect GitHub to mirror it".into()
            } else {
                "that origin is not reachable as a git repository".into()
            }),
        };
    }
    let (default_branch, refs) = parse_ls_remote(&String::from_utf8_lossy(&out.stdout));
    Probe {
        reachable: true,
        private: false,
        default_branch,
        refs,
        reason: if refs == 0 {
            Some("that repository is empty — there is nothing to mirror yet".into())
        } else {
            None
        },
    }
}

/// Wait for the child, killing it at the deadline.
///
/// `Command::output()` has no timeout, and a hostile origin that accepts
/// the connection and then says nothing would hold the request open
/// forever.
///
/// Both pipes are drained on their own threads *while* waiting. The
/// first version of this polled `try_wait` and read the output only after
/// exit, and a child that has more than a pipe buffer to say never exits
/// under that: it blocks on `write`, the parent sees "still running"
/// until the deadline, and a repository with a couple of thousand
/// pull-request refs is reported as an origin that did not answer. See
/// `an_origin_with_many_refs_is_read_not_timed_out`.
fn wait_bounded(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    fn drain(
        pipe: Option<impl std::io::Read + Send + 'static>,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            }
            buf
        })
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(format!("could not wait for git: {e}")),
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err("that origin did not answer in time".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Exit closes git's end of both pipes, so these are already at EOF
    // or about to be; a killed child's readers finish the same way.
    let stdout = stdout.join().map_err(|_| "could not read git's output")?;
    let stderr = stderr.join().map_err(|_| "could not read git's output")?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_github_origin_becomes_owner_name_whatever_was_typed() {
        for typed in [
            "acme/widget",
            "github.com/acme/widget",
            "https://github.com/acme/widget",
            "https://github.com/acme/widget.git",
            "https://www.github.com/acme/widget/tree/main",
            "git@github.com:acme/widget.git",
        ] {
            assert_eq!(github_full_name(typed).unwrap(), "acme/widget", "{typed}");
        }
        assert!(github_full_name("https://gitlab.com/acme/widget")
            .unwrap_err()
            .contains("not github.com"));
        assert!(github_full_name("https://github.com/acme").is_err());
        assert!(github_full_name("https://github.com/acme/widget/sub/dir").is_err());
        assert!(github_full_name("").is_err());
    }

    #[test]
    fn what_people_actually_paste_becomes_one_url() {
        let same = "https://github.com/acme/widget";
        for input in [
            "https://github.com/acme/widget",
            "https://github.com/acme/widget/",
            "https://github.com/acme/widget.git",
            "github.com/acme/widget",
            "acme/widget",
            "git@github.com:acme/widget.git",
            "ssh://git@github.com/acme/widget",
            "  https://github.com/acme/widget  ",
            "https://github.com/acme/widget/tree/main",
            "https://github.com/acme/widget/blob/main/README.md",
            "https://github.com/acme/widget/pull/12",
            "https://github.com/acme/widget?tab=readme",
            "https://github.com/acme/widget#install",
            "http://github.com/acme/widget",
        ] {
            assert_eq!(normalize(input).as_deref(), Ok(same), "{input:?}");
        }

        // A host that is not GitHub keeps its own host.
        assert_eq!(
            normalize("https://git.example.com/team/thing.git").as_deref(),
            Ok("https://git.example.com/team/thing")
        );
        // A deeper path is a real path on some hosts (GitLab subgroups),
        // so only browser furniture past the repo is stripped.
        assert_eq!(
            normalize("https://gitlab.com/group/sub/thing").as_deref(),
            Ok("https://gitlab.com/group/sub/thing")
        );
    }

    #[test]
    fn what_cannot_be_a_repository_is_refused_before_anything_is_fetched() {
        for bad in [
            "",
            "   ",
            "https://github.com",
            "https://github.com/",
            "https://github.com/acme",
            "notaurl",
            "git://github.com/acme/widget",
            "file:///etc/passwd",
            "https://user:password@github.com/acme/widget",
            &"x".repeat(600),
        ] {
            assert!(normalize(bad).is_err(), "{bad:?} was accepted");
        }
        // …and the ones that get past normalize are refused by scheme.
        for bad in ["file:///srv/git/repo", "ftp://example.com/a/b"] {
            let p = probe(bad, Duration::from_secs(1));
            assert!(!p.reachable, "{bad:?}");
            assert!(p.reason.is_some());
        }
    }

    #[test]
    fn nothing_inside_the_network_is_reachable() {
        // Every shape of "somewhere we must not fetch from".
        for ip in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // the cloud metadata endpoint
            "0.0.0.0",
            "100.64.0.1",
            "192.0.0.1",
            "198.18.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(is_forbidden(ip.parse().unwrap()), "{ip} should be refused");
        }
        // …and the public internet is not.
        for ip in ["1.1.1.1", "140.82.121.4", "2606:4700:4700::1111"] {
            assert!(!is_forbidden(ip.parse().unwrap()), "{ip} should be allowed");
        }

        // A literal, however spelled, never gets as far as resolution.
        for host in ["127.0.0.1", "10.0.0.1", "::1"] {
            assert!(host_is_reachable(host, 443).is_err(), "{host}");
        }
        // A name that resolves to loopback is refused on the address,
        // not on the name — which is the whole point.
        assert!(host_is_reachable("localhost", 443).is_err());
    }

    /// The process plumbing, against a repository that really exists.
    ///
    /// `ls_remote` is deliberately reachable without the guard so this
    /// can use a local path — the guard would refuse it, correctly, and
    /// a spawn-and-parse that is only ever exercised against the public
    /// internet is one that never gets exercised.
    #[test]
    fn git_is_asked_and_its_answer_is_read() {
        let scratch = stratum_testkit::gitcli::Scratch::new("probe-lsremote");
        let repo = scratch.path().join("origin");
        std::fs::create_dir_all(&repo).unwrap();
        stratum_testkit::gitcli::fixture_repo(&repo, 2);

        let out = ls_remote(repo.to_str().unwrap(), Duration::from_secs(30)).unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(out.status.success(), "{stderr}");
        let p = from_output(&out);
        assert!(p.reachable, "{p:?}");
        assert!(!p.private);
        assert!(p.refs >= 1, "{p:?}");
        assert!(p.reason.is_none(), "{p:?}");
        assert!(
            matches!(p.default_branch.as_deref(), Some("main") | Some("master")),
            "{p:?}"
        );

        // A directory that is not a repository fails, and is reported as
        // "not a git repository" rather than as needing credentials.
        let empty = scratch.path().join("not-a-repo");
        std::fs::create_dir_all(&empty).unwrap();
        let out = ls_remote(empty.to_str().unwrap(), Duration::from_secs(30)).unwrap();
        assert!(!out.status.success());
        let p = from_output(&out);
        assert!(!p.reachable);
        assert!(
            !p.private,
            "a missing local path must not read as private: {p:?}"
        );
        assert_eq!(
            p.reason.as_deref(),
            Some("that origin is not reachable as a git repository")
        );

        // An origin that says nothing does not hold the request open.
        // A real one: a socket that accepts git's connection and never
        // answers. The first version raced a 1 ns deadline against a
        // local `ls-remote` and expected git to lose, which it does not
        // always do on a loaded machine.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hold = std::thread::spawn(move || {
            let (_conn, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(5));
        });
        let url = format!("http://127.0.0.1:{port}/silent.git");
        let start = std::time::Instant::now();
        let err = ls_remote(&url, Duration::from_millis(500)).unwrap_err();
        assert!(err.contains("in time"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{:?}",
            start.elapsed()
        );
        drop(hold);
    }

    /// An origin with a lot to say is not a slow origin.
    ///
    /// `git ls-remote` against `octocat/Hello-World` — a public repository
    /// with a couple of thousand pull-request refs — answers in half a
    /// second on the command line and produced "that origin did not answer
    /// in time" through the probe. The wait polled the child without
    /// reading its stdout, so once git had written a pipe buffer's worth
    /// (64 KiB on Linux and macOS) it blocked on `write`, the parent kept
    /// seeing "still running", and the deadline killed a process that had
    /// finished its work seconds earlier. Every public repository with a
    /// few hundred pull requests was reported to the person as
    /// unreachable, and every one of them is a repository somebody would
    /// actually want to mirror.
    ///
    /// Three thousand refs is a few hundred kilobytes: comfortably past
    /// the buffer, and a fixture `git` produces in well under a second.
    #[test]
    fn an_origin_with_many_refs_is_read_not_timed_out() {
        let scratch = stratum_testkit::gitcli::Scratch::new("probe-lsremote-wide");
        let repo = scratch.path().join("origin");
        std::fs::create_dir_all(&repo).unwrap();
        stratum_testkit::gitcli::fixture_repo(&repo, 1);
        let head = stratum_testkit::gitcli::git(&repo, &["rev-parse", "HEAD"]);
        let head = head.trim();
        // Straight into packed-refs: one file write instead of three
        // thousand `update-ref` processes.
        let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted\n");
        for i in 0..3000 {
            packed.push_str(&format!("{head} refs/pull/{i}/head\n"));
        }
        std::fs::write(repo.join(".git/packed-refs"), packed).unwrap();

        let out = ls_remote(repo.to_str().unwrap(), Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert!(
            out.stdout.len() > 2 * 64 * 1024,
            "the fixture must overflow the pipe, or this proves nothing: {} bytes",
            out.stdout.len()
        );
        let p = from_output(&out);
        assert!(p.reachable, "{p:?}");
        assert!(p.refs >= 3000, "{p:?}");
    }

    #[test]
    fn gits_answer_is_read_the_way_a_person_needs_it() {
        // `--symref` puts HEAD's target first, then one line per ref.
        let (branch, refs) = parse_ls_remote(
            "ref: refs/heads/trunk\tHEAD\n             abc123\tHEAD\n             abc123\trefs/heads/trunk\n             def456\trefs/tags/v1\n",
        );
        assert_eq!(branch.as_deref(), Some("trunk"));
        assert_eq!(refs, 3);

        // An empty repository advertises nothing, which is a reachable
        // origin with nothing to mirror — not a failure.
        let (branch, refs) = parse_ls_remote("");
        assert_eq!(branch, None);
        assert_eq!(refs, 0);

        // Every way git says "you need credentials", and one way it does
        // not. "not found" is included because GitHub answers that for a
        // private repository as well as a missing one.
        for said in [
            "fatal: Authentication failed for 'https://…'",
            "remote: Repository not found.",
            "could not read Username for 'https://github.com'",
            "fatal: could not read Username: terminal prompts disabled",
            "The requested URL returned error: 403",
            "The requested URL returned error: 401",
        ] {
            assert!(needs_credentials(said), "{said:?}");
        }
        for said in [
            "fatal: repository 'https://example.com/a/b' does not exist",
            "fatal: unable to access: Could not resolve host",
            "",
            // git quotes the URL back, so a number that happens to be in
            // the path must not be read as an HTTP status. A repository
            // really can be called this.
            "fatal: '/tmp/stratum-test-4018-0/x' does not appear to be a git repository",
            "fatal: repository 'https://github.com/acme/rfc-403' does not exist",
            "fatal: repository 'https://github.com/acme/error-401' does not exist",
        ] {
            assert!(!needs_credentials(said), "{said:?}");
        }
        for said in [
            "fatal: unable to access '…': The requested URL returned error: 403",
            "fatal: unable to access '…': The requested URL returned error: 401",
            "remote: HTTP Basic: Access denied",
            "git@github.com: Permission denied (publickey).",
        ] {
            assert!(needs_credentials(said), "{said:?}");
        }
    }

    #[test]
    fn an_empty_origin_is_reachable_but_says_so() {
        let scratch = stratum_testkit::gitcli::Scratch::new("probe-empty");
        let repo = scratch.path().join("empty.git");
        std::fs::create_dir_all(&repo).unwrap();
        stratum_testkit::gitcli::git(&repo, &["init", "--bare", "-q"]);
        let out = ls_remote(repo.to_str().unwrap(), Duration::from_secs(30)).unwrap();
        let p = from_output(&out);
        assert!(p.reachable, "{p:?}");
        assert_eq!(p.refs, 0);
        assert!(p.reason.unwrap().contains("empty"));
    }

    /// The shapes that parse far enough to reach the later refusals.
    #[test]
    fn a_url_with_a_host_but_no_repository_is_refused_at_each_point() {
        // No host at all, only the `git@` the ssh form uses.
        assert!(normalize("ssh://git@/acme/widget").is_err());
        // A path whose last segment is nothing but `.git`.
        assert!(normalize("https://github.com/acme/.git").is_err());
        // Bare browser furniture past the repository is stripped, not
        // refused — the repo is still fully named. (This assert was once
        // `is_err() || true`, which asserted nothing.)
        assert_eq!(
            normalize("https://github.com/acme/widget/tree").unwrap(),
            "https://github.com/acme/widget"
        );
        // A single path segment after the host.
        assert!(normalize("https://github.com/acme").is_err());
    }

    /// An origin that exists but wants credentials is the case that
    /// leads to the GitHub App, so it must not read as "unreachable".
    #[test]
    fn an_origin_that_wants_credentials_says_so() {
        use std::os::unix::process::ExitStatusExt;
        let failed = |stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let p = from_output(&failed("remote: Repository not found.\nfatal: …"));
        assert!(!p.reachable);
        assert!(p.private, "{p:?}");
        assert_eq!(
            p.reason.as_deref(),
            Some("this looks private — connect GitHub to mirror it")
        );

        let p = from_output(&failed("fatal: unable to access: Could not resolve host"));
        assert!(!p.reachable);
        assert!(!p.private, "{p:?}");
        assert_eq!(
            p.reason.as_deref(),
            Some("that origin is not reachable as a git repository")
        );
    }

    #[test]
    fn the_guard_answers_before_anything_is_fetched() {
        // What it lets through, unchanged.
        assert_eq!(
            guard("github.com/acme/widget").as_deref(),
            Ok("https://github.com/acme/widget")
        );
        // An explicit port is kept out of the host when it is checked.
        assert!(guard("https://localhost:8443/a/b").is_err());
        // A malformed port is not a port; the whole authority is the
        // host, and it does not resolve.
        assert!(guard("https://github.com:notaport/a/b").is_err());
    }

    #[test]
    fn an_unresolvable_host_is_a_refusal_not_an_error() {
        let p = probe(
            "https://no-such-host-exists.invalid/acme/widget",
            Duration::from_secs(5),
        );
        assert!(!p.reachable);
        assert!(!p.private);
        assert!(p.reason.unwrap().contains("resolve"));
    }

    #[test]
    fn a_private_address_is_refused_even_dressed_as_a_repository_url() {
        for url in [
            "https://127.0.0.1/acme/widget",
            "https://localhost/acme/widget",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/acme/widget",
        ] {
            let p = probe(url, Duration::from_secs(5));
            assert!(!p.reachable, "{url} was probed");
            assert!(p.reason.is_some(), "{url}");
        }
    }
}
