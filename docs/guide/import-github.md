# Importing from GitHub

**The issue importer does not exist. This page is its design.** There is no
import endpoint and no import job. Do not plan a migration date around this
page; plan it around [the migration page](/migrate), which is the shorter
per-artifact version of the same account and is equally careful about what
is a plan.

**What changed since this page was written:** the issue tracker it imports
*into* now exists — file, comment, close, label, filter and sort, with
filing gated on read access rather than write, because a tracker only
writers can post to is closed to everybody it is for. So the missing half
is the importer alone.

**What does work today is the half that matters most and is pure git:**
mirroring a GitHub repository's commits, branches and tags. That is
[Mirror in 5 minutes](quickstart-mirror.md), it is running now, and it
is the step that moves your history. This page is about the things that are
not in the repository — issues, their conversations, and the URLs that point
at them.

## What an import would move

- **Issues** — title, body, state, author, timestamps, open and closed
- **Comments**, in the order they were written
- **Labels**, with their colors and descriptions
- **Milestones**, with due dates and state
- **Reactions**, per issue and per comment
- **Assignees**
- **Cross-references** — a `#123` in one issue's body still pointing at the
  right issue here
- **Old URLs** — a redirect map, so that a GitHub issue link in a
  mailing-list archive from 2019 resolves to its new home

Conversations are to read in insertion order rather than in identifier
order, because a conversation that reorders itself is not a record of
anything.

## Import order, and why it would be that order

The sequence is not an implementation detail, which is why it is on a page
written before the code. Two of the steps are only correct in one position.

1. **Labels**, then **milestones**. Issues refer to them, so they have to
   exist before anything refers to them.
2. **Issues, ascending by original number.** This is the load-bearing one.
   Issue numbers are to be allocated from a per-repository counter, so
   importing in ascending order leaves that counter at the maximum imported
   number and the next issue filed natively is `max + 1`. Import out of
   order and native numbering collides with imported numbering — which would
   show up weeks later as two issues that both believe they are `#412`.
3. **Comments**, then **reactions**, then **assignees** — each attaching to
   an issue that by then exists.
4. **Cross-references last**, when every possible target has been created. A
   reference resolved earlier would have to guess at issues not yet
   imported, and a guess in a permanent record is worse than a plain `#123`.

Everything arriving from the API is to be treated as untrusted input with
caps on size and count, not as best-effort parsing of a friendly payload.

## Authors that cannot be mapped

An author or commenter with no account here would keep a **display name**
marked as coming from GitHub — `octocat (github)` — with their words
attached to that name.

They are never to be silently attributed to a local account, however closely
a handle or an address matches. Handles are not identity across platforms;
the person who holds `octocat` here need not be the person who held it
there, and quietly merging the two puts words in somebody's mouth in a
permanent record. If the real person later joins and confirms the address,
the mapping can be made deliberately, by them.

## Pull requests would import as issues

A GitHub pull request is to arrive here as an **issue**, carrying a link to
the original and a redirect from its old URL: the conversation, labels,
reactions and cross-references, but not the patchset. An imported PR would
not be a reviewable, landable change here.

This is a deliberate cut, and the reason is accuracy rather than effort. A
pull request's head commits live in the contributor's fork, and for a merged
or closed PR they frequently no longer exist anywhere — the fork is deleted,
the branch is gone, and what remains is a diff GitHub rendered at the time.
Reconstructing patchsets from that is guesswork dressed up as an import, and
a review history that is 90% right is worse than one that is honestly a
record. So they would arrive as what they now are: the written record of a
discussion, with a working link to where it happened.

Open pull requests a project still cares about would be re-opened as
[changes](code-review.md) here by their contributors, from the
[fork](forks.md) they came from. That is a real cost of a move, and
[the migration page](/migrate) counts it as one.

## One door, because we have not launched

An earlier version of this page described **two** doors, and the reasoning
is worth keeping even though the answer changed.

Reading issues from GitHub needs `issues: read` on top of the metadata and
contents permissions a mirror already uses. Adding a permission to a
*published* GitHub App requires **every existing installation to re-accept**
it — an operational event for every customer, not a deploy. That constraint
would have forced a token-paste import as the default, and a pasted token is
a credential we would have to store: encrypted at rest, with a lifetime and
a deletion, because any node in a fleet may claim the job and the token
cannot live in the memory of the node that received it.

**There are no installations in the wild yet.** So the permission goes on
the App now, before launch, and the import uses the App like everything else
does. That deletes the whole token subsystem — no paste, no at-rest secret,
no expiry, no deletion path — because installation tokens are minted from
the App private key that every node already has.

The refusal that mattered stays: where an installation does **not** carry
`issues: read`, the import refuses with a message naming the missing
permission. It does not import an empty list and call it a success — an
import that quietly produces nothing looks exactly like a project that never
had issues, and that is the failure worth a specific sentence.

This is a decision with a shelf life. The moment the App is published and
installed by somebody outside this repository, adding a permission stops
being free, and any further permission this importer turns out to need is a
re-consent event. Anything it needs, it should ask for now.
