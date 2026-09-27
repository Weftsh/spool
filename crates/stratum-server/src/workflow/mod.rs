//! `.weft` workflows: what a repository asks to have run.
//!
//! Layering, kept strict for the same reason `review` keeps it — the
//! parts that decide things are pure, so they can be argued with in a
//! unit test rather than against a container:
//!
//! - `model` — the parsed shape. Data, no behaviour, no I/O.
//! - `parse` — YAML → [`model::Workflow`], or a refusal naming the key
//!   and the line. Pure.
//! - `plan` — a workflow → the concrete jobs it expands to, with the
//!   dependency edges between them. Pure.
//!
//! - `logs` — where a build log lives in the object store, and how the
//!   chunks a running job uploads are put back together.
//! - `mirror` — a job's state written into `check_runs`, which is what
//!   makes it visible to the Checks tab and the land gate with no UI
//!   work at all.
//!
//! The first three decide things and touch nothing. The last two are
//! the seam to the store and the control plane, and they are kept
//! separate from the routes that call them so that both can be tested
//! against a real bucket and a real Postgres without a server.
//!
//! ## Why this refuses so much
//!
//! The YAML is an **Actions-compatible subset**, and the subset is
//! enforced by refusing every key it does not implement, at parse time,
//! naming the key and the line.
//!
//! The alternative — skip what we do not understand and run the rest —
//! is the single worst failure this feature can have. A workflow whose
//! `uses: actions/checkout` step was quietly dropped runs against an
//! empty directory; one whose security-scan step was dropped reports a
//! green check for a build that never scanned anything. Both are
//! *silent*, both look exactly like success, and the check they produce
//! is then used to gate a merge.
//!
//! So the parser is deliberately hostile to its input, and the refusal
//! is the most-read piece of copy in the feature: somebody pasting an
//! Actions workflow will meet it before they meet anything else.

pub mod credentials;
pub mod logs;
pub mod mirror;
pub mod model;
pub mod parse;
pub mod plan;
pub mod read;
pub mod trigger;
