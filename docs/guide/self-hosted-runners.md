---
layout: ../../layouts/Docs.astro
title: Running a self-hosted runner
description: The operator's side of self-hosted runners — getting the binary, registering a machine, a systemd unit, what network access it needs, and how to isolate a process that runs other people's code.
---

# Running a self-hosted runner

[Workflows](/docs/workflows/#self-hosted-runners) describes self-hosted
runners from the workflow author's side: `runs-on: [self-hosted]`,
labels, groups, and the organisation policy that admits them. This page
is the other side — you have a machine, and you want Weft to be able
to run jobs on it.

The whole of it is one binary, `weft-runner`, which **asks for work
and never waits to be asked**. It makes outbound HTTPS calls to your
Weft URL and nothing else. It does not listen on a port, it does not
need a public address, an inbound firewall rule, or a tunnel, and there
is nothing to expose.

The reverse direction — your workflows stay on GitHub Actions and the
jobs run on Weft's fleet with `runs-on: weft` — is
[Weft runners for GitHub Actions](/docs/github-runners/).

Read [Isolating it](#isolating-it) before you put one on a machine that
matters. A runner executes shell commands out of a repository, as
whatever user it runs as, and every other decision on this page follows
from that.

## Getting the binary

There is **no download to `curl`, and no image on a public registry**.
Both come from this repository, and the honest reason is that a runner
is the one component you should want to have built yourself.

**From source.** You need a Rust toolchain matching the one the image
builds with (`rust:1.98-bookworm` today):

```bash
# in a checkout of the Weft repository
cargo build --release -p stratum-runner
sudo install -m 0755 target/release/weft-runner /usr/local/bin/
```

**As a container.** `Dockerfile.runner` at the repository root is the
same image our own hosted fleet runs, and it is deliberately small: the
binary, `git`, `curl`, `ca-certificates`, `build-essential`, `python3`
and `jq` on Debian bookworm, running as an unprivileged `runner` user.
No docker daemon and no docker socket — a runner that can talk to a
daemon can escape its container — and no cloud CLIs; the `docker` on its
`PATH` builds and runs images with kaniko and PRoot instead, as
[the GitHub runners page](/docs/github-runners/) describes.

```bash
docker build -f Dockerfile.runner -t weft-runner:local .
```

If your builds need a toolchain that image does not have, that is a
`FROM weft-runner:local` of your own. A self-hosted job runs its
steps directly on the machine the runner is on, so whatever is on that
machine's `PATH` is what the job gets — the `image:` key still means
`default` and only `default`.

## Registering a machine

Registration is a two-step exchange, and the two secrets are different
things. An organisation admin mints a **registration token** — under
**Settings → Runners → Add a runner**, or:

```bash
curl -sS -X POST "$WEFT_URL/v1/orgs/$ORG/runners/registration-token" \
  -b "$COOKIE_JAR" -H "Content-Type: application/json" \
  -d '{ "group": "default" }'
```

```json
{ "token": "weftg_…", "expires_at": 1800000000000, "group": "default",
  "command": "weft-runner register --url https://weft.sh --token weftg_…" }
```

That token is **single-use and lasts one hour**. It is the right to
obtain a credential, not a credential — which is why it is safe enough to
paste into a cloud-init script and short-lived enough that a leaked one
is usually already spent.

The machine exchanges it once:

```bash
weft-runner register \
  --url "$WEFT_URL" --token weftg_… \
  --name build-01 --labels gpu,cuda-12 \
  --dir /var/lib/weft-runner
weft-runner run --dir /var/lib/weft-runner
```

| Flag | |
|---|---|
| `--url` | your Weft base URL |
| `--token` | the registration token |
| `--name` | defaults to the machine's hostname |
| `--labels` | comma-separated, case-folded to lowercase; `self-hosted`, the OS and the architecture are added for you |
| `--ephemeral` | take one job and exit — see [below](#ephemeral-runners-and-autoscaling) |
| `--dir` | where `.runner` and the working directories live; defaults to `.` |

`register` writes `DIR/.runner` with mode `0600`. That file holds the
runner's own long-lived credential, so it is the thing to protect: back
it up nowhere, and if it leaks, remove the runner from the list and
register again.

`run` then loops — ask for a job, run it, ask again — printing one line
per job it takes and one per job it finishes. Running `register` again
under the same name **replaces** that runner and kills the old
credential; that is how rotation works, and there is nothing else to it.

## A systemd unit

```ini
# /etc/systemd/system/weft-runner.service
[Unit]
Description=Weft self-hosted runner
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=weft-runner
Group=weft-runner
WorkingDirectory=/var/lib/weft-runner
ExecStart=/usr/local/bin/weft-runner run --dir /var/lib/weft-runner
Restart=always
RestartSec=5
KillSignal=SIGTERM
TimeoutStopSec=120

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --home-dir /var/lib/weft-runner --create-home weft-runner
sudo -u weft-runner weft-runner register --url "$WEFT_URL" \
  --token weftg_… --dir /var/lib/weft-runner
sudo systemctl enable --now weft-runner
```

Two details in there are load-bearing:

- **`SIGTERM` is a clean stop, and `TimeoutStopSec` has to allow for
  it.** On `SIGTERM` the runner kills the running job's process group,
  reports it `cancelled` so the check does not sit queued forever, and
  exits `0`. Give that longer than systemd's 90-second default if your
  jobs are large; a `SIGKILL` mid-job leaves a check waiting for a
  verdict nobody is going to send, until the server's own sweep fails it.
- **`Restart=always` is right for an ordinary runner and wrong for an
  ephemeral one.** A removed runner exits `2` after printing `this runner
  has been removed; register it again`, and restarting it into that same
  exit is a loop that fills a journal. If you removed the machine
  deliberately, `systemctl disable --now` it.

The unit deliberately carries no `ProtectSystem=` or `PrivateTmp=`
hardening. Those are worth adding, but they are decisions about what your
builds are allowed to touch, and a hardening line that silently breaks
`make install` reads as Weft being broken. Add them knowing what your
jobs do.

## Ephemeral runners, and autoscaling

`--ephemeral` takes exactly one job and exits `0`, and the server removes
the runner the moment that job reaches a terminal state. It is the only
way to be sure a job cannot see what the previous job left behind, and it
is what to reach for if you are scaling machines up and down.

The shape that works is **a fresh instance per job, registering at
boot**: something holding an `org:admin` credential mints a registration
token, the instance's boot script exchanges it with `register
--ephemeral`, `run` takes one job, and the instance terminates. The token being single-use and
hour-long is what makes that safe to put in user-data.

The shape that does not work is a systemd `Restart=always` around an
ephemeral runner: its registration is gone after its one job, so the
restarted `run` gets a `401` and exits `2`. Ephemeral means the *machine* is
disposable, not just the process.

An ephemeral runner that never comes back is removed from the list after
**1 day** unseen; an ordinary one after **14 days**.

## What the runner needs from the network

Outbound HTTPS to your Weft URL, and whatever your builds themselves
reach. That is the list.

| Direction | |
|---|---|
| **Outbound** | HTTPS to the Weft base URL you registered with — the claim loop, the job's log upload and its verdict — and the same host again for the `git` fetch of the repository |
| **Inbound** | none. Nothing listens. The runner has no port, no health endpoint and no callback |

`weft-runner run` talks to whatever URL you gave `register`, so that
URL has to be reachable from the runner's own machine — a private
hostname or a NAT-side address is entirely fine, and it does not have to
be the server's public URL.

The claim call is a **long poll**: the runner asks for a job and the
server holds the request open for up to twenty seconds before answering
"nothing" rather than replying instantly and being asked again. A proxy
or load balancer between the runner and Weft needs an idle timeout
above that or it will cut every empty poll, which looks like a runner
that flaps between `online` and `offline`.

If your egress goes through a proxy that terminates TLS, its CA has to be
in the machine's own trust store — for the container image, that is the
`extra_ca` build secret `Dockerfile.runner` already takes.

## Isolating it

**A runner executes code from a repository, as the user it runs as, on
the machine it runs on.** Nothing about the design changes that; the
whole point of a self-hosted runner is running your build on your
hardware. So the question is only ever *what would this cost me if a job
were hostile*, and there are four answers worth having:

1. **A dedicated, unprivileged user with nothing of yours in its home.**
   Not your account, not `root`, and not a user that has an SSH key,
   a `~/.aws/credentials`, a kubeconfig or a signed-in package-registry
   token lying around. A job is a shell; everything that user can read,
   it can read.
2. **A machine, VM or container that only does this.** The runner's
   isolation between one job and the next is a fresh working directory
   and nothing more — a job can write outside it, leave a process running
   and start a daemon. A dedicated VM you can throw away, plus
   `--ephemeral`, is the version of this that actually holds.
3. **No ambient cloud credentials.** An instance profile, an IMDS
   endpoint or a mounted service-account token is reachable from any
   `curl` in any step. Our own hosted runners are given no task role at
   all, for exactly this reason. Block the metadata endpoint if the
   machine has one.
4. **Off the network you care about.** Give it internet and give it
   Weft; do not give it the route to your database, your internal
   registry or your admin panel. "It is behind the firewall" is what
   makes a self-hosted runner interesting to somebody else.

And the setting that decides who gets to run code on it at all:

> **Do not turn on "allow public repositories" for a group unless you
> mean it.** Anybody can fork a public repository, and a fork's change
> brings its own `.weft/*.yml`. The
> [fork-approval gate](/docs/workflows/#changes-pushed-from-a-fork)
> stands in front of it — a maintainer must approve each new tip — but
> that is one human decision between a stranger and your machine, and a
> group left closed is zero required decisions.

The [mining watch](/docs/workflows/#what-is-refused-for-abuse) runs here
too: a step caught running a miner has its process group killed and the
job fails. It is protecting *you* in this direction, not our bill, which
is why it does not also suspend your organisation's hosted workflows the
way it does on our fleet.

## Removing one

**Settings → Runners → Remove**, or:

```bash
curl -sS -X DELETE "$WEFT_URL/v1/orgs/$ORG/runners/$RUNNER_ID" -b "$COOKIE_JAR"
```

The credential is dead immediately. The process finds out on its next
call — there is nothing to signal, because nothing connects to it —
prints `this runner has been removed; register it again` and exits `2`. A
job that was running on it is **failed**, with `runner removed while the
job was running`, and is not retried: removing a runner is a decision,
and silently re-running the job somewhere else is not what the person who
pressed the button asked for.

Stop the process and delete `DIR/.runner` on the machine as well. The
credential is already useless, but a file that reads like a live secret
is a thing somebody will later assume is one.
