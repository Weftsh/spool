//! A workflow → the concrete jobs it expands to, and the edges between
//! them. Pure: no I/O, no clock, no database.
//!
//! Two things happen here, and they are the two places a workflow stops
//! being a document and starts being work:
//!
//! - **matrix expansion**, which turns one written job into N,
//! - **the dependency graph**, which decides what may start when.
//!
//! Both follow GitHub's documented semantics, because the YAML claims to
//! be Actions-compatible and a matrix that expanded differently here
//! would be the worst kind of incompatibility: silent, and only visible
//! as "the wrong cells ran".
//!
//! Where GitHub's documentation is *silent* rather than different — and
//! there are four such places, found by reading the docs rather than
//! assuming — this module picks a rule and says so in a comment. It
//! never guesses at theirs.

use super::model::{Job, Matrix, Refusal, Workflow};
use std::collections::{BTreeMap, BTreeSet};

/// The most jobs one workflow run may expand to.
///
/// GitHub's documented limit, and adopted at the same number so a
/// workflow that fits there fits here. It is load-bearing for a second
/// reason they do not have to care about as much: a matrix is a cheap
/// way to spend somebody else's compute, and `axes: [a; 10]` of ten
/// values each is 10,000,000,000 cells written in six lines. The cap is
/// checked *during* expansion rather than after, so the refusal costs a
/// bounded amount of work rather than materialising the bomb first.
pub const MAX_CELLS: usize = 256;

/// One expanded job: a cell of the matrix, or the whole job when there
/// is no matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedJob {
    /// The written job's id. Several cells share it.
    pub job_id: String,
    /// Unique across the plan. The job id for a matrix-less job, and
    /// `id (v1, v2)` for a cell — GitHub's shape, because this string
    /// ends up as a check name and people already know how to read it.
    pub key: String,
    /// The cell's variable bindings. Empty for a matrix-less job.
    pub matrix: BTreeMap<String, String>,
    /// Indices into the plan's `jobs`, resolved from the written
    /// `needs`. **A cell depends on every cell of each needed job**: a
    /// job that needs `build` waits for all of `build`'s cells, which is
    /// what `needs` means when the needed job is a matrix. Pairing cells
    /// up by matching matrix values would be a different feature, and
    /// one GitHub does not have.
    pub needs: Vec<usize>,
}

/// A workflow, expanded and ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub jobs: Vec<PlannedJob>,
    /// A topological order over `jobs` — every job appears after
    /// everything it needs. Ties keep file order, so two runs of the
    /// same workflow list their jobs the same way.
    pub order: Vec<usize>,
}

/// Every combination this matrix describes, in GitHub's documented
/// order, after `exclude` and then `include`.
///
/// The order of operations is the part worth getting right and the part
/// nobody expects: **`exclude` is applied first, and `include` after**,
/// so an `include` entry can put back a combination an `exclude` just
/// removed. Doing it the intuitive way round — filter last — silently
/// drops cells the author asked for.
pub fn expand(matrix: &Matrix, job_id: &str) -> Result<Vec<BTreeMap<String, String>>, Refusal> {
    // 1. The base product, first-declared axis outermost. GitHub: "the
    //    first variable you define will be the first job that is
    //    created", i.e. it varies slowest.
    let mut cells: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
    for (name, values) in &matrix.axes {
        if values.is_empty() {
            return Err(Refusal::at(
                None,
                format!("jobs.{job_id}.strategy.matrix.{name}"),
                format!("matrix axis {name:?} has no values, so this job expands to nothing"),
            )
            .hint("give it at least one value, or remove the axis"));
        }
        // Checked here rather than after the whole product: this is what
        // keeps a ten-axis bomb from being built before it is refused.
        if cells.len().saturating_mul(values.len()) > MAX_CELLS {
            return Err(too_many(job_id));
        }
        let mut next = Vec::with_capacity(cells.len() * values.len());
        for cell in &cells {
            for v in values {
                let mut c = cell.clone();
                c.insert(name.clone(), v.clone());
                next.push(c);
            }
        }
        cells = next;
    }
    // A matrix with only `include` starts from nothing, not from one
    // empty cell: GitHub says "if you don't specify any matrix
    // variables, all configurations under include will run", so the
    // empty cell must not survive to become a spurious extra job.
    if matrix.axes.is_empty() && !matrix.include.is_empty() {
        cells.clear();
    }

    // The original axis names. `include` may add keys and may not
    // overwrite these, and that distinction is the whole of its rule.
    let original: BTreeSet<&str> = matrix.axes.iter().map(|(k, _)| k.as_str()).collect();

    // 2. exclude — a partial match is enough. An entry naming two of
    //    three axes removes every cell agreeing on those two.
    if !matrix.exclude.is_empty() {
        cells.retain(|cell| {
            !matrix
                .exclude
                .iter()
                .any(|ex| ex.iter().all(|(k, v)| cell.get(k) == Some(v)))
        });
    }

    // 3. include — merged where it does not overwrite an original
    //    value, appended as a new cell where it cannot be merged
    //    anywhere.
    //
    // **Only the cells the product made are merge targets.** Two rules
    // depend on this and both are wrong without it:
    //
    //   * With no axes at all, `original` is empty, so *every* entry
    //     vacuously "does not overwrite an original value" and would
    //     merge into whatever cell the previous entry appended —
    //     collapsing `include: [{os: linux}, {os: mac}]` to one job
    //     running on mac. GitHub is explicit that with no matrix
    //     variables every include runs as its own job.
    //   * A cell that an earlier include appended must not absorb a
    //     later one; new combinations do not cascade the way original
    //     ones do.
    //
    // Includes only ever append, so indices below this line stay put.
    let original_cells = cells.len();
    for entry in &matrix.include {
        let mergeable: Vec<usize> = cells
            .iter()
            .enumerate()
            .take(original_cells)
            .filter(|(_, cell)| {
                entry
                    .iter()
                    .filter(|(k, _)| original.contains(k.as_str()))
                    .all(|(k, v)| cell.get(k) == Some(v))
            })
            .map(|(i, _)| i)
            .collect();
        if mergeable.is_empty() {
            if cells.len() + 1 > MAX_CELLS {
                return Err(too_many(job_id));
            }
            cells.push(entry.clone());
            continue;
        }
        for i in mergeable {
            for (k, v) in entry {
                // Original values are never overwritten; values a
                // previous include added may be. GitHub states both, and
                // states no tie-break for two includes touching one
                // synthesised key — so **list order wins, last write
                // standing**, which is ours to choose and is written
                // down here because nobody can look it up.
                if original.contains(k.as_str()) {
                    continue;
                }
                cells[i].insert(k.clone(), v.clone());
            }
        }
    }

    // No trailing cap check: every path that grows `cells` — the product
    // above and each `include` below — checks *before* it grows, which
    // is what keeps a bomb from being built in order to be refused. A
    // check here could only ever agree with one already made.
    Ok(cells)
}

fn too_many(job_id: &str) -> Refusal {
    Refusal::at(
        None,
        format!("jobs.{job_id}.strategy.matrix"),
        format!("this matrix expands to more than {MAX_CELLS} jobs"),
    )
    .hint("narrow the matrix, or split the job in two")
}

/// The name a cell is known by. `id (a, b)` in axis order, which is what
/// GitHub prints and therefore what people already know how to scan.
///
/// Values only, not `k=v`: a reader of `test (linux, 1.83)` knows what
/// they are looking at from the axis order they wrote, and the longer
/// form makes a check list unreadable at four axes.
fn cell_key(job_id: &str, matrix: &Matrix, cell: &BTreeMap<String, String>) -> String {
    if cell.is_empty() {
        return job_id.to_string();
    }
    // Axis order first, then any include-only keys, so the common case
    // reads in the order the file declared and the exotic one is still
    // deterministic rather than hash-ordered.
    let mut parts: Vec<&str> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (name, _) in &matrix.axes {
        if let Some(v) = cell.get(name) {
            parts.push(v.as_str());
            seen.insert(name.as_str());
        }
    }
    for (k, v) in cell {
        if !seen.contains(k.as_str()) {
            parts.push(v.as_str());
        }
    }
    format!("{job_id} ({})", parts.join(", "))
}

/// Expand every job and resolve the graph, or say what is wrong with it.
///
/// Returns **all** the refusals it can find rather than the first: a
/// workflow with three unknown `needs` should report three, so its
/// author fixes them in one pass instead of three pushes.
pub fn plan(wf: &Workflow) -> Result<Plan, Vec<Refusal>> {
    let mut refusals: Vec<Refusal> = Vec::new();
    let mut jobs: Vec<PlannedJob> = Vec::new();
    // Which planned indices belong to each written job, for resolving
    // `needs` once every cell exists.
    let mut by_job: BTreeMap<&str, Vec<usize>> = BTreeMap::new();

    let mut total = 0usize;
    for job in &wf.jobs {
        let cells = if job.matrix.is_empty() {
            vec![BTreeMap::new()]
        } else {
            match expand(&job.matrix, &job.id) {
                Ok(c) => c,
                Err(r) => {
                    refusals.push(r);
                    continue;
                }
            }
        };
        // The cap is per workflow run, not per job — otherwise four jobs
        // of 250 cells each slip through a per-job check and the run is
        // a thousand containers.
        total += cells.len();
        if total > MAX_CELLS {
            refusals.push(
                Refusal::at(
                    None,
                    "jobs",
                    format!("this workflow expands to more than {MAX_CELLS} jobs in total"),
                )
                .hint("narrow a matrix, or split the workflow in two"),
            );
            return Err(refusals);
        }
        for cell in cells {
            let idx = jobs.len();
            by_job.entry(job.id.as_str()).or_default().push(idx);
            jobs.push(PlannedJob {
                job_id: job.id.clone(),
                key: cell_key(&job.id, &job.matrix, &cell),
                matrix: cell,
                needs: Vec::new(),
            });
        }
    }

    // Edges, once every cell exists. A cell needs every cell of each
    // needed job.
    for job in &wf.jobs {
        for need in &job.needs {
            if wf.job(need).is_none() {
                // GitHub's docs do not say what happens here — the
                // research went looking and found nothing — so this is
                // our rule, and it is the loud one. A `needs` naming a
                // job that does not exist is almost always a typo, and
                // the alternatives are both bad: ignoring it runs a job
                // its author believed was gated, and hanging forever
                // looks like a stuck queue.
                refusals.push(
                    Refusal::at(
                        None,
                        format!("jobs.{}.needs", job.id),
                        format!("job {need:?} does not exist in this workflow"),
                    )
                    .hint(format!(
                        "the jobs here are: {}",
                        wf.jobs
                            .iter()
                            .map(|j| j.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
                continue;
            }
            let Some(targets) = by_job.get(need.as_str()) else {
                // The needed job exists but expanded to nothing, which
                // means its own matrix was refused above. Its refusal is
                // the real one; adding a second about this edge would
                // bury it.
                continue;
            };
            let targets = targets.clone();
            if let Some(mine) = by_job.get(job.id.as_str()) {
                for &i in mine {
                    jobs[i].needs.extend(targets.iter().copied());
                }
            }
        }
    }

    if !refusals.is_empty() {
        return Err(refusals);
    }

    match toposort(&jobs) {
        Some(order) => Ok(Plan { jobs, order }),
        None => Err(vec![Refusal::at(
            None,
            "jobs",
            "these jobs need each other in a cycle, so none of them could ever start",
        )
        .hint(cycle_hint(wf))]),
    }
}

/// Kahn's algorithm, taking ready jobs in index order so ties break as
/// the file wrote them. `None` when a cycle leaves work unreachable.
fn toposort(jobs: &[PlannedJob]) -> Option<Vec<usize>> {
    let mut indegree: Vec<usize> = jobs.iter().map(|j| j.needs.len()).collect();
    let mut order: Vec<usize> = Vec::with_capacity(jobs.len());
    let mut ready: Vec<usize> = (0..jobs.len()).filter(|&i| indegree[i] == 0).collect();
    while let Some(i) = ready.first().copied() {
        ready.remove(0);
        order.push(i);
        for (j, job) in jobs.iter().enumerate() {
            if job.needs.contains(&i) {
                indegree[j] -= 1;
                if indegree[j] == 0 {
                    // Insert in index order rather than pushing, so the
                    // whole order is stable and not merely correct.
                    let at = ready.partition_point(|&r| r < j);
                    ready.insert(at, j);
                }
            }
        }
    }
    (order.len() == jobs.len()).then_some(order)
}

/// The jobs caught in the cycle, named. A cycle error that says only
/// "there is a cycle" leaves the author to find it by eye, and the
/// workflows where this happens are the big ones.
fn cycle_hint(wf: &Workflow) -> String {
    let mut remaining: Vec<&Job> = wf.jobs.iter().collect();
    // Repeatedly drop jobs whose needs are all outside the remaining
    // set; what will not drop is the cycle and everything feeding it.
    loop {
        let before = remaining.len();
        let live: BTreeSet<&str> = remaining.iter().map(|j| j.id.as_str()).collect();
        remaining.retain(|j| j.needs.iter().any(|n| live.contains(n.as_str())));
        if remaining.len() == before {
            break;
        }
    }
    // No empty case: this is only reached when the toposort failed, a
    // toposort only fails on a cycle, and a cycle is exactly what does
    // not peel away — so `remaining` always names somebody.
    format!(
        "these jobs are involved: {}",
        remaining
            .iter()
            .map(|j| j.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{Step, Trigger};

    fn axis(name: &str, vals: &[&str]) -> (String, Vec<String>) {
        (
            name.to_string(),
            vals.iter().map(|s| s.to_string()).collect(),
        )
    }

    fn entry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn job(id: &str, needs: &[&str], matrix: Matrix) -> Job {
        Job {
            id: id.to_string(),
            name: None,
            needs: needs.iter().map(|s| s.to_string()).collect(),
            image: None,
            env: BTreeMap::new(),
            matrix,
            timeout_minutes: None,
            labels: vec!["self-hosted".to_string()],
            steps: vec![Step {
                name: None,
                run: "true".into(),
                env: BTreeMap::new(),
            }],
        }
    }

    fn wf(jobs: Vec<Job>) -> Workflow {
        Workflow {
            name: "ci".into(),
            on: vec![Trigger::Push],
            jobs,
        }
    }

    /// The order is documented and people rely on it: the first
    /// variable declared varies slowest, so a reader scanning a check
    /// list sees the first axis grouped.
    #[test]
    fn the_first_axis_declared_varies_slowest() {
        let m = Matrix {
            axes: vec![
                axis("version", &["10", "12"]),
                axis("os", &["linux", "mac"]),
            ],
            ..Default::default()
        };
        let cells = expand(&m, "test").unwrap();
        let seen: Vec<(&str, &str)> = cells
            .iter()
            .map(|c| (c["version"].as_str(), c["os"].as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("10", "linux"),
                ("10", "mac"),
                ("12", "linux"),
                ("12", "mac"),
            ]
        );
    }

    /// An exclude entry naming fewer keys than the matrix has still
    /// matches: "an excluded configuration only has to be a partial
    /// match for it to be excluded".
    #[test]
    fn exclude_matches_partially() {
        let m = Matrix {
            axes: vec![axis("os", &["linux", "mac", "win"]), axis("v", &["1", "2"])],
            exclude: vec![entry(&[("os", "win")])],
            ..Default::default()
        };
        let cells = expand(&m, "test").unwrap();
        assert_eq!(cells.len(), 4, "both win cells go: {cells:?}");
        assert!(cells.iter().all(|c| c["os"] != "win"));
    }

    /// `include` adds keys to the combinations it does not contradict.
    #[test]
    fn include_adds_keys_to_matching_combinations() {
        let m = Matrix {
            axes: vec![axis("os", &["linux", "win"]), axis("node", &["14", "16"])],
            include: vec![entry(&[("os", "win"), ("node", "16"), ("npm", "6")])],
            ..Default::default()
        };
        let cells = expand(&m, "test").unwrap();
        assert_eq!(cells.len(), 4, "no new job, just an added key: {cells:?}");
        let win16 = cells
            .iter()
            .find(|c| c["os"] == "win" && c["node"] == "16")
            .unwrap();
        assert_eq!(win16["npm"], "6");
        // And only that one.
        assert_eq!(cells.iter().filter(|c| c.contains_key("npm")).count(), 1);
    }

    /// An include that contradicts every original combination becomes a
    /// new job rather than being dropped or overwriting one.
    #[test]
    fn include_that_matches_nothing_becomes_a_new_job() {
        let m = Matrix {
            axes: vec![axis("os", &["linux", "win"]), axis("v", &["12", "14"])],
            include: vec![entry(&[("os", "win"), ("v", "17")])],
            ..Default::default()
        };
        let cells = expand(&m, "test").unwrap();
        assert_eq!(cells.len(), 5, "{cells:?}");
        assert_eq!(
            cells.iter().filter(|c| c["v"] == "17").count(),
            1,
            "exactly one new cell"
        );
    }

    /// The counter-intuitive one, and the reason it is worth a test:
    /// **exclude runs first**, so an include can put back what an
    /// exclude removed. Filtering last would silently drop it.
    #[test]
    fn include_can_re_add_what_exclude_removed() {
        let m = Matrix {
            axes: vec![axis("os", &["linux", "win"])],
            exclude: vec![entry(&[("os", "win")])],
            include: vec![entry(&[("os", "win")])],
        };
        let cells = expand(&m, "test").unwrap();
        assert_eq!(cells.len(), 2, "win comes back: {cells:?}");
        assert!(cells.iter().any(|c| c["os"] == "win"));
    }

    /// A matrix of only `include` runs each entry, and does not also
    /// run a phantom empty cell.
    #[test]
    fn include_only_runs_each_entry_and_nothing_else() {
        let m = Matrix {
            include: vec![entry(&[("os", "linux")]), entry(&[("os", "mac")])],
            ..Default::default()
        };
        let cells = expand(&m, "test").unwrap();
        assert_eq!(cells.len(), 2, "{cells:?}");
    }

    /// An original value is never overwritten; a value a previous
    /// include added may be. The second half is our documented rule —
    /// list order, last write standing — because GitHub states the
    /// permission without stating the tie-break.
    #[test]
    fn originals_are_immutable_and_added_keys_are_not() {
        let m = Matrix {
            axes: vec![axis("os", &["linux"])],
            include: vec![
                entry(&[("os", "linux"), ("tag", "first")]),
                entry(&[("os", "linux"), ("tag", "second")]),
            ],
            ..Default::default()
        };
        let cells = expand(&m, "test").unwrap();
        assert_eq!(cells.len(), 1, "{cells:?}");
        assert_eq!(cells[0]["os"], "linux", "the original is untouched");
        assert_eq!(cells[0]["tag"], "second", "the later include wins");
    }

    /// A cell an include appended does not then absorb a later include.
    ///
    /// Only cells from the original product are merge targets. Without
    /// that rule two things break at once: this cascade, and — because
    /// a matrix with no axes has no original values to protect — every
    /// include-only entry vacuously merging into the one before it, so
    /// `[{os: linux}, {os: mac}]` collapses to a single mac job. The
    /// second is what the test below caught first.
    #[test]
    fn a_cell_added_by_include_does_not_absorb_the_next_include() {
        let m = Matrix {
            axes: vec![axis("os", &["linux"])],
            include: vec![
                // Cannot merge (os differs from every original), so it
                // is appended as its own cell.
                entry(&[("os", "win")]),
                // Also cannot merge into the original linux cell, so it
                // must become a third cell — not be folded into the win
                // one just appended.
                entry(&[("os", "bsd")]),
            ],
            ..Default::default()
        };
        let cells = expand(&m, "test").unwrap();
        let mut got: Vec<&str> = cells.iter().map(|c| c["os"].as_str()).collect();
        got.sort_unstable();
        assert_eq!(got, vec!["bsd", "linux", "win"], "{cells:?}");
    }

    /// The cap is a resource control as much as a compatibility one: a
    /// matrix bomb is six lines of YAML, and it must be refused without
    /// first being built.
    #[test]
    fn a_matrix_bomb_is_refused_and_not_materialised() {
        let big: Vec<&str> = vec!["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let m = Matrix {
            axes: (0..10).map(|i| axis(&format!("k{i}"), &big)).collect(),
            ..Default::default()
        };
        let err = expand(&m, "test").expect_err("ten axes of ten is 10^10 cells");
        assert!(err.message.contains("more than 256"), "{err:?}");
    }

    #[test]
    fn an_axis_with_no_values_is_refused_rather_than_erasing_the_job() {
        let m = Matrix {
            axes: vec![axis("os", &[])],
            ..Default::default()
        };
        let err = expand(&m, "test").expect_err("an empty axis expands to nothing");
        assert!(err.message.contains("no values"), "{err:?}");
    }

    /// A cell is named the way GitHub names it, because that string
    /// becomes a check name and people already know how to read it.
    #[test]
    fn cells_are_named_in_axis_order() {
        let m = Matrix {
            axes: vec![axis("os", &["linux"]), axis("v", &["1.83"])],
            ..Default::default()
        };
        let p = plan(&wf(vec![job("test", &[], m)])).unwrap();
        assert_eq!(p.jobs[0].key, "test (linux, 1.83)");
        // A job with no matrix keeps its own name — no empty parens.
        let p = plan(&wf(vec![job("lint", &[], Matrix::default())])).unwrap();
        assert_eq!(p.jobs[0].key, "lint");
    }

    /// A job that needs a matrix job waits for **every** cell of it.
    #[test]
    fn a_cell_depends_on_every_cell_of_what_it_needs() {
        let build = job(
            "build",
            &[],
            Matrix {
                axes: vec![axis("os", &["linux", "mac"])],
                ..Default::default()
            },
        );
        let ship = job("ship", &["build"], Matrix::default());
        let p = plan(&wf(vec![build, ship])).unwrap();
        let ship = p.jobs.iter().find(|j| j.job_id == "ship").unwrap();
        assert_eq!(ship.needs.len(), 2, "both build cells: {:?}", ship.needs);
        // And the order puts both builds before it.
        let pos = |key: &str| p.order.iter().position(|&i| p.jobs[i].key == key).unwrap();
        assert!(pos("ship") > pos("build (linux)"));
        assert!(pos("ship") > pos("build (mac)"));
    }

    /// Every unknown `needs` is reported, not just the first: three
    /// typos should cost one push, not three.
    #[test]
    fn every_unknown_need_is_named_at_once() {
        let a = job("a", &["ghost", "phantom"], Matrix::default());
        let b = job("b", &["missing"], Matrix::default());
        let errs = plan(&wf(vec![a, b])).expect_err("three unknown needs");
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert!(errs.iter().any(|e| e.message.contains("\"ghost\"")));
        assert!(errs.iter().any(|e| e.message.contains("\"phantom\"")));
        assert!(errs.iter().any(|e| e.message.contains("\"missing\"")));
        // And the hint lists what does exist, so the typo is obvious.
        assert!(errs[0].hint.contains("a, b"), "{:?}", errs[0]);
    }

    /// A cycle is refused, and the refusal names the jobs in it — the
    /// workflows where this happens are the big ones, and "there is a
    /// cycle" leaves an author to find it by eye.
    #[test]
    fn a_cycle_is_refused_and_names_the_jobs_in_it() {
        let a = job("a", &["c"], Matrix::default());
        let b = job("b", &["a"], Matrix::default());
        let c = job("c", &["b"], Matrix::default());
        // `lint` is outside the cycle and must not be blamed for it.
        let lint = job("lint", &[], Matrix::default());
        let errs = plan(&wf(vec![a, b, c, lint])).expect_err("a needs c needs b needs a");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].message.contains("cycle"), "{:?}", errs[0]);
        for id in ["a", "b", "c"] {
            assert!(errs[0].hint.contains(id), "{:?}", errs[0]);
        }
        assert!(!errs[0].hint.contains("lint"), "{:?}", errs[0]);
    }

    /// A job needing itself is the same fault and must not hang.
    #[test]
    fn a_job_that_needs_itself_is_a_cycle() {
        let errs =
            plan(&wf(vec![job("a", &["a"], Matrix::default())])).expect_err("self-dependency");
        assert!(errs[0].message.contains("cycle"), "{errs:?}");
    }

    /// The cap is per run, so several jobs cannot each sit under it and
    /// add up to a thousand containers between them.
    #[test]
    fn the_cap_is_over_the_whole_workflow_not_one_job() {
        let wide = || {
            let vals: Vec<String> = (0..100).map(|i| i.to_string()).collect();
            Matrix {
                axes: vec![("n".to_string(), vals)],
                ..Default::default()
            }
        };
        let errs = plan(&wf(vec![
            job("a", &[], wide()),
            job("b", &[], wide()),
            job("c", &[], wide()),
        ]))
        .expect_err("300 cells across three legal jobs");
        assert!(
            errs.iter().any(|e| e.message.contains("in total")),
            "{errs:?}"
        );
    }

    /// The cap catches an `include` that pushes past it, not only a
    /// product that does. A matrix of 256 legal cells plus one include
    /// that matches nothing is 257.
    #[test]
    fn an_include_cannot_push_past_the_cap() {
        let vals: Vec<String> = (0..MAX_CELLS).map(|i| i.to_string()).collect();
        let m = Matrix {
            axes: vec![("n".to_string(), vals)],
            include: vec![entry(&[("n", "beyond")])],
            ..Default::default()
        };
        let err = expand(&m, "test").expect_err("256 + 1");
        assert!(err.message.contains("more than 256"), "{err:?}");
    }

    /// A cell carrying a key no axis declared is still named
    /// deterministically — axis order first, then the rest — rather than
    /// in whatever order a hash produced.
    #[test]
    fn an_include_only_key_still_lands_in_the_name() {
        let m = Matrix {
            axes: vec![axis("os", &["linux"])],
            include: vec![entry(&[("os", "linux"), ("tag", "nightly")])],
            ..Default::default()
        };
        let p = plan(&wf(vec![job("test", &[], m)])).unwrap();
        assert_eq!(p.jobs[0].key, "test (linux, nightly)");
    }

    /// A job whose own matrix was refused does not also produce a
    /// second complaint about every edge into it — the matrix refusal is
    /// the real one and a pile of consequential errors buries it.
    #[test]
    fn a_refused_matrix_reports_once_not_once_per_dependent() {
        let bad = job(
            "build",
            &[],
            Matrix {
                axes: vec![axis("n", &[])],
                ..Default::default()
            },
        );
        let a = job("a", &["build"], Matrix::default());
        let b = job("b", &["build"], Matrix::default());
        let errs = plan(&wf(vec![bad, a, b])).expect_err("empty axis");
        assert_eq!(errs.len(), 1, "one refusal, not three: {errs:?}");
        assert!(errs[0].message.contains("no values"), "{errs:?}");
    }

    /// A job whose own matrix was refused still has its `needs` walked,
    /// and must not then try to hang edges on cells that do not exist.
    /// The refused job is the one with the dependency here, which is the
    /// mirror of the case above.
    #[test]
    fn a_refused_job_that_needs_a_good_one_adds_no_edges() {
        let good = job("build", &[], Matrix::default());
        let bad = job(
            "test",
            &["build"],
            Matrix {
                axes: vec![axis("n", &[])],
                ..Default::default()
            },
        );
        let errs = plan(&wf(vec![good, bad])).expect_err("empty axis");
        assert_eq!(errs.len(), 1, "only the matrix refusal: {errs:?}");
        assert!(errs[0].message.contains("no values"), "{errs:?}");
    }

    /// Order is stable, not merely correct: two runs of one workflow
    /// list their jobs the same way, so a reader comparing them is
    /// comparing the runs and not the scheduler's mood.
    #[test]
    fn independent_jobs_keep_file_order() {
        let p = plan(&wf(vec![
            job("lint", &[], Matrix::default()),
            job("test", &[], Matrix::default()),
            job("docs", &[], Matrix::default()),
        ]))
        .unwrap();
        let keys: Vec<&str> = p.order.iter().map(|&i| p.jobs[i].key.as_str()).collect();
        assert_eq!(keys, vec!["lint", "test", "docs"]);
    }
}
