//! The changeset-level verdict: one answer over every member, composed
//! from the answers each member already has on its own.
//!
//! A changeset is landable when every member is — its sufficiency
//! verdict at its latest patchset says yes and no required check has
//! said no — and the first member in landing order that is not carries
//! the explanation, prefixed with its `repo/change` name so a reader of
//! the changeset knows *where* to go. Nothing here re-decides anything a
//! member decided; it is pure data-in data-out so every branch is
//! unit-tested, the way `sufficiency` is.
//!
//! The check gate keeps its three answers rather than collapsing into
//! the boolean. `Blocked` is a refusal; `Waiting` is not a *review*
//! refusal — a changeset whose members are all approved and waiting on
//! CI has nothing left for a person to do but wait, and the verdict says
//! what for. Whether waiting is a reason not to land is the land
//! route's decision, not this module's: a single change is held until
//! its checks report, a changeset is refused (`changesets_api::land`),
//! because its plan is made against the trunks as they stand and a hold
//! would land a plan nobody re-checked.

use super::sufficiency::Verdict;
use stratum_control::changes::LandGate;

/// One member's own answers, in landing order.
#[derive(Debug, Clone)]
pub struct MemberVerdict {
    /// `repo/change`, the name every explanation uses.
    pub label: String,
    /// The change's state: only `open` can land.
    pub state: String,
    /// Sufficiency at the latest patchset.
    pub verdict: Verdict,
    /// The required-check gate.
    pub gate: LandGate,
}

/// The gate's three answers, over the whole changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Ready,
    Waiting,
    Blocked,
}

impl Gate {
    pub fn as_str(self) -> &'static str {
        match self {
            Gate::Ready => "ready",
            Gate::Waiting => "waiting",
            Gate::Blocked => "blocked",
        }
    }
}

/// What one member contributes to the whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberJudgement {
    pub landable: bool,
    pub explanation: String,
    pub gate: Gate,
    /// Required checks still to report, by name.
    pub waiting_on: Vec<String>,
    /// Why the gate is `Blocked`, when it is.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed {
    pub landable: bool,
    pub gate: Gate,
    pub explanation: String,
    /// One per member, in the order given.
    pub members: Vec<MemberJudgement>,
    /// `repo/change: check` for every check any member waits on.
    pub waiting_on: Vec<String>,
}

fn judge(m: &MemberVerdict) -> MemberJudgement {
    let (gate, waiting_on, reason) = match &m.gate {
        LandGate::Ready => (Gate::Ready, Vec::new(), None),
        LandGate::Waiting { on } => (Gate::Waiting, on.clone(), None),
        LandGate::Blocked { reason } => (Gate::Blocked, Vec::new(), Some(reason.clone())),
    };
    // Precedence is the order a person would fix things in: a change
    // that is not open cannot be helped by approving it; an unapproved
    // one cannot be helped by a green build.
    let (landable, explanation) = if m.state != "open" {
        (false, format!("change is {}", m.state))
    } else if !m.verdict.landable {
        (false, m.verdict.explanation.clone())
    } else if let Some(r) = &reason {
        (false, format!("blocked: {r}"))
    } else if waiting_on.is_empty() {
        (true, m.verdict.explanation.clone())
    } else {
        (
            true,
            format!(
                "{}; waiting on {}",
                m.verdict.explanation,
                waiting_on.join(", ")
            ),
        )
    };
    MemberJudgement {
        landable,
        explanation,
        gate,
        waiting_on,
        reason,
    }
}

/// Compose the changeset's verdict from its members', given in landing
/// order. `state` is the changeset's own, and `composed` is the
/// changeset-level check gate — the verdicts of its own composed runs,
/// which belong to no member and so cannot arrive through one.
///
/// The composed gate folds in with the same precedence as a member's,
/// `Blocked` over `Waiting` over `Ready`, and for the same reason: a
/// build of the combination that has said no cannot be rescued by every
/// member being individually green — that is precisely the failure
/// composed CI exists to catch. When it is the composed gate that
/// decides, the explanation says so and names the check, because
/// "everything I can see is approved" beside a refusal is the reading
/// this has to avoid.
pub fn compose(state: &str, members: &[MemberVerdict], composed: &LandGate) -> Composed {
    let judged: Vec<MemberJudgement> = members.iter().map(judge).collect();
    let mut waiting_on: Vec<String> = members
        .iter()
        .zip(&judged)
        .flat_map(|(m, j)| {
            j.waiting_on
                .iter()
                .map(move |c| format!("{}: {c}", m.label))
        })
        .collect();
    // The composed checks are the changeset's own, so they are named
    // without a `repo/change` prefix — there is no member to attribute
    // them to, and inventing one would send a reader to a repository
    // whose part of the build may be perfectly green. They arrive as
    // `repo: check` from `composed_gate`: the repository the composed
    // job ran in, which is where its log is.
    if let LandGate::Waiting { on } = composed {
        waiting_on.extend(on.iter().cloned());
    }
    let blocked_composed = match composed {
        LandGate::Blocked { reason } => Some(reason.clone()),
        _ => None,
    };
    let gate = if judged.iter().any(|j| j.gate == Gate::Blocked) || blocked_composed.is_some() {
        Gate::Blocked
    } else if judged.iter().any(|j| j.gate == Gate::Waiting)
        || matches!(composed, LandGate::Waiting { .. })
    {
        Gate::Waiting
    } else {
        Gate::Ready
    };
    let (landable, explanation) = if state != "open" {
        (false, format!("changeset is {state}"))
    } else if members.is_empty() {
        (false, "changeset has no members".to_string())
    } else if let Some((m, j)) = members.iter().zip(&judged).find(|(_, j)| !j.landable) {
        (false, format!("{}: {}", m.label, j.explanation))
    } else if let Some(reason) = &blocked_composed {
        // After the members, because a member that is not approved is
        // the thing to fix first: a green composed build would not make
        // an unapproved change landable, and a red one is not the
        // author's next move either.
        (false, format!("blocked: {reason}"))
    } else if waiting_on.is_empty() {
        (
            true,
            format!("ok: all {} member(s) approved", members.len()),
        )
    } else {
        (
            true,
            format!(
                "ok: all {} member(s) approved; waiting on {} check(s)",
                members.len(),
                waiting_on.len()
            ),
        )
    };
    Composed {
        landable,
        gate,
        explanation,
        members: judged,
        waiting_on,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Verdict {
        Verdict {
            landable: true,
            explanation: "ok: all 1 changed path(s) approved".into(),
            per_path: Vec::new(),
        }
    }

    fn needs(who: &str) -> Verdict {
        Verdict {
            landable: false,
            explanation: format!("src/a.rs: needs approval from {who}"),
            per_path: Vec::new(),
        }
    }

    fn member(label: &str, state: &str, verdict: Verdict, gate: LandGate) -> MemberVerdict {
        MemberVerdict {
            label: label.into(),
            state: state.into(),
            verdict,
            gate,
        }
    }

    #[test]
    fn every_member_approved_and_green_is_landable_and_ready() {
        let c = compose(
            "open",
            &[
                member("api/I1", "open", ok(), LandGate::Ready),
                member("web/I2", "open", ok(), LandGate::Ready),
            ],
            &LandGate::Ready,
        );
        assert!(c.landable);
        assert_eq!(c.gate, Gate::Ready);
        assert_eq!(c.explanation, "ok: all 2 member(s) approved");
        assert!(c.waiting_on.is_empty());
        assert!(c.members.iter().all(|m| m.landable && m.reason.is_none()));
        assert_eq!(
            c.members[0].explanation,
            "ok: all 1 changed path(s) approved"
        );
    }

    #[test]
    fn the_first_unapproved_member_in_landing_order_names_the_blocker() {
        let c = compose(
            "open",
            &[
                member("api/I1", "open", ok(), LandGate::Ready),
                member("web/I2", "open", needs("bo@x.test"), LandGate::Ready),
                member("cli/I3", "open", needs("cy@x.test"), LandGate::Ready),
            ],
            &LandGate::Ready,
        );
        assert!(!c.landable);
        assert_eq!(c.gate, Gate::Ready);
        assert_eq!(
            c.explanation,
            "web/I2: src/a.rs: needs approval from bo@x.test"
        );
        assert!(c.members[0].landable);
        assert!(!c.members[1].landable && !c.members[2].landable);
    }

    #[test]
    fn a_member_that_is_not_open_cannot_be_helped_by_approval() {
        let c = compose(
            "open",
            &[member(
                "api/I1",
                "landed",
                needs("bo@x.test"),
                LandGate::Blocked {
                    reason: "required check 'ci' is failing".into(),
                },
            )],
            &LandGate::Ready,
        );
        assert_eq!(c.explanation, "api/I1: change is landed");
        // The gate is still reported for what it is.
        assert_eq!(c.gate, Gate::Blocked);
        assert_eq!(
            c.members[0].reason.as_deref(),
            Some("required check 'ci' is failing")
        );
    }

    #[test]
    fn a_failing_check_blocks_and_an_unreported_one_only_waits() {
        let c = compose(
            "open",
            &[
                member(
                    "api/I1",
                    "open",
                    ok(),
                    LandGate::Waiting {
                        on: vec!["ci/tests".into(), "ci/lint".into()],
                    },
                ),
                member(
                    "web/I2",
                    "open",
                    ok(),
                    LandGate::Blocked {
                        reason: "required check 'ci/tests' is failing".into(),
                    },
                ),
            ],
            &LandGate::Ready,
        );
        assert!(!c.landable);
        assert_eq!(c.gate, Gate::Blocked);
        assert_eq!(
            c.explanation,
            "web/I2: blocked: required check 'ci/tests' is failing"
        );
        assert_eq!(c.waiting_on, vec!["api/I1: ci/tests", "api/I1: ci/lint"]);
        assert!(c.members[0].landable, "waiting is not a refusal");
        assert_eq!(
            c.members[0].explanation,
            "ok: all 1 changed path(s) approved; waiting on ci/tests, ci/lint"
        );

        let c = compose(
            "open",
            &[member(
                "api/I1",
                "open",
                ok(),
                LandGate::Waiting {
                    on: vec!["ci/tests".into()],
                },
            )],
            &LandGate::Ready,
        );
        assert!(c.landable);
        assert_eq!(c.gate, Gate::Waiting);
        assert_eq!(
            c.explanation,
            "ok: all 1 member(s) approved; waiting on 1 check(s)"
        );
    }

    #[test]
    fn approval_comes_before_checks_in_the_explanation() {
        let c = compose(
            "open",
            &[member(
                "api/I1",
                "open",
                needs("bo@x.test"),
                LandGate::Blocked {
                    reason: "required check 'ci' is failing".into(),
                },
            )],
            &LandGate::Ready,
        );
        assert_eq!(
            c.members[0].explanation,
            "src/a.rs: needs approval from bo@x.test"
        );
        assert_eq!(c.gate, Gate::Blocked);
    }

    #[test]
    fn a_changeset_that_is_not_open_is_not_landable_whatever_its_members_say() {
        for state in ["landing", "landed", "abandoned", "failed"] {
            let c = compose(
                state,
                &[member("api/I1", "open", ok(), LandGate::Ready)],
                &LandGate::Ready,
            );
            assert!(!c.landable, "{state}");
            assert_eq!(c.explanation, format!("changeset is {state}"));
            assert!(c.members[0].landable, "the member's own answer stands");
        }
        let c = compose("open", &[], &LandGate::Ready);
        assert!(!c.landable);
        assert_eq!(c.explanation, "changeset has no members");
        assert_eq!(c.gate, Gate::Ready);
    }

    /// The composed gate is the changeset's own answer, and it decides
    /// at every one of its three values even when every member is green.
    ///
    /// This is the whole point of composed CI: two changes that pass
    /// apart and break together. A fold that let the members outvote the
    /// composed build would report exactly that combination as landable.
    #[test]
    fn the_composed_gate_decides_when_every_member_is_green() {
        let green = || {
            vec![
                member("api/I1", "open", ok(), LandGate::Ready),
                member("web/I2", "open", ok(), LandGate::Ready),
            ]
        };

        let c = compose("open", &green(), &LandGate::Ready);
        assert!(c.landable);
        assert_eq!(c.gate, Gate::Ready);

        // Waiting is not a refusal — the changeset is still landable and
        // the wait is named, without a `repo/change` prefix, because the
        // check belongs to no member.
        let c = compose(
            "open",
            &green(),
            &LandGate::Waiting {
                on: vec!["composed / integration".into()],
            },
        );
        assert!(c.landable, "a build still running is not a refusal");
        assert_eq!(c.gate, Gate::Waiting);
        assert_eq!(c.waiting_on, vec!["composed / integration"]);
        assert_eq!(
            c.explanation,
            "ok: all 2 member(s) approved; waiting on 1 check(s)"
        );

        // Blocked is, and the sentence names the composed check rather
        // than saying everything is approved.
        let c = compose(
            "open",
            &green(),
            &LandGate::Blocked {
                reason: "composed check composed / integration in web is failing".into(),
            },
        );
        assert!(!c.landable);
        assert_eq!(c.gate, Gate::Blocked);
        assert_eq!(
            c.explanation,
            "blocked: composed check composed / integration in web is failing"
        );
        // The members' own answers are untouched: each is fine on its
        // own, which is exactly what the composed verdict is disputing.
        assert!(c.members.iter().all(|m| m.landable));
    }

    /// A member that needs approval is named before the composed build,
    /// because approving is the reader's next move and a red combined
    /// build is not something they can act on yet — and a member's own
    /// blocked check is named ahead of it for the same reason.
    #[test]
    fn a_member_refusal_is_reported_before_the_composed_one() {
        let composed_red = LandGate::Blocked {
            reason: "composed check ci / e2e in web is failing".into(),
        };
        let c = compose(
            "open",
            &[member(
                "api/I1",
                "open",
                needs("bo@x.test"),
                LandGate::Ready,
            )],
            &composed_red,
        );
        assert!(!c.landable);
        assert_eq!(c.gate, Gate::Blocked);
        assert_eq!(
            c.explanation,
            "api/I1: src/a.rs: needs approval from bo@x.test"
        );

        // And a changeset that is not open says so first of all: there
        // is nothing to do about a composed check on a landed review.
        let c = compose(
            "landed",
            &[member("api/I1", "open", ok(), LandGate::Ready)],
            &composed_red,
        );
        assert_eq!(c.explanation, "changeset is landed");
        assert_eq!(c.gate, Gate::Blocked, "the gate still reports what it is");
    }

    /// Waits from both levels appear together, each named the way its
    /// reader can act on: a member's prefixed with `repo/change`, the
    /// changeset's own bare.
    #[test]
    fn waits_from_a_member_and_from_the_composition_are_both_listed() {
        let c = compose(
            "open",
            &[member(
                "api/I1",
                "open",
                ok(),
                LandGate::Waiting {
                    on: vec!["ci/tests".into()],
                },
            )],
            &LandGate::Waiting {
                on: vec!["composed / e2e".into()],
            },
        );
        assert!(c.landable);
        assert_eq!(c.gate, Gate::Waiting);
        assert_eq!(c.waiting_on, vec!["api/I1: ci/tests", "composed / e2e"]);
    }

    #[test]
    fn the_gate_has_three_words() {
        assert_eq!(Gate::Ready.as_str(), "ready");
        assert_eq!(Gate::Waiting.as_str(), "waiting");
        assert_eq!(Gate::Blocked.as_str(), "blocked");
    }
}
