# Org settings vs repo settings

Weft has two rings of authority, and every setting lives in exactly
one of them. **Org settings** shape the organization: who is in it,
what they may do everywhere, and which credentials exist. **Repo
settings** shape one repository: where its trunk is, what is fenced,
who is raised or held down *here*. Nothing is configured in both
places, so there is never a question of which copy wins.

## The roles, and where they reach

An org role is the default answer everywhere; a repo grant rewrites the
answer for one repo only.

| Role | Org settings | A repo, by default |
|---|---|---|
| **Owner** | everything, and cannot be removed last | admin |
| **Admin** | everything except removing the last owner | admin |
| **Member** | read the org, mint personal tokens | read + write |
| **Viewer** | read the org, mint read-only tokens | read |

Two override rules, deliberately different:

- A **direct grant** naming a person on a repo **replaces** their org
  role there — up *or down*. An org admin held to `viewer` on one repo
  cannot delete it; a viewer granted `admin` on one repo runs its
  settings.
- A **team grant** only ever **raises**. Joining a team never takes
  access away, so team membership is safe to automate.

"Repo admin" below always means *effective role at that repo* —
an org admin anywhere, or whoever a grant raised to admin right there.

## Who controls what

| Setting | Ring | Who changes it | Audit action |
|---|---|---|---|
| Members, org roles, invites | org | org admin | `member.*`, `invite.*` |
| Teams and their rosters | org | org admin | `team.*` |
| Service + personal tokens | org | admin; anyone for their own personal token | `token.*` |
| SSH keys | org | admin; owners of the key's token | `sshkey.*` |
| Plan and billing | org | org admin | `billing.*` |
| Repo create / delete | repo (born in the org) | writer creates; repo admin deletes | `repo.create`, `repo.delete` |
| Visibility (public) | repo | set at creation | `repo.create` |
| **Default branch** | repo | repo admin | `repo.default_branch` |
| **Branch protections** | repo | repo admin | `repo.protect`, `repo.unprotect` |
| Access grants (people, teams) | repo | repo admin | `grant.*` |
| Webhooks | repo | repo admin | `webhook.*` |
| Mirror origin & sync | repo | repo admin; default branch follows origin HEAD | `mirror.*` |
| OWNERS rules | repo, in the tree | whoever review lets touch `OWNERS` files | it's a commit — reviewed like code |

The last row is the point of the design: *who must approve what* is not
a setting at all. It lives in `OWNERS` files inside the repository, so
changing the rules takes a change, with a diff, through the same review
it governs — see [Changes, OWNERS & landing](code-review.md).

## Where each ring lives in the dashboard

- **Settings** (top right) is the org ring: Members, Teams, Tokens,
  SSH keys, Activity, Password.
- **A repo's overview** is the repo ring: clone URLs, metrics, the
  Access panel (grants), and Branch policy (default branch and
  protections). Mutating forms appear only for people whose effective
  role at that repo is admin — everyone else sees the fence, not the
  gate.

## The three rules underneath

1. **Masking**: a credential that cannot touch a thing cannot see it —
   settings answer `404` to the wrong ring, never "forbidden", so
   existence leaks nothing.
2. **Every authority move is audited**, in the same transaction that
   makes it, under the actions in the table — "who unprotected main on
   Thursday" is one filtered query.
3. **Half-configured is refused loudly.** A setting that must exist to
   act (a default branch that isn't a real branch, a protection on a
   branch that doesn't exist) is rejected at the door, in words.
