#!/usr/bin/env python3
"""A GitHub-shaped responder for the manual walkthrough.

Answers the App endpoints the product calls, plus a stand-in for the
install page GitHub would show. `stratum-testkit`'s `fake_github` is the
authority on this behaviour and is what `cargo test` uses; this exists
because the manual pass drives the *built binary* against a real stack,
which needs a process rather than a library. Without it the connect flow
is a screen nobody ever clicks — the way per-user SSH keys were
registered and revoked for weeks without anything ever cloning with one.

The install stand-in is what makes the round trip real: it redirects
straight back to the server's `/v1/github/setup` with an
`installation_id`, the `state` it was handed, and — as GitHub does when
the App requests user authorization during installation — a `code` the
server exchanges for the installer's token. `/apps/stratum/installations/update`
is GitHub's return after an installation is *edited*: the same, with no
`state`, which the server binds to the org the signed-in person began a
connect for. `code_owning_<id>` names a person who controls exactly that
installation; any other code is refused the way GitHub refuses a spent
one.

`/login/oauth/authorize` is the same idea for the other flow: signing
*in* with GitHub. It redirects back to the server's own callback with
the state it was handed, so the anti-CSRF cookie check the callback does
is a check the walkthrough really passes rather than one it steps around.
`/user` and `/user/emails` then answer who arrived — and, crucially,
whether GitHub has *proved* their primary address, which is the single
fact the sign-in trusts when it skips our confirmation mail.
"""
import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, quote, urlparse

# Where to send the browser back to. The server's own public URL.
STRATUM = os.environ.get("STRATUM_PUBLIC_URL", "http://127.0.0.1:8080")

INSTALLATIONS = [
    {"id": 4001, "account": {"login": "acme-inc"}},
    {"id": 4002, "account": {"login": "ada"}},
]

# One installation's detail, as `GET /app/installations/{id}` answers it.
# Mirrors `installation_json` in crates/stratum-testkit/src/fake_github.rs
# exactly: 4001 is an organisation and 4002 a person, both holding
# everything the App asks for today — `contents: write` being the one
# that lets a push to a mirror be forwarded to its origin. 4003 and 4007
# installed the App before it asked for that and have not approved the
# change, which is how a real installation that predates a manifest
# change looks.
FULL_PERMISSIONS = {"actions": "read", "contents": "write", "issues": "read",
                    "metadata": "read"}
OLD_PERMISSIONS = {"actions": "read", "contents": "read", "issues": "read",
                   "metadata": "read"}
EVENTS = ["push"]
INSTALLATION_DETAIL = {
    "4001": ("acme-inc", "Organization", True),
    "4002": ("ada", "User", True),
    "4003": ("noadmin-inc", "Organization", False),
    "4007": ("prepush-inc", "Organization", False),
}


def installation_json(id_, login, target_type, full):
    return {
        "id": int(id_),
        "account": {"login": login, "type": target_type},
        "target_type": target_type,
        "permissions": FULL_PERMISSIONS if full else OLD_PERMISSIONS,
        "events": EVENTS,
        "suspended_at": None,
    }


# The three sign-in code shapes, alongside the install flow's two.
# `code_as_<id>_<login>` is somebody with a proved primary address,
# `code_unverified_…` somebody whose primary GitHub has not proved, and
# `code_noemail_…` an App that may not read addresses at all. Kept
# identical to the Rust fake in `stratum-testkit` so a walkthrough and
# the e2e suite are driving the same provider.
SIGNIN_PREFIXES = ("as_", "unverified_", "noemail_", "badmail_")


def _who_from_code(code):
    """The user token a `code` mints, or None for a code GitHub refuses."""
    if code.startswith("code_owning_"):
        return "owning_" + code[len("code_owning_"):]
    if code.startswith("code_for_"):
        return code[len("code_for_"):]
    rest = code[len("code_"):] if code.startswith("code_") else ""
    if any(rest.startswith(p) for p in SIGNIN_PREFIXES):
        return rest
    return None


def _fake_user(who):
    """`(id, login, email_mode)` for a `ghu_<who>` token.

    The id is spelled separately from the login because a login is
    renameable on GitHub and the account behind it does not change —
    which is the whole reason the sign-in keys on the id.
    """
    for prefix, mode in (("as_", "verified"), ("unverified_", "unverified"),
                         ("noemail_", "forbidden"), ("badmail_", "malformed")):
        if who.startswith(prefix):
            uid, _, login = who[len(prefix):].partition("_")
            if uid.isdigit() and login:
                return int(uid), login, mode
    # The install flow's tokens (`acme`, `owning_4001`) still answer
    # these routes rather than 404ing into a walkthrough that reads the
    # miss as a product failure.
    return 900000 + sum(who.encode()), who, "verified"


REPOSITORIES = [
    ("acme-inc/widget", False, "the public one", 16),
    ("acme-inc/ledger", True, "private, needs the installation", 32),
    ("acme-inc/atlas", True, "also private", 48),
    ("acme-inc/docs", False, "public docs", 64),
]


# How many issues a repository has, by owner. `many` is deliberately
# larger than one page so the manual pass exercises the paging loop.
ISSUE_COUNT = {"many": 250, "empty": 0}


def issue_json(owner, repo, n):
    """One issue in GitHub's shape, matching the Rust fake's fixture."""
    closed = n % 4 == 0
    out = {
        "number": n,
        "title": f"issue {n}",
        "body": f"body of {n}",
        "state": "closed" if closed else "open",
        # `user` is null for a deleted GitHub account — not an error, and
        # the case an importer that unwraps it dies on.
        "user": None if n == 5 else {"login": f"octocat-{n}", "id": 1000 + n},
        "labels": [{"name": "bug", "color": "d73a4a",
                    "description": "something is broken"}] if n % 2 == 0 else [],
        "comments": n % 3,
        "created_at": f"2024-01-{(n % 28) + 1:02d}T10:00:00Z",
        "updated_at": f"2024-02-{(n % 28) + 1:02d}T10:00:00Z",
        "closed_at": "2024-03-01T10:00:00Z" if closed else None,
        "html_url": f"https://github.com/{owner}/{repo}/issues/{n}",
    }
    # Pull requests come back from /issues too, marked only by this.
    if n % 3 == 0:
        out["pull_request"] = {
            "url": f"https://api.github.com/repos/{owner}/{repo}/pulls/{n}"
        }
    return out


class H(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def _json(self, status, body, headers=None):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        body = self.rfile.read(n).decode()
        if self.path.startswith("/login/oauth/access_token"):
            code = (parse_qs(body).get("code") or [""])[0]
            who = _who_from_code(code)
            if who is not None:
                self._json(200, {"access_token": f"ghu_{who}", "token_type": "bearer"})
            else:
                # GitHub's answer to a bad or spent code: 200, with an error.
                self._json(200, {"error": "bad_verification_code",
                                 "error_description": "The code passed is incorrect or expired."})
            return
        if "/access_tokens" in self.path:
            if "/4006/" in self.path:
                # GitHub not answering: the connection is dropped without
                # a byte, as the Rust fake does for the same id.
                self.close_connection = True
                return
            self._json(
                201,
                {"token": "ghs_walkthrough", "expires_at": "2099-01-01T00:00:00Z"},
            )
            return
        self._json(404, {"message": "no such endpoint"})

    def do_GET(self):
        u = urlparse(self.path)
        # The install page GitHub would show. Approving is automatic
        # here; what matters for the walkthrough is that the browser
        # comes back the way GitHub sends it, carrying the state.
        if u.path in ("/apps/stratum/installations/new", "/apps/stratum/installations/update"):
            q = parse_qs(u.query)
            state = (q.get("state") or [""])[0]
            inst = (q.get("installation_id") or ["4001"])[0]
            # `code` names the person GitHub just authorized: the one who
            # controls this installation, unless the query says otherwise
            # (`who=ada` arrives as somebody who does not).
            who = (q.get("who") or [""])[0]
            code = f"code_for_{who}" if who else f"code_owning_{inst}"
            to = f"{STRATUM}/v1/github/setup?installation_id={inst}&code={code}"
            if state and u.path.endswith("/new"):
                to += f"&state={state}"
            self.send_response(302)
            self.send_header("location", to)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        # The authorization screen GitHub would show when somebody
        # presses "Continue with GitHub". Approving is automatic here;
        # what matters for the walkthrough is that the browser comes
        # back the way GitHub sends it — to *our* `redirect_uri`,
        # carrying the state it was given, so the callback's cookie
        # check is a real check rather than a step the harness skips.
        #
        # `?who=`/`?id=` choose the person, `?mode=unverified|noemail`
        # the two refusals worth seeing, and `?mode=deny` is the Cancel
        # button.
        if u.path == "/login/oauth/authorize":
            q = parse_qs(u.query)
            state = (q.get("state") or [""])[0]
            back = (q.get("redirect_uri") or [f"{STRATUM}/v1/auth/github/callback"])[0]
            mode = (q.get("mode") or ["as"])[0]
            who = (q.get("who") or ["octocat"])[0]
            uid = (q.get("id") or ["9001"])[0]
            if mode == "deny":
                to = f"{back}?error=access_denied&state={quote(state)}"
            else:
                to = f"{back}?code=code_{mode}_{uid}_{who}&state={quote(state)}"
            self.send_response(302)
            self.send_header("location", to)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        if u.path == "/user":
            auth = self.headers.get("authorization", "")
            if "ghu_" not in auth:
                self._json(401, {"message": "Requires authentication"})
                return
            uid, login, _ = _fake_user(auth.split("ghu_", 1)[1].strip())
            self._json(200, {"id": uid, "login": login,
                             "name": f"Person {login}", "type": "User"})
            return
        if u.path == "/user/emails":
            auth = self.headers.get("authorization", "")
            if "ghu_" not in auth:
                self._json(401, {"message": "Requires authentication"})
                return
            _, login, mode = _fake_user(auth.split("ghu_", 1)[1].strip())
            if mode == "forbidden":
                # Not an empty list: an App without the `Email addresses`
                # permission is refused, and a client that read an empty
                # list as "no proved address" would pass here and be
                # wrong against real GitHub.
                self._json(403, {"message": "Resource not accessible by integration"})
                return
            if mode == "malformed":
                # Primary, verified, and not an address. A provider can
                # answer something we cannot store, and the sign-in has
                # to read that as "no proved address" rather than push
                # the refusal down into account creation.
                self._json(200, [{"email": "not-an-address", "primary": True,
                                  "verified": True, "visibility": "private"}])
                return
            self._json(200, [
                {"email": f"{login}@example.com", "primary": True,
                 "verified": mode == "verified", "visibility": "private"},
                {"email": f"{login}@users.noreply.github.com", "primary": False,
                 "verified": True, "visibility": None},
            ])
            return
        if u.path == "/user/installations":
            auth = self.headers.get("authorization", "")
            who = auth.split("ghu_", 1)[1] if "ghu_" in auth else None
            if who is None:
                self._json(401, {"message": "Requires authentication"})
                return
            owned = {"acme": ["4001"], "ada": ["4002"]}.get(who) or (
                [who[len("owning_"):]] if who.startswith("owning_") and who[len("owning_"):] in ("4001", "4002") else []
            )
            items = [i for i in INSTALLATIONS if str(i["id"]) in owned]
            self._json(200, {"total_count": len(items), "installations": items})
            return
        if u.path.startswith("/app/installations/"):
            inst_id = u.path[len("/app/installations/"):].strip("/")
            if not inst_id.isdigit():
                self._json(404, {"message": "no such endpoint"})
                return
            if not self.headers.get("authorization", "").startswith("Bearer "):
                self._json(401, {"message": "bad jwt"})
                return
            # 4004 is a read the App is not allowed; 4005 is a rate
            # limit — as the Rust fake answers them.
            if inst_id == "4004":
                self._json(403, {"message": "Resource not accessible by integration"},
                           {"X-RateLimit-Limit": "5000", "X-RateLimit-Remaining": "4999"})
                return
            if inst_id == "4005":
                self._json(403, {"message": "API rate limit exceeded"},
                           {"Retry-After": "7", "X-RateLimit-Remaining": "0"})
                return
            detail = INSTALLATION_DETAIL.get(inst_id)
            if detail is None:
                self._json(404, {"message": "Not Found"})
                return
            self._json(200, installation_json(inst_id, *detail))
            return
        if u.path == "/app/installations":
            self._json(200, INSTALLATIONS)
            return
        if u.path == "/installation/repositories":
            repos = [
                {
                    "full_name": name,
                    "private": private,
                    "default_branch": "main",
                    "description": desc,
                    "size": size,
                }
                for (name, private, desc, size) in REPOSITORIES
            ]
            self._json(200, {"total_count": len(repos), "repositories": repos})
            return
        # Issue-import routes, before the metadata route below — that one
        # matches anything after `/repos/`.
        #
        # These exist so the **manual** pass can drive a real import.
        # There are two fakes in this repository: the Rust one in
        # `stratum-testkit` that the automated suites use, and this one,
        # which the manual stack runs. A route taught to only one of them
        # is a feature the other cannot exercise — that has already
        # happened once, with `GET /repos/{full_name}`, and it made the
        # imported star count invisible to the one gate meant to look at
        # it.
        parts = u.path[len("/repos/") :].strip("/").split("/") if u.path.startswith("/repos/") else []
        if len(parts) >= 3 and parts[2] in ("issues", "labels", "milestones"):
            owner, repo, kind = parts[0], parts[1], parts[2]
            q = parse_qs(u.query)
            per_page = min(int(q.get("per_page", ["30"])[0]), 100)
            page = max(int(q.get("page", ["1"])[0]), 1)
            state = q.get("state", ["open"])[0]

            if kind == "labels":
                self._json(200, [
                    {"name": "bug", "color": "d73a4a",
                     "description": "something is broken"},
                    {"name": "good first issue", "color": "7057ff",
                     "description": "a gentle way in"},
                ])
                return
            if kind == "milestones":
                self._json(200, [{"number": 1, "title": "v1.0",
                                  "description": "the first one",
                                  "state": "open",
                                  "due_on": "2024-12-01T00:00:00Z"}])
                return
            # Comments on one issue: /repos/{owner}/{repo}/issues/{n}/comments
            if len(parts) == 5 and parts[4] == "comments" and parts[3].isdigit():
                n = int(parts[3])
                self._json(200, [
                    {"id": n * 100 + i,
                     "body": f"comment {i} on #{n}",
                     "user": {"login": f"commenter-{i}", "id": 2000 + i},
                     "created_at": "2024-01-05T11:00:00Z",
                     "updated_at": "2024-01-05T11:00:00Z",
                     "html_url": f"https://github.com/{owner}/{repo}/issues/{n}#issuecomment-{n * 100 + i}"}
                    for i in range(n % 3)
                ])
                return

            total = ISSUE_COUNT.get(owner, 12)
            wanted = [
                n for n in range(1, total + 1)
                if state == "all"
                or (state == "open" and n % 4 != 0)
                or (state == "closed" and n % 4 == 0)
            ]
            window = wanted[(page - 1) * per_page: page * per_page]
            body = [issue_json(owner, repo, n) for n in window]
            headers = {}
            last = max(1, -(-len(wanted) // per_page))
            if page < last:
                base = f"http://127.0.0.1:{os.environ.get('FAKE_GITHUB_PORT', '29110')}"
                headers["Link"] = (
                    f'<{base}/repos/{owner}/{repo}/{kind}?state={state}'
                    f'&per_page={per_page}&page={page + 1}>; rel="next"'
                )
            self._json(200, body, headers)
            return

        # One repository's public metadata, unauthenticated. The mirror
        # path reads it for the default branch, the description and the
        # upstream's own star count (kept as `origin_stars`); without the
        # route that read 404s. Anything deeper under `/repos/` that no
        # route above serves is a 404 too, as the Rust fake answers it —
        # a repository body for `…/pulls` would be the worst answer
        # available: a client looking for a list finds none and reports
        # an empty project rather than an unserved route.
        if u.path.startswith("/repos/"):
            full_name = u.path[len("/repos/") :].strip("/")
            if full_name.count("/") != 1:
                self._json(404, {"message": "Not Found"})
                return
            self._json(
                200,
                {
                    "full_name": full_name,
                    "private": False,
                    "default_branch": "main",
                    "description": "a mirrored project",
                    "size": 128,
                    "stargazers_count": 60300,
                },
            )
            return
        self._json(404, {"message": "no such endpoint"})


if __name__ == "__main__":
    port = int(os.environ.get("FAKE_GITHUB_PORT", "29110"))
    HTTPServer(("127.0.0.1", port), H).serve_forever()
