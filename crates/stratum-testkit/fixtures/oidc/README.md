# What identity providers answer

One directory per provider family — `okta`, `entra`, `google`,
`keycloak`, `other` — written by `scripts/manual-oidc.sh fixtures` from
a real sign-in under the client a deployment uses, and `belief`, which
is what `stratum-testkit`'s fake provider answers and says
`"observed": false`.

Each holds the discovery document, the key set, an ID token's header
and the claims the server reads, the userinfo answer, and what the token
endpoint said to a code it never issued. The provider's host, tenant
and client and the person's identity are replaced consistently across
the files; the *shape* — which claims exist, their types, the
algorithm, the client authentication discovery offers — is what these
pin.

`oidc_fixtures_parse_like_the_fake` (`crates/stratum-server/src/oidc.rs`)
runs every directory through the server's own `discovery_from`,
`keys_from`, `check_claims`, `merge_userinfo` and `trusted_email`, and
through the token endpoint's refusal classification. A recording the
server cannot read is a red test, not a surprise at a customer.
