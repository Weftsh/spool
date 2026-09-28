#!/usr/bin/env python3
"""The GitHub sign-in contract, against the real API.

Driven by `scripts/manual-github-signin.sh`, which holds the argument
for why this gate exists and what it needs. This file is the steps.

Every assertion here is about a belief `stratum-testkit`'s fake states
on our behalf. Where GitHub disagrees with the fake, this prints what it
actually answered and exits non-zero — the point is not that the call
worked, it is that the *shape* the product reads is the shape that
arrives.
"""
import http.server
import json
import os
import pathlib
import secrets
import sys
import threading
import urllib.error
import urllib.parse
import urllib.request

API = os.environ.get("STRATUM_GITHUB_API_BASE", "https://api.github.com").rstrip("/")
OAUTH = os.environ.get("STRATUM_GITHUB_OAUTH_BASE", "https://github.com").rstrip("/")
CLIENT_ID = os.environ["STRATUM_GITHUB_CLIENT_ID"]
CLIENT_SECRET = os.environ["STRATUM_GITHUB_CLIENT_SECRET"]
PORT = int(os.environ.get("STRATUM_GITHUB_SIGNIN_PORT", "8765"))
# GitHub compares `redirect_uri` as an exact string — `localhost` and
# `127.0.0.1` are different hosts to it, and a missing scheme is a
# different string again. Overridable so a run can match whatever the
# App actually has registered rather than the other way round.
REDIRECT = os.environ.get("STRATUM_GITHUB_SIGNIN_REDIRECT",
                          f"http://127.0.0.1:{PORT}/callback")
# How long to hold the callback open. GitHub shows a "Be careful!"
# warning page before the approve button — which is a page a person is
# meant to actually read — and five minutes turned out to be tight
# enough to time out a real run mid-read.
APPROVE_TIMEOUT = int(os.environ.get("STRATUM_GITHUB_SIGNIN_TIMEOUT", "900"))

FIXTURES = pathlib.Path(__file__).resolve().parents[2] / "crates/stratum-testkit/fixtures/github-signin"

# What `authorize` observed, for `fixtures` to write out.
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


def get(url, token):
    """A GET as the product makes it, returning (status, body-text)."""
    req = urllib.request.Request(url, headers={
        "Authorization": f"Bearer {token}",
        "Accept": "application/vnd.github+json",
        "User-Agent": "spool-manual-github-signin",
    })
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def exchange(code, client_id=CLIENT_ID, client_secret=CLIENT_SECRET):
    """The token exchange, exactly as `GithubApp::exchange_code` sends it."""
    body = urllib.parse.urlencode({
        "client_id": client_id,
        "client_secret": client_secret,
        "code": code,
    }).encode()
    req = urllib.request.Request(
        f"{OAUTH}/login/oauth/access_token",
        data=body,
        headers={"Accept": "application/json",
                 "User-Agent": "spool-manual-github-signin"},
    )
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def catch_code(state, client_id=None):
    """Serve one redirect on the local callback and hand back its `code`."""
    caught = {}
    done = threading.Event()

    class H(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_a):
            pass

        def do_GET(self):
            u = urllib.parse.urlparse(self.path)
            if u.path != "/callback":
                self.send_response(404)
                self.send_header("content-length", "0")
                self.end_headers()
                return
            q = urllib.parse.parse_qs(u.query)
            caught["code"] = (q.get("code") or [""])[0]
            caught["state"] = (q.get("state") or [""])[0]
            caught["error"] = (q.get("error") or [""])[0]
            page = b"<h1>Caught it.</h1><p>Back to the terminal.</p>"
            self.send_response(200)
            self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(page)))
            self.end_headers()
            self.wfile.write(page)
            done.set()

    srv = http.server.HTTPServer(("127.0.0.1", PORT), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    # An empty REDIRECT omits the parameter, and GitHub then redirects to
    # the App's *registered* callback URL. That is the one form that
    # cannot mismatch: GitHub compares `redirect_uri` as an exact string,
    # so a missing scheme or `localhost` for `127.0.0.1` is a refusal
    # after login, where it is easy to mistake for something else.
    url = (f"{OAUTH}/login/oauth/authorize?client_id={urllib.parse.quote(client_id or CLIENT_ID)}"
           f"&state={urllib.parse.quote(state)}")
    if REDIRECT:
        url += f"&redirect_uri={urllib.parse.quote(REDIRECT, safe='')}"
    print("\n  Open this and approve, as the person signing in:\n")
    print(f"    {url}\n")
    print(f"  Waiting on {REDIRECT} for up to {APPROVE_TIMEOUT}s …")
    if not done.wait(timeout=APPROVE_TIMEOUT):
        srv.shutdown()
        bad(f"nobody approved within {APPROVE_TIMEOUT}s")
        return None
    srv.shutdown()
    if caught.get("error"):
        bad(f"GitHub refused at the authorization screen: {caught['error']}")
        return None
    # The same check the product's callback makes, for the same reason:
    # a state that came back different is not the flow we started.
    if caught.get("state") != state:
        bad(f"state came back as {caught.get('state')!r}, not the one we sent")
        return None
    return caught.get("code")


def step_authorize():
    print("\n\033[1m== authorize\033[0m")
    state = secrets.token_urlsafe(24)
    code = catch_code(state)
    if not code:
        return

    status, text = exchange(code)
    try:
        tok = json.loads(text)
    except ValueError:
        bad(f"token exchange did not answer JSON: {status} {text[:200]}")
        return
    if "error" in tok:
        bad(f"token exchange refused a fresh code: {tok}")
        return
    token = tok.get("access_token")
    if not token:
        bad(f"token exchange answered no access_token: {tok}")
        return
    ok("a fresh code exchanges for a user token")

    # BELIEF 4: GET /user carries a numeric id.
    status, text = get(f"{API}/user", token)
    if status != 200:
        bad(f"GET /user answered {status}: {text[:200]}")
        return
    user = json.loads(text)
    OBSERVED["user"] = user
    if isinstance(user.get("id"), int):
        ok(f"GET /user carries a numeric id ({user['id']}), the field an account is keyed on")
    else:
        bad(f"GET /user `id` is {user.get('id')!r}, not an integer — "
            "`identities.provider_user_id` keys on it")
    if isinstance(user.get("login"), str) and user["login"]:
        ok(f"GET /user carries a login ({user['login']}), which becomes the default namespace")
    else:
        bad(f"GET /user `login` is {user.get('login')!r}")

    # BELIEFS 1 and 2: the shape the whole feature rests on.
    status, text = get(f"{API}/user/emails?per_page=100", token)
    if status == 403:
        bad("GET /user/emails answered 403 — this App does not hold the "
            "`Email addresses` account permission, so every real sign-in "
            "would land on `github=noemail`. Grant it and run this again.")
        return
    if status != 200:
        bad(f"GET /user/emails answered {status}: {text[:200]}")
        return
    emails = json.loads(text)
    OBSERVED["emails"] = emails
    if isinstance(emails, list):
        ok("GET /user/emails answers a flat array, as the fake says")
    else:
        bad(f"GET /user/emails answered {type(emails).__name__}, not a list — "
            "`verified_primary_email` iterates it directly")
        return
    shaped = [e for e in emails
              if isinstance(e, dict) and isinstance(e.get("email"), str)
              and isinstance(e.get("primary"), bool)
              and isinstance(e.get("verified"), bool)]
    if len(shaped) == len(emails) and emails:
        ok("every entry carries `email`, `primary` and `verified` as separate fields")
    else:
        bad(f"an entry is not the shape the product reads: {emails[:2]}")
        return
    primaries = [e for e in emails if e["primary"]]
    if len(primaries) == 1:
        ok("exactly one entry is primary")
    else:
        bad(f"{len(primaries)} entries are primary — `verified_primary_email` "
            "takes the first and would be picking arbitrarily")

    # And the product's own rule, applied to what really arrived.
    chosen = next((e["email"] for e in emails if e["primary"] and e["verified"]), None)
    if chosen:
        ok(f"the rule the sign-in uses finds a proved address: {chosen}")
    else:
        note("this account's primary address is not verified on GitHub, so a real "
             "sign-in would answer `github=noemail` — correct, but it means this "
             "run did not watch a proved address arrive. Verify the primary "
             "address on this GitHub account and run it again to claim that.")
    # Never the published field: it is not checked by GitHub and is empty
    # for anybody who keeps it private.
    if user.get("email") and chosen and user["email"] != chosen:
        note(f"GET /user `email` is {user['email']!r}, which differs from the "
             f"proved primary {chosen!r} — the product reads the second, and "
             "this is why")


def step_refused():
    print("\n\033[1m== refused\033[0m")
    status, text = exchange("code_this_was_never_issued")
    try:
        body = json.loads(text)
    except ValueError:
        bad(f"a bad code did not answer JSON: {status} {text[:200]}")
        return
    # BELIEF 5: the trap. A 4xx here would mean our client is checking
    # the wrong thing, and a client that checked only the status would
    # carry an empty token into the next call.
    if status == 200 and body.get("error"):
        ok(f"a code GitHub never issued is a 200 carrying error={body['error']!r}, "
           "which is why the client checks the body and not the status")
    else:
        bad(f"a bad code answered {status} {body} — `exchange_code` reads the "
            "`error` field on a 200 and would miss this")
    OBSERVED["refused"] = {"status": status, "body": body}


def step_noperm():
    print("\n\033[1m== noperm\033[0m")
    cid = os.environ.get("STRATUM_GITHUB_SIGNIN_NOEMAIL_CLIENT_ID")
    sec = os.environ.get("STRATUM_GITHUB_SIGNIN_NOEMAIL_CLIENT_SECRET")
    if not cid or not sec:
        note("no second App configured (STRATUM_GITHUB_SIGNIN_NOEMAIL_CLIENT_ID / "
             "_SECRET), so belief 3 — that an App without `Email addresses` is "
             "answered 403 and not an empty array — is NOT claimed. It cannot be "
             "checked under a client that holds the permission.")
        return
    note("`noperm` needs the same browser approval as `authorize`, under the "
         "second App. Approve when the URL appears.")
    state = secrets.token_urlsafe(24)
    # The second App needs the same local callback registered on it too.
    code = catch_code(state, client_id=cid)
    if not code:
        return
    status, text = exchange(code, cid, sec)
    tok = json.loads(text)
    token = tok.get("access_token")
    if not token:
        bad(f"the second App's exchange answered no token: {tok}")
        return
    status, text = get(f"{API}/user/emails?per_page=100", token)
    # What matters to the product is only that a refusal is *not* a 200
    # carrying an empty array — `verified_primary_email` maps every
    # non-2xx to `None` alike, so 403 and 404 are the same to it. Which
    # one GitHub picks is a fact about GitHub, recorded rather than
    # assumed: a `gh` token missing the `user` scope answers **404**,
    # not the 403 the fake states, so the fake's belief is already in
    # doubt and this run is what settles it for the App-permission case.
    if status in (403, 404):
        ok(f"an App without `Email addresses` is refused with {status}, not an "
           "empty array — so a client that read an empty list as 'no proved "
           "address' would be wrong here, and ours is not")
        if status != 403:
            note(f"the fake answers 403 where GitHub answered {status}. The "
                 "product cannot tell them apart, but the fake should say what "
                 "GitHub says — update `EmailMode::Forbidden` in fake_github.rs.")
    elif status == 200 and json.loads(text) == []:
        bad("an App without `Email addresses` answered 200 with an EMPTY ARRAY. "
            "`verified_primary_email` treats that as None so the product is "
            "right by luck — but a client checking only the status would read "
            "'no proved address' for every person alive. The fake must say so.")
    else:
        bad(f"an App without `Email addresses` answered {status}: {text[:200]}")
    OBSERVED["noperm"] = {"status": status}


def step_fixtures():
    print("\n\033[1m== fixtures\033[0m")
    if "emails" not in OBSERVED:
        bad("nothing was observed — run `authorize` first (or use `all`)")
        return
    FIXTURES.mkdir(parents=True, exist_ok=True)
    # Addresses, logins and profile fields are personal data and these
    # files are committed, so the recorded bodies keep their *shape* and
    # lose their content. A **whitelist**, not a denylist: the first
    # version of this scrubbed a handful of named fields and still wrote
    # out `notification_email`, `blog`, `location`, `twitter_username`
    # and the account's login inside a dozen `*_url` fields. A denylist
    # over somebody else's schema is a leak waiting for the provider to
    # add a field.
    #
    # What is kept is exactly what the product reads, plus the two
    # fields whose *presence* is the belief: `id` must be a number, and
    # `email` is kept precisely because it is so often **null** — see
    # the note below.
    keep_user = ("id", "login", "name", "type", "email")
    user = {k: OBSERVED["user"].get(k) for k in keep_user}
    user["login"] = "octocat"
    user["name"] = "Person Octocat"
    keep_email = ("email", "primary", "verified", "visibility")

    def scrub_email(e, i):
        out = {k: e.get(k) for k in keep_email}
        out["email"] = f"person{i}@example.com"
        return out

    (FIXTURES / "user-emails.json").write_text(
        json.dumps([scrub_email(e, i) for i, e in enumerate(OBSERVED["emails"])],
                   indent=2) + "\n")
    prov = {
        "recorded_at": __import__("datetime").datetime.now(
            __import__("datetime").timezone.utc).isoformat(),
        "api_base": API,
        "oauth_base": OAUTH,
        "beliefs": {
            "emails_is_a_flat_array": isinstance(OBSERVED["emails"], list),
            "entries_carry_primary_and_verified_separately": True,
            "exactly_one_primary": sum(1 for e in OBSERVED["emails"] if e["primary"]) == 1,
            "user_id_is_numeric": isinstance(OBSERVED["user"].get("id"), int),
            "bad_code_is_200_with_error": OBSERVED.get("refused", {}).get("status") == 200,
            "no_permission_is_403": OBSERVED.get("noperm", {}).get("status"),
        },
        "note": "addresses and the login are scrubbed; the SHAPE is what these pin.",
    }
    (FIXTURES / "provenance.json").write_text(json.dumps(prov, indent=2) + "\n")
    ok(f"wrote {FIXTURES}")
    note("now run `cargo test -p stratum-server github_signin_fixtures_parse_like_the_fake` "
         "(mirror/origin.rs): it parses these the way the fake answers, and goes red "
         "the moment the recorded wire and the fake disagree.")


STEPS = {
    "authorize": step_authorize,
    "refused": step_refused,
    "noperm": step_noperm,
    "fixtures": step_fixtures,
}


def main():
    args = sys.argv[1:]
    wanted = ["authorize", "refused", "noperm", "fixtures"] if args == ["all"] else args
    for name in wanted:
        if name not in STEPS:
            print(f"unknown step {name!r}; one of: {', '.join(STEPS)}, all", file=sys.stderr)
            return 2
    for name in wanted:
        STEPS[name]()
    print("\n\033[1m== summary\033[0m")
    print(f"  {len(PASS)} checked, {len(FAIL)} failed, {len(NOTE)} not claimed")
    for n in NOTE:
        print(f"  \033[33mNOTE\033[0m {n}")
    for f in FAIL:
        print(f"  \033[31mFAIL\033[0m {f}")
    if FAIL:
        print("\n  A NOTE is not a pass. A FAIL means the fake and GitHub disagree,")
        print("  and the fake is what every test in the suite believes.")
        return 1
    if NOTE:
        print("\n  Nothing failed — but the NOTEs above were NOT checked.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
