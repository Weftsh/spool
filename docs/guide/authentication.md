# Authentication

There are two kinds of caller, and the difference decides everything else.

A **person** signs in — through your company's identity provider when
the server is [configured for single sign-on](#single-sign-on), with an
email address and a password otherwise — and gets an HttpOnly session
cookie. That is how the dashboard works; no script on the page can read
the credential.

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

### Accepting an invitation

```bash
curl -X POST https://spool.example.com/v1/auth/accept-invite \
  -H "Content-Type: application/json" \
  -d '{ "invite": "stinv_01hx…_…", "name": "Dev Eloper",
        "password": "a long enough password", "handle": "dev" }'
```

For somebody new, this makes the account and signs them in (`201`, with
a session cookie). `name` and `password` are required; `handle` is
optional. The address needs no confirming: the invitation reached it,
and that is the proof a confirmation link would have been.

For somebody who already has an account on this server, only `invite`
matters: the account joins the organization at the invited role, keeps
its password and its handle, and is signed in.

On a server that signs in with [SSO only](#sso-only), accepting needs
the invited person to be signed in already, and makes nobody new.

The **handle** is the new account's personal namespace — the `you` in
`/you/repo`. It holds repositories of the person's own, such as their
[forks](forks.md), and nobody else's: a personal namespace cannot have
members, so a team's work belongs in an organization. Left out, it is
made from the part of the address before the `@` (`dev.eloper@acme.dev`
becomes `dev-eloper`), with a short suffix if that name is taken or
reserved. Asked for and refused, it is refused plainly — `400` for a bad
shape or a reserved word, `409` for one already taken — and nothing is
written, so the same link works again with another name.

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

## How people get accounts

There is no signing yourself up. An account comes from one of three
places, and each proves its address on the way in:

- **An invitation** from an admin of an organization, [accepted](#accepting-an-invitation)
  by the person it was mailed to.
- **An operator**, on the server. This is how the very first person gets
  in — there is nobody yet to invite them — and how an operator adds
  people in bulk:

  ```bash
  stratum-server admin user-create --org acme \
    --email you@acme.dev --name "Your Name" --password '…' --role owner
  ```

  `--role` defaults to `owner`; `--handle` picks the personal namespace,
  which is otherwise made from the address exactly as an invitation
  makes it. Run against an address that already has an account, it adds
  that account to the organization instead.
- **Your company's identity provider**, when the server is configured
  for [single sign-on](#single-sign-on). Anybody the provider signs in
  gets an account on their first visit — the provider is where people
  are admitted and turned away, not this server.

### Signing in with GitHub

When your server has a GitHub App configured, the sign-in screen offers
**Continue with GitHub** — unless it signs in with [SSO only](#sso-only),
when GitHub sign-in is off too. It signs you in to an account you
already have here; it never makes one. The round trip is the ordinary OAuth
one — `GET /v1/auth/github/start` sends you to GitHub's authorization
screen and GitHub returns you to `GET /v1/auth/github/callback`, which
redirects back into the dashboard with the outcome in `?github=`.

The first time, the account is found by address, and **only one address
is trusted**: the one GitHub reports as your **primary** and
**verified** — GitHub itself sent a link to it and saw it clicked. It
has to be the address your account here signs in with. An unproved
primary, or a GitHub App never granted the *Email addresses* permission,
lands you back on the sign-in screen saying so (`noemail`), with the
password path still open; an address that matches nobody here says that
too (`noaccount`), and the fix is an invitation. We never read `GET
/user`'s own `email` field: that is whatever you chose to publish, GitHub
does not check it, and trusting it would let somebody claim your account
here by typing your address into a profile.

That first sign-in links your GitHub account to this one, keyed on
GitHub's **numeric user id**, never your login. After it, the address no
longer matters: renaming yourself on GitHub, or changing your primary
address there, still lands you on your own account, and whoever claims
your old login next gets nothing.

### Single sign-on

With single sign-on configured, the sign-in screen offers **Continue
with** your provider — Okta, Entra ID, Google Workspace, Keycloak, or
anything else that speaks OpenID Connect. The operator's side of it is
in [operations.md](../operations.md#single-sign-on); this is what it
does for the people signing in.

`GET /v1/auth/sso/start` sends you to the provider, and the provider
returns you to `GET /v1/auth/sso/callback`, which redirects back into
the dashboard with the outcome in `?sso=`:

| Outcome | Means |
|---|---|
| `ok` | signed in |
| `denied` | you cancelled at the provider |
| `expired` | the round trip took too long, was started in another browser, or its code was already used — start again |
| `noemail` | the provider did not give an address this server trusts, so there is no account to find or make |
| `domain` | the address is outside the domains the operator said this provider speaks for |
| `disabled` | your account here has been switched off |
| `unavailable` | this server has no single sign-on |
| `error` | the provider did not answer, or answered something that did not check out; the server's log says which |

**Which account you land on**, in this order:

1. The one already linked to you at this provider. The link is keyed on
   the provider's own immutable id for you (`sub`), never on an address
   or a username — both of which the provider lets people change — so a
   renamed address still lands on the same account.
2. The account that signs in with your address, or has proved it as
   one of its own. This is how people who had a password account before
   SSO was switched on keep everything they had: their first SSO
   sign-in links them, and their role in every organization is
   untouched.
3. Nobody yet: a new account, with your address proved, a
   [handle](#accepting-an-invitation) made from your provider username
   or your address, and membership of the one organization the
   operator named, at the role the operator chose (`member` unless they
   said otherwise).

That last step happens once. Somebody an administrator takes out of the
organization afterwards is not put back into it the next time they sign
in: they still have an account, with nothing in it.

**Which addresses are trusted.** An address is only used to find or
make an account when the provider vouches for it — it marks it
`email_verified`, or the operator has named the domains this provider
speaks for and the address is in one of them. A provider that
explicitly says an address is *not* verified is believed. With a domain
list, nothing outside it gets in, not even a person already linked.

**Sessions last twelve hours** by default, not a password session's
fourteen days, and the next sign-in after that is a trip to the
provider, which usually needs no typing at all. The provider is where
people are switched off, and a session this server minted keeps working
until it expires; twelve hours means somebody disabled there is out by
the next working day.

#### SSO only

When single sign-on is configured, **it is the only way into the
dashboard** unless the operator says otherwise. Every other door is
closed:

- signing in with a password, changing it, and the forgotten-password
  pair all answer `403` naming the provider;
- signing in with GitHub redirects back with `?github=unavailable`;
- accepting an invitation needs you to be signed in through the provider
  as the invited address first, and joins that account to the
  organization rather than making a new one — so an invitation is how an
  administrator adds somebody to a *second* organization, or at a
  different role.

A password or a linked GitHub account would otherwise let somebody the
company switched off at its provider keep signing in here.

**Tokens and SSH keys are not sign-ins**, and keep working: they are
how `git`, CI and scripts reach the server, and they are not sessions.
The other side of that is offboarding. Disabling somebody at the
provider stops their next sign-in, and their session runs out within
the session length, but a personal access token or an SSH key they made
keeps working until it is revoked or the account is disabled here —
which is what `stratum-server admin user-disable` is for (see
[Offboarding](../operations.md#offboarding)). Run it as part of the
same checklist as the provider.

### Forgotten passwords

```bash
curl -X POST https://spool.example.com/v1/auth/forgot-password \
  -H "Content-Type: application/json" -d '{ "email": "you@example.dev" }'
curl -X POST https://spool.example.com/v1/auth/reset-password \
  -H "Content-Type: application/json" \
  -d '{ "token": "weftrs_…", "new_password": "a different long password" }'
```

A reset link lives for an hour, works once, and **ends every other
session on the account** — whoever asked for it may have done so because
somebody else was signed in. `forgot-password` answers `202` with the
same body whether or not the address has an account, and its limits are
per address and global rather than per source: behind a proxy this
server does not see a peer address it can trust, and a limit that
`X-Forwarded-For` can bypass is worse than an honest global one.

A disabled account gets no reset link. Recovering an account an operator
switched off would undo the switching off.

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
