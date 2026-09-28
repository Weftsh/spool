# CDN-offloaded clones

A clone normally streams every byte through a server node. With offload,
the bulk comes straight from a CDN instead, and the node streams nothing.
It is available when whoever runs your server has put a CDN in front of
it — the AWS reference deployment uses CloudFront.

This uses git's own **`packfile-uri`** capability (git ≥ 2.34) — no
custom client, no wrapper, no plugin.

## Turning it on

It is **opt-in per client**, because git only follows an advertised pack
URL when you have told it which protocols are acceptable:

```bash
git -c fetch.uriprotocols=https clone https://spool.example.com/acme/session-8412.git
```

To make it the default for yourself:

```bash
git config --global fetch.uriprotocols https
```

For a CI image, set it once in the image's global git config and every
job that uses the image inherits it.

**Nothing changes for clients that have not opted in.** They clone exactly
as before and never contact the CDN. There is no flag day and no
compatibility risk to machines you do not control.

## What you get

The pack is immutable — named by tip and content hash. Where the CDN
reads it straight from the bucket, the edge caches it, and a CI fleet
cloning the same repo all day fetches it from the nearest edge location
rather than assembling it at the server every time.

Offloaded clones are metered separately as `cdn_clone`, so you can see the
split on the [metrics endpoint](metrics.md).

## When it engages

Offload engages while the repo's CDN pack covers the **current tip**.

After a push the pack lags, and until the background packer catches up
(seconds) clones are served inline — correct, just not offloaded. So the
pattern that benefits most is **clone-heavy and push-light**: mirrors,
release repositories, anything your CI pulls constantly. A repo under
continuous push churn spends most of its time lagging and offloads rarely.

Serving a lagging pack is deliberately *not* attempted. The remainder
would have to be streamed inline, and a push's pack is **thin** — its
deltas refer back to objects that are inside the CDN pack. git indexes
the inline pack before it downloads the advertised URLs, so those bases
would be missing and the clone would fail. That failure depends on
whether your commits happen to touch similar files, which makes it
exactly the kind of bug that passes a test suite and breaks in
production. We do not go near it.

## Authorization

Every repository is private, and git sends **no credentials at all** when
it fetches an advertised pack URL, so authorization lives in the URL
itself: a short-lived signed URL that the edge (or the server) validates
before it serves a byte. URLs expire (an hour by default). A signature
that is wrong, expired or for another pack is answered `404`. When the
CDN's origin is the server rather than the bucket, the pack is served
`Cache-Control: private, no-store`, so no shared cache in between keeps
a copy past the signature's life.

## Failure behaviour

- **A pack that cannot be confirmed present is never advertised.** git
  does not fall back to the server for a URL it was given, so the server
  checks the object exists before offering it.
- **A corrupt or truncated pack fails loudly.** git verifies what it
  downloads; you get a failed clone and no repository, never a
  silently-incomplete one.
- **Whoever runs the server can turn it off everywhere** with
  `STRATUM_CDN_ENABLED=0`; clones keep working, served inline.

## Over SSH

Offload is a property of the git protocol, not of HTTP, so an SSH clone
offloads exactly the same way — the negotiation rides SSH and the
advertised pack URL is HTTPS. See [Git over SSH](ssh.md).
