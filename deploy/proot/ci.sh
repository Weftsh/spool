#!/usr/bin/env bash
# deploy-validation without a container runtime, end to end.
#
# What the Fargate fleet can run of the deploy-validation job: fetch
# PRoot, prove the sandbox is what we think it is, start the stack,
# build the production image with kaniko under PRoot, run it, and run
# the real smoke against it over HTTP and SSH. Every step prints PASS
# or FAIL and the script exits non-zero on the first FAIL, so the job's
# log reads as a checklist. `SKIP_BUILD=1` runs everything but the
# build and the app (for a fleet whose allow-list cannot yet reach the
# kaniko image).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"; repo="$(cd "$here/../.." && pwd)"
export PROOT_WORK="${PROOT_WORK:-$HOME/.proot}"
step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; "$here/stack.sh" down >/dev/null 2>&1; exit 1; }

step "sandbox"
echo "uid $(id -u); $(grep -E 'CapEff|Seccomp:' /proc/self/status | tr '\n' ' ')"
if unshare -Ur true 2>/dev/null; then echo "user namespaces: allowed (not a Fargate task)"; else echo "user namespaces: refused (as on Fargate)"; fi

step "proot"
"$here/fetch-proot.sh" "$PROOT_WORK/bin/proot" || fail "could not fetch PRoot"
export PROOT="$PROOT_WORK/bin/proot"

step "stack up: postgres + minio under PRoot"
"$here/stack.sh" up || fail "stack did not come up"
# The two probes that decided the experiment: a chown by a multi-threaded
# process under fake root, and a static Go binary answering on a socket.
pg="$PROOT_WORK/stack/postgres"
"$PROOT" -0 -r "$pg" -b /proc -b /dev -w / /bin/sh -c 'touch /tmp/p && chown 1234:1234 /tmp/p && chown 0:0 / && echo "fake-root chown, incl. of /: ok"' || fail "PRoot fake root does not fake chown"
"$PROOT" -0 -r "$pg" -b /proc -b /dev -w / /usr/bin/python3 -c 'import os,threading; t=threading.Thread(target=lambda: os.chown("/",0,0)); t.start(); t.join(); print("chown from a second thread: ok")' 2>/dev/null || echo "(no python3 in the postgres image; thread probe skipped)"

step "admin task launcher: RunTask refusals against a fake aws"
# deploy-validation's step of the same name; bash and python3, no daemon.
bash "$repo/deploy/admin-ecs.test.sh" || fail "admin-ecs.test.sh failed"

if [ "${SKIP_BUILD:-0}" = "1" ]; then
  echo; echo "SKIP_BUILD=1: not building or running the production image"
else
  step "build the production image with kaniko under PRoot"
  SECONDS=0
  "$here/build-image.sh" "$repo" Dockerfile "$PROOT_WORK/app" || fail "image build failed"
  echo "build took ${SECONDS}s"

  # The two runner images the same way, so the smoke's runner legs run:
  # the ECS stand-in starts the Weft runner as a task, and the self-hosted
  # leg starts one itself. SKIP_RUNNERS=1 leaves them out and the legs
  # off, which is what a laptop wants while the app image is the question.
  if [ "${SKIP_RUNNERS:-0}" = "1" ]; then
    echo; echo "SKIP_RUNNERS=1: not building the runner images; the smoke's runner legs stay off"
  else
    step "build the runner images with kaniko under PRoot"
    SECONDS=0
    "$here/build-image.sh" "$repo" Dockerfile.runner "$PROOT_WORK/runner" || fail "runner image build failed"
    # The smoke drives no GitHub runner (there is no GitHub here), so its
    # image is checked for shape only; SKIP_GITHUB_RUNNER=1 leaves it out.
    if [ "${SKIP_GITHUB_RUNNER:-0}" = "1" ]; then
      echo "SKIP_GITHUB_RUNNER=1: not building the GitHub Actions runner image"
    else
      "$here/build-image.sh" "$repo" Dockerfile.github-runner "$PROOT_WORK/github-runner" || fail "GitHub runner image build failed"
      "$PROOT" -r "$PROOT_WORK/github-runner" -b /proc -b /dev -w / /usr/bin/zstd --version >/dev/null \
        && echo "the GitHub runner image has zstd, as a job's cache step assumes" \
        || fail "the GitHub runner image lacks zstd"
      # The pin on PRoot translating openat2 and fchmodat2: this image's
      # tar is GNU tar 1.35, which creates every nested directory through
      # the first and chmods it through the second. Under the PRoot the
      # fleet used to run, the first fleet build of this very image failed
      # on exactly this, 22,607 times.
      "$PROOT" -0 -r "$PROOT_WORK/github-runner" -b /proc -b /dev -w /tmp /bin/sh -c \
        'rm -rf /tmp/proot-tar && mkdir -p /tmp/proot-tar/t/x/y && : > /tmp/proot-tar/t/x/y/f \
         && tar czf /tmp/proot-tar/s.tgz -C /tmp/proot-tar/t . && mkdir /tmp/proot-tar/w \
         && cd /tmp/proot-tar/w && tar xzf /tmp/proot-tar/s.tgz && test -f x/y/f && rm -rf /tmp/proot-tar' \
        && echo "the GitHub runner image's tar extracts a nested tarball under PRoot: openat2 and fchmodat2 are translated" \
        || fail "GNU tar 1.35 could not extract a nested tarball under this PRoot: openat2 or fchmodat2 is not translated"
      # `docker build` and `docker run` inside a job on this image: the
      # tool the image ships, driving its own kaniko and PRoot, with the
      # runner itself under PRoot here as it is in the smoke's runner
      # legs. The base image comes from Docker Hub, as a job's would.
      step "docker build + run inside the GitHub runner image"
      "$PROOT" -i 10002:10002 -r "$PROOT_WORK/github-runner" -b /proc -b /dev -b /sys -b /etc/resolv.conf \
        -w /home/runner /bin/bash -c '
          set -e; export HOME=/home/runner PATH=/usr/local/bin:/usr/bin:/bin
          rm -rf /tmp/proj && mkdir /tmp/proj && cd /tmp/proj
          printf "FROM busybox\nRUN echo built-inside > /built\nCMD [\"cat\", \"/built\"]\n" > Dockerfile
          docker build -t weft-test:1 . | tail -3
          test "$(docker run --rm weft-test:1)" = built-inside
          test "$(docker run --rm -e GREETING=hi weft-test:1 sh -c "echo \$GREETING")" = hi
          test "$(docker run --rm -v /tmp/proj:/ctx -w /ctx weft-test:1 sh -c "ls Dockerfile")" = Dockerfile
          docker images | grep -q "^weft-test "
          docker compose up 2>&1 | grep -q "no Docker daemon" ' \
        && echo "docker build, run, -e, -v and images work inside the GitHub runner image; compose is refused by name" \
        || fail "docker inside the GitHub runner image did not build and run an image"
    fi
    echo "runner builds took ${SECONDS}s"
  fi

  step "run the production image under PRoot"
  "$here/stack.sh" app "$PROOT_WORK/app" || fail "the image did not become healthy"

  step "smoke: health, REST, git over HTTP and SSH, runners, CDN, web"
  "$here/stack.sh" smoke || fail "smoke failed"

  # `docker push` from a job, into this fleet's own container registry:
  # the image the GitHub runner image built above, pushed by the
  # `docker` that image ships, with nothing but a docker config holding
  # the credential — the same file the runner writes for its registry
  # proxy — then removed, pulled back and run. The server under test is
  # on loopback, which is what the proxy is too.
  if [ "${SKIP_RUNNERS:-0}" != "1" ] && [ "${SKIP_GITHUB_RUNNER:-0}" != "1" ]; then
    step "docker push + pull through the registry, inside the GitHub runner image"
    # Unique per run: the stack's data directory outlives a run, and a
    # second bootstrap of one name is refused.
    rorg="regpush-$(date +%s)"
    boot="$("$PROOT_WORK/stack/bootstrap.sh" --org "$rorg" --plan paid | tail -1)"
    tok="$(printf '%s' "$boot" | python3 -c 'import json,sys; print(json.load(sys.stdin)["admin_token"])')" \
      || fail "could not bootstrap an org for the registry push"
    curl -fsS -X PUT -H "Authorization: Bearer $tok" -H 'Content-Type: application/json' \
      -d '{"ecosystem":"oci","mode":"private"}' http://127.0.0.1:8080/v1/orgs/$rorg/packages/ecosystems >/dev/null \
      || fail "could not enable the container registry for the org"
    auth="$(printf 'x:%s' "$tok" | base64 | tr -d '\n')"
    "$PROOT" -i 10002:10002 -r "$PROOT_WORK/github-runner" -b /proc -b /dev -b /sys -b /etc/resolv.conf \
      -w /home/runner /usr/bin/env AUTH="$auth" RORG="$rorg" /bin/bash -c '
        set -e; export HOME=/home/runner PATH=/usr/local/bin:/usr/bin:/bin
        mkdir -p "$HOME/.docker"
        printf "{\"auths\":{\"127.0.0.1:8080\":{\"auth\":\"%s\"}}}" "$AUTH" > "$HOME/.docker/config.json"
        docker tag weft-test:1 127.0.0.1:8080/$RORG/weft-test:1
        docker push 127.0.0.1:8080/$RORG/weft-test:1
        docker rmi 127.0.0.1:8080/$RORG/weft-test:1 weft-test:1 >/dev/null
        test "$(docker run --rm 127.0.0.1:8080/$RORG/weft-test:1)" = built-inside
        rm -f "$HOME/.docker/config.json" ' \
      && echo "an image built in the runner image was pushed to the registry, pulled back and run" \
      || fail "docker push/pull through the registry did not round-trip inside the GitHub runner image"
  fi
fi

step "down"
"$here/stack.sh" down
echo; echo "PASS"
