# Authentication

There are two kinds of caller, and the difference decides everything else.

A **person** signs in with an email address and a password and gets an
HttpOnly session cookie. That is how the dashboard works; no script on
the page can read the credential.

A **machine** — CI, a script, `git` itself — sends a bearer token of the
form `weft_<id>_<secret>`. Only a hash of the secret is stored; the
plaintext is shown exactly once at mint time.

Both resolve to the same authority model, so every endpoint accepts
either. When both are presented the bearer token wins, so a developer
with the dashboard open in the same browser can still test a token by
pasting it into a request and get *that* token's authority.

## People, roles and orgs

Every repository belongs to one organization and is private to it.
There are no public repositories and nothing is readable without a
credential. A person belongs to one or more orgs, at one role in each:

| Role | Can |
|------|-----|
| `viewer` | read every repo in the org |
| `member` | everything a viewer can, plus push, commit and create repos |
| `admin` | everything, including managing people and credentials |
| `owner` | the same as admin; an org must always have at least one |

An org can never be left without an owner: removing or demoting the last
one answers `409`.

A **per-repo grant** replaces the org role on one repo — in either
direction. Granting `member` to a viewer opens exactly that repo;
granting `viewer` to an admin holds them down on exactly that repo, and
nowhere else. One call can name several people at once:

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/repos/widget/grants \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "user_ids": ["01hx…", "01hy…"], "role": "member" }'
```

A batch is all-or-nothing. If any id in it is not a member of the org,
nothing is granted — a half-applied change is one you cannot reason
about afterwards.

## Teams

"The payments squad can write here" is one statement about the org.
Saying it person by person means it drifts the moment somebody joins, so
a **team** can be granted a role on a repo directly:

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/teams \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "name": "payments", "description": "the squad" }'

curl -X PUT https://spool.example.com/v1/orgs/acme/teams/$TEAM/members/$USER \
  -H "Authorization: Bearer $ADMIN_TOKEN"

curl -X POST https://spool.example.com/v1/orgs/acme/repos/widget/grants \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "team_id": "'"$TEAM"'", "role": "member" }'
```

**A team grant only ever raises.** Being in a team is how people get more
access on a repo; it is never how they quietly lose some, because nobody
reads a team's grant list before adding a colleague to it. Lowering
somebody stays a deliberate, per-person act.

So three rules decide what you can do on a repo, in this order:

1. A grant naming **you** — that role, outright, up or down.
2. Otherwise the **highest** of your org role and every team grant on
   that repo. Two teams disagreeing takes the higher.
3. No org membership at all — no access, whatever the grants say. Team
   membership requires org membership, so a grant can never outlive it.

Deleting a team withdraws its membership and every grant it carries, on
the very next request. Teams are named per org, case-folded, so `Payments`
and `payments` cannot both exist.

Which rule applied to whom is a question worth answering directly, and
`GET …/repos/:repo/access` answers it — every person who can reach the
repo, their role there, and where it came from:

```bash
curl -H "Authorization: Bearer $ADMIN_TOKEN" \
  https://spool.example.com/v1/orgs/acme/repos/widget/access
```

```json
{ "people": [ { "email": "dev@acme.dev", "role": "member",
                "source": "team", "team_name": "payments" } ],
  "teams":  [ { "team_name": "payments", "role": "member",
                "member_count": 4 } ] }
```

In the dashboard the same two things are **Settings → Teams** and the
**Access** panel on a repo.

People join an organization by invitation, which is **emailed** when a
mail transport is configured. The link works once and expires after
seven days. Only an organization takes members: inviting somebody into a
personal namespace is a `400`, `a personal namespace cannot have members
— create an organization`.

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/invites \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "email": "dev@acme.dev", "role": "member" }'
```

```json
{ "id": "01hx…", "email": "dev@acme.dev", "role": "member",
  "expires_at": 1787428539000,
  "invite_link": "stinv_01hx…_…",
  "mail": { "sent": true } }
```

The link comes back **as well as** being sent. A relay that is down, or a
server with no transport configured, must not stop you onboarding
somebody — so `mail.sent` tells you whether to deliver it yourself, and
`mail.error` says what went wrong when something did. `sent: false` with
no `error` means no transport is configured.

The link lands on a screen that says what is being joined. It asks the
server first:

```bash
curl -X POST https://spool.example.com/v1/auth/invite/preview \
  -H "Content-Type: application/json" \
  -d '{ "invite": "stinv_01hx…_…" }'
```

```json
{ "org": "acme", "role": "member", "email": "dev@acme.dev",
  "expires_at": 1787428539000 }
```

No credentials: the token in the body **is** the credential, so this
tells its holder nothing they were not already sent. It is a question
rather than an action — previewing does not spend the link — and every
dead shape (malformed, unknown, expired, already accepted, wrong secret)
answers the same 404, so a link cannot be used to ask which invitations
exist. It is a POST, not a GET, because a token in a path or query lands
in every access log between the browser and here.

### Configuring mail

`STRATUM_MAIL_TRANSPORT` picks one of four, and `STRATUM_MAIL_FROM` is
the sender address for all but the first two.

| Transport | What it does | Also needs |
|---|---|---|
| `null` (default) | drops every message | — |
| `capture` | writes each message to a file, for local development and tests | `STRATUM_MAIL_DIR` |
| `smtp` | delivers through a relay | `STRATUM_MAIL_SMTP_HOST` (`host` or `host:port`) |
| `ses` | Amazon SES v2, signed with the instance's own credentials | `STRATUM_MAIL_SES_REGION` (defaults to `AWS_REGION`) |

A name that is none of those is a **boot failure**, not a silent
fallback to dropping mail: a typo in a deployment variable must not look
like a working system.

**SMTP has no STARTTLS.** Nothing in the server links a TLS client
library, so this transport speaks cleartext — which is the normal
self-hosting arrangement (a relay on `localhost`, or a sidecar on a
private network) and fine until credentials are involved. Setting
`STRATUM_MAIL_SMTP_USER` and `STRATUM_MAIL_SMTP_PASSWORD` for a
**non-loopback** host is refused at boot unless you state that the link
is already private with `STRATUM_MAIL_SMTP_ALLOW_CLEARTEXT_AUTH=1`. On
AWS, use `ses`, which is HTTPS.

## Signing yourself up

Anybody who can reach your server can sign up. An account starts with
its own personal namespace and no organizations: it sees nothing of
anybody else's until an organization invites it.

```bash
curl -X POST https://spool.example.com/v1/auth/signup \
  -H "Content-Type: application/json" \
  -d '{ "email": "you@example.dev", "name": "Your Name",
        "password": "a long enough password", "handle": "you" }'
```

The **handle** is your personal namespace — the `you` in `/you/repo`.
It holds repositories of your own, such as your [forks](forks.md), and
nobody else: a personal namespace cannot have members, so a team's work
belongs in an organization. The handle is asked for rather than derived
from your address, because it appears in every clone URL you hand out.
It is validated and its refusals are plain (`400` for a bad shape or a
reserved word, `409` for one already taken): namespace names are unique
on the server and are not secrets, so "that one is taken" gives nothing
away.

Everything after the handle is **uniform**. Signup always answers `202`
with the same body, whether a confirmation message was sent, the address
already has an account, or you have asked too many times — any
difference would be a way to ask who has an account here. When the
address is already registered, its owner gets a message saying so and
that nothing was created; that way somebody who forgot they had an
account is not left staring at "check your email" with an empty inbox.

Rate limits are per address and global, not per source: behind a proxy
this server does not see a peer address it can trust, and a limit that
`X-Forwarded-For` can bypass is worse than an honest global one.

### Signing up with GitHub instead

When your server has a GitHub App configured, the sign-in screen offers
**Continue with GitHub**. Press it and there is no
confirmation message at all. The round trip is the ordinary OAuth one —
`GET /v1/auth/github/start` sends you to GitHub's authorization screen
and GitHub returns you to `GET /v1/auth/github/callback` — and what
makes it safe to skip the server's own mail is that GitHub has already done the
work: `GET /user/emails` reports which of your addresses GitHub itself
sent a link to and saw clicked.

**Only that flag is trusted.** The address has to be your **primary**
one and GitHub has to report it **verified**; anything else — an
unproved primary, or a GitHub App that was never granted the
*Email addresses* permission — lands you back on the sign-in screen
saying so, with the password path still open. We never read `GET
/user`'s own `email` field: that is whatever you chose to publish,
GitHub does not check it, and trusting it would let somebody claim your
account here by typing your address into a profile.

An account made this way has **no password** and its address is proved
on arrival, so it can create a repository immediately. Your GitHub
login becomes your handle, suffixed if that name is reserved or already
taken; set a password later from **Settings** if you want the second
way in.

If you already have an account here whose sign-in address is the one
GitHub proved, this **links to it** rather than refusing — same account,
same organizations, a second door. One exception, and it is deliberate:
if that account had never confirmed its address, its password is
cleared and its sessions ended at the moment you prove the address.
Anyone can sign up with an address they do not own; that account can do
nothing, but it sits on the address with a password its maker knows, and
handing it to you intact would hand you an account somebody else can
still open.

The account is keyed on GitHub's **numeric user id**, never your login.
Renaming yourself on GitHub still lands you on your own account, and
whoever claims your old login next gets nothing.

### Confirming, and what is blocked until you do

```bash
curl -X POST https://spool.example.com/v1/auth/verify \
  -H "Content-Type: application/json" -d '{ "token": "weftv_…" }'
```

Confirming signs you in — somebody holding a link from their own inbox
has proved as much as the sign-in form asks for.

An unconfirmed account **may sign in, look around, and read whatever its
role allows. It may not create a repository or a mirror.** That is the
line: everything cheap stays open, everything that costs storage or an
outbound fetch does not. `POST /v1/auth/resend-verification` sends
another link, and issuing one spends the previous one.

**Service tokens are exempt.** A token with no user behind it was minted
by somebody who is confirmed, and every token minted before addresses
were proved at all is one of these — CI does not stop working because a
colleague has not read their email.

### Forgotten passwords

```bash
curl -X POST https://spool.example.com/v1/auth/forgot-password \
  -H "Content-Type: application/json" -d '{ "email": "you@example.dev" }'
curl -X POST https://spool.example.com/v1/auth/reset-password \
  -H "Content-Type: application/json" \
  -d '{ "token": "weftrs_…", "new_password": "a different long password" }'
```

A reset link lives for an hour (a confirmation link, for a day), works
once, and **ends every other session on the account** — whoever asked for
it may have done so because somebody else was signed in. It also counts
as proof of the address, so it finishes a signup that was abandoned. A
verification link presented to the reset endpoint is refused, and the
reverse too: the token's row says what it is for.

A disabled account gets no reset link. Recovering an account an operator
switched off would undo the switching off.

The very first account is created from the command line on the server,
because there is nobody yet to invite it:

```bash
stratum-server admin user-create --org acme \
  --email you@acme.dev --name "Your Name" --password '…' --role owner
```

## Minting and revoking tokens

A signed-in person mints a **personal access token** for themselves — no
administrator needed:

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/tokens \
  -b "$COOKIE_JAR" -H "Content-Type: application/json" \
  -d '{ "scopes": ["repo:write"], "label": "laptop" }'
```

A personal token carries your authority *now*, not the authority you had
when it was minted. The scopes you mint it with are a **ceiling**; what
it actually does on a given repo is that ceiling intersected with your
effective role there. So:

- Demote yourself to viewer and the token in your hand stops writing on
  the next request. Nothing has to be hunted down and revoked by hand.
- Get granted `member` on one repo and the same token starts pushing to
  that repo — and to nothing else. A grant that could only ever take
  access away would be a one-way ratchet.
- Get promoted to admin and the token stays what it was minted for. The
  ceiling never rises.

You may mint any scope you can exercise *somewhere* in the org — your org
role, a per-repo grant, or a grant to a team you are in, if one gives you
more. A viewer with a `member`
grant on one repo can hold a `repo:write` token; it writes there and
reads everywhere else. Asking for more than that is refused at mint time,
because a credential that silently does less than it says is worse than
no credential.

An org **service token** belongs to nobody and is the right shape for CI.
Minting one needs `org:admin`:

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/tokens \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "scopes": ["repo:read"], "repo": "widget", "label": "ci-runner" }'
```

Revocation is instant — every request verifies against the control plane, so
a revoked token fails on its very next use:

```bash
curl -X DELETE https://spool.example.com/v1/orgs/acme/tokens/$TOKEN_ID \
  -H "Authorization: Bearer $ADMIN_TOKEN"
```

`GET /v1/orgs/acme/tokens` lists tokens without their secrets — every
token in the org for an admin, your own for a member. Somebody else's
token answers `404` rather than `403`, so a member cannot use revocation
to discover which token ids exist.

## Scopes

| Scope | Grants |
|-------|--------|
| `repo:read` | clone/fetch and all read endpoints |
| `repo:write` | everything in `repo:read`, plus push, commits, refs, repo create/delete |
| `org:read` | listings, metrics, usage, audit queries |
| `org:admin` | everything, including token management |

**Every token acts in the one organization it was minted in.** The route
you mint it on names the org, and presented anywhere else it is as good
as no access: another organization's repositories answer `404`. A person
who belongs to several organizations has a token per organization, and
a browser session, which spans every organization they belong to. What
crosses two namespaces — [forking](forks.md) from one into another or
into your personal namespace, and opening a change from that fork —
therefore takes a session. The one exception is [search](search.md): a
personal token searches every organization its person belongs to.

A token minted with `"repo": "<name>"` is also **bound to that repo**: it
can't touch any other repo, can't perform org-level operations, and
can't search. This is the right shape for per-runner and per-agent
credentials.

## On the git wire

git sends credentials over HTTP Basic; put the token in either field:

```bash
git clone https://x:weft_…@spool.example.com/acme/widget.git
```

A request with no credential answers `401` (so git retries with
credentials) — for every repository, and for a name that does not exist.
A valid credential without access answers `404`, exactly as a repository
that does not exist does: one organization can never learn what exists
in another. A push by somebody who can read a repository but not write
to it is refused with the reason — `you can read acme/widget but not
push to it; fork it and open a change from your fork, or ask an owner
for write access` — as an in-band error on the advert (git prints it as
`remote error:`) and as `403` on the RPC itself. Only a repository you
cannot read at all is masked as `404`.

The REST API answers the same way: `401` with no credential, `404` for a
repository you cannot read, whether or not it exists.

## SSH keys

Deployments that expose the SSH front door also accept
`git clone ssh://git@host:port/acme/widget.git`, authenticated by public
key instead of a pasted token. A key never carries permissions of its
own; it names something that does, and the row says which.

A **personal key** names you. Add it from the dashboard's Settings → SSH
keys, or over the API while signed in — no token id anywhere:

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/ssh-keys \
  -b "$COOKIE_JAR" -H "Content-Type: application/json" \
  -d "{ \"public_key\": \"$(cat ~/.ssh/id_ed25519.pub)\", \"label\": \"laptop\" }"
```

Its authority is re-resolved from your role on every connection, per-repo
grants included. An administrator changes your role and the next
`git push` from that laptop obeys it — there is no key to re-issue and
nothing cached to expire.

A **deploy key** names a token instead, which is what an unattended
machine wants. It inherits that token's scopes and repo binding, and
creating one needs `org:admin`:

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/ssh-keys \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{ \"public_key\": \"$(cat deploy.pub)\", \"token_id\": \"$TOKEN_ID\", \"label\": \"ci\" }"
```

Either way, revoking the key (`DELETE /v1/orgs/acme/ssh-keys/$KEY_ID`),
the token, the membership or the account cuts SSH access on the very next
connection — the fingerprint is resolved against the control plane every
time, never cached. `GET /v1/orgs/acme/ssh-keys` lists keys with their
OpenSSH SHA-256 fingerprints (compare with `ssh-keygen -lf`): every key
in the org for an admin, your own for a member. Accepted key types:
ed25519, ECDSA (P-256/384/521), and RSA.
