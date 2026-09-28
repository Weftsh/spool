//! The parsed shape of a workflow. Data only.
//!
//! Deliberately not `serde::Deserialize`. A derived deserializer would
//! make the *absence* of a field the only error it can report, and this
//! subset's whole job is to refuse fields it does not implement — which
//! a derive cannot see, because an unknown key is either ignored or
//! rejected with a message naming a Rust type. `parse` walks the
//! document itself so every refusal can name the key, the line, and what
//! to do instead.

use std::collections::BTreeMap;

/// What makes a workflow run.
///
/// Three, and not GitHub's forty. `push` is the one every project wants;
/// `change` is this forge's word for what GitHub calls a pull request,
/// and naming it after our own object rather than theirs is the same
/// choice the Checks tab made. `changeset` is the one GitHub has no word
/// for at all: the composed run over every member of a changeset, with
/// all of them checked out side by side.
///
/// A file may ask for several. `on: [change, changeset]` is the ordinary
/// combination — the same suite run against the change alone and against
/// the changeset it belongs to — and the two produce separate runs whose
/// verdicts go to separate places, which is why `mirror` keeps the
/// composed one out of `check_runs`.
///
/// A workflow with no trigger it recognises is refused rather than
/// treated as manual-only: a file that will never run, sitting in
/// `.weft/`, is indistinguishable from one that is about to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    /// A commit reaching a branch.
    Push,
    /// A change opened, or a new patchset pushed to one.
    Change,
    /// A changeset composed, recomposed, or a member re-pushed: the run
    /// that sees every member's tip at once.
    Changeset,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Push => "push",
            Trigger::Change => "change",
            Trigger::Changeset => "changeset",
        }
    }

    /// Actions' own spellings map onto ours where they mean the same
    /// thing, so a pasted workflow triggered on `pull_request` runs on
    /// changes rather than being refused for a word.
    pub fn parse(s: &str) -> Option<Trigger> {
        match s {
            "push" => Some(Trigger::Push),
            "change" | "pull_request" => Some(Trigger::Change),
            "changeset" => Some(Trigger::Changeset),
            _ => None,
        }
    }
}

/// One command, and what to call it while it runs.
///
/// `run` only. A step that `uses:` an action is refused by the parser —
/// see `mod`'s note on why silence is the dangerous option — so by the
/// time a step exists here it is a shell command and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What the log calls it. Absent means the runner shows the command.
    pub name: Option<String>,
    pub run: String,
    /// Step environment, layered over the job's.
    pub env: BTreeMap<String, String>,
}

/// The strategy for expanding one job definition into several.
///
/// Held as parsed values rather than expanded here: expansion is
/// `plan`'s job, and keeping the two apart means the rules can be tested
/// against a matrix nobody has to build a `Job` around.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matrix {
    /// The dimensions, in the order the file wrote them. Order is
    /// load-bearing: it decides the order the expanded jobs come out in,
    /// and a reader comparing two runs should see the cells in the same
    /// places both times.
    pub axes: Vec<(String, Vec<String>)>,
    /// Extra combinations, or extra keys on existing ones.
    pub include: Vec<BTreeMap<String, String>>,
    /// Combinations to drop.
    pub exclude: Vec<BTreeMap<String, String>>,
}

impl Matrix {
    /// Whether this matrix says anything at all. An empty `strategy:`
    /// block is one job, not zero.
    pub fn is_empty(&self) -> bool {
        self.axes.is_empty() && self.include.is_empty()
    }
}

/// One job as written — before any matrix expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The key it was written under. Also how `needs` names it.
    pub id: String,
    /// A human name for the check row. Defaults to the id.
    pub name: Option<String>,
    /// Jobs that must finish first, by id. Order preserved for the sake
    /// of readable errors; duplicates are refused at parse time.
    pub needs: Vec<String>,
    /// The image the steps run in.
    pub image: Option<String>,
    pub env: BTreeMap<String, String>,
    pub matrix: Matrix,
    /// Wall-clock bound for the whole job.
    pub timeout_minutes: Option<u32>,
    /// The `runs-on` list, lowercased and deduped, **in file order**,
    /// always holding `self-hosted`: `[self-hosted]` when the file said
    /// nothing.
    ///
    /// File order rather than sorted because it is what the refusal
    /// sentence prints back — "no runner with labels [self-hosted, gpu]
    /// is registered" has to be recognisable as the line the author
    pub labels: Vec<String>,
    pub steps: Vec<Step>,
}

/// A whole `.weft/*.yml` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    /// What the run is called. Defaults to the file's stem.
    pub name: String,
    /// Deduplicated and sorted, so `on: [push, push]` is one trigger and
    /// two files listing the same pair compare equal.
    pub on: Vec<Trigger>,
    /// In file order, which is the order `plan` reports them in when
    /// nothing else decides.
    pub jobs: Vec<Job>,
}

impl Workflow {
    pub fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }
}

/// Why a workflow was refused, in the words its author needs.
///
/// The line is carried because a refusal that says only "`uses:` is not
/// supported" sends somebody scrolling a 200-line file for it. `key` is
/// the thing that was wrong; `hint` is what to do instead, and is where
/// the useful half of the message lives — "not supported" tells a reader
/// they are stuck, and "not supported; run the command directly with
/// `run:`" tells them they are not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// 1-based, as an editor counts. `None` when the problem is the
    /// document as a whole rather than a place in it.
    pub line: Option<usize>,
    /// A dotted path to what was refused, e.g. `jobs.test.steps[2].uses`.
    pub key: String,
    pub message: String,
    /// What to do instead. Empty when there is genuinely nothing.
    pub hint: String,
}

impl Refusal {
    pub fn at(line: Option<usize>, key: impl Into<String>, message: impl Into<String>) -> Refusal {
        Refusal {
            line,
            key: key.into(),
            message: message.into(),
            hint: String::new(),
        }
    }

    pub fn hint(mut self, h: impl Into<String>) -> Refusal {
        self.hint = h.into();
        self
    }

    /// One line, the way it is shown in a check summary and a log.
    pub fn render(&self, file: &str) -> String {
        let mut s = match self.line {
            Some(n) => format!("{file}:{n}: {}", self.message),
            None => format!("{file}: {}", self.message),
        };
        if !self.hint.is_empty() {
            s.push_str(" — ");
            s.push_str(&self.hint);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_accept_both_spellings_and_refuse_the_rest() {
        assert_eq!(Trigger::parse("push"), Some(Trigger::Push));
        assert_eq!(Trigger::parse("change"), Some(Trigger::Change));
        // A pasted Actions workflow says `pull_request`, and refusing it
        // over a word would be a refusal about nothing.
        assert_eq!(Trigger::parse("pull_request"), Some(Trigger::Change));
        // Ones we genuinely do not have. `schedule` is the interesting
        // refusal: we announce a push, we do not own a timer.
        assert_eq!(Trigger::parse("schedule"), None);
        assert_eq!(Trigger::parse("workflow_dispatch"), None);
        assert_eq!(Trigger::parse(""), None);
    }

    #[test]
    fn every_trigger_prints_as_the_word_it_parses_from() {
        // `as_str` is what the workflows listing shows a person; a
        // variant that printed something `parse` would refuse would list
        // a file as asking for an event nobody can write.
        for t in [Trigger::Push, Trigger::Change, Trigger::Changeset] {
            assert_eq!(Trigger::parse(t.as_str()), Some(t), "{}", t.as_str());
        }
        assert_eq!(Trigger::Changeset.as_str(), "changeset");
    }

    #[test]
    fn a_refusal_reads_as_an_editor_would_point_at_it() {
        let r = Refusal::at(
            Some(14),
            "jobs.test.steps[2].uses",
            "`uses:` is not supported",
        )
        .hint("run the command directly with `run:`");
        assert_eq!(
            r.render(".weft/ci.yml"),
            ".weft/ci.yml:14: `uses:` is not supported — run the command \
             directly with `run:`"
        );
        // A whole-document problem has no line to point at, and inventing
        // line 1 would send a reader to a line that is fine.
        let whole = Refusal::at(None, "jobs", "a workflow needs at least one job");
        assert_eq!(
            whole.render(".weft/ci.yml"),
            ".weft/ci.yml: a workflow needs at least one job"
        );
    }

    #[test]
    fn an_empty_strategy_block_is_one_job_not_none() {
        assert!(Matrix::default().is_empty());
        let axes = Matrix {
            axes: vec![("os".into(), vec!["linux".into()])],
            ..Default::default()
        };
        assert!(!axes.is_empty());
        // `include` alone is a matrix too — it is how a job says "these
        // three specific combinations and nothing else".
        let only_include = Matrix {
            include: vec![BTreeMap::from([("os".to_string(), "linux".to_string())])],
            ..Default::default()
        };
        assert!(!only_include.is_empty());
    }
}
