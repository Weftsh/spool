#!/usr/bin/env python3
"""Create the GitHub App a Stratum fleet needs, without typing it in.

GitHub's App Manifest flow: this script serves one local page that posts a
manifest to GitHub, a person confirms it there with a single click, GitHub
redirects back here with a one-hour code, and the script exchanges the code
for the App's id, private key and webhook secret. The key and secret are
written under `.secrets/` (gitignored, mode 0600) and never printed.

    scripts/github-app-create.py --org weft --name weft --public-url https://api.weft.sh

The manifest is exactly what `mirror/`, `installations::` and the GitHub
Actions runner feature rely on: read-only metadata/issues, write on
contents (a push to a mirror is forwarded to its origin); the
webhook at `<public-url>/webhooks/github`; and the post-install Setup URL
at `<public-url>/v1/github/setup` — the route that binds an installation
to one org (docs/LAUNCH.md, "The GitHub App is public").

Two of the permissions are `write`, and both are for hosted runners:

  * `administration: write` is what `POST …/actions/runners/generate-jitconfig`
    needs — registering a just-in-time runner against a repository is an
    administration call, not an actions one — and what `DELETE
    …/actions/runners/{id}` needs to remove one that never picked its job up.
  * `actions: write` is what `POST …/actions/runs/{id}/cancel` needs, so a
    job this fleet cannot run (over budget, no capacity) is cancelled at
    GitHub rather than left queued forever.

And two of the events: `workflow_job` is how GitHub asks this fleet to
run a job (the intake in `github_runner/`), and `installation` is how it
tells us an installation was suspended, removed or had these permissions
approved — an installation that predates this manifest holds the old
grants until its owner accepts the new ones, which is what
`GET /app/installations/{id}` is read for.

Compare against `gh api apps/<slug> --jq .permissions,.events` for an
existing App; an App created before this file changed needs its
permissions edited by hand and re-approved on every installation.
"""

import argparse
import http.server
import json
import os
import secrets
import stat
import sys
import urllib.parse
import urllib.request
import webbrowser
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# One App that does everything: what every deployment started with, and
# what a deployment without a Runners App still runs.
PERMISSIONS = {
    "metadata": "read",
    # `write`, not `read`: a push to a mirror is forwarded to its origin
    # under the installation token. An installation made while this said
    # `read` has to approve the change before its mirrors forward.
    "contents": "write",
    "issues": "read",
    # `write`, not `read`: cancelling a workflow run at GitHub.
    "actions": "write",
    # Registering and removing just-in-time runners.
    "administration": "write",
}
# `installation` is not listed: GitHub delivers an App's own installation
# events (created, deleted, new_permissions_accepted) to every App and does
# not offer them as a subscription — an App edited by hand on 2026-09-07
# reports events ["push", "workflow_job"] and still receives them.
EVENTS = ["push", "workflow_job"]

# The split: a Marketplace listing is per App, and the two products want
# different permissions. The mirror App writes `contents` and reads the
# rest; the Runners App writes administration (just-in-time runners) and
# actions (cancelling a run), and hears `workflow_job` on a hook of its
# own. A deployment that has both sets STRATUM_GITHUB_RUNNERS_* beside
# STRATUM_GITHUB_*.
#
# `contents: write` on the *mirror* App is the whole of write-through: a
# push to a mirror is forwarded to its origin under that App's
# installation token, so the App that owns mirroring is the one that
# needs it. This said `read` when the split was written, which would have
# handed anyone who created a mirror-only App from this script one that
# cannot forward a push — and they would not find out until the first
# push came back refused.
KINDS = {
    "all": (PERMISSIONS, EVENTS, "/webhooks/github", "/v1/github/setup",
            "Connects GitHub repositories to a Stratum fleet: mirrors, imports, CI verdicts and hosted Actions runners."),
    "mirror": ({"metadata": "read", "contents": "write", "issues": "read", "actions": "read"},
               ["push"], "/webhooks/github", "/v1/github/setup",
               "Weft Mirror: a live, provably-fresh copy of your repositories that your CI clones from."),
    "runners": ({"metadata": "read", "actions": "write", "administration": "write"},
                ["workflow_job"], "/webhooks/github-runners", "/v1/github/setup/runners",
                "Weft Runners: runs-on: weft sends a GitHub Actions job to Weft's hosted runners."),
}


def manifest(name: str, public_url: str, redirect: str, kind: str = "all") -> dict:
    permissions, events, hook, setup, description = KINDS[kind]
    return {
        "name": name,
        "url": public_url,
        "description": description,
        "public": True,
        "hook_attributes": {"url": f"{public_url}{hook}", "active": True},
        "redirect_url": redirect,
        "setup_url": f"{public_url}{setup}",
        # Come back after an installation is *edited* too. Such a return
        # carries no `state`; the callback binds it to the org the
        # signed-in person began a connect for.
        "setup_on_update": True,
        # Ask the person to authorize the App as themselves during the
        # install. GitHub then appends a `code` to the redirect, which
        # the callback exchanges to learn which installations that
        # person controls — the only proof that the id in the URL is
        # theirs and not another customer's.
        "request_oauth_on_install": True,
        # GitHub refuses a manifest that requests OAuth on install and
        # names no callback URL ("Callback URLs at least one callback URL
        # is required", seen creating the runners App on 2026-09-14). With
        # user authorization requested, the browser comes back to the
        # *callback* URL, and GitHub keeps only its path — which is why
        # each kind's setup path is a path and never a query.
        "callback_urls": [f"{public_url}{setup}"],
        "default_permissions": permissions,
        "default_events": events,
    }


def exchange(code: str) -> dict:
    req = urllib.request.Request(
        f"https://api.github.com/app-manifests/{urllib.parse.quote(code)}/conversions",
        method="POST",
        headers={"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"},
    )
    with urllib.request.urlopen(req) as resp:  # noqa: S310 - fixed host
        return json.load(resp)


def write_secret(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, stat.S_IRUSR | stat.S_IWUSR)
    with os.fdopen(fd, "w") as f:
        f.write(content)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--org", help="organization that will own the App (omit for your user account)")
    ap.add_argument("--name", required=True, help="App name; must be unique across GitHub")
    ap.add_argument("--public-url", required=True, help="the deployed API origin, e.g. https://api.weft.sh")
    ap.add_argument("--port", type=int, default=8477)
    ap.add_argument("--no-browser", action="store_true", help="print the local URL instead of opening it")
    ap.add_argument("--kind", choices=sorted(KINDS), default="all",
                    help="which App to create: `all` (one App for everything), `mirror`, or `runners` (the second App, STRATUM_GITHUB_RUNNERS_*)")
    args = ap.parse_args()

    public_url = args.public_url.rstrip("/")
    state = secrets.token_urlsafe(24)
    redirect = f"http://127.0.0.1:{args.port}/callback"
    target = (
        f"https://github.com/organizations/{args.org}/settings/apps/new?state={state}"
        if args.org
        else f"https://github.com/settings/apps/new?state={state}"
    )
    body = json.dumps(manifest(args.name, public_url, redirect, args.kind))
    outcome: dict = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):  # quiet
            pass

        def _html(self, code: int, html: str) -> None:
            self.send_response(code)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(html.encode())

        def do_GET(self):
            url = urllib.parse.urlparse(self.path)
            if url.path == "/":
                escaped = body.replace("&", "&amp;").replace('"', "&quot;")
                self._html(
                    200,
                    f"<!doctype html><title>Create {args.name}</title>"
                    f"<p>Sending the manifest for <b>{args.name}</b> to GitHub; confirm it there.</p>"
                    f'<form id="f" method="post" action="{target}">'
                    f'<input type="hidden" name="manifest" value="{escaped}">'
                    '<button>Continue to GitHub</button></form>'
                    "<script>document.getElementById('f').submit()</script>",
                )
                return
            if url.path == "/callback":
                q = urllib.parse.parse_qs(url.query)
                if q.get("state", [None])[0] != state:
                    self._html(400, "<p>state mismatch; start again.</p>")
                    outcome["error"] = "state mismatch"
                    return
                try:
                    app = exchange(q["code"][0])
                except Exception as e:  # noqa: BLE001
                    self._html(502, f"<p>exchange failed: {e}</p>")
                    outcome["error"] = str(e)
                    return
                slug = app["slug"]
                pem_path = ROOT / ".secrets" / f"{slug}.private-key.pem"
                env_path = ROOT / ".secrets" / f"{slug}.env"
                write_secret(pem_path, app["pem"])
                write_secret(
                    env_path,
                    f"STRATUM_GITHUB_APP_ID={app['id']}\n"
                    f"STRATUM_GITHUB_WEBHOOK_SECRET={app['webhook_secret']}\n"
                    f"STRATUM_GITHUB_APP_KEY_PEM=$(cat {pem_path})\n"
                    f"STRATUM_GITHUB_INSTALL_URL={app['html_url']}/installations/new\n"
                    # The conversion answer is the only time GitHub shows the
                    # OAuth client secret; the App page can only mint another.
                    # The install callback needs both, and the fleet's
                    # precondition refuses a secret with either blank — the
                    # runners App created on 2026-09-14 had to have a second
                    # secret generated by hand because these were dropped.
                    f"STRATUM_GITHUB_CLIENT_ID={app.get('client_id', '')}\n"
                    f"STRATUM_GITHUB_CLIENT_SECRET={app.get('client_secret', '')}\n",
                )
                outcome.update(id=app["id"], slug=slug, html_url=app["html_url"], pem=str(pem_path), env=str(env_path))
                self._html(200, f"<p>Created <b>{slug}</b> (App ID {app['id']}). You can close this tab.</p>")
                return
            self._html(404, "")

    srv = http.server.HTTPServer(("127.0.0.1", args.port), Handler)
    local = f"http://127.0.0.1:{args.port}/"
    print(f"open {local}" if args.no_browser else f"opening {local}", flush=True)
    if not args.no_browser:
        webbrowser.open(local)
    while not outcome:
        srv.handle_request()
    if "error" in outcome:
        print(f"failed: {outcome['error']}", file=sys.stderr)
        return 1
    print(
        f"created {outcome['slug']} (App ID {outcome['id']}) at {outcome['html_url']}\n"
        f"  private key: {outcome['pem']}\n"
        f"  env (source it): {outcome['env']}\n"
        "Remaining by hand: nothing — install it on an org through the dashboard's Connect button."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
