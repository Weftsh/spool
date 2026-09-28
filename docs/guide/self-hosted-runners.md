# Running a self-hosted runner

Every workflow job on a Spool server runs on a runner your organization
registered. [Workflows](workflows.md#runners) describes runners from the
workflow author's side: `runs-on`, labels, groups, and the organization
policy that admits them. This page is the other side — you have a
machine, and you want your Spool server to run jobs on it.

The whole of it is one binary, `weft-runner`, which **asks for work
and never waits to be asked**. It makes outbound HTTPS calls to your
Spool server and nothing else. It does not listen on a port, it does not
need a public address, an inbound firewall rule, or a tunnel, and there
is nothing to expose.

Read [Isolating it](#isolating-it) before you put one on a machine that
matters. A runner executes shell commands out of a repository, as
whatever account it runs as, and every other decision on this page
follows from that.

## Getting the binary

There are no published binaries or images yet. Both come from the Spool
repository.

**From source.** You need the Rust toolchain `rust-toolchain.toml` pins:

```bash
# in a checkout of the Spool repository
cargo build --release -p stratum-runner
sudo install -m 0755 target/release/weft-runner /usr/local/bin/
```

**As a container.** `Dockerfile.runner` at the repository root builds a
small image: the binary, `git`, `bash`, `curl`, `ca-certificates`,
`build-essential`, `python3` and `jq` on Debian bookworm, with `tini` as
PID 1, running as an unprivileged `runner` user. There is no docker CLI
or socket in it — a job that can talk to a daemon can escape its
container — and no cloud CLIs.

```bash
docker build -f Dockerfile.runner -t weft-runner:local .
```

If your builds need a toolchain that image does not have, that is a
`FROM weft-runner:local` of your own. A job runs its steps directly on
whatever the runner runs on — here, inside that container — so whatever
is on its `PATH` is what the job gets. A workflow cannot pick an image
with `image:`; you pick it by building the runner.

Run with no command, `weft-runner` prints its usage and exits `2`;
`weft-runner --help` prints the same and exits `0`.

## Registering a machine

Registration is a two-step exchange, and the two secrets are different
things. An organization admin mints a **registration token** — under
**Settings → Runners → Add a runner**, or:

```bash
curl -sS -X POST "$WEFT_URL/v1/orgs/$ORG/runners/registration-token" \
  -b "$COOKIE_JAR" -H "Content-Type: application/json" \
  -d '{ "group": "default" }'
```

```json
{ "token": "weftg_…", "expires_at": 1800000000000, "group": "default",
  "command": "weft-runner register --url https://spool.example.com --token weftg_…" }
```

That token is **single-use and lasts one hour**. It is the right to
obtain a credential, not a credential — which is why it is safe enough to
paste into a cloud-init script and short-lived enough that a leaked one
is usually already spent. The URL in `command` is the server's public
URL (`STRATUM_PUBLIC_URL`, set by whoever runs the server).

The machine exchanges it once:

```bash
weft-runner register \
  --url https://spool.example.com --token weftg_… \
  --name build-01 --labels gpu,cuda-12 \
  --dir /var/lib/weft-runner
weft-runner run --dir /var/lib/weft-runner
```

| Flag | |
|---|---|
| `--url` | your Spool server's base URL |
| `--token` | the registration token |
| `--name` | defaults to the machine's hostname |
| `--labels` | comma-separated, case-folded to lowercase; `self-hosted`, the OS and the architecture are added for you |
| `--ephemeral` | take one job and exit — see [below](#ephemeral-runners-and-autoscaling) |
| `--dir` | where `.runner` and the job working directories live; defaults to `.` |

Every flag takes `--flag value` or `--flag=value`. `register` writes
`DIR/.runner` with mode `0600`. That file holds the runner's own
long-lived credential (`weftr_…`), so it is the thing to protect: back it
up nowhere, and if it leaks, remove the runner from the list and
register again.

`run` then loops — ask for a job, run it, ask again — printing one line
when it starts listening, one per job it takes and one per job it
finishes. Running `register` again under the same name **replaces** that
runner and kills the old credential; that is how rotation works, and
there is nothing else to it.

### Two knobs for `run`

`weft-runner run` reads two environment variables. Each must be a whole
number above zero; anything else is refused by name before `.runner` is
read, rather than quietly ignored. Unset and empty both mean the
default, so a service unit can clear one with `Environment=NAME=`.

| Variable | Default | |
|---|---|---|
| `STRATUM_RUNNER_MAX_PROCS` | `4096` | the most processes one job may have at once. Lower it on a small machine |
| `STRATUM_RUNNER_FLUSH_MS` | `1000` | how often a running job's log is sent to the server, in milliseconds |

## Run it as its own account, not root

A step runs as whoever started `weft-runner`, and the ceiling on a job's
processes is `RLIMIT_NPROC`, which the kernel does not apply to root.
So a runner started as root runs every workflow step as root, with the
whole machine and no bound on what it forks. It tells you so when it
starts:

```
weft-runner: warning: running as root — every workflow step runs as root on this machine, and the process ceiling that stops a fork bomb does not apply to root. Run the agent under an account of its own.
```

It is a warning, not a refusal: a throwaway build VM is a reasonable
place to run as root, and it is your machine to decide about. A systemd
unit with no `User=` line runs as root, which is the usual way to end up
here without meaning to.

## A systemd unit

```ini
# /etc/systemd/system/weft-runner.service
[Unit]
Description=Spool self-hosted runner
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=weft-runner
Group=weft-runner
WorkingDirectory=/var/lib/weft-runner
ExecStart=/usr/local/bin/weft-runner run --dir /var/lib/weft-runner
Restart=on-failure
RestartPreventExitStatus=2
RestartSec=5
KillSignal=SIGTERM
TimeoutStopSec=120

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --home-dir /var/lib/weft-runner --create-home weft-runner
sudo -u weft-runner weft-runner register --url https://spool.example.com \
  --token weftg_… --dir /var/lib/weft-runner
sudo systemctl enable --now weft-runner
```

Three details in there are load-bearing:

- **`User=` is what keeps it off root.** See the section above.
- **`SIGTERM` is a clean stop, and `TimeoutStopSec` has to allow for
  it.** On `SIGTERM` the runner kills the running job's process group,
  reports it `cancelled` so the check does not sit queued forever, and
  exits `0`. Give that longer than systemd's 90-second default if your
  jobs are large; a `SIGKILL` mid-job leaves a check waiting for a
  verdict nobody is going to send, until the server's own sweep fails it.
- **`Restart=on-failure` with `RestartPreventExitStatus=2`.** A removed
  runner exits `2` after printing `this runner has been removed; register
  it again`, and so does one whose `.runner` cannot be read or whose
  knobs are malformed; a clean stop exits `0`. Restarting into that same
  exit is a loop that fills the journal and fixes nothing, so exit `2` is
  left stopped, and a crash is restarted. A runner that loses its
  network does not exit at all — it backs off and asks again. If you
  removed the machine deliberately, `systemctl disable --now` it too.

The unit deliberately carries no `ProtectSystem=` or `PrivateTmp=`
hardening. Those are worth adding, but they are decisions about what your
builds are allowed to touch, and a hardening line that silently breaks
`make install` reads as the runner being broken. Add them knowing what
your jobs do.

## In a container

With the image from `Dockerfile.runner`, keep `.runner` in a volume so
restarts reuse the credential:

```bash
# once: exchange a registration token for this runner's credential
docker run --rm -v weft-runner:/work/runner weft-runner:local \
  register --url https://spool.example.com --token weftg_… --dir /work/runner
# then: take jobs until stopped
docker run -d --restart unless-stopped --name weft-runner \
  -v weft-runner:/work/runner weft-runner:local
```

The image's default command is `run --dir /work/runner`, as the
`runner` user. `--restart unless-stopped` restarts it on any exit,
including a removed runner's exit `2`, so when you remove the runner,
`docker rm -f` the container as well; for an ephemeral runner, leave the
restart policy off. `docker stop` sends `SIGTERM`, which is the clean stop
described above; give it long enough with `docker stop -t`. The image
has no health check because the runner listens on nothing: it is healthy
when **Settings → Runners** lists it online.

## Ephemeral runners, and autoscaling

`--ephemeral` takes exactly one job and exits `0`, and the server removes
the runner the moment that job reaches a terminal state. It is the only
way to be sure a job cannot see what the previous job left behind, and it
is what to reach for if you are scaling machines up and down.

The shape that works is **a fresh instance per job, registering at
boot**: something holding an `org:admin` credential mints a registration
token, the instance's boot script exchanges it with `register
--ephemeral`, `run` takes one job, and the instance terminates. The token
being single-use and hour-long is what makes that safe to put in
user-data.

The shape that does not work is a restart loop around an ephemeral
runner: its registration is gone after its one job, so the restarted
`run` gets a `401` and exits `2`. Ephemeral means the *machine* is
disposable, not just the process.

An ephemeral runner that never comes back is removed from the list after
**1 day** unseen; an ordinary one after **14 days**.

## What the runner needs from the network

Outbound HTTPS to your Spool server, and whatever your builds themselves
reach. That is the list.

| Direction | |
|---|---|
| **Outbound** | HTTPS to the URL you registered with — the claim loop, the job's log upload and its verdict — and the same host again for the `git` fetch of the repository |
| **Inbound** | none. Nothing listens. The runner has no port, no health endpoint and no callback |

`weft-runner run` talks to whatever URL you gave `register`, and it
clones from that URL too, whatever address the server believes it is
reachable at. So that URL has to be reachable from the runner's own
machine — a private hostname or a NAT-side address is entirely fine, and
it does not have to be the server's public URL.

The claim call is a **long poll**: the runner asks for a job and the
server holds the request open for up to twenty seconds
(`STRATUM_RUNNER_CLAIM_WAIT_MS` on the server) before answering "nothing",
rather than replying instantly and being asked again. A proxy or load
balancer between the runner and the server needs an idle timeout above
that or it will cut every empty poll, which looks like a runner that
flaps between `online` and `offline`.

If your egress goes through a proxy that terminates TLS, its CA has to be
in the machine's own trust store — for the container image, that is the
`extra_ca` build secret `Dockerfile.runner` already takes.

## Isolating it

**A runner executes code from a repository, as the account it runs as,
on the machine it runs on.** Nothing about the design changes that; the
whole point of a runner is running your build on your hardware. So the
question is only ever *what would this cost me if a job were hostile*,
and there are four answers worth having:

1. **A dedicated, unprivileged account with nothing of yours in its
   home.** Not your account, not `root`, and not an account that has an
   SSH key, a `~/.aws/credentials`, a kubeconfig or a signed-in
   package-registry token lying around. A job is a shell; everything that
   account can read, it can read.
2. **A machine, VM or container that only does this.** The runner's
   isolation between one job and the next is a fresh working directory
   and nothing more — a job can write outside it, leave a process running
   and start a daemon. A dedicated VM you can throw away, plus
   `--ephemeral`, is the version of this that actually holds.
3. **No ambient cloud credentials.** An instance profile, an IMDS
   endpoint or a mounted service-account token is reachable from any
   `curl` in any step. Block the metadata endpoint if the machine has
   one.
4. **Off the network you care about.** Give it the internet it needs and
   your Spool server; do not give it the route to your database, your
   internal registry or your admin panel. "It is behind the firewall" is
   what makes a runner interesting to somebody else.

And the settings that decide whose code reaches it at all:

- **Runner groups.** A group admits `all` repositories in the
  organization or `selected` ones. Put a sensitive machine in a group
  that admits only the repositories you trust to run on it
  ([Runner groups](workflows.md#runner-groups)).
- **The fork gate.** A change from a fork carries the contributor's own
  `.weft/*.yml`. Its workflows wait until a maintainer presses **Approve
  and run workflows**, per tip
  ([Changes pushed from a fork](workflows.md#changes-pushed-from-a-fork)).
  Anyone who can read a repository can fork it, so read that file before
  you approve it.

The [mining watch](workflows.md#mining-software) runs on every runner: a
step caught running a miner has its process group killed, the job fails,
and the event lands in the audit log as `workflow.abuse`.

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
