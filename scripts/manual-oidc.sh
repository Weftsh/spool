#!/usr/bin/env bash
# The single sign-on contract, against a real identity provider. A
# manual gate.
#
# What this proves that CI cannot
# ------------------------------
# Signing in with SSO makes an account for anybody the company's
# provider vouches for, and links an existing account by address. Every
# automated test of it runs against `stratum-testkit`'s fake provider
# (`oidc.rs`), which we wrote from the OpenID Connect specification and
# from what the providers' documentation says — and this repository has
# paid for trusting a fake three times already: the `Retry-After` rate
# limit, the mirror push's `atomic transaction failed`, and GitHub's
# just-in-time runner labels. A fake encodes what we believe the
# provider does, and a suite built on a fake that is wrong is green
# precisely where the product is broken.
#
# The beliefs this gate exists to check:
#
#    1. Discovery names the issuer **exactly** as configured. The server
#       compares character for character (the Discovery rule, and what
#       stops a document from elsewhere steering it to another's keys).
#       Entra: configure the tenant by its GUID, not
#       `contoso.onmicrosoft.com`; discovery answers with the GUID.
#    2. The provider offers RS256 ID tokens, and publishes RSA signing
#       keys of at least 2048 bits, each under a `kid`.
#    3. The token endpoint takes the client credentials **as the server
#       sends them**: HTTP Basic with each half form-encoded first (RFC
#       6749 §2.3.1), or in the body when discovery offers only that. A
#       provider that does not percent-decode Basic credentials refuses a
#       secret with `~` or `%` in it, and every sign-in fails.
#    4. A code the provider never issued is a **4xx carrying
#       `error=invalid_grant`** — the client was authenticated and only
#       the code refused. (GitHub's OAuth does the opposite: a 200. The
#       two are different protocols and the server reads both.)
#    5. A real ID token is RS256 under a `kid` in the key set, and its
#       signature verifies (PKCS#1 v1.5, SHA-256) — checked here in pure
#       Python, and its claims then held to the server's own rules by
#       the fixture test.
#    6. `iss`, `aud`/`azp`, `exp`/`iat`, `nonce` and `sub` are the shapes
#       `check_claims` reads: `aud` a string or an array, times numbers.
#    7. Where the token has no `email` (Entra's default), userinfo
#       answers for the **same `sub`**. Entra's is pairwise; if userinfo
#       ever answered differently, every newcomer would be refused.
#    8. The trust rule, applied to what really arrived, admits this
#       person: `email_verified` really `true` (a boolean, or the string
#       some providers send), or the domain in STRATUM_OIDC_ALLOWED_DOMAINS
#       — and, for Google, the Workspace `hd`.
#
#   scripts/manual-oidc.sh discovery   # beliefs 1–2: no browser
#   scripts/manual-oidc.sh refused     # beliefs 3–4: a made-up code, no browser
#   scripts/manual-oidc.sh authorize   # beliefs 5–8: a person signs in once
#   scripts/manual-oidc.sh fixtures    # the observed shapes → crates/stratum-testkit/fixtures/oidc/<provider>
#   scripts/manual-oidc.sh all
#
# What it needs
# -------------
#   STRATUM_OIDC_ISSUER               the issuer you DEPLOY with, exactly
#   STRATUM_OIDC_CLIENT_ID            the client you deploy with
#   STRATUM_OIDC_CLIENT_SECRET        its secret
#   STRATUM_OIDC_ALLOWED_DOMAINS      (optional) as deployed — belief 8 is judged under it
#   STRATUM_OIDC_CONTRACT_PORT        (optional) default 8766 — the local callback
#   STRATUM_OIDC_CONTRACT_REDIRECT    (optional) default http://127.0.0.1:8766/callback
#   STRATUM_OIDC_CONTRACT_TIMEOUT     (optional) default 900 — seconds to wait for the sign-in
#
# One-time setup at the provider
# ------------------------------
# `authorize` catches the redirect itself, so the client must also list
#
#   http://127.0.0.1:8766/callback
#
# as a redirect URI, beside the deployment's own
# `<STRATUM_PUBLIC_URL>/v1/auth/sso/callback`. Okta, Entra ID, Google and
# Keycloak all allow a loopback `http` redirect. Remove it afterwards if
# your policy says so.
#
# Run it under the client you DEPLOY with, and sign in as an ordinary
# person the application is assigned to — not an administrator of the
# provider, whose account is often the one with every claim configured.
#
# Run it once per provider you support. Each run records under its own
# directory (okta, entra, google, keycloak, other), and a run against
# Okta has not claimed Entra — the same way a single-addressing-style S3
# run does not claim the other style.
#
# What it cannot prove
# --------------------
# A person has to sign in, which cannot be automated without a password
# this script should never hold. `authorize` without a person is a
# timeout, and says so. Cancelling at the provider (`denied`) is not
# checked: several providers have no cancel button on their sign-in
# page, and the server answers any error other than `access_denied` with
# `sso=error`, which is not wrong for a cancel either.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

if [ $# -lt 1 ]; then
  sed -n '2,93p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi

for v in STRATUM_OIDC_ISSUER STRATUM_OIDC_CLIENT_ID STRATUM_OIDC_CLIENT_SECRET; do
  [ -n "${!v:-}" ] || { echo "$v is not set (as the deployment sets it)" >&2; exit 2; }
done
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

exec python3 "$here/manual-oidc/contract.py" "$@"
