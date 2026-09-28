# Git over SSH

Every repo is reachable over SSH as well as HTTPS:

```bash
git clone ssh://git@ssh.weft.sh/acme/session-8412.git
```

The `ssh_clone_url` field on any repo response gives you the exact URL, or
`null` if the deployment has not enabled the SSH door.

SSH needs no domain and no certificate, so on a fresh deployment it is the
fully-encrypted git path from the first minute — before DNS and ACM are
sorted out.

## Registering a key

A key never carries permissions of its own. It names something that does,
and you choose which when you register it.

**Your own key** — the normal case, and the one the dashboard offers
under Settings → SSH keys. Sign in, paste the contents of your public key
file, done:

```bash
curl -X POST https://api.weft.sh/v1/orgs/acme/ssh-keys \
  -b "$COOKIE_JAR" -H 'Content-Type: application/json' \
  -d '{"public_key":"ssh-ed25519 AAAA… you@laptop","label":"laptop"}'
```

It signs in as *you*, so its authority is re-resolved from your role on
every connection — per-repo grants included, in both directions. Change a
role or a grant in the dashboard and the next `git push` from that laptop
obeys it; there is no key to re-issue and nothing cached to expire.

**Register it once, and it works in every namespace you belong to.** A
personal key names a person, not a namespace, so one laptop key clones
from your own repos and from every organization you have joined. Which
one you are reaching is decided by the URL, where your membership is
checked. Adding the same key a second time is refused — it is already
yours — and revoking it revokes it everywhere, because there is only one
of it.

The key list is likewise one list. It is the same whichever
organization's Settings page you open, because these are your keys and
they reach everywhere you do; a list that changed per page would be a
lie about what the key can do.

**A deploy key** names a token instead, and inherits that token's scopes
and repo binding. This is what an unattended machine wants, and creating
one needs `org:admin`:

```bash
curl -X POST https://api.weft.sh/v1/orgs/acme/ssh-keys \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"public_key":"ssh-ed25519 AAAA… deploy@ci",
       "token_id":"'"$TOKEN_ID"'",
       "label":"ci"}'
```

The response carries `fingerprint_sha256`, which matches what
`ssh-keygen -lf key.pub` prints locally — check it if you want to be sure
the server stored the key you meant.

List and revoke:

```bash
curl  https://api.weft.sh/v1/orgs/acme/ssh-keys      -H "Authorization: Bearer $TOKEN"
curl -X DELETE https://api.weft.sh/v1/orgs/acme/ssh-keys/<id> -H "Authorization: Bearer $TOKEN"
```

An admin sees and can revoke every deploy key in the org; you see and can
revoke your own personal keys. Revoking the key — or the token, the
membership, or the account behind it — takes effect on the next
connection. There is no cached session to outlive it.

A disabled account reaches nothing: its keys stop authenticating, exactly
as its tokens stop being credentials — the connection is refused before a
repository is named, so a disabled key cannot even read a public one. A
key that *is* live and simply has no role in a namespace reads that
namespace's public repositories and is answered "not found" for everything
else, the same masked answer a namespace you were never in gives, so a key
cannot be used to map who still works where.

## What the SSH user name means: nothing

Connect as `git@`, `root@`, or `anything@` — it makes no difference. **The
key is the only credential.** The username is not a trust boundary and is
never consulted, which is why the docs use `git@` purely by convention.

A key that resolves to read-only authority cannot push, whatever it
connects as; `git-receive-pack` is refused with an in-band error your git
client prints as `remote error: weft: you can read acme/widget but not
push to it; fork it and open a change from your fork, or ask an owner for
write access`. The same key reads any public repository, in any
namespace — a maintainer's key fetches a contributor's fork, a
contributor's key fetches the upstream — and a repository it cannot read
at all answers `repository not found`, whichever way it is asked.

## Host keys

The server presents one fleet-stable host key. Every node presents the
same one, so a client that pinned it on first connect keeps trusting the
fleet across restarts, deploys, and scale events — a per-node key would
look like a machine-in-the-middle attack to every user.

Operators: this is `STRATUM_SSH_HOST_KEY`, and booting with an SSH bind
but no host key is a deliberate startup failure rather than a
generated-per-boot key.

## Protocol notes

- **Protocol v2 is required.** git ≥ 2.26 sends it by default; if you have
  pinned `protocol.version=0` somewhere, the server tells you so in-band
  rather than misparsing the request.
- **The same engine serves both transports.** Clones over SSH read the
  same immutable segments as HTTPS clones, and are `git fsck --full
  --strict` clean under the same test gate.
- **CDN offload works over SSH too.** The advertised pack URL is HTTPS
  even when the negotiation rode SSH — see
  [CDN-offloaded clones](cdn-offload.md).

## Limits

- SSH serves **git only**. There is no shell, no SFTP, no port forwarding,
  and no command other than `git-upload-pack` / `git-receive-pack` is
  accepted.
- A push to a mirror over SSH is forwarded to its origin exactly as over
  HTTPS, and refused with the same in-band sentence when it cannot be —
  no credential behind the mirror, or an installation that has not
  approved `Contents: write`.
