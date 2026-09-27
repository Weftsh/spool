//! The synthetic git repository a changeset is cloned as.
//!
//! A changeset is a set of changes in different repositories that land
//! together, and the question everybody asks first is "how do I get all
//! of it at once". This module answers it with a repository that does not
//! exist anywhere: one commit, on one branch, whose tree is nothing but
//! gitlinks — one per member, at that member's proposed head — beside a
//! `.gitmodules` that points at the member repositories by *relative*
//! URL. `git clone --recurse-submodules` of that URL checks the whole
//! combination out, over whichever transport and with whichever
//! credential the superproject itself came in on.
//!
//! Nothing is stored. The objects are built in memory from the member
//! tips, packed, and served; the "repository" is a `Workspace` value that
//! lives as long as the request. That is what makes it safe to hand out:
//! there is no ref to move, no push to accept, no layout to corrupt, and
//! a member that the caller may not read is simply not in the tree that
//! was built for them.
//!
//! **Deterministic.** The same members at the same tips, with the same
//! timestamp, produce the same commit id and the same pack bytes. A
//! clone that is retried, or served by a different node, must not answer
//! with a different history for the same proposal — and `git pull` in a
//! checked-out workspace has to be able to see that nothing moved.

use crate::pktline::{write_delim, write_flush, write_text};
use crate::serve::SidebandWriter;
use std::io::Write;
use stratum_engine::objwrite::{
    self, encode_commit, encode_tree, hex, new_object, CommitInfo, NewObject, TreeEntry, OBJ_BLOB,
    OBJ_COMMIT, OBJ_TREE,
};

/// Who authors a workspace commit. Not a person: nobody wrote it, and a
/// contributor's name on a commit they did not make is a lie a `git log`
/// will repeat for as long as the clone exists.
const AUTHOR: &str = "Weft <workspace@weft.sh>";

/// One member of the changeset, as the workspace tree sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMember {
    /// The member repository's name. It is the directory in the tree,
    /// the submodule's name in `.gitmodules`, and the last component of
    /// the URL — deliberately all three, so a person reading `git
    /// submodule status` sees the repository they would clone.
    pub name: String,
    /// The member's proposed head — the tip of its latest patchset.
    pub commit: [u8; 20],
    /// The submodule URL, relative to the superproject's own: see
    /// [`WorkspaceSpec`].
    pub url: String,
}

/// Everything a workspace is built from.
#[derive(Debug, Clone)]
pub struct WorkspaceSpec {
    pub org: String,
    pub key: String,
    pub title: String,
    /// The composition hash CI names its composed runs with. Printed in
    /// the commit message so a clone can be matched back to the build
    /// that was run against it — the same string, not a second one
    /// computed here, or the two would drift and nobody would notice.
    pub composition: String,
    /// Commit time, unix seconds. The caller passes the newest member
    /// patchset's timestamp, so the commit is dated by the proposal
    /// rather than by the moment somebody happened to clone.
    pub timestamp_secs: i64,
    pub members: Vec<WorkspaceMember>,
}

/// The built repository: one ref, one commit, one pack.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub head_ref: String,
    /// The workspace commit, hex. Empty for a changeset with no members
    /// to compose — see [`Workspace::is_empty`].
    pub tip: String,
    /// A complete pack (header, entries, trailer) holding the blob, the
    /// tree and the commit. Built once and served as-is: there is
    /// nothing to negotiate, since every fetch of this repository is a
    /// clone of its only commit.
    pub pack: Vec<u8>,
}

/// The relative URL a member is reached by from the workspace's own.
///
/// Two `..`: git resolves a relative submodule URL by chopping one path
/// component off the superproject's remote per `../`, so from
/// `/acme/changesets/Ic5.git` — three components — `../../api.git` is
/// `/acme/api.git`. Relative rather than absolute because the
/// superproject's URL is what carries the transport *and the
/// credential*: a clone over SSH must fetch its members over SSH, and an
/// HTTPS clone that authenticated with a token must reuse that token.
/// An absolute URL baked in here would send every member fetch back
/// through whichever front door this server happened to think it was
/// behind, and ask for a password again.
pub fn member_url(repo_name: &str) -> String {
    format!("../../{repo_name}.git")
}

impl Workspace {
    /// A changeset that cannot be composed — no live members, or a
    /// member with no patchset — has no commit and no pack.
    ///
    /// It is still a *reachable* repository rather than a 404: whether
    /// the changeset exists and whether it has anything in it are
    /// different questions, and answering the second with "not found"
    /// would tell a person their changeset had been deleted. `ls-refs`
    /// answers with an empty advert, which is exactly what git shows for
    /// a repository with no commits yet, and a fetch says why.
    pub fn is_empty(&self) -> bool {
        self.tip.is_empty()
    }

    /// Build the objects. Deterministic in the spec.
    pub fn build(spec: &WorkspaceSpec) -> Workspace {
        let head_ref = "refs/heads/workspace".to_string();
        let mut members = spec.members.clone();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        if members.is_empty() {
            return Workspace {
                head_ref,
                tip: String::new(),
                pack: Vec::new(),
            };
        }
        let gitmodules = new_object(OBJ_BLOB, gitmodules(&members).into_bytes());
        let mut entries = vec![TreeEntry {
            mode: "100644".into(),
            name: ".gitmodules".into(),
            oid: gitmodules.oid,
        }];
        entries.extend(members.iter().map(|m| TreeEntry {
            // 160000 is a gitlink: a commit id recorded in a tree, which
            // is what a submodule *is*. The object it names is not in
            // this pack and does not need to be — git fetches it from
            // the member repository when the submodule is updated.
            mode: "160000".into(),
            name: m.name.clone(),
            oid: m.commit,
        }));
        let tree = new_object(OBJ_TREE, encode_tree(&mut entries));
        let commit = new_object(
            OBJ_COMMIT,
            encode_commit(&CommitInfo {
                tree: tree.oid,
                // No parents. A workspace has no history: the previous
                // combination is not an ancestor of this one, it is a
                // different proposal, and claiming otherwise would make
                // `git log` read as though the changeset had been
                // developed rather than assembled.
                parents: Vec::new(),
                author: AUTHOR.into(),
                committer: AUTHOR.into(),
                timestamp: spec.timestamp_secs,
                message: message(spec, &members),
            }),
        );
        let objects: Vec<NewObject> = vec![gitmodules, tree, commit];
        let tip = hex(&objects[2].oid);
        // `build_pack_payload` only fails on a zlib write, which cannot
        // fail into a `Vec`; there is no useful error to hand a caller
        // that has already been given a valid spec.
        let (payload, entries) =
            objwrite::build_pack_payload(&objects).expect("workspace objects deflate");
        Workspace {
            head_ref,
            tip,
            pack: objwrite::seal_pack(&payload, entries),
        }
    }

    /// v2 `ls-refs`. Honours `ref-prefix` and `symrefs` like the real
    /// one, because git sends both and a client that asked for a prefix
    /// and got everything is a client we have taught to distrust the
    /// filter.
    pub fn ls_refs(&self, args: &[String], out: &mut impl Write) -> Result<(), String> {
        let symrefs = args.iter().any(|a| a == "symrefs");
        let prefixes: Vec<&str> = args
            .iter()
            .filter_map(|a| a.strip_prefix("ref-prefix "))
            .collect();
        let want = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
        (|| -> std::io::Result<()> {
            if !self.is_empty() {
                if want("HEAD") {
                    let line = if symrefs {
                        format!("{} HEAD symref-target:{}", self.tip, self.head_ref)
                    } else {
                        format!("{} HEAD", self.tip)
                    };
                    write_text(out, &line)?;
                }
                if want(&self.head_ref) {
                    write_text(out, &format!("{} {}", self.tip, self.head_ref))?;
                }
            }
            write_flush(out)
        })()
        .map_err(|e| e.to_string())
    }

    /// v2 `fetch`. There is exactly one thing to send, so the whole
    /// negotiation collapses: every want must be the tip, and the answer
    /// is always the same pack.
    ///
    /// The arguments are parsed as strictly as [`crate::serve`] parses
    /// them — an unknown one is an error rather than something ignored —
    /// for the reason that rule exists there: a client that asked for
    /// something we silently dropped gets an answer that is wrong in a
    /// way it cannot see.
    pub fn fetch(&self, args: &[String], out: &mut impl Write) -> Result<(), String> {
        let mut wants = Vec::new();
        let mut haves = Vec::new();
        let mut done = false;
        let mut ofs_delta = false;
        let mut deepen = false;
        for arg in args {
            if let Some(oid) = arg.strip_prefix("want ") {
                wants.push(oid.to_string());
            } else if let Some(oid) = arg.strip_prefix("have ") {
                haves.push(oid.to_string());
            } else if arg.strip_prefix("deepen ").is_some() {
                // Accepted and ignored: the history is one commit with no
                // parents, so it is already depth 1 and every depth means
                // the same pack. See the `shallow-info` note below.
                deepen = true;
            } else {
                match arg.as_str() {
                    "done" => done = true,
                    "ofs-delta" => ofs_delta = true,
                    "thin-pack" | "no-progress" | "include-tag" => {}
                    // A graft the client is holding. It cannot be one of
                    // ours — we never send a `shallow` line — so there is
                    // nothing to lift and nothing to keep.
                    other if other.starts_with("shallow ") => {}
                    // We have no CDN pack for a repository that exists
                    // only for this request; the opt-in is simply unused.
                    other if other.starts_with("packfile-uris") => {}
                    other => return Err(format!("unsupported fetch arg: {other}")),
                }
            }
        }
        if !ofs_delta {
            return Err("client without ofs-delta support".into());
        }
        if wants.is_empty() {
            return Err("fetch with no wants".into());
        }
        if self.is_empty() {
            return Err("this changeset has no members to clone".into());
        }
        // A want of a *member's* commit lands here, and it must: those
        // objects are in the member's repository, which the client is
        // about to clone separately, and answering with the workspace
        // pack would send it a commit it did not ask for and none of the
        // history it did.
        if let Some(w) = wants.iter().find(|w| **w != self.tip) {
            return Err(format!("want {w} is not the changeset workspace tip"));
        }
        let e = |e: std::io::Error| e.to_string();
        let acked: Vec<&String> = haves.iter().filter(|h| **h == self.tip).collect();
        if !done {
            // git waits for this section before it will accept a pack on
            // a negotiated fetch. A client that already has the tip is
            // told so and told we are ready; one that has nothing in
            // common gets a NAK and comes back with `done`.
            write_text(out, "acknowledgments").map_err(e)?;
            if acked.is_empty() {
                write_text(out, "NAK").map_err(e)?;
                return write_flush(out).map_err(e);
            }
            for h in acked {
                write_text(out, &format!("ACK {h}")).map_err(e)?;
            }
            write_text(out, "ready").map_err(e)?;
            write_delim(out).map_err(e)?;
        }
        if deepen {
            // Empty, and that is the point: the client asked to be made
            // shallow and there is no boundary to graft, because the one
            // commit here has no parents. git's own upload-pack answers a
            // deepen over a graftless history with exactly this — the
            // section present, no `shallow` lines — and a client that
            // opted into shallow expects the section to be there.
            write_text(out, "shallow-info").map_err(e)?;
            write_delim(out).map_err(e)?;
        }
        write_text(out, "packfile").map_err(e)?;
        let mut band = SidebandWriter::new(out);
        band.write_all(&self.pack).map_err(e)?;
        band.flush_band().map_err(e)?;
        write_flush(out).map_err(e)
    }
}

/// `.gitmodules`, in the order the tree lists the members.
fn gitmodules(members: &[WorkspaceMember]) -> String {
    let mut out = String::new();
    for m in members {
        out.push_str(&format!(
            "[submodule \"{}\"]\n\tpath = {}\n\turl = {}\n",
            m.name, m.name, m.url
        ));
    }
    out
}

/// The commit message: what this is, which combination it is, and the
/// members with their tips — so `git show` alone answers "what was I
/// given" without a second request to the API.
fn message(spec: &WorkspaceSpec, members: &[WorkspaceMember]) -> String {
    let mut out = format!("Workspace of changeset {}: {}\n", spec.key, spec.title);
    out.push_str(&format!("\ncomposition {}\n\n", spec.composition));
    for m in members {
        out.push_str(&format!("{} {}\n", m.name, hex(&m.commit)));
    }
    out
}

/// Serve one `git-upload-pack` body against a workspace — the entry
/// point both front doors call, mirroring [`crate::serve::upload_pack`].
pub fn upload_pack(ws: &Workspace, body: &[u8], out: &mut impl Write) -> Result<(), String> {
    let req = crate::serve::parse_request(body)?;
    match req.command.as_str() {
        "ls-refs" => ws.ls_refs(&req.args, out),
        "fetch" => ws.fetch(&req.args, out),
        other => Err(format!("unsupported command {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pktline::{read_pkt, Pkt};

    fn oid(b: u8) -> [u8; 20] {
        [b; 20]
    }

    fn spec() -> WorkspaceSpec {
        WorkspaceSpec {
            org: "acme".into(),
            key: "Ic5c5c5c5".into(),
            title: "Move the widget".into(),
            composition: "c0ffee".into(),
            timestamp_secs: 1_700_000_000,
            members: vec![
                WorkspaceMember {
                    name: "web".into(),
                    commit: oid(0xbb),
                    url: member_url("web"),
                },
                WorkspaceMember {
                    name: "api".into(),
                    commit: oid(0xaa),
                    url: member_url("api"),
                },
            ],
        }
    }

    /// A request body as git sends it: `command=…`, delim, arguments,
    /// flush.
    fn body(command: &str, args: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        write_text(&mut b, &format!("command={command}")).unwrap();
        write_delim(&mut b).unwrap();
        for a in args {
            write_text(&mut b, a).unwrap();
        }
        write_flush(&mut b).unwrap();
        b
    }

    /// The response, as a list of `Some(line)` for data pkts and `None`
    /// for a delim or flush — enough to assert the *shape* of a v2
    /// response, which is where the protocol bugs live.
    fn lines(buf: &[u8]) -> Vec<Option<String>> {
        let mut r = std::io::Cursor::new(buf);
        let mut out = Vec::new();
        loop {
            match read_pkt(&mut r).unwrap() {
                Pkt::Data(d) => out.push(Some(
                    String::from_utf8_lossy(&d)
                        .trim_end_matches('\n')
                        .to_string(),
                )),
                Pkt::Eof => return out,
                _ => out.push(None),
            }
        }
    }

    /// The pack, reassembled from sideband channel 1 exactly as git does.
    fn banded(buf: &[u8]) -> Vec<u8> {
        let mut r = std::io::Cursor::new(buf);
        let mut out = Vec::new();
        let mut in_pack = false;
        loop {
            match read_pkt(&mut r).unwrap() {
                Pkt::Data(d) if in_pack => {
                    assert_eq!(d[0], 1, "pack bytes must be on channel 1");
                    out.extend_from_slice(&d[1..]);
                }
                Pkt::Data(d) => in_pack = d.starts_with(b"packfile"),
                Pkt::Eof => return out,
                _ => {}
            }
        }
    }

    /// Same spec, same bytes — including the pack, not just the id. A
    /// clone retried against a second node has to be the same history,
    /// and "same commit id" would not catch a pack whose entries were
    /// ordered by a hash map.
    #[test]
    fn the_same_proposal_builds_the_same_repository_every_time() {
        let a = Workspace::build(&spec());
        let b = Workspace::build(&spec());
        assert_eq!(a.tip, b.tip);
        assert_eq!(a.pack, b.pack);
        assert!(a.pack.starts_with(b"PACK"));
        // Three objects: the blob, the tree, the commit.
        assert_eq!(a.pack[8..12], 3u32.to_be_bytes());
        assert_eq!(a.head_ref, "refs/heads/workspace");
        // And the order the members were given in is not part of it: the
        // identity of a combination is its set of tips.
        let mut flipped = spec();
        flipped.members.reverse();
        assert_eq!(Workspace::build(&flipped).tip, a.tip);
    }

    /// The tree is a `.gitmodules` blob and one **gitlink** per member,
    /// at the member's proposed head — asserted by rebuilding the three
    /// objects by hand and comparing the commit id, which changes if any
    /// mode, name or oid in the tree does.
    #[test]
    fn the_tree_is_gitlinks_at_the_members_proposed_heads() {
        let ws = Workspace::build(&spec());
        let blob = new_object(OBJ_BLOB, gitmodules_text().into_bytes());
        let mut entries = vec![
            TreeEntry {
                mode: "100644".into(),
                name: ".gitmodules".into(),
                oid: blob.oid,
            },
            TreeEntry {
                mode: "160000".into(),
                name: "api".into(),
                oid: oid(0xaa),
            },
            TreeEntry {
                mode: "160000".into(),
                name: "web".into(),
                oid: oid(0xbb),
            },
        ];
        let tree = new_object(OBJ_TREE, encode_tree(&mut entries));
        let commit = new_object(
            OBJ_COMMIT,
            encode_commit(&CommitInfo {
                tree: tree.oid,
                parents: Vec::new(),
                author: AUTHOR.into(),
                committer: AUTHOR.into(),
                timestamp: 1_700_000_000,
                message: format!(
                    "Workspace of changeset Ic5c5c5c5: Move the widget\n\
                     \ncomposition c0ffee\n\n\
                     api {}\nweb {}\n",
                    hex(&oid(0xaa)),
                    hex(&oid(0xbb))
                ),
            }),
        );
        assert_eq!(ws.tip, hex(&commit.oid));
    }

    fn gitmodules_text() -> String {
        "[submodule \"api\"]\n\tpath = api\n\turl = ../../api.git\n\
         [submodule \"web\"]\n\tpath = web\n\turl = ../../web.git\n"
            .to_string()
    }

    /// The submodule URLs are relative, two components up, and the
    /// members are in tree order. Written out in full because this text
    /// is a contract with git's own resolver, not with us.
    #[test]
    fn gitmodules_points_at_the_members_relative_to_the_workspace() {
        let mut members = spec().members;
        members.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(gitmodules(&members), gitmodules_text());
    }

    #[test]
    fn ls_refs_advertises_one_branch_and_a_symref_head() {
        let ws = Workspace::build(&spec());
        let mut out = Vec::new();
        ws.ls_refs(&[], &mut out).unwrap();
        assert_eq!(
            lines(&out),
            vec![
                Some(format!("{} HEAD", ws.tip)),
                Some(format!("{} refs/heads/workspace", ws.tip)),
                None,
            ]
        );

        let mut out = Vec::new();
        ws.ls_refs(&["symrefs".into()], &mut out).unwrap();
        assert_eq!(
            lines(&out)[0],
            Some(format!(
                "{} HEAD symref-target:refs/heads/workspace",
                ws.tip
            ))
        );

        // A prefix filters, and it filters HEAD too — git asks for
        // `refs/heads/` when it wants branches and nothing else.
        let mut out = Vec::new();
        ws.ls_refs(&["ref-prefix refs/heads/".into()], &mut out)
            .unwrap();
        assert_eq!(
            lines(&out),
            vec![Some(format!("{} refs/heads/workspace", ws.tip)), None]
        );
        let mut out = Vec::new();
        ws.ls_refs(&["ref-prefix refs/tags/".into()], &mut out)
            .unwrap();
        assert_eq!(lines(&out), vec![None]);
    }

    /// A changeset with nothing to compose is an empty repository, not a
    /// missing one: `ls-refs` flushes, and a fetch says what is wrong
    /// rather than handing over a pack with no commit in it.
    #[test]
    fn a_changeset_with_no_members_is_an_empty_repository() {
        let ws = Workspace::build(&WorkspaceSpec {
            members: Vec::new(),
            ..spec()
        });
        assert!(ws.is_empty());
        let mut out = Vec::new();
        ws.ls_refs(&[], &mut out).unwrap();
        assert_eq!(lines(&out), vec![None]);
        let err = ws
            .fetch(&["want abc".into(), "ofs-delta".into()], &mut Vec::new())
            .unwrap_err();
        assert!(err.contains("no members"), "{err}");
    }

    /// Every refusal a malformed or impossible fetch gets. Each is an
    /// error rather than a partial answer, because the transport turns
    /// an error before the first byte into a clean status and a wrong
    /// answer into a clone somebody has to debug.
    #[test]
    fn a_fetch_refuses_anything_it_cannot_answer_exactly() {
        let ws = Workspace::build(&spec());
        let cases: Vec<(Vec<String>, &str)> = vec![
            (vec!["want ".to_string() + &ws.tip], "ofs-delta"),
            (vec!["ofs-delta".into()], "no wants"),
            (
                vec![
                    "want ".to_string() + &hex(&oid(0xaa)),
                    "ofs-delta".into(),
                    "done".into(),
                ],
                "is not the changeset workspace tip",
            ),
            (
                vec![
                    "want ".to_string() + &ws.tip,
                    "ofs-delta".into(),
                    "filter blob:none".into(),
                ],
                "unsupported fetch arg",
            ),
        ];
        for (args, expect) in cases {
            let err = ws.fetch(&args, &mut Vec::new()).unwrap_err();
            assert!(err.contains(expect), "{args:?} gave {err:?}");
        }
    }

    #[test]
    fn a_fetch_negotiates_and_then_sends_the_pack() {
        let ws = Workspace::build(&spec());
        let want = format!("want {}", ws.tip);

        // Haves with nothing in common: NAK, no pack, and the client
        // will come back with `done`.
        let mut out = Vec::new();
        ws.fetch(
            &[
                want.clone(),
                "have ".to_string() + &hex(&oid(0x11)),
                "ofs-delta".into(),
            ],
            &mut out,
        )
        .unwrap();
        assert_eq!(
            lines(&out),
            vec![Some("acknowledgments".into()), Some("NAK".into()), None]
        );

        // A client already at the tip is acknowledged and told we are
        // ready, and the pack follows in the same response.
        let mut out = Vec::new();
        ws.fetch(
            &[want.clone(), format!("have {}", ws.tip), "ofs-delta".into()],
            &mut out,
        )
        .unwrap();
        let l = lines(&out);
        assert_eq!(l[0], Some("acknowledgments".into()));
        assert_eq!(l[1], Some(format!("ACK {}", ws.tip)));
        assert_eq!(l[2], Some("ready".into()));
        assert_eq!(l[3], None, "delim closes the acknowledgments section");
        assert_eq!(l[4], Some("packfile".into()));
        assert_eq!(banded(&out), ws.pack);

        // `done`: straight to the pack, no negotiation section at all.
        let mut out = Vec::new();
        ws.fetch(&[want.clone(), "ofs-delta".into(), "done".into()], &mut out)
            .unwrap();
        assert_eq!(lines(&out)[0], Some("packfile".into()));
        assert_eq!(banded(&out), ws.pack);

        // `--depth 1`: the section is present and empty. There is no
        // graft to report, and git wants to be told that rather than
        // left to infer it.
        let mut out = Vec::new();
        ws.fetch(
            &[want, "ofs-delta".into(), "deepen 1".into(), "done".into()],
            &mut out,
        )
        .unwrap();
        let l = lines(&out);
        assert_eq!(l[0], Some("shallow-info".into()));
        assert_eq!(l[1], None);
        assert_eq!(l[2], Some("packfile".into()));
        assert_eq!(banded(&out), ws.pack);
    }

    /// The dispatcher routes the two commands it speaks and refuses the
    /// rest by name.
    #[test]
    fn upload_pack_dispatches_the_two_v2_commands() {
        let ws = Workspace::build(&spec());
        let mut out = Vec::new();
        upload_pack(&ws, &body("ls-refs", &["symrefs"]), &mut out).unwrap();
        assert_eq!(lines(&out).len(), 3);

        let mut out = Vec::new();
        upload_pack(
            &ws,
            &body("fetch", &[&format!("want {}", ws.tip), "ofs-delta", "done"]),
            &mut out,
        )
        .unwrap();
        assert_eq!(banded(&out), ws.pack);

        let err = upload_pack(&ws, &body("object-info", &[]), &mut Vec::new()).unwrap_err();
        assert!(err.contains("object-info"), "{err}");
    }
}
