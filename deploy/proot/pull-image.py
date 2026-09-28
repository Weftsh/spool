#!/usr/bin/env python3
"""Pull a container image into a root filesystem with no container runtime.

    pull-image.py IMAGE[:TAG] DEST
    pull-image.py --archive IMAGE.tar DEST     # a docker-archive, e.g. kaniko's --tar-path

Talks the registry HTTP API directly — anonymous token auth for Docker
Hub, ghcr.io and quay.io, manifest lists resolved to this machine's
architecture (PULL_ARCH overrides) — then
flattens the layers into DEST the way a runtime would: in order, with
`.wh.` whiteouts applied between layers. The image config is left at
DEST/.image.json so `run.sh` can apply its Env, Entrypoint, Cmd, User
and WorkingDir.

Why this exists rather than `docker pull`: the sandboxes deploy/proot is
for have no daemon and cannot have one, so an image has to arrive the
way `scripts/fetch-minio.sh` already brings MinIO — off the wire, into a
directory. This is that script generalised to any image.

Credentials come from the docker client's own file — $DOCKER_CONFIG/
config.json, else ~/.docker/config.json — so a private registry you
`docker login`ed to is answered with that entry, Basic or through a
Bearer token exchange, whichever the registry's challenge asks for. A
loopback registry is spoken to over plain HTTP.

Standard library only.
"""
import gzip
import http.client
import io
import json
import os
import platform
import shutil
import sys
import tarfile
import time
import urllib.error
import urllib.parse
import urllib.request

# The layers for the machine running this, unless PULL_ARCH says otherwise:
# CI is usually amd64, and local-model.sh on Apple Silicon is arm64.
ARCH = os.environ.get("PULL_ARCH") or {"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64", "arm64": "arm64"}[platform.machine()]

ACCEPT = ", ".join([
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.oci.image.index.v1+json",
])


def parse(ref):
    """'ghcr.io/weftsh/minio:TAG' -> (registry, repository, tag)."""
    if "@" in ref:
        ref, digest = ref.split("@", 1)
    else:
        digest = None
    first, _, rest = ref.partition("/")
    if rest and ("." in first or ":" in first or first == "localhost"):
        registry, path = first, rest
    else:
        registry, path = "registry-1.docker.io", ref
        if "/" not in path:
            path = "library/" + path
    if ":" in path.rsplit("/", 1)[-1]:
        path, tag = path.rsplit(":", 1)
    else:
        tag = "latest"
    return registry, path, digest or tag


class DropAuthOnRedirect(urllib.request.HTTPRedirectHandler):
    seen = set()

    """A registry hands blob fetches to a CDN with a signed URL, and the CDN
    answers 400 when the registry's bearer token comes along (Docker Hub
    via Cloudflare, on the fleet's first run). Docker strips the header on
    a cross-host redirect; urllib does not, so this does."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is not None and urllib.parse.urlparse(newurl).netloc != urllib.parse.urlparse(req.full_url).netloc:
            new.remove_header("Authorization")
            host = urllib.parse.urlparse(newurl).netloc
            if host not in self.seen:
                self.seen.add(host)
                print(f"  blobs served by {host}", file=sys.stderr)
        return new


OPENER = urllib.request.build_opener(DropAuthOnRedirect)


def is_loopback(host):
    bare = host.rsplit(":", 1)[0] if not host.endswith("]") else host
    return bare in ("localhost", "[::1]", "::1") or bare.startswith("127.")


def docker_config_path():
    return os.path.join(os.environ.get("DOCKER_CONFIG") or os.path.expanduser("~/.docker"), "config.json")


def docker_auth(host):
    """The `auth` the docker config holds for HOST (base64 user:secret), or None.

    Docker Hub is filed under its legacy index URL, and `docker login`
    writes a bare host for everything else; both spellings are read."""
    try:
        with open(docker_config_path()) as f:
            auths = json.load(f).get("auths") or {}
    except (OSError, ValueError):
        return None
    keys = [host, f"https://{host}", f"http://{host}"]
    if host in ("registry-1.docker.io", "docker.io", "index.docker.io"):
        keys.append("https://index.docker.io/v1/")
    for k in keys:
        a = (auths.get(k) or {}).get("auth")
        if a:
            return a
    return None


class Registry:
    def __init__(self, host, repo):
        self.host, self.repo, self.token = host, repo, None
        self.scheme = "http" if is_loopback(host) else "https"
        self.basic = docker_auth(host)
        # Basic is sent only once a registry has asked for it: a Bearer
        # registry would read an unrequested Basic header as a failed login.
        self.use_basic = False
        self.challenges = set()

    def url(self, path):
        return f"{self.scheme}://{self.host}/v2/{self.repo}/{path}"

    def _req(self, url, accept=None, method=None, data=None, headers=None):
        h = {"Accept": accept or ACCEPT}
        h.update(headers or {})
        if self.token:
            h["Authorization"] = "Bearer " + self.token
        elif self.use_basic and self.basic:
            h["Authorization"] = "Basic " + self.basic
        return urllib.request.Request(url, headers=h, method=method, data=data)

    def _answer(self, e):
        """Take a 401's challenge; True when there is something new to retry with."""
        challenge = e.headers.get("WWW-Authenticate", "") or ""
        if challenge in self.challenges:
            return False
        self.challenges.add(challenge)
        if challenge.lower().startswith("bearer "):
            self.token = self._token(challenge)
            return self.token is not None
        if challenge.lower().startswith("basic") and self.basic and not self.use_basic:
            self.use_basic = True
            return True
        return False

    def request(self, method, url, data=None, headers=None, accept=None):
        """One request, answering an auth challenge; DATA is bytes or a
        callable returning a fresh readable (a retry needs the body again)."""
        for _ in range(4):
            body = data() if callable(data) else data
            try:
                return OPENER.open(self._req(url, accept, method, body, headers), timeout=600)
            except urllib.error.HTTPError as e:
                if e.code == 401 and self._answer(e):
                    continue
                raise
        raise SystemExit(f"{url}: the registry kept refusing the credential")

    def get(self, path, accept=None):
        url = self.url(path)
        # Docker Hub meters anonymous pulls per source address, and a NAT
        # shares one address across every job behind it: a 429 is a wait,
        # not a verdict. Three waits, then it is a verdict.
        for attempt, pause in enumerate((10, 30, 60), start=1):
            try:
                return OPENER.open(self._req(url, accept), timeout=120)
            except urllib.error.HTTPError as e:
                if e.code not in (401, 429):
                    # The answer, not just the code: a CDN's 403 says why
                    # in its body, and the fleet's first pull needed that.
                    body = e.read(600).decode(errors="replace")
                    print(f"  {e.code} from {urllib.parse.urlparse(e.url).netloc}: {body.strip()[:400]}", file=sys.stderr)
                if e.code == 401 and self._answer(e):
                    continue
                if e.code == 429:
                    print(f"  {self.host}: 429 too many requests, waiting {pause}s ({attempt}/3)", file=sys.stderr)
                    time.sleep(pause)
                    continue
                raise
        return OPENER.open(self._req(url, accept), timeout=120)

    def blob(self, digest):
        """A layer, whole: a transfer cut short mid-layer is retried, not trusted."""
        for attempt in range(1, 4):
            try:
                with self.get(f"blobs/{digest}", "application/octet-stream") as r:
                    return r.read()
            except (http.client.IncompleteRead, ConnectionResetError, TimeoutError, urllib.error.URLError) as e:
                if attempt == 3:
                    raise
                print(f"  {digest[7:19]}: transfer failed ({type(e).__name__}), retrying ({attempt}/3)", file=sys.stderr)
                time.sleep(5 * attempt)

    def _token(self, challenge):
        # Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:x:pull"
        params = dict(p.split("=", 1) for p in challenge[len("Bearer "):].split(","))
        params = {k.strip(): v.strip().strip('"') for k, v in params.items()}
        realm = params.pop("realm")
        query = "&".join(f"{k}={urllib.parse.quote(v, safe=':,/')}" for k, v in params.items())
        req = urllib.request.Request(f"{realm}?{query}")
        # A registry that needs a login exchanges the stored credential for
        # the token; an anonymous one ignores it.
        if self.basic:
            req.add_header("Authorization", "Basic " + self.basic)
        with OPENER.open(req, timeout=60) as r:
            body = json.load(r)
        return body.get("token") or body.get("access_token")


def pull(ref, dest):
    host, repo, tag = parse(ref)
    reg = Registry(host, repo)
    with reg.get(f"manifests/{tag}") as r:
        manifest = json.load(r)
    if "manifests" in manifest:  # an index: pick linux/<ARCH>
        for m in manifest["manifests"]:
            p = m.get("platform", {})
            if p.get("os") == "linux" and p.get("architecture") == ARCH:
                with reg.get(f"manifests/{m['digest']}", m["mediaType"]) as r:
                    manifest = json.load(r)
                break
        else:
            sys.exit(f"{ref}: no linux/{ARCH} manifest in the index")
    with reg.get(f"blobs/{manifest['config']['digest']}", "application/octet-stream") as r:
        config = json.load(r)

    if os.path.isdir(dest):
        shutil.rmtree(dest)
    os.makedirs(dest)
    for i, layer in enumerate(manifest["layers"]):
        blob = reg.blob(layer["digest"])
        if layer["mediaType"].endswith("gzip") or blob[:2] == b"\x1f\x8b":
            blob = gzip.decompress(blob)
        apply_layer(io.BytesIO(blob), dest)
        print(f"  layer {i + 1}/{len(manifest['layers'])} {layer['digest'][7:19]} {len(blob) // 1024} KiB", file=sys.stderr)
    with open(os.path.join(dest, ".image.json"), "w") as f:
        json.dump({"ref": ref, "config": config.get("config", {})}, f)


def apply_layer(fileobj, dest):
    """Extract one layer over DEST: whiteouts first, then the files."""
    with tarfile.open(fileobj=fileobj) as t:
        members = t.getmembers()
        keep = []
        for m in members:
            name = m.name.lstrip("./").lstrip("/")
            base = os.path.basename(name)
            if base == ".wh..wh..opq":
                d = os.path.join(dest, os.path.dirname(name))
                if os.path.isdir(d):
                    for child in os.listdir(d):
                        rm(os.path.join(d, child))
            elif base.startswith(".wh."):
                rm(os.path.join(dest, os.path.dirname(name), base[4:]))
            else:
                # A file replacing a directory (or the reverse) must not merge.
                target = os.path.join(dest, name)
                if os.path.lexists(target) and not (m.isdir() and os.path.isdir(target) and not os.path.islink(target)):
                    rm(target)
                keep.append(m)
        # Nothing here runs as root: ownership is not applied, so a uid
        # the image sets is honoured by run.sh's -i instead. `strip` is
        # applied to the members by hand rather than as a `filter=`:
        # Debian bookworm's Python 3.11.2 — the runner images' — predates
        # that keyword, and the first build of Dockerfile.runner with this
        # file inside it died on exactly that TypeError.
        members = [strip(m) for m in keep]
        try:
            t.extractall(dest, members=members, filter="fully_trusted")
        except TypeError:
            t.extractall(dest, members=members)


def strip(ti, _dest=None):
    ti.uid = ti.gid = os.getuid()
    ti.mode |= 0o600  # the owner must be able to read and replace it
    if ti.islnk():
        # kaniko writes hard-link targets absolute; tarfile resolves them
        # relative to the extraction root only when they are relative.
        ti.linkname = ti.linkname.lstrip("/")
    return ti


def rm(path):
    if os.path.islink(path) or os.path.isfile(path):
        os.unlink(path)
    elif os.path.isdir(path):
        shutil.rmtree(path)


def import_archive(path, dest):
    """Flatten a docker-archive tar (kaniko's --tar-path output) into DEST."""
    if os.path.isdir(dest):
        shutil.rmtree(dest)
    os.makedirs(dest)
    with tarfile.open(path) as outer:
        manifest = json.load(outer.extractfile("manifest.json"))[0]
        for i, layer in enumerate(manifest["Layers"]):
            blob = outer.extractfile(layer).read()
            if blob[:2] == b"\x1f\x8b":
                blob = gzip.decompress(blob)
            apply_layer(io.BytesIO(blob), dest)
            print(f"  layer {i + 1}/{len(manifest['Layers'])} {len(blob) // 1024} KiB", file=sys.stderr)
        config = json.load(outer.extractfile(manifest["Config"]))
    with open(os.path.join(dest, ".image.json"), "w") as f:
        json.dump({"ref": manifest.get("RepoTags", [path])[0], "config": config.get("config", {})}, f)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "--archive":
        import_archive(sys.argv[2], sys.argv[3])
    elif len(sys.argv) == 3:
        pull(sys.argv[1], sys.argv[2])
    else:
        sys.exit(__doc__)
