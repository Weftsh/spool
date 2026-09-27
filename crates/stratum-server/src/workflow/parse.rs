//! `.weft/*.yml` → [`Workflow`], or refusals naming the key and line.
//!
//! ## What this is defending against
//!
//! A workflow file arrives from **any repository anybody forked**, so
//! this is unauthenticated attacker-controlled input on a path that runs
//! before any authorization decision. Three hazards, and the mitigations
//! are not all in the same place:
//!
//! * **Stack exhaustion.** A YAML parser descends recursively, and a
//!   Rust stack overflow *aborts the process* — not an error anybody can
//!   catch or answer 400 to. A deeply nested file in a stranger's pull
//!   request would be a crash of the whole forge. `granit-parser` bounds
//!   block and flow nesting itself and returns
//!   `RecursionLimitExceeded`, which is the reason it was chosen over
//!   the alternatives; verified by test below rather than taken on
//!   trust.
//! * **Alias bombs.** Anchors referring to anchors expand
//!   exponentially. Two things save us: the parser's *event* layer
//!   reports `Alias` without expanding it — the deep-clone expansion
//!   lives in tree loaders we deliberately do not use — and this module
//!   refuses anchors and aliases outright. A workflow using them is
//!   exotic; refusing kills the class at no cost.
//! * **Size.** Bounded here, because a workflow is a page of
//!   configuration and nothing downstream should have to wonder.
//!
//! ## Why a hand-written walk
//!
//! Because the refusals are the product. A derived deserializer reports
//! "unknown field" against a Rust type with no position; this walk names
//! the key, the line, and what to do instead — and a reader pasting an
//! Actions workflow meets those messages before they meet anything else.

use super::model::{Job, Matrix, Pool, Refusal, Step, Trigger, Workflow};
use granit_parser::{Event, Parser};
use std::collections::BTreeMap;

/// The largest workflow file we will look at. Two orders of magnitude
/// more than any real one; the point is that there is a bound.
pub const MAX_BYTES: usize = 64 * 1024;

/// `runs-on` values that mean "your ordinary Linux runner on our fleet".
///
/// Accepting these rather than refusing the key is the one place this
/// parser bends toward a pasted Actions file, and it bends only where
/// the answer is unambiguous: `ubuntu-latest` asks for a standard Linux
/// runner and gets one. macOS and Windows are still refused, because
/// running those on Linux is not a smaller version of what was asked
/// for, it is a different thing reported green — and if the author has
/// a Mac of their own, the answer is now `[self-hosted, macos]` rather
/// than a lie.
const RUNS_ON_OK: [&str; 4] = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04", "linux"];

/// What a hosted job's labels say when the file said nothing at all.
///
/// A job with no `runs-on:` still has to answer "what did you ask for"
/// in the run JSON, and the honest answer is the standard Linux runner
/// it is about to get.
const DEFAULT_HOSTED_LABEL: &str = "ubuntu-latest";

/// The label that turns `runs-on` into a request for somebody else's
/// machine.
const SELF_HOSTED: &str = "self-hosted";

/// The hosted labels that can only ever mean our fleet, and so are the
/// ones that contradict `self-hosted` when a list holds both.
///
/// `linux` is deliberately **not** here even though it is in
/// [`RUNS_ON_OK`], and the omission is the whole reason this is a
/// separate list. The server gives every registered Linux machine the
/// labels `self-hosted`, `linux` and its architecture, so
/// `[self-hosted, linux, gpu]` — the form GitHub's own documentation
/// uses, and the form this feature's contract gives as its example — is
/// an ordinary self-hosted request naming the OS. Testing the
/// contradiction against `RUNS_ON_OK` refused it, which would have made
/// the most common way to write a self-hosted job the one thing the
/// parser would not accept.
const HOSTED_ONLY: [&str; 3] = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04"];

/// A YAML document, reduced to the three shapes this subset has.
///
/// Built from the event stream rather than a tree loader: loaders expand
/// aliases by deep-cloning the anchored subtree, which is the alias-bomb
/// vector. Nothing here clones anything it did not read from the input
/// once.
#[derive(Debug, Clone)]
pub(super) enum Node {
    Scalar {
        value: String,
        line: usize,
    },
    Seq {
        items: Vec<Node>,
        line: usize,
    },
    /// Entries in file order; a duplicate key is refused rather than
    /// silently taking one of them.
    Map {
        entries: Vec<(String, usize, Node)>,
        line: usize,
    },
}

impl Node {
    pub(super) fn line(&self) -> usize {
        match self {
            Node::Scalar { line, .. } | Node::Seq { line, .. } | Node::Map { line, .. } => *line,
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            Node::Scalar { .. } => "a value",
            Node::Seq { .. } => "a list",
            Node::Map { .. } => "a block",
        }
    }
}

/// Read the whole document into a [`Node`], refusing everything this
/// subset has no meaning for.
pub(super) fn load(src: &str) -> Result<Node, Refusal> {
    let mut parser = Parser::new_from_str(src);
    let mut stack: Vec<Node> = Vec::new();
    // The key a map is waiting for a value for, and where it was.
    let mut pending: Vec<Option<(String, usize)>> = Vec::new();
    let mut root: Option<Node> = None;
    let mut docs = 0usize;

    while let Some(ev) = parser.next_event() {
        let (event, span) = match ev {
            Ok(x) => x,
            Err(e) => {
                let line = e.marker().line();
                // The parser's own refusals, in its words but our shape.
                // `recursion limit exceeded` is the one that matters:
                // it is the crash this design exists to prevent, and it
                // arrives here as an ordinary error.
                return Err(
                    Refusal::at(Some(line), "", format!("{e}")).hint("this file is not valid YAML")
                );
            }
        };
        let line = span.start.line();

        // Anchors and aliases, refused before anything can act on them.
        // The anchor id is non-zero on a node that declares one.
        let anchored = match &event {
            Event::Scalar(_, _, a, _)
            | Event::SequenceStart(_, a, _)
            | Event::MappingStart(_, a, _) => *a != 0,
            _ => false,
        };
        if anchored {
            return Err(
                Refusal::at(Some(line), "", "YAML anchors are not supported")
                    .hint("write the value out where it is used"),
            );
        }
        let tagged = matches!(
            &event,
            Event::Scalar(_, _, _, Some(_))
                | Event::SequenceStart(_, _, Some(_))
                | Event::MappingStart(_, _, Some(_))
        );
        if tagged {
            return Err(Refusal::at(Some(line), "", "YAML tags are not supported")
                .hint("remove the `!` tag; values here are plain text"));
        }

        match event {
            Event::Alias(_) => {
                return Err(
                    Refusal::at(Some(line), "", "YAML aliases are not supported")
                        .hint("write the value out where it is used"),
                );
            }
            Event::DocumentStart(..) => {
                docs += 1;
                if docs > 1 {
                    return Err(Refusal::at(
                        Some(line),
                        "",
                        "this file has more than one YAML document",
                    )
                    .hint("one workflow per file; split the `---` sections into two files"));
                }
            }
            Event::SequenceStart(..) => {
                stack.push(Node::Seq {
                    items: Vec::new(),
                    line,
                });
                pending.push(None);
            }
            Event::MappingStart(..) => {
                stack.push(Node::Map {
                    entries: Vec::new(),
                    line,
                });
                pending.push(None);
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let done = stack
                    .pop()
                    .expect("a close without an open is a parser bug");
                pending.pop();
                place(&mut stack, &mut pending, done, &mut root)?;
            }
            Event::Scalar(v, _, _, _) => {
                let value = v.into_owned();
                // Inside a map, alternate key and value. Checked before
                // the node is built so the key case never has to take a
                // scalar apart again — an `unreachable!` there would be
                // a line no test could reach and no reader could check.
                if let Some(Node::Map { .. }) = stack.last() {
                    let slot = pending.last_mut().expect("a map has a pending slot");
                    if slot.is_none() {
                        *slot = Some((value, line));
                        continue;
                    }
                }
                place(
                    &mut stack,
                    &mut pending,
                    Node::Scalar { value, line },
                    &mut root,
                )?;
            }
            _ => {}
        }
    }
    root.ok_or_else(|| {
        Refusal::at(None, "", "this file is empty").hint("a workflow needs `on:` and `jobs:`")
    })
}

/// Attach a finished node to whatever is open above it.
fn place(
    stack: &mut [Node],
    pending: &mut [Option<(String, usize)>],
    node: Node,
    root: &mut Option<Node>,
) -> Result<(), Refusal> {
    let Some(parent) = stack.last_mut() else {
        *root = Some(node);
        return Ok(());
    };
    match parent {
        Node::Seq { items, .. } => items.push(node),
        Node::Map { entries, .. } => {
            let slot = pending.last_mut().expect("a map has a pending slot");
            let (key, key_line) = slot.take().expect("a value arrived without a key");
            if entries.iter().any(|(k, _, _)| *k == key) {
                // Refused rather than last-wins: a file with `steps:`
                // twice has one of them doing nothing, and which one is
                // not something anybody should have to know.
                return Err(Refusal::at(
                    Some(key_line),
                    &key,
                    format!("{key:?} appears twice in the same block"),
                )
                .hint("remove one of them"));
            }
            entries.push((key, key_line, node));
        }
        // A scalar is never on the stack — only `Seq` and `Map` are
        // pushed — so there is no arm here to be unreachable.
        Node::Scalar { .. } => {}
    }
    Ok(())
}

// --- reading the shapes out of the tree -------------------------------

pub(super) fn as_map<'a>(
    node: &'a Node,
    key: &str,
) -> Result<&'a [(String, usize, Node)], Refusal> {
    match node {
        Node::Map { entries, .. } => Ok(entries),
        other => Err(Refusal::at(
            Some(other.line()),
            key,
            format!("{key} must be a block, not {}", other.kind()),
        )),
    }
}

pub(super) fn as_scalar<'a>(node: &'a Node, key: &str) -> Result<&'a str, Refusal> {
    match node {
        Node::Scalar { value, .. } => Ok(value),
        other => Err(Refusal::at(
            Some(other.line()),
            key,
            format!("{key} must be a value, not {}", other.kind()),
        )),
    }
}

/// A scalar, or a list of them. `on: push` and `on: [push]` both work,
/// and so does `needs: build`.
pub(super) fn scalars(node: &Node, key: &str) -> Result<Vec<(String, usize)>, Refusal> {
    match node {
        Node::Scalar { value, line } => Ok(vec![(value.clone(), *line)]),
        Node::Seq { items, .. } => items
            .iter()
            .map(|i| Ok((as_scalar(i, key)?.to_string(), i.line())))
            .collect(),
        other => Err(Refusal::at(
            Some(other.line()),
            key,
            format!("{key} must be a value or a list, not {}", other.kind()),
        )),
    }
}

/// A block of `name: value` pairs, for `env` and matrix `include`.
fn string_map(node: &Node, key: &str) -> Result<BTreeMap<String, String>, Refusal> {
    let mut out = BTreeMap::new();
    for (k, _, v) in as_map(node, key)? {
        out.insert(k.clone(), as_scalar(v, &format!("{key}.{k}"))?.to_string());
    }
    Ok(out)
}

pub(super) fn unknown(key: &str, path: &str, line: usize, known: &[&str]) -> Refusal {
    Refusal::at(
        Some(line),
        format!("{path}.{key}"),
        format!("{key:?} is not supported here"),
    )
    .hint(format!("what is: {}", known.join(", ")))
}

// --- the workflow ------------------------------------------------------

const WORKFLOW_KEYS: [&str; 3] = ["name", "on", "jobs"];
const JOB_KEYS: [&str; 9] = [
    "name",
    "needs",
    "image",
    "container",
    "runs-on",
    "env",
    "strategy",
    "steps",
    "timeout-minutes",
];
const STEP_KEYS: [&str; 3] = ["name", "run", "env"];

// --- mining refusal ----------------------------------------------------

/// Programs whose only purpose is to mine a cryptocurrency.
///
/// This is layer 2 of four. Layer 1 is the Network Firewall's domain
/// allowlist, which lets nothing reach a pool; layer 3 is the runner's
/// process watch, which kills a step that starts one anyway; layer 4
/// suspends the organisation when layer 3 fires. Only this layer answers
/// the *author*: nothing is scheduled, nothing is billed, and somebody
/// who pasted a workflow they had not read is told what is in it, on the
/// line it is on.
///
/// It is a refusal, not a detector. `curl -o m https://…/x && ./m` walks
/// straight past it, which is why there are four layers and not one.
///
/// The same list lives in `crates/stratum-runner/src/watch.rs`: the
/// runner is a standalone binary that deliberately does not link this
/// crate. A test there reads this file and fails if the two drift.
const MINERS: [&str; 17] = [
    "xmrig",
    "xmrigdaemon",
    "xmr-stak",
    "minerd",
    "cpuminer",
    "cpuminer-multi",
    "ethminer",
    "t-rex",
    "nbminer",
    "lolminer",
    "phoenixminer",
    "teamredminer",
    "gminer",
    "bfgminer",
    "cgminer",
    "nanominer",
    "srbminer",
];

/// The URL schemes a mining pool is spoken to over. A build has no use
/// for one, so the scheme alone is enough — the host does not matter.
const POOL_SCHEMES: [&str; 4] = [
    "stratum+tcp://",
    "stratum+ssl://",
    "stratum2+tcp://",
    "stratum+tls://",
];

/// Command names that stand *in front* of the program they run, so that
/// `sudo ./xmrig` is `xmrig` and not `sudo`.
const WRAPPERS: [&str; 11] = [
    "sudo", "doas", "nohup", "env", "exec", "setsid", "nice", "ionice", "stdbuf", "time", "timeout",
];

/// The offending line of a `run:` block, as an offset into it, and what
/// gave it away — or `None` for the overwhelming majority of workflows.
///
/// Two shapes are refused: a **pool URL** anywhere on the line, and a
/// **known miner invoked as a command**. As a *command*, deliberately:
/// `grep -r xmrig .` and `echo "no mining here"` are somebody writing
/// about the rule, not breaking it, and a refusal that cannot tell those
/// apart is one people learn to work around rather than read.
fn refuse_mining(line: usize, key: String, what: String) -> Refusal {
    Refusal::at(
        Some(line),
        key,
        "mining software is not permitted on hosted runners",
    )
    .hint(format!(
        "{what}; hosted runners are for building and testing your code"
    ))
}

fn mining_offence(run: &str) -> Option<(usize, String)> {
    for (i, line) in run.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        if let Some(scheme) = POOL_SCHEMES.iter().find(|s| lower.contains(**s)) {
            return Some((i, format!("`{scheme}` is a mining pool address")));
        }
        for cmd in commands_in(line) {
            if MINERS.contains(&cmd.as_str()) {
                return Some((i, format!("`{cmd}` is mining software")));
            }
        }
    }
    None
}

/// A pool address hidden in an `env:` value, and the name it was hidden
/// under.
///
/// `run: ./m $POOL` says nothing on its own, so a check that only read
/// `run:` lines would be walked around by the first person who tried.
/// Only the URL half applies here: a *value* that happens to contain the
/// word `xmrig` is a person naming a file or writing a message, and the
/// command half of this has no meaning where there is no command.
fn mining_in_env(env: &BTreeMap<String, String>) -> Option<String> {
    for (key, value) in env {
        let lower = value.to_ascii_lowercase();
        if let Some(scheme) = POOL_SCHEMES.iter().find(|s| lower.contains(**s)) {
            return Some(format!("`{key}` is a mining pool address (`{scheme}`)"));
        }
    }
    None
}

/// The program each command on a shell line starts, lower-cased and with
/// its directory stripped: `A=1 sudo ./miners/xmrig -o pool` is `xmrig`.
///
/// The shell is not parsed. The line is cut at the characters that begin
/// a new command, and each piece then has its leading environment
/// assignments, wrappers and flags skipped until a word is left. That is
/// as far as this goes on purpose: a real shell parser is still defeated
/// by `$MINER`, and the layers below this one exist because *any* static
/// reading of the text can be walked around.
fn commands_in(line: &str) -> Vec<String> {
    line.split([';', '|', '&', '(', ')', '`', '{', '}'])
        .filter_map(first_command)
        .collect()
}

fn first_command(segment: &str) -> Option<String> {
    let mut after_wrapper = false;
    for raw in segment.split_whitespace() {
        let tok = raw.trim_matches(['"', '\'', '\\']);
        if tok.is_empty() || assignment(tok) || tok.starts_with('-') {
            continue;
        }
        // `timeout 30m xmrig`: the wrapper's own argument, never a
        // program, so skipping it is safe and only reachable behind one.
        if after_wrapper && tok.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let name = tok.rsplit('/').next().unwrap_or(tok).to_ascii_lowercase();
        if WRAPPERS.contains(&name.as_str()) {
            after_wrapper = true;
            continue;
        }
        return Some(name);
    }
    None
}

/// `KEY=value` in front of a command, as the shell reads it.
fn assignment(tok: &str) -> bool {
    match tok.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !k.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Parse one workflow file.
///
/// `stem` is the file's name without its extension, used when the
/// document does not name itself.
pub fn parse(stem: &str, src: &str) -> Result<Workflow, Refusal> {
    if src.len() > MAX_BYTES {
        return Err(Refusal::at(
            None,
            "",
            format!("this file is {} bytes; the limit is {MAX_BYTES}", src.len()),
        )
        .hint("a workflow is a page of configuration, not a payload"));
    }
    let root = load(src)?;
    let top = as_map(&root, "the workflow")?;

    let mut name = stem.to_string();
    let mut on: Vec<Trigger> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    let mut saw_jobs = false;

    for (key, line, value) in top {
        match key.as_str() {
            "name" => name = as_scalar(value, "name")?.to_string(),
            "on" => {
                for (word, wline) in scalars(value, "on")? {
                    let Some(t) = Trigger::parse(&word) else {
                        return Err(Refusal::at(
                            Some(wline),
                            "on",
                            format!("{word:?} is not a trigger this forge has"),
                        )
                        .hint(
                            "what is: push, change (pull_request also means change), changeset",
                        ));
                    };
                    if !on.contains(&t) {
                        on.push(t);
                    }
                }
            }
            "jobs" => {
                saw_jobs = true;
                for (id, jline, jnode) in as_map(value, "jobs")? {
                    jobs.push(job(id, *jline, jnode)?);
                }
            }
            other => return Err(unknown(other, "", *line, &WORKFLOW_KEYS)),
        }
    }

    if on.is_empty() {
        return Err(
            Refusal::at(None, "on", "this workflow has no `on:` triggers")
                .hint("add `on: [push]`, or it would never run"),
        );
    }
    if !saw_jobs || jobs.is_empty() {
        return Err(
            Refusal::at(None, "jobs", "this workflow has no jobs").hint("add a `jobs:` block")
        );
    }
    on.sort_unstable();
    Ok(Workflow { name, on, jobs })
}

/// Read `runs-on:` into the pool it names and the labels it asked for.
///
/// Four shapes are accepted and they fall into two families:
///
/// ```yaml
/// runs-on: ubuntu-latest              # hosted, unchanged
/// runs-on: self-hosted                # the bare string form
/// runs-on: [self-hosted]              # any runner in an admitting group
/// runs-on: [self-hosted, linux, gpu]  # …that also has these labels
/// ```
///
/// A list that names both a hosted runner and `self-hosted` is refused
/// rather than resolved in either direction. There is no reading of it
/// that is more likely than the other, and both readings run somebody's
/// code somewhere they did not choose — which is exactly the mistake
/// this key must not make quietly.
///
/// Labels are lowercased here, once, because routing compares label
/// *sets*: `GPU` in the file and `gpu` on the runner would otherwise be
/// a job that never runs and an operator with nothing to look at.
/// Duplicates are dropped and the order the file wrote is kept, because
/// the order is what the "no runner with labels […]" refusal prints
/// back, and a reader has to recognise it as the line they wrote.
fn runs_on(value: &Node, path: &str) -> Result<(Pool, Vec<String>), Refusal> {
    let key = format!("{path}.runs-on");
    let entries = scalars(value, &key)?;
    if entries.is_empty() {
        return Err(Refusal::at(
            Some(value.line()),
            key.as_str(),
            "`runs-on` is empty, so this job asks for no runner at all",
        )
        .hint(format!(
            "name one of: {}, or `[self-hosted, …]` for a runner you registered",
            RUNS_ON_OK.join(", ")
        )));
    }
    // Lowercased once, deduplicated, file order kept.
    let mut labels: Vec<String> = Vec::with_capacity(entries.len());
    let mut lines: Vec<usize> = Vec::with_capacity(entries.len());
    for (raw, line) in &entries {
        let label = raw.trim().to_ascii_lowercase();
        if !labels.contains(&label) {
            labels.push(label);
            lines.push(*line);
        }
    }

    let no_such_runner = |label: &str, line: usize| {
        Refusal::at(
            Some(line),
            key.as_str(),
            format!("there is no {label:?} runner here"),
        )
        .hint(format!(
            "this forge runs Linux containers; what is: {}, or `[self-hosted, …]` \
             for a runner you registered. Choose the image with `image:`",
            RUNS_ON_OK.join(", ")
        ))
    };

    if let Some(at) = labels.iter().position(|l| l == SELF_HOSTED) {
        // A file that names a hosted runner *and* `self-hosted` has said
        // two incompatible things, and picking one would run untrusted
        // code on a fleet its author did not name.
        if let Some(i) = labels
            .iter()
            .position(|l| HOSTED_ONLY.contains(&l.as_str()))
        {
            return Err(Refusal::at(
                Some(lines[i]),
                key.as_str(),
                "`runs-on` names both a hosted runner and `self-hosted`; pick one",
            )
            .hint(
                "a hosted job is `runs-on: ubuntu-latest`; a job for a machine you \
                 registered is `runs-on: [self-hosted, …]`",
            ));
        }
        // Every other entry is a free-form label. Only its shape is
        // constrained — the server has no list of legal labels, because
        // the whole point is that an operator invents them.
        for (i, label) in labels.iter().enumerate() {
            if i != at && !stratum_control::runners::valid_label(label) {
                return Err(Refusal::at(
                    Some(lines[i]),
                    key.as_str(),
                    format!("{label:?} is not a runner label"),
                )
                .hint(
                    "a label is lowercase letters, digits, dot, dash or underscore, \
                     at most 64 characters — the same thing you pass to \
                     `weft-runner register --labels`",
                ));
            }
        }
        return Ok((Pool::SelfHosted, labels));
    }

    // No `self-hosted`: this is a hosted job, and a hosted job runs on
    // exactly one runner. A list of two hosted labels is not a wider
    // request, it is a file that has not decided.
    if labels.len() > 1 {
        return Err(Refusal::at(
            Some(lines[1]),
            key.as_str(),
            "a hosted job names exactly one runner",
        )
        .hint(format!(
            "write one of: {}, or `[self-hosted, …]` to ask for a machine you \
             registered",
            RUNS_ON_OK.join(", ")
        )));
    }
    if !RUNS_ON_OK.contains(&labels[0].as_str()) {
        return Err(no_such_runner(&labels[0], lines[0]));
    }
    Ok((Pool::Hosted, labels))
}

fn job(id: &str, line: usize, node: &Node) -> Result<Job, Refusal> {
    let path = format!("jobs.{id}");
    let entries = as_map(node, &path)?;
    let mut job = Job {
        id: id.to_string(),
        name: None,
        needs: Vec::new(),
        image: None,
        env: BTreeMap::new(),
        matrix: Matrix::default(),
        timeout_minutes: None,
        pool: Pool::Hosted,
        labels: vec![DEFAULT_HOSTED_LABEL.to_string()],
        steps: Vec::new(),
    };
    let mut saw_steps = false;

    for (key, kline, value) in entries {
        match key.as_str() {
            "name" => job.name = Some(as_scalar(value, &path)?.to_string()),
            "needs" => {
                for (need, nline) in scalars(value, &format!("{path}.needs"))? {
                    if need == id {
                        return Err(Refusal::at(
                            Some(nline),
                            format!("{path}.needs"),
                            format!("job {id:?} needs itself"),
                        )
                        .hint("remove it, or name the job that really runs first"));
                    }
                    if job.needs.contains(&need) {
                        return Err(Refusal::at(
                            Some(nline),
                            format!("{path}.needs"),
                            format!("{need:?} is listed twice"),
                        )
                        .hint("remove one of them"));
                    }
                    job.needs.push(need);
                }
            }
            // `container: node:18` and the map form both mean the image.
            "image" | "container" => {
                job.image = Some(match value {
                    Node::Scalar { value, .. } => value.clone(),
                    Node::Map { entries, .. } => {
                        let mut img = None;
                        for (k, l, v) in entries {
                            match k.as_str() {
                                "image" => img = Some(as_scalar(v, &path)?.to_string()),
                                other => {
                                    return Err(unknown(
                                        other,
                                        &format!("{path}.container"),
                                        *l,
                                        &["image"],
                                    )
                                    .hint(
                                        "credentials, ports, volumes and options are not \
                                         supported yet",
                                    ))
                                }
                            }
                        }
                        img.ok_or_else(|| {
                            Refusal::at(
                                Some(value.line()),
                                format!("{path}.container"),
                                "a container block needs an `image:`",
                            )
                        })?
                    }
                    other => {
                        return Err(Refusal::at(
                            Some(other.line()),
                            &path,
                            format!("container must be a value or a block, not {}", other.kind()),
                        ))
                    }
                });
            }
            "runs-on" => {
                let (pool, labels) = runs_on(value, &path)?;
                job.pool = pool;
                job.labels = labels;
            }
            "env" => {
                job.env = string_map(value, &format!("{path}.env"))?;
                if let Some(what) = mining_in_env(&job.env) {
                    return Err(refuse_mining(value.line(), format!("{path}.env"), what));
                }
            }
            "timeout-minutes" => {
                let raw = as_scalar(value, &path)?;
                let minutes: u32 = raw.parse().map_err(|_| {
                    Refusal::at(
                        Some(value.line()),
                        format!("{path}.timeout-minutes"),
                        format!("{raw:?} is not a whole number of minutes"),
                    )
                })?;
                // Zero parses, and the runner honours it: the budget is
                // spent before the first step, so the job fails every
                // time with "timed out after 0 minutes". A limit no run
                // can meet is a mistake in the file, and the file is
                // where it should be reported.
                if minutes == 0 {
                    return Err(Refusal::at(
                        Some(value.line()),
                        format!("{path}.timeout-minutes"),
                        "must be at least 1 minute".to_string(),
                    ));
                }
                job.timeout_minutes = Some(minutes);
            }
            "strategy" => job.matrix = strategy(value, &path)?,
            "steps" => {
                saw_steps = true;
                let Node::Seq { items, .. } = value else {
                    return Err(Refusal::at(
                        Some(value.line()),
                        format!("{path}.steps"),
                        format!("steps must be a list, not {}", value.kind()),
                    ));
                };
                for (i, item) in items.iter().enumerate() {
                    job.steps.push(step(&format!("{path}.steps[{i}]"), item)?);
                }
            }
            other => {
                let mut r = unknown(other, &path, *kline, &JOB_KEYS);
                // The two people meet most, answered specifically.
                if other == "if" {
                    r = r.hint(
                        "conditions are not supported yet; a job runs when everything \
                         it `needs` has passed",
                    );
                } else if other == "outputs" || other == "defaults" {
                    r = r.hint("not supported yet");
                }
                return Err(r);
            }
        }
    }

    if !saw_steps || job.steps.is_empty() {
        return Err(Refusal::at(
            Some(line),
            format!("{path}.steps"),
            format!("job {id:?} has no steps, so it would do nothing"),
        )
        .hint("add a `steps:` list, or remove the job"));
    }
    Ok(job)
}

fn step(path: &str, node: &Node) -> Result<Step, Refusal> {
    let entries = as_map(node, path)?;
    let mut out = Step {
        name: None,
        run: String::new(),
        env: BTreeMap::new(),
    };
    let mut saw_run = false;
    for (key, kline, value) in entries {
        match key.as_str() {
            "name" => out.name = Some(as_scalar(value, path)?.to_string()),
            "run" => {
                saw_run = true;
                let text = as_scalar(value, path)?;
                if let Some((offset, what)) = mining_offence(text) {
                    return Err(refuse_mining(
                        value.line() + offset,
                        format!("{path}.run"),
                        what,
                    ));
                }
                out.run = text.to_string();
            }
            "env" => {
                out.env = string_map(value, &format!("{path}.env"))?;
                if let Some(what) = mining_in_env(&out.env) {
                    return Err(refuse_mining(value.line(), format!("{path}.env"), what));
                }
            }
            // The refusal this whole subset exists for.
            "uses" => {
                return Err(Refusal::at(
                    Some(*kline),
                    format!("{path}.uses"),
                    "`uses:` is not supported — this forge does not run Actions",
                )
                .hint("run the command directly with `run:`"))
            }
            other => return Err(unknown(other, path, *kline, &STEP_KEYS)),
        }
    }
    if !saw_run || out.run.trim().is_empty() {
        return Err(Refusal::at(
            Some(node.line()),
            format!("{path}.run"),
            "a step needs a `run:` command",
        )
        .hint("every step here is a shell command"));
    }
    Ok(out)
}

fn strategy(node: &Node, path: &str) -> Result<Matrix, Refusal> {
    let mut matrix = Matrix::default();
    for (key, kline, value) in as_map(node, &format!("{path}.strategy"))? {
        match key.as_str() {
            "matrix" => matrix = matrix_block(value, &format!("{path}.strategy.matrix"))?,
            // Accepted and ignored would be a lie; both are scheduler
            // behaviour we have not built.
            other => {
                return Err(
                    unknown(other, &format!("{path}.strategy"), *kline, &["matrix"])
                        .hint("fail-fast and max-parallel are not supported yet"),
                )
            }
        }
    }
    Ok(matrix)
}

fn matrix_block(node: &Node, path: &str) -> Result<Matrix, Refusal> {
    let mut m = Matrix::default();
    for (key, _, value) in as_map(node, path)? {
        match key.as_str() {
            "include" | "exclude" => {
                let Node::Seq { items, .. } = value else {
                    return Err(Refusal::at(
                        Some(value.line()),
                        format!("{path}.{key}"),
                        format!("{key} must be a list, not {}", value.kind()),
                    ));
                };
                let mut out = Vec::new();
                for item in items {
                    out.push(string_map(item, &format!("{path}.{key}"))?);
                }
                if key == "include" {
                    m.include = out;
                } else {
                    m.exclude = out;
                }
            }
            axis => {
                let values = scalars(value, &format!("{path}.{axis}"))?;
                m.axes.push((
                    axis.to_string(),
                    values.into_iter().map(|(v, _)| v).collect(),
                ));
            }
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> Refusal {
        parse("ci", src).expect_err("should have been refused")
    }

    const GOOD: &str = "\
name: build and test
on: [push, pull_request]
jobs:
  test:
    image: rust:1.83
    env:
      RUST_LOG: debug
    strategy:
      matrix:
        os: [linux, mac]
    steps:
      - name: build
        run: cargo build
      - run: cargo test
  lint:
    needs: test
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy
";

    #[test]
    fn an_ordinary_workflow_parses() {
        let wf = parse("ci", GOOD).expect("valid");
        assert_eq!(wf.name, "build and test");
        // `pull_request` and `change` are the same trigger, deduplicated
        // and sorted so two spellings compare equal.
        assert_eq!(wf.on, vec![Trigger::Push, Trigger::Change]);
        assert_eq!(wf.jobs.len(), 2);
        let test = wf.job("test").unwrap();
        assert_eq!(test.image.as_deref(), Some("rust:1.83"));
        assert_eq!(test.env["RUST_LOG"], "debug");
        assert_eq!(test.matrix.axes[0].0, "os");
        assert_eq!(test.steps.len(), 2);
        assert_eq!(test.steps[0].name.as_deref(), Some("build"));
        assert_eq!(wf.job("lint").unwrap().needs, vec!["test"]);
    }

    /// A file with no `name:` is named after itself, so a run is never
    /// nameless.
    #[test]
    fn a_nameless_workflow_takes_the_file_name() {
        let wf = parse(
            "nightly",
            "on: push\njobs:\n  a:\n    steps:\n      - run: x\n",
        )
        .unwrap();
        assert_eq!(wf.name, "nightly");
    }

    /// The refusal this whole subset exists for, and the one a reader
    /// pasting an Actions workflow meets first. It must name the line
    /// and say what to do instead.
    #[test]
    fn uses_is_refused_with_a_line_and_a_way_forward() {
        let r = err("\
on: push
jobs:
  a:
    steps:
      - uses: actions/checkout@v4
      - run: make
");
        assert_eq!(r.line, Some(5), "point at the `uses:` line");
        assert!(r.message.contains("`uses:`"), "{r:?}");
        assert!(
            r.hint.contains("run:"),
            "the hint is the useful half: {r:?}"
        );
    }

    /// The alias-bomb class, refused by policy rather than survived by
    /// budget. The parser hands us `Alias` without expanding it, so this
    /// costs nothing at all.
    #[test]
    fn anchors_and_aliases_are_refused_before_anything_expands() {
        let bomb = "\
on: push
jobs:
  a: &a
    steps:
      - run: x
  b: *a
";
        let r = parse("ci", bomb).expect_err("anchored");
        assert!(r.message.contains("anchors"), "{r:?}");
        // And an alias with no anchor in sight is refused too.
        let r = err("on: push\njobs: *ghost\n");
        assert!(
            r.message.contains("aliases") || r.message.contains("anchor"),
            "{r:?}"
        );
    }

    /// The crash class. A Rust stack overflow aborts the process, so
    /// this is the difference between a bad request and the forge going
    /// down — and it is why this parser was chosen. Asserted, not
    /// assumed: if a future version drops the limit, this test is what
    /// notices.
    #[test]
    fn deeply_nested_input_is_an_error_and_not_a_crash() {
        // Both shapes: block nesting without indentation, and flow.
        let block = format!("{}x\n", "- ".repeat(5000));
        let r = parse("ci", &block).expect_err("5000 block levels");
        assert!(r.message.contains("recursion limit"), "{r:?}");

        let flow = format!("a: {}{}\n", "[".repeat(5000), "]".repeat(5000));
        let r = parse("ci", &flow).expect_err("5000 flow levels");
        assert!(r.message.contains("recursion limit"), "{r:?}");
    }

    #[test]
    fn a_file_too_large_to_be_configuration_is_refused_unparsed() {
        let big = format!("on: push\n# {}\n", "a".repeat(MAX_BYTES));
        let r = parse("ci", &big).expect_err("over the cap");
        assert!(r.message.contains("the limit is"), "{r:?}");
    }

    /// A key written twice has one copy doing nothing, and which one is
    /// not something anybody should have to know.
    #[test]
    fn a_duplicated_key_is_refused_rather_than_silently_last_wins() {
        let r = err("\
on: push
jobs:
  a:
    steps:
      - run: first
    steps:
      - run: second
");
        assert!(r.message.contains("twice"), "{r:?}");
    }

    /// A job with no steps would report a green check for having done
    /// nothing, which is the failure this subset is built to avoid.
    #[test]
    fn a_job_that_would_do_nothing_is_refused() {
        let r = err("on: push\njobs:\n  a:\n    image: alpine\n");
        assert!(r.message.contains("no steps"), "{r:?}");
        let r = err("on: push\njobs:\n  a:\n    steps: []\n");
        assert!(r.message.contains("no steps"), "{r:?}");
        // And a step with no command.
        let r = err("on: push\njobs:\n  a:\n    steps:\n      - name: nothing\n");
        assert!(r.message.contains("`run:`"), "{r:?}");
    }

    /// A workflow that could never run is refused rather than sitting
    /// in the tree looking like it might.
    #[test]
    fn a_workflow_with_no_trigger_or_no_jobs_is_refused() {
        let r = err("jobs:\n  a:\n    steps:\n      - run: x\n");
        assert!(r.message.contains("no `on:`"), "{r:?}");
        let r = err("on: push\njobs: {}\n");
        assert!(r.message.contains("no jobs"), "{r:?}");
    }

    /// An unknown trigger is named back, and the message says what we
    /// do have — `schedule` is the interesting one, since we announce a
    /// push and do not own a timer.
    #[test]
    fn an_unknown_trigger_names_what_exists() {
        let r = err("on: [push, schedule]\njobs:\n  a:\n    steps:\n      - run: x\n");
        assert!(r.message.contains("schedule"), "{r:?}");
        assert!(r.hint.contains("push"), "{r:?}");
        // And it names the composed trigger, which is the one nobody
        // pastes from Actions and so the one they have to be told about.
        assert!(r.hint.contains("changeset"), "{r:?}");
    }

    /// `changeset` is a trigger of its own, combines with the others,
    /// and sorts after them so two files listing it in either order
    /// compare equal.
    #[test]
    fn a_workflow_can_ask_for_the_composed_trigger() {
        let wf = parse(
            "ci",
            "on: changeset\njobs:\n  a:\n    steps:\n      - run: x\n",
        )
        .unwrap();
        assert_eq!(wf.on, vec![Trigger::Changeset]);

        let both = parse(
            "ci",
            "on: [changeset, change]\njobs:\n  a:\n    steps:\n      - run: x\n",
        )
        .unwrap();
        assert_eq!(both.on, vec![Trigger::Change, Trigger::Changeset]);
        // The same file written the other way round is the same file.
        let flipped = parse(
            "ci",
            "on: [pull_request, changeset]\njobs:\n  a:\n    steps:\n      - run: x\n",
        )
        .unwrap();
        assert_eq!(flipped.on, both.on);
    }

    /// `runs-on` is the one key that bends toward a pasted Actions file,
    /// and only where the answer is unambiguous.
    #[test]
    fn runs_on_is_accepted_only_where_it_means_our_runner() {
        let ok = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: x\n";
        let wf = parse("ci", ok).unwrap();
        assert_eq!(wf.jobs[0].pool, Pool::Hosted);
        assert_eq!(wf.jobs[0].labels, vec!["ubuntu-latest"]);
        // Refused, because running a macOS job on Linux is not a
        // smaller version of what was asked for — it is a different
        // thing reported green. The hint now offers the honest
        // alternative — a Mac of their own — as well as `image:`.
        for label in ["macos-latest", "windows-latest"] {
            let src = format!(
                "on: push\njobs:\n  a:\n    runs-on: {label}\n    steps:\n      - run: x\n"
            );
            let r = parse("ci", &src).expect_err("no such runner");
            assert!(r.message.contains(label), "{r:?}");
            assert!(r.hint.contains("image:"), "{r:?}");
            assert!(r.hint.contains("[self-hosted, …]"), "{r:?}");
        }
        // A job with no `runs-on:` at all still answers "what did you
        // ask for" — the run JSON shows labels for every job, and a
        // blank there reads as a job with no runner.
        let bare = "on: push\njobs:\n  a:\n    steps:\n      - run: x\n";
        let wf = parse("ci", bare).unwrap();
        assert_eq!(wf.jobs[0].pool, Pool::Hosted);
        assert_eq!(wf.jobs[0].labels, vec!["ubuntu-latest"]);
    }

    /// The four shapes a self-hosted `runs-on` may take, and what each
    /// leaves in the job.
    #[test]
    fn runs_on_reads_every_self_hosted_shape() {
        let cases: [(&str, Vec<&str>); 4] = [
            ("self-hosted", vec!["self-hosted"]),
            ("[self-hosted]", vec!["self-hosted"]),
            (
                "[self-hosted, linux, gpu]",
                vec!["self-hosted", "linux", "gpu"],
            ),
            // File order is kept even when `self-hosted` is not first:
            // the refusal sentence prints these back, and it has to be
            // recognisable as the line its author wrote.
            ("[gpu, self-hosted]", vec!["gpu", "self-hosted"]),
        ];
        for (written, want) in cases {
            let src = format!(
                "on: push\njobs:\n  a:\n    runs-on: {written}\n    steps:\n      - run: x\n"
            );
            let wf = parse("ci", &src).unwrap_or_else(|e| panic!("{written}: {e:?}"));
            assert_eq!(wf.jobs[0].pool, Pool::SelfHosted, "{written}");
            assert_eq!(wf.jobs[0].labels, want, "{written}");
        }
        // Case and repetition are the author's, not the routing's: a
        // job asking for `GPU` must reach a runner labelled `gpu`, or
        // it silently never runs.
        let wf = parse(
            "ci",
            "on: push\njobs:\n  a:\n    runs-on: [Self-Hosted, GPU, gpu]\n    steps:\n      - run: x\n",
        )
        .unwrap();
        assert_eq!(wf.jobs[0].labels, vec!["self-hosted", "gpu"]);
    }

    /// The refusals `runs-on` owns, each naming the line it is on.
    #[test]
    fn runs_on_refuses_what_it_cannot_route() {
        // Both fleets at once. There is no reading of this that is more
        // likely than the other, and both run somebody's code somewhere
        // they did not choose.
        let r = err(
            "on: push\njobs:\n  a:\n    runs-on: [ubuntu-latest, self-hosted]\n    steps:\n      - run: x\n",
        );
        assert_eq!(
            r.message, "`runs-on` names both a hosted runner and `self-hosted`; pick one",
            "{r:?}"
        );
        // Two hosted labels is a file that has not decided, not a wider
        // request.
        let r = err(
            "on: push\njobs:\n  a:\n    runs-on: [ubuntu-latest, linux]\n    steps:\n      - run: x\n",
        );
        assert!(r.message.contains("exactly one runner"), "{r:?}");
        // A label that is not the shape a label can be. Only checked in
        // the self-hosted family — there is no list of legal labels,
        // because the whole point is that an operator invents them.
        let r = err(
            "on: push\njobs:\n  a:\n    runs-on: [self-hosted, \"has space\"]\n    steps:\n      - run: x\n",
        );
        assert!(r.message.contains("is not a runner label"), "{r:?}");
        assert!(r.hint.contains("--labels"), "{r:?}");
        // Asking for no runner at all.
        let r = err("on: push\njobs:\n  a:\n    runs-on: []\n    steps:\n      - run: x\n");
        assert!(r.message.contains("empty"), "{r:?}");
    }

    /// `container: node:18` and the block form both name the image,
    /// because both appear in real workflows.
    #[test]
    fn container_is_accepted_in_both_shapes() {
        let bare = "on: push\njobs:\n  a:\n    container: node:18\n    steps:\n      - run: x\n";
        assert_eq!(
            parse("ci", bare).unwrap().jobs[0].image.as_deref(),
            Some("node:18")
        );
        let block = "on: push\njobs:\n  a:\n    container:\n      image: node:18\n    steps:\n      - run: x\n";
        assert_eq!(
            parse("ci", block).unwrap().jobs[0].image.as_deref(),
            Some("node:18")
        );
        // The parts of a container block we cannot honour are named.
        let r = err("on: push\njobs:\n  a:\n    container:\n      image: node:18\n      ports: [80]\n    steps:\n      - run: x\n");
        assert!(r.message.contains("ports"), "{r:?}");
    }

    /// The keys people meet most get their own answer rather than a
    /// bare "not supported".
    #[test]
    fn the_common_unsupported_keys_each_say_something_useful() {
        let r = err("on: push\njobs:\n  a:\n    if: always()\n    steps:\n      - run: x\n");
        assert!(r.hint.contains("needs"), "conditions: {r:?}");
        let r = err(
            "on: push\njobs:\n  a:\n    strategy:\n      fail-fast: false\n      matrix:\n        os: [linux]\n    steps:\n      - run: x\n",
        );
        assert!(r.message.contains("fail-fast"), "{r:?}");
        // An unknown key at the top level lists what is allowed.
        let r = err("on: push\nenv:\n  A: 1\njobs:\n  a:\n    steps:\n      - run: x\n");
        assert!(r.hint.contains("jobs"), "{r:?}");
    }

    /// The "what is:" hint on a mistyped job key is the list of keys the
    /// parser accepts — the whole list. `timeout-minutes` was accepted
    /// but missing from it, so someone who wrote `timeout_minutes` was
    /// told the key did not exist. Every key the hint names must parse,
    /// and every key the parser accepts must be in the hint.
    #[test]
    fn the_job_key_hint_lists_exactly_what_the_parser_accepts() {
        let r = err("on: push\njobs:\n  a:\n    timeout_minutes: 30\n    steps:\n      - run: x\n");
        assert!(r.hint.contains("timeout-minutes"), "{r:?}");
        let value = |k: &str| match k {
            "name" | "image" | "container" => "x",
            "needs" => "[]",
            "runs-on" => "linux",
            "env" | "strategy" => "{}",
            "steps" => "[{run: x}]",
            "timeout-minutes" => "30",
            other => panic!("JOB_KEYS grew {other:?}; teach this test its value"),
        };
        for key in JOB_KEYS {
            let text = format!(
                "on: push\njobs:\n  a:\n    {key}: {}\n    steps:\n      - run: x\n",
                value(key)
            );
            let text = if key == "steps" {
                format!("on: push\njobs:\n  a:\n    steps: {}\n", value(key))
            } else {
                text
            };
            assert!(
                parse("ci.yml", &text).is_ok(),
                "{key} is in the hint but is refused: {:?}",
                parse("ci.yml", &text).err()
            );
        }
    }

    /// A job needing itself is caught at parse time, where the line is
    /// still known — the planner catches the longer cycles.
    #[test]
    fn a_job_that_needs_itself_is_caught_with_its_line() {
        let r = err("on: push\njobs:\n  a:\n    needs: a\n    steps:\n      - run: x\n");
        assert!(r.message.contains("needs itself"), "{r:?}");
        assert!(r.line.is_some(), "{r:?}");
    }

    /// The full matrix shape survives the round trip into the planner's
    /// input, including `include`/`exclude`.
    #[test]
    fn a_matrix_with_include_and_exclude_parses() {
        let wf = parse(
            "ci",
            "\
on: push
jobs:
  t:
    strategy:
      matrix:
        os: [linux, mac]
        v: ['1', '2']
        exclude:
          - os: mac
            v: '1'
        include:
          - os: linux
            extra: yes
    steps:
      - run: x
",
        )
        .expect("valid");
        let m = &wf.jobs[0].matrix;
        assert_eq!(m.axes.len(), 2, "exclude/include are not axes: {m:?}");
        assert_eq!(m.axes[0].0, "os", "declaration order is preserved");
        assert_eq!(m.exclude.len(), 1);
        assert_eq!(m.include[0]["extra"], "yes");
    }

    /// Wrong shapes are named as shapes, so a reader knows whether they
    /// wrote a list where a block belonged.
    #[test]
    fn a_wrong_shape_says_which_shape_it_wanted() {
        let r = err("on: push\njobs:\n  - run: x\n");
        assert!(r.message.contains("must be a block"), "{r:?}");
        let r = err("on: push\njobs:\n  a:\n    steps:\n      run: x\n");
        assert!(r.message.contains("must be a list"), "{r:?}");
    }

    #[test]
    fn an_empty_or_multi_document_file_is_refused() {
        let r = err("");
        assert!(r.message.contains("empty"), "{r:?}");
        let r = err("on: push\njobs:\n  a:\n    steps:\n      - run: x\n---\non: push\n");
        assert!(r.message.contains("more than one"), "{r:?}");
    }

    /// Every wrong-shape path, because each one is a sentence somebody
    /// will read and none of them had been exercised. A refusal that
    /// panics or says the wrong thing is worse than the mistake it is
    /// reporting.
    #[test]
    fn every_wrong_shape_refuses_in_words_rather_than_panicking() {
        // A scalar wanted, a block given.
        let r = err("on: push\njobs:\n  a:\n    name:\n      x: y\n    steps:\n      - run: x\n");
        assert!(r.message.contains("must be a value"), "{r:?}");
        // A scalar-or-list wanted, a block given.
        let r = err("on:\n  push: yes\njobs:\n  a:\n    steps:\n      - run: x\n");
        assert!(r.message.contains("must be a value or a list"), "{r:?}");
        // A list item that is not a scalar.
        let r =
            err("on: push\njobs:\n  a:\n    needs:\n      - x: y\n    steps:\n      - run: x\n");
        assert!(r.message.contains("must be a value"), "{r:?}");
        // A container block with no image at all.
        let r = err("on: push\njobs:\n  a:\n    container:\n      env:\n        A: 1\n    steps:\n      - run: x\n");
        assert!(
            r.message.contains("env") || r.message.contains("image"),
            "{r:?}"
        );
        // A container that is neither a value nor a block.
        let r = err("on: push\njobs:\n  a:\n    container: [a, b]\n    steps:\n      - run: x\n");
        assert!(r.message.contains("must be a value or a block"), "{r:?}");
        // include/exclude that are not lists.
        let r = err("on: push\njobs:\n  a:\n    strategy:\n      matrix:\n        include: nope\n    steps:\n      - run: x\n");
        assert!(r.message.contains("must be a list"), "{r:?}");
        // A step that is not a block.
        let r = err("on: push\njobs:\n  a:\n    steps:\n      - just-a-string\n");
        assert!(r.message.contains("must be a block"), "{r:?}");
        // An unknown key on a step.
        let r = err("on: push\njobs:\n  a:\n    steps:\n      - run: x\n        shell: bash\n");
        assert!(r.message.contains("shell"), "{r:?}");
        assert!(r.hint.contains("run"), "{r:?}");
    }

    /// A YAML tag is refused. `!!str` and friends select a type, and a
    /// subset that reads everything as text has nothing to do with one
    /// except mislead somebody into thinking it was honoured.
    #[test]
    fn yaml_tags_are_refused() {
        let r = err("on: !!str push\njobs:\n  a:\n    steps:\n      - run: x\n");
        assert!(r.message.contains("tags"), "{r:?}");
        assert!(r.line.is_some(), "{r:?}");
    }

    /// An empty container block names no image, which is the one way to
    /// reach that refusal — every other key in the block is refused
    /// before the image is missed.
    #[test]
    fn an_empty_container_block_is_refused_for_its_missing_image() {
        let r = err("on: push\njobs:\n  a:\n    container: {}\n    steps:\n      - run: x\n");
        assert!(r.message.contains("needs an `image:`"), "{r:?}");
    }

    /// A container block that names an image *and* something we cannot
    /// honour is refused for the part we cannot honour — quietly
    /// dropping `ports:` would run a job that cannot reach its service.
    #[test]
    fn a_container_block_with_an_image_still_refuses_what_it_cannot_do() {
        let r = err("on: push\njobs:\n  a:\n    container:\n      image: node:18\n      credentials:\n        username: u\n    steps:\n      - run: x\n");
        assert!(r.message.contains("credentials"), "{r:?}");
    }

    /// A duplicate whose value is a plain scalar, not a block. The
    /// refusal is raised from a different placement path than the
    /// `steps:` case above, and one of the two had never run.
    #[test]
    fn a_duplicated_scalar_key_is_refused_too() {
        let r = err("name: a\nname: b\non: push\njobs:\n  a:\n    steps:\n      - run: x\n");
        assert!(r.message.contains("twice"), "{r:?}");
    }

    #[test]
    fn a_need_listed_twice_is_refused() {
        let r = err("on: push\njobs:\n  b:\n    steps:\n      - run: x\n  a:\n    needs: [b, b]\n    steps:\n      - run: x\n");
        assert!(r.message.contains("twice"), "{r:?}");
    }

    /// `outputs:` and `defaults:` get their own answer, because they are
    /// the next two keys people reach for after `if:`.
    #[test]
    fn outputs_and_defaults_are_named_rather_than_lumped_in() {
        for key in ["outputs", "defaults"] {
            let src = format!(
                "on: push\njobs:\n  a:\n    {key}:\n      x: y\n    steps:\n      - run: x\n"
            );
            let r = parse("ci", &src).expect_err("not supported");
            assert!(r.message.contains(key), "{r:?}");
            assert!(!r.hint.is_empty(), "it should say something: {r:?}");
        }
    }

    #[test]
    fn a_timeout_must_be_a_number() {
        let r =
            err("on: push\njobs:\n  a:\n    timeout-minutes: soon\n    steps:\n      - run: x\n");
        assert!(r.message.contains("whole number"), "{r:?}");
        let wf = parse(
            "ci",
            "on: push\njobs:\n  a:\n    timeout-minutes: 30\n    steps:\n      - run: x\n",
        )
        .unwrap();
        assert_eq!(wf.jobs[0].timeout_minutes, Some(30));
    }

    #[test]
    fn a_timeout_of_zero_or_below_is_refused_in_the_file_not_on_the_runner() {
        // `0` parsed and shipped; the runner then failed every job with
        // "timed out after 0 minutes" — a verdict about a mistake that
        // belongs on the file, where it can be fixed.
        let r = err("on: push\njobs:\n  a:\n    timeout-minutes: 0\n    steps:\n      - run: x\n");
        assert_eq!(r.key, "jobs.a.timeout-minutes", "{r:?}");
        assert!(r.message.contains("at least 1 minute"), "{r:?}");
        let r = err("on: push\njobs:\n  a:\n    timeout-minutes: -5\n    steps:\n      - run: x\n");
        assert!(r.message.contains("whole number"), "{r:?}");
        let wf = parse(
            "ci",
            "on: push\njobs:\n  a:\n    timeout-minutes: 1\n    steps:\n      - run: x\n",
        )
        .unwrap();
        assert_eq!(wf.jobs[0].timeout_minutes, Some(1));
    }

    /// Layer 2 of the mining defence, and the only one that answers the
    /// person who wrote the file. Each of these would be a job we ran and
    /// billed before anything else noticed.
    #[test]
    fn a_run_line_that_starts_a_miner_or_names_a_pool_is_refused() {
        let starts_one = [
            "xmrig -o pool.example:3333",
            "./xmrig",
            "sudo /usr/local/bin/nbminer",
            "A=1 env B=2 nice -n 19 ./miners/T-Rex",
            "make && cpuminer-multi",
            "timeout 30m xmrig",
            "curl -s https://example.invalid/x | minerd",
            "srbminer &",
            "cargo build; ethminer",
            "(cd /tmp; lolminer)",
            "\"/opt/bin/phoenixminer\"",
        ];
        let names_a_pool = [
            "./m --url stratum+tcp://pool.example:3333 -u me",
            "MINER_URL=stratum2+tcp://pool.example:3333 ./m",
            "echo STRATUM+SSL://POOL.EXAMPLE:443",
            "./m -o stratum+tls://pool.example:443",
        ];
        for run in starts_one.into_iter().chain(names_a_pool) {
            let src = format!("on: push\njobs:\n  a:\n    steps:\n      - run: {run}\n");
            let r = parse("ci", &src)
                .err()
                .unwrap_or_else(|| panic!("this workflow was accepted: {run}"));
            assert_eq!(
                r.message, "mining software is not permitted on hosted runners",
                "{run}"
            );
            assert_eq!(r.key, "jobs.a.steps[0].run", "{run}");
            assert_eq!(r.line, Some(5), "{run}");
        }
    }

    /// The other half of the rule, and the half a blunt substring match
    /// would get wrong: naming a miner is not running one. A refusal that
    /// fires on `grep xmrig` is one people route around — with a variable,
    /// or by not reading the message — and then the interesting refusals
    /// are not read either.
    #[test]
    fn a_workflow_that_only_mentions_mining_still_parses() {
        for run in [
            "echo \"we do not permit xmrig on this forge\"",
            "grep -rn xmrig .",
            "cat xmrig.log",
            "rm -f /tmp/xmrig.pid",
            "./scripts/xmrig-check.sh",
            "cargo build --release && cargo test",
            "make -j8 all",
        ] {
            let src = format!("on: push\njobs:\n  a:\n    steps:\n      - run: {run}\n");
            let wf = parse("ci", &src).unwrap_or_else(|e| panic!("{run}: {e:?}"));
            assert_eq!(wf.jobs[0].steps[0].run, run);
        }
    }

    /// `run: ./m $POOL` says nothing by itself, so the value has to be
    /// looked at too — at both levels, because a job's `env:` reaches
    /// every step in it.
    #[test]
    fn a_pool_address_smuggled_through_env_is_refused_too() {
        for src in [
            "on: push\njobs:\n  a:\n    steps:\n      - run: ./m $POOL\n        env:\n          POOL: stratum+tcp://pool.example:3333\n",
            "on: push\njobs:\n  a:\n    env:\n      POOL: stratum+ssl://pool.example:443\n    steps:\n      - run: ./m $POOL\n",
        ] {
            let r = err(src);
            assert_eq!(
                r.message, "mining software is not permitted on hosted runners",
                "{src}"
            );
            assert!(r.hint.contains("`POOL` is a mining pool address"), "{r:?}");
        }
        // …and an ordinary environment is left alone, including one whose
        // value names a miner, which is a person writing a message.
        parse(
            "ci",
            "on: push\njobs:\n  a:\n    env:\n      NOTE: we refuse xmrig\n    steps:\n      - run: make\n",
        )
        .expect("an ordinary env");
    }

    /// A block `run:` is a script, and pointing at the top of it makes the
    /// reader find the offending line themselves. The offset is counted
    /// from the first line of the block, which is where the scalar starts.
    #[test]
    fn the_refusal_points_at_the_line_the_miner_is_on() {
        let src = "\
on: push
jobs:
  a:
    steps:
      - name: Build
        run: |
          cargo build --release
          ./target/release/x --check
          ./xmrig -o stratum+tcp://pool.example:3333
      - run: echo done
";
        let r = err(src);
        assert_eq!(
            r.message,
            "mining software is not permitted on hosted runners"
        );
        assert_eq!(r.line, Some(9), "the miner is on line 9");
        assert!(r.hint.contains("mining pool address"), "{r:?}");
    }
}
