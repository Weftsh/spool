#!/usr/bin/env python3
"""The single sign-on contract, against a real identity provider.

Driven by `scripts/manual-oidc.sh`, which holds the argument for why this
gate exists, the beliefs it checks, and what it needs. This file is the
steps. Standard library only: the signature is checked with Python's own
big integers, so the run needs nothing installed.

Every request is made the way `crates/stratum-server/src/oidc.rs` makes
it — the same scope, the same PKCE, the same client authentication with
the same encoding — because what is being checked is that the provider
answers *the server*, not that it answers somebody.
"""
import base64
import hashlib
import http.server
import json
import os
import pathlib
import secrets
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

ISSUER = os.environ["STRATUM_OIDC_ISSUER"].strip()
CLIENT_ID = os.environ["STRATUM_OIDC_CLIENT_ID"].strip()
CLIENT_SECRET = os.environ["STRATUM_OIDC_CLIENT_SECRET"].strip()
ALLOWED = [d.strip().lstrip("@").lower()
           for d in os.environ.get("STRATUM_OIDC_ALLOWED_DOMAINS", "").split(",") if d.strip()]
PORT = int(os.environ.get("STRATUM_OIDC_CONTRACT_PORT", "8766"))
REDIRECT = os.environ.get("STRATUM_OIDC_CONTRACT_REDIRECT", f"http://127.0.0.1:{PORT}/callback")
TIMEOUT = int(os.environ.get("STRATUM_OIDC_CONTRACT_TIMEOUT", "900"))

FIXTURES = pathlib.Path(os.environ.get(
    "STRATUM_OIDC_CONTRACT_FIXTURES",
    pathlib.Path(__file__).resolve().parents[2] / "crates/stratum-testkit/fixtures/oidc"))

GOOGLE = ISSUER.rstrip("/") == "https://accounts.google.com"

OBSERVED = {}
PASS, FAIL, NOTE = [], [], []


def ok(msg):
    PASS.append(msg)
    print(f"  \033[32mok\033[0m   {msg}")


def bad(msg):
    FAIL.append(msg)
    print(f"  \033[31mFAIL\033[0m {msg}")


def note(msg):
    NOTE.append(msg)
    print(f"  \033[33mNOTE\033[0m {msg}")


def family():
    host = urllib.parse.urlparse(ISSUER).hostname or ""
    if host.endswith(".okta.com") or host.endswith(".oktapreview.com"):
        return "okta"
    if host == "login.microsoftonline.com":
        return "entra"
    if GOOGLE:
        return "google"
    if "/realms/" in urllib.parse.urlparse(ISSUER).path:
        return "keycloak"
    return "other"


def secure(u):
    """`oidc::check_url`: https, or plain http to this machine only — so
    the contract can be run against the stack's stand-in provider."""
    p = urllib.parse.urlparse(u or "")
    return p.scheme == "https" or (p.scheme == "http" and p.hostname in ("127.0.0.1", "localhost", "::1"))


def b64url_decode(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def b64url(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def form_encode(s):
    """`mail::templates::urlencode`: everything but ASCII alphanumerics
    and `-._~` percent-encoded — the alphabet the server encodes each
    half of the Basic credentials with."""
    return "".join(c if (c.isascii() and c.isalnum()) or c in "-._~"
                   else "".join(f"%{b:02X}" for b in c.encode()) for c in s)


def request(method, url, data=None, headers=None):
    req = urllib.request.Request(url, data=data, method=method, headers={
        "Accept": "application/json", "User-Agent": "spool-manual-oidc", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def get_json(url, token=None):
    status, text = request("GET", url, headers={"Authorization": f"Bearer {token}"} if token else None)
    try:
        return status, json.loads(text)
    except ValueError:
        return status, {"_not_json": text[:300]}


def discovery():
    if "discovery" not in OBSERVED:
        status, doc = get_json(f"{ISSUER.rstrip('/')}/.well-known/openid-configuration")
        OBSERVED["discovery_status"] = status
        OBSERVED["discovery"] = doc
    return OBSERVED["discovery"]


def basic_auth(doc):
    """`discovery_from`'s rule: Basic unless discovery offers only POST."""
    methods = doc.get("token_endpoint_auth_methods_supported") or ["client_secret_basic"]
    return "client_secret_basic" in methods


def token_request(doc, fields, raw_basic=False):
    """The token call as `Oidc::exchange` makes it."""
    headers = {"Content-Type": "application/x-www-form-urlencoded"}
    fields = dict(fields)
    if basic_auth(doc):
        enc = (lambda s: s) if raw_basic else form_encode
        pair = f"{enc(CLIENT_ID)}:{enc(CLIENT_SECRET)}"
        headers["Authorization"] = "Basic " + base64.b64encode(pair.encode()).decode()
    else:
        fields["client_id"] = CLIENT_ID
        fields["client_secret"] = CLIENT_SECRET
    status, text = request("POST", doc["token_endpoint"],
                           data=urllib.parse.urlencode(fields).encode(), headers=headers)
    try:
        return status, json.loads(text)
    except ValueError:
        return status, {"_not_json": text[:300]}


def rsa_keys(jwks):
    """`keys_from`: RSA, for signing, RS256 or unmarked, 2048 bits up."""
    out = {}
    for k in jwks.get("keys", []):
        if k.get("kty") != "RSA" or k.get("use") not in (None, "sig") \
                or k.get("alg") not in (None, "RS256"):
            continue
        try:
            n = int.from_bytes(b64url_decode(k["n"]), "big")
            e = int.from_bytes(b64url_decode(k["e"]), "big")
        except (KeyError, ValueError):
            continue
        if n.bit_length() < 2048:
            continue
        out[k.get("kid", "")] = (n, e)
    return out


# DigestInfo for SHA-256, RFC 8017 §9.2 note 1.
SHA256_PREFIX = bytes.fromhex("3031300d060960864801650304020105000420")


def rs256_verifies(signing_input, sig, n, e):
    k = (n.bit_length() + 7) // 8
    if len(sig) != k:
        return False
    em = pow(int.from_bytes(sig, "big"), e, n).to_bytes(k, "big")
    t = SHA256_PREFIX + hashlib.sha256(signing_input).digest()
    return em == b"\x00\x01" + b"\xff" * (k - len(t) - 3) + b"\x00" + t


def step_discovery():
    print("\n\033[1m== discovery\033[0m")
    doc = discovery()
    if OBSERVED["discovery_status"] != 200 or "_not_json" in doc:
        bad(f"discovery answered {OBSERVED['discovery_status']}: {doc}")
        return
    # BELIEF 1
    if doc.get("issuer") == ISSUER:
        ok(f"discovery names the issuer exactly as configured: {ISSUER}")
    else:
        bad(f"discovery names issuer {doc.get('issuer')!r}, not {ISSUER!r} — the server "
            "compares them exactly and would refuse every sign-in. Configure the issuer "
            "the way discovery spells it.")
    for k in ("authorization_endpoint", "token_endpoint", "jwks_uri"):
        u = doc.get(k, "")
        if secure(u):
            ok(f"{k} is https" if u.startswith("https://") else f"{k} is http to this machine (a stand-in)")
        else:
            bad(f"{k} is {u!r}; the server requires https off this machine")
    if doc.get("userinfo_endpoint"):
        ok("discovery names a userinfo endpoint, for a token that carries no address")
    else:
        note("discovery names no userinfo endpoint: a token with no `email` has nowhere "
             "else to get one, and newcomers would be refused")
    # BELIEF 2
    algs = doc.get("id_token_signing_alg_values_supported") or []
    if "RS256" in algs:
        ok("RS256 ID tokens are offered")
    else:
        bad(f"id_token_signing_alg_values_supported is {algs}: the server takes RS256 only")
    pkce = doc.get("code_challenge_methods_supported")
    if pkce is None:
        note("discovery does not say whether it supports PKCE; `authorize` will show whether S256 is honoured")
    elif "S256" in pkce:
        ok("PKCE S256 is supported")
    else:
        bad(f"code_challenge_methods_supported is {pkce}: the server always sends S256")
    methods = doc.get("token_endpoint_auth_methods_supported")
    how = "HTTP Basic" if basic_auth(doc) else "the form body"
    ok(f"token_endpoint_auth_methods_supported is {methods}; the server will authenticate in {how}")

    status, jwks = get_json(doc.get("jwks_uri", ""))
    OBSERVED["jwks"] = jwks
    keys = rsa_keys(jwks) if status == 200 else {}
    if keys:
        ok(f"{len(keys)} usable RSA signing key(s) of 2048 bits or more")
    else:
        bad(f"the key set ({status}) holds no RSA signing key of 2048 bits the server can use")
    if "" in keys:
        note("a key has no `kid`: the server accepts a token without `kid` only when "
             "exactly one key is published")


def step_refused():
    print("\n\033[1m== refused\033[0m")
    doc = discovery()
    if "token_endpoint" not in doc:
        bad("no discovery to find the token endpoint in — run `discovery` first")
        return
    fields = {"grant_type": "authorization_code", "code": "weft-contract-not-a-code",
              "redirect_uri": REDIRECT, "code_verifier": secrets.token_urlsafe(32)}
    status, body = token_request(doc, fields)
    OBSERVED["token_error"] = {"status": status, "error": body.get("error")}
    err = body.get("error")
    # BELIEFS 3 and 4
    if 400 <= status < 500 and err == "invalid_grant":
        ok(f"a code the provider never issued is {status} invalid_grant — the client "
           "credentials were taken as the server sends them, and only the code refused")
    elif err in ("invalid_client", "unauthorized_client"):
        bad(f"the provider refused the client credentials ({status} {err}) as the server sends them")
        if basic_auth(doc) and form_encode(CLIENT_SECRET) != CLIENT_SECRET:
            status2, body2 = token_request(doc, fields, raw_basic=True)
            if body2.get("error") == "invalid_grant":
                bad("…and took them UNENCODED: this provider does not percent-decode Basic "
                    "credentials (RFC 6749 §2.3.1). The secret has characters the server "
                    "encodes; every sign-in will fail until `Oidc::exchange` sends it raw "
                    "for this provider or the secret is regenerated without them.")
            else:
                note("…and refused them unencoded too: the secret is most likely wrong")
    elif status == 200 and err:
        note(f"a made-up code is a 200 carrying error={err!r}. The server reads that as a "
             "refusal too; record it so the fake can say so")
    else:
        bad(f"a made-up code answered {status} {body}")


def catch_code(url, state):
    caught, done = {}, threading.Event()

    class H(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_a):
            pass

        def do_GET(self):
            u = urllib.parse.urlparse(self.path)
            if u.path != urllib.parse.urlparse(REDIRECT).path:
                self.send_response(404)
                self.send_header("content-length", "0")
                self.end_headers()
                return
            q = urllib.parse.parse_qs(u.query)
            for k in ("code", "state", "error", "error_description"):
                caught[k] = (q.get(k) or [""])[0]
            page = b"<h1>Caught it.</h1><p>Back to the terminal.</p>"
            self.send_response(200)
            self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(page)))
            self.end_headers()
            self.wfile.write(page)
            done.set()

    srv = http.server.HTTPServer(("127.0.0.1", PORT), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    print("\n  Open this and sign in, as an ordinary person the application is assigned to:\n")
    print(f"    {url}\n")
    print(f"  Waiting on {REDIRECT} for up to {TIMEOUT}s …")
    got = done.wait(timeout=TIMEOUT)
    srv.shutdown()
    if not got:
        bad(f"nobody signed in within {TIMEOUT}s")
        return None
    if caught.get("error"):
        bad(f"the provider answered error={caught['error']}: {caught.get('error_description')}")
        return None
    if caught.get("state") != state:
        bad(f"state came back as {caught.get('state')!r}, not the one sent")
        return None
    return caught.get("code")


def step_authorize():
    print("\n\033[1m== authorize\033[0m")
    doc = discovery()
    if "authorization_endpoint" not in doc:
        bad("no discovery — run `discovery` first")
        return
    state, nonce, verifier = (secrets.token_urlsafe(24) for _ in range(3))
    challenge = b64url(hashlib.sha256(verifier.encode()).digest())
    ep = doc["authorization_endpoint"]
    # `Oidc::authorize_url`, parameter for parameter.
    url = (f"{ep}{'&' if '?' in ep else '?'}response_type=code"
           f"&client_id={form_encode(CLIENT_ID)}&redirect_uri={form_encode(REDIRECT)}"
           f"&scope={form_encode('openid email profile')}&state={form_encode(state)}"
           f"&nonce={form_encode(nonce)}&code_challenge={form_encode(challenge)}"
           "&code_challenge_method=S256")
    code = catch_code(url, state)
    if not code:
        return
    status, tok = token_request(doc, {"grant_type": "authorization_code", "code": code,
                                      "redirect_uri": REDIRECT, "code_verifier": verifier})
    if status != 200 or "id_token" not in tok:
        bad(f"the code exchange answered {status}: {tok.get('error', tok)}")
        return
    ok("the code exchanges for an ID token, with PKCE S256 and the server's client authentication")
    # A wrong verifier must be refused too, or S256 is decoration — but a
    # code is single use, so that is only observable on a second sign-in.
    h, p, s = tok["id_token"].split(".")
    header = json.loads(b64url_decode(h))
    claims = json.loads(b64url_decode(p))
    OBSERVED.update(header=header, claims=claims, nonce=nonce)

    # BELIEF 5
    if header.get("alg") == "RS256":
        ok("the ID token is RS256")
    else:
        bad(f"the ID token's alg is {header.get('alg')!r}; the server refuses anything but RS256")
    status, jwks = get_json(doc["jwks_uri"])
    keys = rsa_keys(jwks)
    kid = header.get("kid")
    key = keys.get(kid) if kid is not None else (next(iter(keys.values())) if len(keys) == 1 else None)
    if key is None:
        bad(f"kid {kid!r} is not among the published keys {sorted(keys)}")
    elif rs256_verifies(f"{h}.{p}".encode(), b64url_decode(s), *key):
        ok(f"the signature verifies under the published key {kid!r} (PKCS#1 v1.5, SHA-256)")
    else:
        bad("the signature does NOT verify under the published key — the server would refuse it")

    # BELIEF 6 — the server's own rules run over these recorded claims in
    # `oidc_fixtures_parse_like_the_fake`; here they are checked once so a
    # run tells you now.
    issuers = [ISSUER] + (["accounts.google.com"] if GOOGLE else [])
    ok("iss is the configured issuer") if claims.get("iss") in issuers else \
        bad(f"iss is {claims.get('iss')!r}, not {ISSUER!r}")
    aud = claims.get("aud")
    auds = [aud] if isinstance(aud, str) else (aud if isinstance(aud, list) else [])
    if CLIENT_ID in auds and (claims.get("azp") in (None, CLIENT_ID)) \
            and not (len(auds) > 1 and claims.get("azp") is None):
        ok(f"aud ({type(aud).__name__}) and azp name this client")
    else:
        bad(f"aud {aud!r} / azp {claims.get('azp')!r} do not name {CLIENT_ID!r} the way the server requires")
    for k in ("exp", "iat"):
        v = claims.get(k)
        if isinstance(v, (int, float)) and not isinstance(v, bool):
            ok(f"{k} is a {type(v).__name__}")
        else:
            bad(f"{k} is {v!r}")
    now = time.time()
    if not (claims.get("exp", 0) + 120 >= now and claims.get("iat", 0) <= now + 120):
        bad("exp/iat are outside the server's two-minute allowance of this machine's clock")
    ok("the nonce came back") if claims.get("nonce") == nonce else \
        bad(f"nonce is {claims.get('nonce')!r}, not the one sent")
    ok("sub is present") if isinstance(claims.get("sub"), str) and 0 < len(claims["sub"]) <= 255 \
        else bad(f"sub is {claims.get('sub')!r}")

    email = claims.get("email")
    verified = claims.get("email_verified")
    where = "the ID token"
    if email:
        ok(f"the ID token carries an address; email_verified is {verified!r} ({type(verified).__name__})")
    else:
        note("the ID token carries no address (Entra's default); the server asks userinfo")
    # BELIEF 7
    ui = doc.get("userinfo_endpoint")
    if ui and tok.get("access_token"):
        status, info = get_json(ui, tok["access_token"])
        OBSERVED["userinfo"] = info if status == 200 else None
        if status != 200:
            (bad if not email else note)(f"userinfo answered {status}: {info}")
        elif info.get("sub") == claims.get("sub"):
            ok("userinfo answers for the same sub as the ID token")
        else:
            (bad if not email else note)(
                "userinfo answered for a DIFFERENT sub — the server believes nothing from it, "
                "so a token without an address signs in nobody new")
        if not email and status == 200 and info.get("sub") == claims.get("sub"):
            email, verified, where = info.get("email"), info.get("email_verified"), "userinfo"
            if email:
                ok(f"userinfo carries the address; email_verified is {verified!r}")
    elif not email:
        bad("no address in the token and no userinfo to ask: newcomers cannot be given accounts")

    # BELIEF 8 — `trusted_email`, over what really arrived.
    trust = trust_rule(email, verified, claims.get("hd") or (OBSERVED.get("userinfo") or {}).get("hd"))
    OBSERVED["trust"] = trust
    if trust == "email":
        ok(f"the server would trust this address (from {where}) and give a newcomer an account")
    elif trust == "domain":
        bad("the server would refuse this person as outside STRATUM_OIDC_ALLOWED_DOMAINS "
            f"({ALLOWED}) — sign in as somebody inside it, or fix the list")
    else:
        bad("the server would answer `noemail`: this provider does not vouch for the address "
            "(email_verified is not true) and STRATUM_OIDC_ALLOWED_DOMAINS does not cover it. "
            "Set the domain list, or have the provider mark addresses verified.")


def trust_rule(email, verified, hd):
    """`oidc::trusted_email`, line for line."""
    if not isinstance(email, str) or "@" not in email:
        return "noemail"
    email = email.strip().lower()
    flag = {True: True, False: False, "true": True, "false": False}.get(verified) \
        if isinstance(verified, (bool, str)) else None
    if flag is False:
        return "noemail"
    domain = email.rsplit("@", 1)[1]
    if GOOGLE:
        return "email" if hd and hd.lower() in ALLOWED and domain == hd.lower() else "domain"
    if ALLOWED:
        return "email" if domain in ALLOWED else "domain"
    return "email" if flag is True else "noemail"


def step_fixtures():
    print("\n\033[1m== fixtures\033[0m")
    if "claims" not in OBSERVED or "jwks" not in OBSERVED or "token_error" not in OBSERVED:
        bad("nothing complete was observed — run `all`")
        return
    fam = family()
    out = FIXTURES / fam
    out.mkdir(parents=True, exist_ok=True)
    claims = OBSERVED["claims"]
    real_domain = None
    for src in (claims.get("email"), (OBSERVED.get("userinfo") or {}).get("email")):
        if isinstance(src, str) and "@" in src:
            real_domain = src.rsplit("@", 1)[1].lower()
            break

    # Committed files, so: the provider's identity and the person's are
    # replaced, consistently, and only what the server reads is kept — a
    # whitelist, never a denylist over somebody else's schema.
    placeholder = ISSUER if GOOGLE else f"https://{fam}.example.test/issuer"
    tenant = None
    if fam == "entra":
        tenant = urllib.parse.urlparse(ISSUER).path.strip("/").split("/")[0]

    def domain_of(d):
        d = (d or "").lower()
        return "example.com" if d == real_domain or d == (claims.get("hd") or "").lower() \
            else f"other{ALLOWED.index(d) if d in ALLOWED else 9}.example"

    def scrub_str(v):
        if not isinstance(v, str):
            return v
        v = v.replace(ISSUER, placeholder)
        if tenant:
            v = v.replace(tenant, "00000000-0000-0000-0000-000000000000")
        if not GOOGLE:
            v = v.replace(urllib.parse.urlparse(ISSUER).netloc, f"{fam}.example.test")
        return v.replace(CLIENT_ID, "spool-client")

    def scrub(v):
        if isinstance(v, dict):
            return {k: scrub(x) for k, x in v.items()}
        if isinstance(v, list):
            return [scrub(x) for x in v]
        return scrub_str(v)

    sub = claims.get("sub")

    def person(c, keep):
        o = {}
        for k in keep:
            if k not in c:
                continue
            v = c[k]
            if k == "sub":
                v = "recorded-subject" if v == sub else "recorded-other-subject"
            elif k == "nonce":
                v = "recorded-nonce"
            elif k in ("email", "preferred_username") and isinstance(v, str):
                v = f"person@{domain_of(v.rsplit('@', 1)[1])}" if "@" in v else "person"
            elif k == "name" and isinstance(v, str):
                v = "Recorded Person"
            elif k == "hd" and isinstance(v, str):
                v = domain_of(v)
            elif k in ("aud", "azp", "iss"):
                v = scrub(v)
            o[k] = v
        return o

    d = OBSERVED["discovery"]
    keep_d = ("issuer", "authorization_endpoint", "token_endpoint", "jwks_uri", "userinfo_endpoint",
              "id_token_signing_alg_values_supported", "token_endpoint_auth_methods_supported",
              "code_challenge_methods_supported", "response_types_supported",
              "subject_types_supported", "scopes_supported", "claims_supported")
    write(out / "discovery.json", scrub({k: d[k] for k in keep_d if k in d}))
    write(out / "jwks.json", {"keys": [{k: key[k] for k in ("kty", "use", "alg", "kid", "n", "e")
                                        if k in key} for key in OBSERVED["jwks"].get("keys", [])]})
    write(out / "id-token-header.json", {k: OBSERVED["header"][k]
                                         for k in ("alg", "kid", "typ") if k in OBSERVED["header"]})
    reads = ("iss", "sub", "aud", "azp", "exp", "iat", "nonce", "email", "email_verified",
             "name", "preferred_username", "hd")
    write(out / "id-token-claims.json", person(claims, reads))
    if OBSERVED.get("userinfo") is not None:
        write(out / "userinfo.json", person(OBSERVED["userinfo"],
                                            ("sub", "email", "email_verified", "name", "hd")))
    elif (out / "userinfo.json").exists():
        (out / "userinfo.json").unlink()
    write(out / "token-error.json", OBSERVED["token_error"])
    write(out / "provenance.json", {
        "observed": True,
        "recorded_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "family": fam,
        "issuer": placeholder,
        "client_id": "spool-client",
        "client_auth": "basic" if basic_auth(d) else "post",
        "allowed_domains": [domain_of(a) for a in ALLOWED],
        "nonce": "recorded-nonce",
        "trust": OBSERVED.get("trust"),
        "claim_names": sorted(claims),
        "note": "written by scripts/manual-oidc.sh fixtures. The provider's host, tenant and "
                "client, and the person's identity, are replaced; the SHAPE is what these pin.",
    })
    ok(f"wrote {out}")
    print("       now run `cargo test -p stratum-server oidc_fixtures_parse_like_the_fake`: it holds the "
         "server's discovery, key, claim, userinfo and trust rules to these recordings, and goes "
         "red the moment the wire and what the fake taught the suite disagree.")
    if not GOOGLE and urllib.parse.urlparse(ISSUER).scheme == "http":
        note("this was a stand-in on this machine, not a provider: it claims nothing about any real one")


def write(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


STEPS = {"discovery": step_discovery, "refused": step_refused,
         "authorize": step_authorize, "fixtures": step_fixtures}


def main():
    args = sys.argv[1:]
    wanted = list(STEPS) if args == ["all"] else args
    for name in wanted:
        if name not in STEPS:
            print(f"unknown step {name!r}; one of: {', '.join(STEPS)}, all", file=sys.stderr)
            return 2
    print(f"provider: {family()} ({ISSUER})")
    for name in wanted:
        STEPS[name]()
    print("\n\033[1m== summary\033[0m")
    print(f"  {len(PASS)} checked, {len(FAIL)} failed, {len(NOTE)} not claimed")
    for n in NOTE:
        print(f"  \033[33mNOTE\033[0m {n}")
    for f in FAIL:
        print(f"  \033[31mFAIL\033[0m {f}")
    if FAIL:
        print("\n  A FAIL means this provider and the fake disagree, and the fake is what")
        print("  every test in the suite believes.")
        return 1
    if NOTE:
        print("\n  Nothing failed — but the NOTEs above were NOT checked.")
    print(f"\n  This run claims {family()} only.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
