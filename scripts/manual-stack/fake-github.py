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
import base64
import itertools
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
# exactly: 4001 is an organisation and 4002 a person, both holding what
# the GitHub Actions runner feature needs; 4003 installed the App before
# the feature existed and has not approved the new permissions, which is
# how a real installation that predates a manifest change looks.
FULL_PERMISSIONS = {"actions": "write", "administration": "write",
                    "contents": "write", "issues": "read", "metadata": "read"}
FULL_EVENTS = ["push", "workflow_job"]
OLD_PERMISSIONS = {"actions": "read", "contents": "read", "issues": "read",
                   "metadata": "read"}
OLD_EVENTS = ["push"]
INSTALLATION_DETAIL = {
    "4001": ("acme-inc", "Organization", True),
    "4002": ("ada", "User", True),
    "4003": ("noadmin-inc", "Organization", False),
}


def installation_json(id_, login, target_type, full):
    return {
        "id": int(id_),
        "account": {"login": login, "type": target_type},
        "target_type": target_type,
        "permissions": FULL_PERMISSIONS if full else OLD_PERMISSIONS,
        "events": FULL_EVENTS if full else OLD_EVENTS,
        "suspended_at": None,
    }


# Just-in-time runner ids, from 500 as the Rust fake's do.
RUNNER_IDS = itertools.count(500)
# Per-repository call counts for the alternating rate-limit refusals
# below — the Rust fake's `refusals` counter, keyed the same way.
RUNNER_CALLS = {}


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


def b64(data):
    return base64.b64encode(data).decode()


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
        if self.path.startswith("/repos/") and "/actions/" in self.path:
            self._runners(self.path, body)
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

    def do_DELETE(self):
        if self.path.startswith("/repos/") and "/actions/" in self.path:
            self._runners(self.path, "")
            return
        self._json(404, {"message": "no such endpoint"})

    def _runners(self, path, body):
        """The self-hosted-runner routes: register a just-in-time runner,
        remove one, cancel a run. A line-for-line mirror of `runners_route`
        in crates/stratum-testkit/src/fake_github.rs — the two fakes must
        agree, because a route taught to only one is a feature the other
        gate cannot exercise (see the note on the issue routes below).

        `noadmin/*` lacks `administration: write` and `noactionswrite/*`
        lacks `actions: write`: a 403 WITH a budget still on it, which is
        what tells a missing permission from the primary rate limit.
        `private/*` answers 404 to a registration, as GitHub does for a
        repository the installation cannot see. `ratelimited/*` and
        `budgetspent/*` refuse every odd call with the secondary and the
        primary limit respectively, so one pass sees both the refusal and
        the recovery.
        """
        parts = urlparse(path).path[len("/repos/"):].strip("/").split("/")
        if len(parts) < 4 or parts[2] != "actions":
            self._json(404, {"message": "Not Found"})
            return
        owner, repo, kind, tail = parts[0], parts[1], parts[3], parts[4:]
        if self.command == "POST" and kind == "runners" and tail == ["generate-jitconfig"]:
            call = "jit"
        elif self.command == "DELETE" and kind == "runners" and len(tail) == 1:
            call = "delete"
        elif self.command == "POST" and kind == "runs" and len(tail) == 2 and tail[1] == "cancel":
            call = "cancel"
        else:
            self._json(404, {"message": "Not Found"})
            return

        if "ghs_" not in self.headers.get("authorization", ""):
            self._json(401, {"message": "requires installation token"})
            return

        key = f"runners:{owner}/{repo}"
        RUNNER_CALLS[key] = RUNNER_CALLS.get(key, 0) + 1
        if RUNNER_CALLS[key] % 2 == 1:
            if owner == "ratelimited":
                self._json(403, {"message": "API rate limit exceeded"},
                           {"Retry-After": "2", "X-RateLimit-Remaining": "0"})
                return
            if owner == "budgetspent":
                import time
                self._json(403, {"message": "API rate limit exceeded for installation ID 777."},
                           {"X-RateLimit-Limit": "5000", "X-RateLimit-Remaining": "0",
                            "X-RateLimit-Reset": str(int(time.time()) + 120)})
                return

        def denied(what):
            self._json(403, {"message": f"Resource not accessible by integration ({what})"},
                       {"X-RateLimit-Limit": "5000", "X-RateLimit-Remaining": "4999"})

        def not_found():
            self._json(404, {"message": "Not Found"})

        if call == "jit":
            if owner == "noadmin":
                denied("administration: write")
                return
            if owner == "private":
                not_found()
                return
            try:
                req = json.loads(body) if body else {}
            except ValueError:
                req = {}
            name = req.get("name") or ""
            asked = [l for l in (req.get("labels") or []) if isinstance(l, str)]
            # GitHub's 422s as the manual gate observed them (2026-09-07):
            # a message and nothing else; a label with a space is accepted.
            if not asked:
                self._json(422, {"message": "Invalid request.\n\nInvalid property /labels: 1 item "
                                            "required; only 0 were supplied.", "status": "422"})
                return
            if not name or not isinstance(req.get("runner_group_id"), int):
                self._json(422, {"message": "Invalid request.\n\nInvalid property /name: required.",
                                 "status": "422"})
                return
            long = next((l for l in asked if len(l) >= 256), None)
            if long is not None:
                self._json(422, {"message": f"Invalid Argument - Label '{long}' is not valid. Labels must "
                                            "be less than 256 characters in length", "status": "422"})
                return
            runner_id = next(RUNNER_IDS)
            # Exactly the labels asked for, lowercased — no defaults — as
            # the real API answered the Rust fake's sibling.
            labels = []
            for l in (x.lower() for x in asked):
                if l not in labels:
                    labels.append(l)
            labels_json = [{"id": 0, "name": l, "type": "read-only"} for l in labels]
            runner_file = json.dumps({"agentName": name, "ephemeral": True}, separators=(",", ":"))
            cfg = b64(json.dumps({".runner": b64(runner_file.encode())},
                                 separators=(",", ":")).encode())
            self._json(201, {
                "runner": {"id": runner_id, "name": name, "os": "unknown",
                           "status": "offline", "busy": False, "version": "2.337.0",
                           "labels": labels_json, "runner_group_id": 1},
                "encoded_jit_config": cfg,
            })
            return
        if call == "delete":
            if tail[0] == "0":
                not_found()
                return
            if owner == "nodelete":
                denied("administration: write (delete)")
                return
            self.send_response(204)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        # cancel
        if owner == "noactionswrite":
            denied("actions: write")
            return
        if owner == "private":
            not_found()
            return
        if tail[0] == "0":
            # BELIEF: a run that is already over.
            self._json(409, {"message": "Cannot cancel a workflow run that is completed."})
            return
        self._json(202, {})

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
                           [("X-RateLimit-Limit", "5000"), ("X-RateLimit-Remaining", "4999")])
                return
            if inst_id == "4005":
                self._json(403, {"message": "API rate limit exceeded"},
                           [("Retry-After", "7"), ("X-RateLimit-Remaining", "0")])
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
        # One repository's public metadata, unauthenticated.
        #
        # The mirror path reads this to learn what the upstream says its
        # star count is, so that a mirrored project can show "60.3k on
        # GitHub" beside its own honest count here. Without this route
        # the request 404s, `origin_stars` stays NULL, and the manual
        # browser pass sees a mirror with no provenance at all — which
        # is precisely the thing stars were built to demonstrate, absent
        # from the one gate meant to look at it.
        #
        # 60300 is the number from the product argument: a migrated
        # project's real reputation, shown separately and never summed
        # with the handful of stars it honestly has here.
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
                base = f"http://127.0.0.1:{os.environ.get('FAKE_GITHUB_PORT', '59110')}"
                headers["Link"] = (
                    f'<{base}/repos/{owner}/{repo}/{kind}?state={state}'
                    f'&per_page={per_page}&page={page + 1}>; rel="next"'
                )
            self._json(200, body, headers)
            return

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
    port = int(os.environ.get("FAKE_GITHUB_PORT", "59110"))
    HTTPServer(("127.0.0.1", port), H).serve_forever()
