#!/usr/bin/env python3
"""Create the GitHub App a Spool deployment needs, without typing it in.

GitHub's App Manifest flow: this script serves one local page that posts a
manifest to GitHub, a person confirms it there with a single click, GitHub
redirects back here with a one-hour code, and the script exchanges the code
for the App's id, private key, webhook secret and OAuth client. The key and
secrets are written under `.secrets/` (gitignored, mode 0600) and never
printed.

    scripts/github-app-create.py --name my-forge --public-url https://git.example.com
    scripts/github-app-create.py --org my-github-org --name my-forge --public-url …

One App does everything the server does with GitHub, and the manifest asks
for exactly that and nothing more. Each permission is here because of a
call the server makes — the file and the call are named, so a permission
nobody needs any more is easy to see and take away:

  * `metadata: read` — every App holds it; `GET /installation/repositories`
    (the repository picker, `mirror/origin.rs`) is read under it.
  * `contents: write` — a mirror is cloned and synced with `contents: read`,
    and a push *to* a mirror is forwarded to its origin
    (`mirror/forward.rs`) under the installation token, which needs
    `write`. An installation made while this said `read` keeps `read`
    until its owner approves the change on GitHub.
  * `issues: read` — issue import (`workers/importer.rs`): an origin's
    issues, their comments, labels and milestones.
  * `actions: read` — the CI poller (`workers/checks_poll.rs`) reads an
    origin's workflow runs so a mirror's checks carry GitHub's verdict.
  * `email_addresses: read` — an *account* permission, used by signing in
    with GitHub (`api/github_auth.rs`): `GET /user/emails` under the
    signed-in person's token is how the server learns that GitHub has
    proved their primary address, which is what links a first GitHub
    sign-in to the account here with that address. Without it that call
    is refused and GitHub sign-in answers `noemail`; the password path
    still works.

One event: `push`, which the webhook at `<public-url>/webhooks/github`
turns into a mirror sync (`mirror/webhook.rs`). Every other event GitHub
sends there is acknowledged and ignored, so the App does not subscribe to
any. GitHub delivers an App's own `installation` events whether or not it
subscribes; they are not offered as a subscription.

Two URLs come back to the server: the post-install Setup URL
`<public-url>/v1/github/setup`, the route that binds an installation to one
org, and the sign-in callback `<public-url>/v1/auth/github/callback`. Both
are listed as OAuth callback URLs, because with user authorization
requested GitHub returns the browser to a *callback* URL.

  * `workflows: write` — GitHub refuses an App token a push that creates
    or changes a file under `.github/workflows/` unless the App holds it,
    so without it a push to a mirror that touches the origin's Actions
    workflows would be refused at the origin. **Not yet observed against
    real GitHub**: `scripts/manual-mirror-push.sh` has not pushed such a
    change, so the exact sentence GitHub answers without the permission —
    and whether `mirror/forward.rs`'s `classify` names it — is unrecorded.

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

PERMISSIONS = {
    "metadata": "read",
    # `write`, not `read`: a push to a mirror is forwarded to its origin
    # under the installation token.
    "contents": "write",
    "issues": "read",
    "actions": "read",
    # A forwarded mirror push that touches `.github/workflows/`. Asked for
    # on GitHub's documented rule, not yet on an observed refusal — see
    # the module docstring.
    "workflows": "write",
    # Account permission: `GET /user/emails` during GitHub sign-in.
    "email_addresses": "read",
}
EVENTS = ["push"]

DESCRIPTION = (
    "Connects GitHub repositories to a self-hosted Spool forge: mirrors that "
    "sync on push and forward pushes back, issue import, CI verdicts, and "
    "signing in with GitHub."
)


def manifest(name: str, public_url: str, redirect: str, description: str = DESCRIPTION,
             public: bool = False) -> dict:
    setup = f"{public_url}/v1/github/setup"
    return {
        "name": name,
        "url": public_url,
        "description": description,
        # Private by default: only the account that owns the App can
        # install it, which is what a single self-hosted forge wants.
        # `--public` for a forge whose users install it on their own
        # GitHub organisations.
        "public": public,
        "hook_attributes": {"url": f"{public_url}/webhooks/github", "active": True},
        "redirect_url": redirect,
        "setup_url": setup,
        # Come back after an installation is *edited* too. Such a return
        # carries no `state`; the callback binds it to the org the
        # signed-in person began a connect for.
        "setup_on_update": True,
        # Ask the person to authorize the App as themselves during the
        # install. GitHub then appends a `code` to the redirect, which
        # the callback exchanges to learn which installations that
        # person controls — the only proof that the id in the URL is
        # theirs and not somebody else's.
        "request_oauth_on_install": True,
        # GitHub refuses a manifest that requests OAuth on install and
        # names no callback URL. With user authorization requested, the
        # browser comes back to a *callback* URL and GitHub keeps only its
        # path — which is why both are paths and never a query. The
        # second is where signing in with GitHub returns
        # (`api/github_auth.rs` derives it from the public URL).
        "callback_urls": [setup, f"{public_url}/v1/auth/github/callback"],
        "default_permissions": PERMISSIONS,
        "default_events": EVENTS,
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
    ap.add_argument("--org", help="GitHub organization that will own the App (omit for your user account)")
    ap.add_argument("--name", required=True, help="App name; must be unique across GitHub")
    ap.add_argument("--public-url", required=True,
                    help="the forge's public origin, as STRATUM_PUBLIC_URL says it, e.g. https://git.example.com")
    ap.add_argument("--description", default=DESCRIPTION, help="shown on the App's GitHub page")
    ap.add_argument("--public", action="store_true",
                    help="let any GitHub account install the App (default: only its owner)")
    ap.add_argument("--port", type=int, default=8477)
    ap.add_argument("--no-browser", action="store_true", help="print the local URL instead of opening it")
    args = ap.parse_args()

    public_url = args.public_url.rstrip("/")
    state = secrets.token_urlsafe(24)
    redirect = f"http://127.0.0.1:{args.port}/callback"
    target = (
        f"https://github.com/organizations/{args.org}/settings/apps/new?state={state}"
        if args.org
        else f"https://github.com/settings/apps/new?state={state}"
    )
    body = json.dumps(manifest(args.name, public_url, redirect, args.description, args.public))
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
                    # A *path*: the server reads the key from the file this
                    # names (`app.rs`, STRATUM_GITHUB_APP_KEY carries the PEM
                    # itself). This wrote `$(cat <path>)`, which a sourcing
                    # shell expanded into the PEM and the server then tried
                    # to open as a file name — and which an EnvironmentFile or
                    # `--env-file` never expands at all.
                    f"STRATUM_GITHUB_APP_KEY_PEM={pem_path}\n"
                    f"STRATUM_GITHUB_INSTALL_URL={app['html_url']}/installations/new\n"
                    # The conversion answer is the only time GitHub shows the
                    # OAuth client secret; the App page can only mint another.
                    # The install callback and GitHub sign-in need both, and
                    # the server refuses to boot with one set and not the
                    # other.
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
        f"  env: {outcome['env']} (KEY=value lines: source it, or hand it to\n"
        "       systemd's EnvironmentFile= or docker's --env-file)\n"
        "Remaining by hand: nothing — install it on an org through the dashboard's Connect button."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
