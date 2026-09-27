---
layout: ../../layouts/Docs.astro
title: The maintainer firewall
description: The design for staged contribution intake — machines spending machine time before humans spend human time — and the two capabilities it deliberately will not have.
---

# The maintainer firewall

**None of this is built. This page is a design, published as one.** There is
no intake worker, no trust ladder and no style gate in the product today; a
change reaching a repository today reaches review the moment it is
registered. What *does* exist, and is unchanged by any of this, is the
machinery the design sits in front of: [changes, patchsets, `OWNERS`
approval sufficiency and the land queue](/docs/code-review/). We publish the
design ahead of the build for the same reason this site publishes what the
land queue cannot do as plainly as what it can — a maintainer deciding where
to host a project is better served by a plan they can argue with than by
silence.

Read every "would" below as load-bearing.

## The shape: what would happen to an inbound change

A change from an untrusted contributor would be admitted in stages, each
either passing it along or refusing it in a sentence naming the rule it
broke. It is not a score and not a filter you have to trust — the intent is
four gates whose results you can read.

**It applies.** The change must fast-forward the target branch. The design
calls the same ancestry function the land queue already uses at claim time,
at the door instead of at the end — one function, so that the firewall and
the queue cannot disagree about whether something is landable. A change that
could never have landed would never become a tab a maintainer has to close.

**Checks pass.** Intake would read the `change_checks` your CI already
posts — that reporting API exists today and is documented under
[Changes, OWNERS & landing](/docs/code-review/) — and hold the change at the
door until they are green. Nothing new is invented here; a red check would
stop a change at intake for the same reason it already stops one at the
queue.

**It is not the fourth copy of the same patch.** A fingerprint over the
normalized diff lines plus a hash of the sorted set of changed paths,
compared against the recent open changes on the repository — bounded, not
the whole history. Slop travels in herds, and six near-identical patches to
one file should arrive as one thread with five siblings attached rather than
as six separate demands on an evening.

**It matches house style.** A declarative gate over the diff, read from a
`.weft/style.toml` in the repository. The file is designed to look like
this, and does nothing at all today:

```toml
max_file_bytes = 262144
forbidden_paths = ["vendor/**", "*.min.js"]
required_trailer = "Signed-off-by"
line_endings = "lf"
```

Declared in the repository, evaluated at the door, and the refusal names the
rule — so that a first-time contributor learns something rather than being
ignored.

Admission would be a **separate axis** from the land queue's state,
deliberately. A change would have an admission decision and, independently,
a place in the queue's lifecycle. The lander would re-check admission when
it claims a change, in the same way it already re-checks approval
sufficiency and failing checks, and refuse to land one that has not been
admitted. Collapsing the two into one enum would make that re-check
ambiguous, which is how a change gets landed on the strength of a decision
that has since been withdrawn.

## The trust ladder

Which gates a contributor would meet depends on what the repository already
knows about them:

| Rung | Who | What they would meet |
|---|---|---|
| `collaborator` | an owner, admin or member of the namespace, or anyone an `OWNERS` file already trusts with the paths they touched | nothing — straight to review |
| `trusted` | landed changes behind them, no recent ejections | nothing — straight to review |
| `first_timer` / `unknown` | everybody else | the full gauntlet above |
| `blocked` | set by hand | refused at the door, both transports |

The ladder would be derived, with a manual override in either direction.

**The top rung is exactly today's behaviour, which is the point.** Inside a
company every principal is already known, so the ladder collapses to its top
rung and intake never fires. An existing private repository would therefore
see no change at all when this lands — not a migration, not a new setting to
turn off, no new step between a member and review. That is a constraint on
the design rather than a happy accident: the firewall has to be the same
machinery wearing open-source defaults, because a second product bolted on
beside the first is how the first one rots.

## Agent policy, per repository

The design has a repository declare how it wants to be contributed to by
agents, with the platform enforcing it:

| Policy | Intended effect |
|---|---|
| `welcome` | agent principals contribute like anyone else |
| `labeled` | agent principals contribute, and their changes are labeled as theirs (the intended default) |
| `human-only` | an agent principal is refused at both doors, with a sentence naming the policy |

Enforcement, not documentation — that is the part worth designing. A policy
that asks maintainers to police it themselves is a policy that costs
maintainer attention, which is the resource this whole page exists to
protect. The one piece already in force is that an agent never borrows a
human's name to get past it: service principals have their own identity
today.

## Two things this deliberately will not do

These are cuts, not gaps waiting to be filled. The first is about what
we will execute on behalf of a stranger; the second is about the rule
that every test in this repository is hermetic.

**The firewall will not run your lint command against a stranger's
diff.** Executable gates on an *untrusted* patch — project-supplied
commands, run on our servers, on code a person who is not a contributor
just sent you — is a sandboxing product, not a forge feature, and
shipping a half-built one is how a forge becomes an
arbitrary-code-execution surface. This is why a change whose commits come from
another repository is recorded as `blocked` and its
[workflows](/docs/workflows/) do not start: they were written by the
contributor, and running them would hand a stranger a repository token
and a machine. There is no approval button yet. Branches in the
repository itself are a different question, and their workflows do run
here.

**The server will never call a model.** There is to be no outbound model
call anywhere in intake. An outbound call could not be tested hermetically,
could not be reasoned about when the far end is slow or wrong, and would put
your diffs in front of a third party — which would contradict
[what we say about AI](/ai-policy).

The design's replacement is an inbound seam: a triage report **posted to**
the change by an agent principal holding a token, exactly like a CI check,
landing as a gate row on the change. Model-assisted triage would then be an
agent you run, under your policy, with your model, reporting in like any
other machine — a deployment decision you make, rather than one made on your
behalf inside our worker.
