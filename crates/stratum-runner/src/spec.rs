//! The job document the control plane hands a runner, and the environment
//! a step is given.
//!
//! Walked out of a `serde_json::Value` by hand rather than derived. A
//! derived deserializer can only report "expected a string at line 1
//! column 812", and the one person who ever reads that message is
//! debugging a control plane they cannot see. Walking it means every
//! refusal names the field, and the runner's stderr — the operator's
//! terminal or journal, which is all they get — says which one.

use std::collections::BTreeMap;
use std::path::Path;

/// One command and what to call it while it runs.
///
/// `name` is optional because the workflow parser makes it optional; an
/// unnamed step is shown by its command, which is what a reader of the
/// log wants anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: Option<String>,
    pub run: String,
    pub env: BTreeMap<String, String>,
}

impl Step {
    /// What the log calls this step, and what a failure message names.
    pub fn label(&self) -> &str {
        match &self.name {
            Some(n) => n,
            None => &self.run,
        }
    }
}

/// Everything about *what to run*, as stored in `workflow_jobs.spec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    pub image: String,
    pub timeout_minutes: u32,
    pub env: BTreeMap<String, String>,
    pub matrix: BTreeMap<String, String>,
    pub steps: Vec<Step>,
}

/// One repository of a composed run: which tree to materialise, and the
/// credential that is allowed to read it.
///
/// `token` is `None` on the job's own repository, where the job token
/// already grants the read, and `Some` on every other member — a token
/// minted for that one repository, so that a member's CI script cannot
/// read the rest of the organisation.
#[derive(Clone, PartialEq, Eq)]
pub struct MemberSpec {
    pub repo: String,
    pub change: String,
    /// Credential-free, like `Assignment::clone_url`.
    pub clone_url: String,
    pub fetch_ref: String,
    pub commit_sha: String,
    pub token: Option<String>,
}

/// Hand-written, and the whole point is the token: a derived `Debug` is
/// how a credential ends up in a panic message that gets pasted into an
/// issue. `Config` in `main.rs` has no `Debug` at all for the same
/// reason; this one is inside an `Assignment` that the rest of the runner
/// prints, so it redacts instead.
impl std::fmt::Debug for MemberSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemberSpec")
            .field("repo", &self.repo)
            .field("change", &self.change)
            .field("clone_url", &self.clone_url)
            .field("fetch_ref", &self.fetch_ref)
            .field("commit_sha", &self.commit_sha)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The changeset a composed job is one member of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangesetSpec {
    pub key: String,
    pub members: Vec<MemberSpec>,
}

impl ChangesetSpec {
    /// The job's own member: the tokenless one. `parse_assignment`
    /// refuses a changeset that does not carry exactly one, so this is
    /// total for any spec that got as far as being run.
    pub fn own(&self) -> &MemberSpec {
        self.members
            .iter()
            .find(|m| m.token.is_none())
            .expect("parsing admits exactly one member without a token")
    }
}

/// A job as handed to this runner: the spec, plus the identity and the
/// coordinates of the tree to check out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub id: String,
    pub run_id: String,
    pub attempt: i64,
    /// The matrix-expanded key (`test (rust=1.80)`), used as `WEFT_JOB`.
    pub key: String,
    /// The job's declared id in the workflow file.
    pub job: String,
    /// Credential-free; the token travels in a header, never in the URL.
    pub clone_url: String,
    pub fetch_ref: String,
    pub commit_sha: String,
    pub ref_name: String,
    pub event: String,
    pub change_key: Option<String>,
    /// Present only on a composed run (`event == "changeset"`); absent
    /// from every push- and change-triggered document, including every
    /// document written before composed runs existed.
    pub changeset: Option<ChangesetSpec>,
    pub spec: JobSpec,
}

/// Parse the `GET /v1/runner/jobs/:id` body.
///
/// The spec fields are flattened alongside the identity fields, which is
/// what the control plane sends; `error` names the first field that is
/// missing or the wrong shape.
pub fn parse_assignment(body: &str) -> Result<Assignment, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("job document is not JSON: {e}"))?;
    Ok(Assignment {
        id: string_field(&v, "id")?,
        run_id: string_field(&v, "run_id")?,
        attempt: int_field(&v, "attempt")?,
        key: string_field(&v, "key")?,
        job: string_field(&v, "job")?,
        clone_url: string_field(&v, "clone_url")?,
        fetch_ref: string_field(&v, "fetch_ref")?,
        commit_sha: string_field(&v, "commit_sha")?,
        ref_name: string_field(&v, "ref_name")?,
        event: string_field(&v, "event")?,
        change_key: optional_string_field(&v, "change_key")?,
        changeset: parse_changeset(&v)?,
        spec: parse_spec(&v)?,
    })
}

/// The `changeset` object, when the run is a composed one.
///
/// Absent and `null` both mean "not composed", which is what every
/// push- and change-triggered document says and what every document
/// written before composed runs existed says by omission.
fn parse_changeset(v: &serde_json::Value) -> Result<Option<ChangesetSpec>, String> {
    let cs = match v.get("changeset") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(o @ serde_json::Value::Object(_)) => o,
        _ => return Err("field \"changeset\" is not an object".into()),
    };
    let list = match cs.get("members") {
        Some(serde_json::Value::Array(a)) => a,
        _ => return Err("field \"changeset.members\" is missing or not an array".into()),
    };
    let mut members = Vec::with_capacity(list.len());
    for (i, m) in list.iter().enumerate() {
        let at = |field: &str| format!("changeset.members[{i}].{field}");
        let repo = named_string(m, "repo", &at("repo"))?;
        // The name is joined onto the workspace root to make the
        // member's directory, so it has to be a directory *name*. A
        // control plane — or anything that could pass for one — must not
        // be able to spell `../../etc` and have the runner write there.
        if repo.is_empty() || repo.contains('/') || repo.contains('\\') || repo.starts_with('.') {
            return Err(format!(
                "field \"{}\" is not a directory name: {repo}",
                at("repo")
            ));
        }
        members.push(MemberSpec {
            repo,
            change: named_string(m, "change", &at("change"))?,
            clone_url: named_string(m, "clone_url", &at("clone_url"))?,
            fetch_ref: named_string(m, "fetch_ref", &at("fetch_ref"))?,
            commit_sha: named_string(m, "commit_sha", &at("commit_sha"))?,
            token: named_optional_string(m, "token", &at("token"))?,
        });
    }
    // Exactly one member is the job's own, and the control plane says
    // which by leaving its token out. Zero would leave the steps with no
    // directory to run in; two would make which one silent and arbitrary.
    let own = members.iter().filter(|m| m.token.is_none()).count();
    if own != 1 {
        return Err(format!(
            "changeset.members must hold exactly one member without a token, found {own}"
        ));
    }
    Ok(Some(ChangesetSpec {
        key: named_string(cs, "key", "changeset.key")?,
        members,
    }))
}

/// The spec half of the same document.
///
/// `image` and `timeout_minutes` default the way the workflow model does,
/// so a control plane that omits them behaves like one that sent the
/// defaults rather than refusing the job.
fn parse_spec(v: &serde_json::Value) -> Result<JobSpec, String> {
    let steps = match v.get("steps") {
        Some(serde_json::Value::Array(a)) => a,
        _ => return Err("field \"steps\" is missing or not an array".into()),
    };
    let mut parsed = Vec::with_capacity(steps.len());
    for (i, s) in steps.iter().enumerate() {
        parsed.push(parse_step(s, i)?);
    }
    Ok(JobSpec {
        image: match v.get("image") {
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => "default".into(),
        },
        timeout_minutes: match v.get("timeout_minutes") {
            Some(serde_json::Value::Number(n)) => n
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| format!("field \"timeout_minutes\" is out of range: {n}"))?,
            _ => 360,
        },
        env: string_map(v, "env")?,
        matrix: string_map(v, "matrix")?,
        steps: parsed,
    })
}

fn parse_step(s: &serde_json::Value, i: usize) -> Result<Step, String> {
    let run = match s.get("run") {
        Some(serde_json::Value::String(r)) => r.clone(),
        _ => return Err(format!("step {i} has no \"run\" string")),
    };
    Ok(Step {
        name: match s.get("name") {
            Some(serde_json::Value::String(n)) => Some(n.clone()),
            _ => None,
        },
        run,
        env: string_map(s, "env")?,
    })
}

fn string_field(v: &serde_json::Value, field: &str) -> Result<String, String> {
    named_string(v, field, field)
}

/// `string_field` where the key and the name to complain about differ —
/// a member's `repo` is `changeset.members[0].repo` to the one person
/// reading the runner's stderr.
fn named_string(v: &serde_json::Value, field: &str, shown: &str) -> Result<String, String> {
    match v.get(field) {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        _ => Err(format!("field \"{shown}\" is missing or not a string")),
    }
}

/// `null` and absent both mean "no value" — a change key only exists on a
/// change-triggered run, and the control plane spells that either way.
fn optional_string_field(v: &serde_json::Value, field: &str) -> Result<Option<String>, String> {
    named_optional_string(v, field, field)
}

fn named_optional_string(
    v: &serde_json::Value,
    field: &str,
    shown: &str,
) -> Result<Option<String>, String> {
    match v.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(format!("field \"{shown}\" is not a string")),
    }
}

fn int_field(v: &serde_json::Value, field: &str) -> Result<i64, String> {
    match v.get(field) {
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| format!("field \"{field}\" is not an integer")),
        _ => Err(format!("field \"{field}\" is missing or not a number")),
    }
}

/// An absent map is an empty map; a present one must be strings all the
/// way down, because everything here ends up in `execve`'s environment
/// and a number silently stringified is a difference nobody would see
/// until a step compared it.
fn string_map(v: &serde_json::Value, field: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    match v.get(field) {
        None | Some(serde_json::Value::Null) => Ok(out),
        Some(serde_json::Value::Object(o)) => {
            for (k, val) in o {
                match val {
                    serde_json::Value::String(s) => {
                        out.insert(k.clone(), s.clone());
                    }
                    _ => return Err(format!("{field}.{k} is not a string")),
                }
            }
            Ok(out)
        }
        _ => Err(format!("field \"{field}\" is not an object")),
    }
}

/// The variables inherited from the runner's own process.
///
/// Three, deliberately. The runner's own environment is the operator's —
/// their shell, or their service unit — and carries whatever they keep
/// there: a cloud key, a `GITHUB_TOKEN`, an `SSH_AUTH_SOCK`. Scrubbing to
/// an allowlist is what keeps "run arbitrary shell from a fork" from
/// meaning "read the credentials of the machine it ran on".
pub fn inherited_env() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for key in ["PATH", "HOME", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            out.insert(key.to_string(), v);
        }
    }
    out
}

/// The environment one step runs with.
///
/// Layered lowest-to-highest: inherited, then the job facts, then the
/// workflow's `env:`, then the step's own `env:`. The matrix arrives last
/// under its own `WEFT_MATRIX_` prefix, where nothing else can be
/// shadowed by it.
///
/// `workspace` is the root the members of a composed job were
/// materialised under, and is `None` for every other job. No member
/// token goes in here: a step that could read one could read a sibling
/// repository for as long as the job lives, which is the whole reason
/// the tokens are minted per member in the first place.
pub fn step_env(
    inherited: &BTreeMap<String, String>,
    a: &Assignment,
    step: &Step,
    workspace: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut env = inherited.clone();
    env.insert("CI".into(), "true".into());
    env.insert("WEFT_CI".into(), "true".into());
    env.insert("WEFT_JOB".into(), a.key.clone());
    env.insert("WEFT_SHA".into(), a.commit_sha.clone());
    env.insert("WEFT_REF".into(), a.ref_name.clone());
    env.insert("WEFT_EVENT".into(), a.event.clone());
    if let Some(change) = &a.change_key {
        env.insert("WEFT_CHANGE".into(), change.clone());
    }
    if let (Some(cs), Some(root)) = (&a.changeset, workspace) {
        env.insert("WEFT_WORKSPACE".into(), root.display().to_string());
        env.insert("WEFT_CHANGESET".into(), cs.key.clone());
        env.insert("WEFT_CHANGESET_MEMBERS".into(), members_json(cs, root));
    }
    for (k, v) in &a.spec.env {
        env.insert(k.clone(), v.clone());
    }
    for (k, v) in &step.env {
        env.insert(k.clone(), v.clone());
    }
    for (k, v) in &a.spec.matrix {
        env.insert(format!("WEFT_MATRIX_{}", matrix_var(k)), v.clone());
    }
    env
}

/// `WEFT_CHANGESET_MEMBERS`: the workspace, in member order, as JSON.
///
/// A step that wants a sibling's tree needs its path, and a step that
/// wants to know what it is being tested against needs the commit; the
/// order is the changeset's own, so a script can rely on it. The token
/// is deliberately not here — see `step_env`.
fn members_json(cs: &ChangesetSpec, root: &Path) -> String {
    let members: Vec<serde_json::Value> = cs
        .members
        .iter()
        .map(|m| {
            serde_json::json!({
                "repo": m.repo,
                "change": m.change,
                "commit": m.commit_sha,
                "path": root.join(&m.repo).display().to_string(),
            })
        })
        .collect();
    serde_json::Value::Array(members).to_string()
}

/// A matrix key becomes an environment variable name, so it is upper-cased
/// and everything outside `[A-Z0-9_]` becomes `_`. A key like `node-version`
/// would otherwise produce a name no shell can expand.
fn matrix_var(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A complete document, as the control plane sends it.
    pub(crate) fn doc() -> String {
        serde_json::json!({
            "id": "job1", "run_id": "run1", "attempt": 1,
            "key": "test (rust=1.80)", "job": "test",
            "clone_url": "https://stratum.example/acme/widget.git",
            "fetch_ref": "refs/heads/main",
            "commit_sha": "abc123", "ref_name": "main", "event": "push",
            "change_key": serde_json::Value::Null,
            "image": "default", "timeout_minutes": 5,
            "env": {"WORKFLOW": "yes"},
            "matrix": {"rust-toolchain": "1.80"},
            "steps": [{"name": "Test", "run": "cargo test", "env": {"STEP": "yes"}}],
        })
        .to_string()
    }

    #[test]
    fn a_complete_document_round_trips_into_an_assignment() {
        let a = parse_assignment(&doc()).expect("parse");
        assert_eq!(a.id, "job1");
        assert_eq!(a.run_id, "run1");
        assert_eq!(a.attempt, 1);
        assert_eq!(a.job, "test");
        assert_eq!(a.clone_url, "https://stratum.example/acme/widget.git");
        assert_eq!(a.fetch_ref, "refs/heads/main");
        assert_eq!(a.ref_name, "main");
        assert_eq!(a.change_key, None);
        assert_eq!(a.spec.image, "default");
        assert_eq!(a.spec.timeout_minutes, 5);
        assert_eq!(a.spec.steps.len(), 1);
        assert_eq!(a.spec.steps[0].label(), "Test");
        // The document a push or a change produces has no `changeset`,
        // and neither has any document written before composed runs
        // existed. Both must go on parsing.
        assert_eq!(a.changeset, None);
    }

    #[test]
    fn image_and_timeout_default_when_the_control_plane_omits_them() {
        let a = parse_assignment(
            &serde_json::json!({
                "id": "j", "run_id": "r", "attempt": 2, "key": "k", "job": "j",
                "clone_url": "u", "fetch_ref": "f", "commit_sha": "s",
                "ref_name": "main", "event": "push", "change_key": "chg",
                "steps": [{"run": "echo hi"}],
            })
            .to_string(),
        )
        .expect("parse");
        assert_eq!(a.spec.image, "default");
        assert_eq!(a.spec.timeout_minutes, 360);
        assert_eq!(a.spec.env, BTreeMap::new());
        assert_eq!(a.spec.matrix, BTreeMap::new());
        assert_eq!(a.change_key.as_deref(), Some("chg"));
        // An unnamed step is shown by its command.
        assert_eq!(a.spec.steps[0].label(), "echo hi");
        assert_eq!(a.spec.steps[0].name, None);
    }

    #[test]
    fn every_malformed_field_is_refused_by_name() {
        let cases: &[(serde_json::Value, &str)] = &[
            (
                serde_json::json!({}),
                "field \"id\" is missing or not a string",
            ),
            (
                serde_json::json!({"id": "j", "run_id": "r"}),
                "field \"attempt\" is missing or not a number",
            ),
            (
                serde_json::json!({"id": "j", "run_id": "r", "attempt": 1.5}),
                "field \"attempt\" is not an integer",
            ),
            (
                serde_json::json!({
                    "id": "j", "run_id": "r", "attempt": 1, "key": "k", "job": "j",
                    "clone_url": "u", "fetch_ref": "f", "commit_sha": "s",
                    "ref_name": "m", "event": "push", "change_key": 7,
                }),
                "field \"change_key\" is not a string",
            ),
        ];
        for (body, want) in cases {
            let err = parse_assignment(&body.to_string()).expect_err("must refuse");
            assert_eq!(&err, want);
        }
        assert!(parse_assignment("{")
            .expect_err("must refuse")
            .starts_with("job document is not JSON:"));
    }

    /// The spec half, refused field by field. Built on a valid identity
    /// half so the failure under test is the only one that can fire.
    fn spec_err(extra: serde_json::Value) -> String {
        let mut v: serde_json::Value = serde_json::from_str(&doc()).unwrap();
        for (k, val) in extra.as_object().unwrap() {
            v[k.as_str()] = val.clone();
        }
        parse_assignment(&v.to_string()).expect_err("must refuse")
    }

    #[test]
    fn a_spec_that_cannot_be_run_is_refused_by_field() {
        assert_eq!(
            spec_err(serde_json::json!({"steps": "cargo test"})),
            "field \"steps\" is missing or not an array"
        );
        assert_eq!(
            spec_err(serde_json::json!({"steps": [{"name": "x"}]})),
            "step 0 has no \"run\" string"
        );
        assert_eq!(
            spec_err(serde_json::json!({"timeout_minutes": 99999999999u64})),
            "field \"timeout_minutes\" is out of range: 99999999999"
        );
        assert_eq!(
            spec_err(serde_json::json!({"env": {"K": 1}})),
            "env.K is not a string"
        );
        assert_eq!(
            spec_err(serde_json::json!({"matrix": []})),
            "field \"matrix\" is not an object"
        );
        assert_eq!(
            spec_err(serde_json::json!({"steps": [{"run": "x", "env": {"K": null}}]})),
            "env.K is not a string"
        );
        // A non-string image is not a refusal: it defaults. The runner
        // never acts on the image — a job runs on whichever machine
        // claimed it — so there is nothing here for it to refuse.
        let a = parse_assignment(
            &{
                let mut v: serde_json::Value = serde_json::from_str(&doc()).unwrap();
                v["image"] = serde_json::json!(3);
                v["timeout_minutes"] = serde_json::json!("soon");
                v
            }
            .to_string(),
        )
        .expect("parse");
        assert_eq!(a.spec.image, "default");
        assert_eq!(a.spec.timeout_minutes, 360);
    }

    #[test]
    fn a_step_sees_the_job_facts_and_the_layers_in_order() {
        let a = parse_assignment(&doc()).unwrap();
        let inherited = BTreeMap::from([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("WORKFLOW".to_string(), "inherited".to_string()),
        ]);
        let env = step_env(&inherited, &a, &a.spec.steps[0], None);
        assert_eq!(env["PATH"], "/usr/bin");
        assert_eq!(env["CI"], "true");
        assert_eq!(env["WEFT_CI"], "true");
        assert_eq!(env["WEFT_JOB"], "test (rust=1.80)");
        assert_eq!(env["WEFT_SHA"], "abc123");
        assert_eq!(env["WEFT_REF"], "main");
        assert_eq!(env["WEFT_EVENT"], "push");
        assert!(!env.contains_key("WEFT_CHANGE"));
        // The workflow's env beats what the runner happened to inherit.
        assert_eq!(env["WORKFLOW"], "yes");
        assert_eq!(env["STEP"], "yes");
        // A matrix key that is not a legal variable name is made into one.
        assert_eq!(env["WEFT_MATRIX_RUST_TOOLCHAIN"], "1.80");
    }

    /// The job facts are a published contract — a repository's `ci.sh`
    /// reads `$WEFT_SHA` — so the old `STRATUM_` spelling must not come
    /// back beside the new one, and it must not come back alone either.
    #[test]
    fn the_job_facts_carry_the_product_name_and_not_the_old_one() {
        let mut v: serde_json::Value = serde_json::from_str(&doc()).unwrap();
        v["change_key"] = serde_json::json!("I1234");
        let a = parse_assignment(&v.to_string()).unwrap();
        let env = step_env(&BTreeMap::new(), &a, &a.spec.steps[0], None);
        let ours: Vec<&String> = env.keys().filter(|k| k.starts_with("WEFT_")).collect();
        assert_eq!(
            ours,
            [
                "WEFT_CHANGE",
                "WEFT_CI",
                "WEFT_EVENT",
                "WEFT_JOB",
                "WEFT_MATRIX_RUST_TOOLCHAIN",
                "WEFT_REF",
                "WEFT_SHA",
            ]
        );
        assert!(
            env.keys().all(|k| !k.starts_with("STRATUM_")),
            "old-name key leaked into the job environment: {env:?}"
        );
    }

    #[test]
    fn a_step_env_beats_the_workflow_env_and_a_change_sets_its_key() {
        let mut v: serde_json::Value = serde_json::from_str(&doc()).unwrap();
        v["change_key"] = serde_json::json!("I1234");
        v["steps"] = serde_json::json!([{"run": "x", "env": {"WORKFLOW": "step wins"}}]);
        let a = parse_assignment(&v.to_string()).unwrap();
        let env = step_env(&BTreeMap::new(), &a, &a.spec.steps[0], None);
        assert_eq!(env["WORKFLOW"], "step wins");
        assert_eq!(env["WEFT_CHANGE"], "I1234");
    }

    /// A composed document: the job is `api`'s, and it carries `web` as a
    /// sibling with a token of its own.
    pub(crate) fn composed_doc() -> serde_json::Value {
        let mut v: serde_json::Value = serde_json::from_str(&doc()).unwrap();
        v["event"] = serde_json::json!("changeset");
        v["change_key"] = serde_json::json!("Iaa000001");
        v["changeset"] = serde_json::json!({
            "key": "Ic5000001",
            "members": [
                {"repo": "api", "change": "Iaa000001",
                 "clone_url": "https://stratum.example/acme/api.git",
                 "fetch_ref": "refs/patchsets/aaa", "commit_sha": "aaa",
                 "token": serde_json::Value::Null},
                {"repo": "web", "change": "Ibb000001",
                 "clone_url": "https://stratum.example/acme/web.git",
                 "fetch_ref": "refs/patchsets/bbb", "commit_sha": "bbb",
                 "token": "member-s3cret"},
            ],
        });
        v
    }

    #[test]
    fn a_composed_document_carries_its_members_in_order() {
        let a = parse_assignment(&composed_doc().to_string()).expect("parse");
        let cs = a.changeset.as_ref().expect("a changeset");
        assert_eq!(cs.key, "Ic5000001");
        assert_eq!(
            cs.members
                .iter()
                .map(|m| m.repo.as_str())
                .collect::<Vec<_>>(),
            ["api", "web"]
        );
        assert_eq!(cs.members[1].change, "Ibb000001");
        assert_eq!(
            cs.members[1].clone_url,
            "https://stratum.example/acme/web.git"
        );
        assert_eq!(cs.members[1].fetch_ref, "refs/patchsets/bbb");
        assert_eq!(cs.members[1].commit_sha, "bbb");
        assert_eq!(cs.members[1].token.as_deref(), Some("member-s3cret"));
        // The tokenless member is the job's own, and is what the steps
        // will run in.
        assert_eq!(cs.own().repo, "api");
        assert_eq!(cs.own().token, None);
        // And printing the assignment — which the runner does — does not
        // print a member's credential.
        let shown = format!("{a:?}");
        assert!(!shown.contains("member-s3cret"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    #[test]
    fn a_changeset_that_cannot_be_materialised_is_refused_by_field() {
        let with = |cs: serde_json::Value| {
            let mut v = composed_doc();
            v["changeset"] = cs;
            parse_assignment(&v.to_string()).expect_err("must refuse")
        };
        assert_eq!(
            with(serde_json::json!("Ic5")),
            "field \"changeset\" is not an object"
        );
        assert_eq!(
            with(serde_json::json!({"key": "Ic5"})),
            "field \"changeset.members\" is missing or not an array"
        );
        assert_eq!(
            with(serde_json::json!({"key": "Ic5", "members": [{"change": "I1"}]})),
            "field \"changeset.members[0].repo\" is missing or not a string"
        );
        assert_eq!(
            with(serde_json::json!({"key": "Ic5", "members": [
                {"repo": "api", "change": "I1", "clone_url": "u", "fetch_ref": "f",
                 "commit_sha": "s", "token": null},
                {"repo": "web", "change": "I2", "clone_url": "u", "fetch_ref": "f"}]})),
            "field \"changeset.members[1].commit_sha\" is missing or not a string"
        );
        assert_eq!(
            with(serde_json::json!({"key": "Ic5", "members": [
                {"repo": "api", "change": "I1", "clone_url": "u", "fetch_ref": "f",
                 "commit_sha": "s", "token": 7}]})),
            "field \"changeset.members[0].token\" is not a string"
        );
        // A member name is a directory name, and nothing else: these
        // would each escape the workspace when joined onto its root.
        for bad in ["../etc", "a/b", "..", ".", "", "a\\b"] {
            assert_eq!(
                with(serde_json::json!({"key": "Ic5", "members": [
                    {"repo": bad, "change": "I1", "clone_url": "u", "fetch_ref": "f",
                     "commit_sha": "s", "token": null}]})),
                format!("field \"changeset.members[0].repo\" is not a directory name: {bad}")
            );
        }
        // Exactly one member is the job's own. None would leave the steps
        // with no tree to run in; two would make the choice arbitrary.
        let member = |repo: &str, token: serde_json::Value| {
            serde_json::json!({"repo": repo, "change": "I1", "clone_url": "u",
                               "fetch_ref": "f", "commit_sha": "s", "token": token})
        };
        assert_eq!(
            with(
                serde_json::json!({"key": "Ic5", "members": [member("api", serde_json::json!("t"))]})
            ),
            "changeset.members must hold exactly one member without a token, found 0"
        );
        assert_eq!(
            with(serde_json::json!({"key": "Ic5", "members": []})),
            "changeset.members must hold exactly one member without a token, found 0"
        );
        assert_eq!(
            with(serde_json::json!({"key": "Ic5", "members": [
                member("api", serde_json::Value::Null), member("web", serde_json::Value::Null)]})),
            "changeset.members must hold exactly one member without a token, found 2"
        );
        assert_eq!(
            with(serde_json::json!({"members": [member("api", serde_json::Value::Null)]})),
            "field \"changeset.key\" is missing or not a string"
        );
    }

    #[test]
    fn a_composed_step_is_told_where_the_workspace_is_and_never_a_token() {
        let a = parse_assignment(&composed_doc().to_string()).unwrap();
        let root = Path::new("/work/workspace");
        let env = step_env(&BTreeMap::new(), &a, &a.spec.steps[0], Some(root));
        assert_eq!(env["WEFT_EVENT"], "changeset");
        // The own member's change, exactly as an uncomposed change run.
        assert_eq!(env["WEFT_CHANGE"], "Iaa000001");
        assert_eq!(env["WEFT_WORKSPACE"], "/work/workspace");
        assert_eq!(env["WEFT_CHANGESET"], "Ic5000001");
        let members: serde_json::Value =
            serde_json::from_str(&env["WEFT_CHANGESET_MEMBERS"]).expect("JSON");
        assert_eq!(
            members,
            serde_json::json!([
                {"repo": "api", "change": "Iaa000001", "commit": "aaa",
                 "path": "/work/workspace/api"},
                {"repo": "web", "change": "Ibb000001", "commit": "bbb",
                 "path": "/work/workspace/web"},
            ])
        );
        // The three composed variables and not a fourth.
        let composed: Vec<&str> = env
            .keys()
            .filter(|k| k.starts_with("WEFT_WORKSPACE") || k.starts_with("WEFT_CHANGESET"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            composed,
            ["WEFT_CHANGESET", "WEFT_CHANGESET_MEMBERS", "WEFT_WORKSPACE"]
        );
        // The property the per-member tokens exist for: a member's shell
        // must not be able to read the credential that fetched a sibling.
        assert!(
            !env.iter()
                .any(|(k, v)| k.contains("member-s3cret") || v.contains("member-s3cret")),
            "{env:?}"
        );
    }

    #[test]
    fn an_uncomposed_job_gets_none_of_the_composed_variables() {
        // Belt and braces: the workspace is offered, but the document is
        // not a composed one, so there is nothing to say about it.
        let a = parse_assignment(&doc()).unwrap();
        let env = step_env(
            &BTreeMap::new(),
            &a,
            &a.spec.steps[0],
            Some(Path::new("/work/ws")),
        );
        for k in ["WEFT_WORKSPACE", "WEFT_CHANGESET", "WEFT_CHANGESET_MEMBERS"] {
            assert!(!env.contains_key(k), "{k} in {env:?}");
        }
    }

    #[test]
    fn the_inherited_set_is_the_allowlist_and_nothing_else() {
        // Whatever this process happens to hold, only the three names can
        // come through — the job token above all.
        let env = inherited_env();
        assert!(env
            .keys()
            .all(|k| ["PATH", "HOME", "LANG"].contains(&k.as_str())));
        assert!(env.contains_key("PATH"), "the test process always has PATH");
    }
}
