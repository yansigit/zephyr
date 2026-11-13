#!/usr/bin/env bash
# The owner's self-hosted CI runners, label zephyr-local, which .github/workflows run on beside
# GitHub's (README "CI"):
#   deploy/runner/start.sh up [N]   build zephyr-runner:local (deploy/runner/Dockerfile), start
#                                   zephyr-buildkitd and N runner slots (default 2) as systemd user
#                                   units zephyr-runner@<i>; run again after changing the
#                                   Dockerfile or the limits
#   deploy/runner/start.sh down     stop the slots and unregister them; the BuildKit cache stays
#   deploy/runner/start.sh serve I  what unit zephyr-runner@I runs: one runner for one job
#   deploy/runner/start.sh warm     what timer zephyr-ci-warm runs: build main's latest commit into
#                                   the cache volume the runners read
# Ephemeral: each container registers with --ephemeral, takes one job and exits, and is removed
# (--rm); systemd then starts a fresh one from the image. No home, no ~/.cargo, no work tree or
# target dir outlives a job, so nothing one job's code leaves behind reaches the next. A new image
# or new limits apply from each slot's next job. Each runner gets ZEPHYR_RUNNER_CPUS (default 12)
# CPUs and ZEPHYR_RUNNER_MEMORY (default 24g), no capabilities and no-new-privileges.
# The one volume a runner has is zephyr-ci-cache, read only: main's last build, which a job copies
# in to start warm. Only `warm` writes it, from the host, never a job: it fetches main's commit with
# the owner's gh and builds it in a throwaway container, so no branch's code ever lands in it.
# Everything sits on the zephyr-ci bridge network, never the host's: jobs get no Docker socket, no bind
# mounts and no route to 127.0.0.1, so the live `zephyr` container (--network host) is out of reach.
# zephyr-buildkitd answers only a client with the certificate this script makes (mTLS), which
# image.yml alone is given, as repo secrets; the test jobs on the same network cannot use it.
# Known risk: rootless BuildKit needs unprivileged user namespaces, which Ubuntu's AppArmor restriction
# (sysctl kernel.apparmor_restrict_unprivileged_userns=1) can block. If the builder fails to start,
# stop there: never switch to a privileged builder.
# The units call gh as the owner, which reads ~/.config/gh/hosts.yml. If gh keeps its token in a
# keyring a user service cannot reach, put GH_TOKEN=<token> in ~/.config/zephyr-runner/env (0600).
set -euo pipefail
cd "$(dirname "$0")"

# The repository the runners register with; a fork names its own.
repo=${ZEPHYR_GH_REPO:-yansigit/zephyr}
cpus=${ZEPHYR_RUNNER_CPUS:-12}
memory=${ZEPHYR_RUNNER_MEMORY:-24g}
# Main's last build, which the runners mount read only (ci.sh seed) and `warm` alone writes.
ci_cache=zephyr-ci-cache
# What `warm` builds with: with two runners at 12, it leaves a few of the machine's 28 cores to the
# live container even while all three run.
warm_cpus=${ZEPHYR_WARM_CPUS:-6}
warm_memory=${ZEPHYR_WARM_MEMORY:-12g}
net=zephyr-ci
builder=zephyr-buildkitd
cache=zephyr-buildkit-cache
buildkit_image=moby/buildkit:v0.34.1-rootless
runner_image=zephyr-runner:local
units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
config=${XDG_CONFIG_HOME:-$HOME/.config}/zephyr-runner
# BuildKit's CA, the daemon's and image.yml's certificates and keys: the owner's alone (0700).
tls=$config/buildkit
# The copy of this script the units run, so they do not depend on the checkout `up` ran from.
self=${XDG_DATA_HOME:-$HOME/.local/share}/zephyr-runner/start.sh
# How runners and the builder are started; a container started otherwise is replaced (once idle).
kind=ephemeral-3
builder_kind=mtls-1

die() { echo "runner: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
exists() { docker container inspect "$1" >/dev/null 2>&1; }
label() { docker inspect -f "{{index .Config.Labels \"$2\"}}" "$1"; }
# Short-lived (1 h) tokens go straight into `docker exec` args, never to disk or the container's env.
token() { gh api -X POST "repos/$repo/actions/runners/$1" --jq .token; }
# online|offline and busy (true|false) of a registered runner; empty when it is not registered.
state() { gh api "repos/$repo/actions/runners?per_page=100" --jq ".runners[] | select(.name == \"$1\") | \"\(.status) \(.busy)\""; }
# Removes a runner's registration (GitHub refuses while it runs a job); none is fine.
unregister() {
  local id
  id=$(gh api "repos/$repo/actions/runners?per_page=100" --jq ".runners[] | select(.name == \"$1\") | .id") || return
  [ -z "$id" ] || gh api -X DELETE "repos/$repo/actions/runners/$id" >/dev/null 2>&1
}

# One runner for one job, in the foreground: unit zephyr-runner@<i> runs this, and systemd starts
# it again when it returns.
serve() {
  local name=zephyr-runner-$1 t n
  local fails=${XDG_RUNTIME_DIR:-/tmp}/zephyr-runner-$1.fails
  n=$(cat "$fails" 2>/dev/null || echo 0)
  # A failed login or registration waits 20 s, doubling to 10 min, so it never spins.
  failed() {
    local wait=$((10 << (n < 5 ? n + 1 : 6)))
    [ "$wait" -le 600 ] || wait=600
    echo $((n + 1)) > "$fails"
    echo "$name: $1; trying again in ${wait}s" >&2
    docker rm -f "$name" >/dev/null 2>&1 || true
    sleep "$wait"
    exit 1
  }
  docker rm -f "$name" >/dev/null 2>&1 || true
  t=$(token registration-token) || failed "no registration token (is gh logged in? see this script's header)"
  # Root only to remove /.dockerenv, which Docker makes at each start (without it the zephyr tests
  # see a host, as on GitHub's runners: host::in_container), then the runner user for good, with no
  # capability left and no-new-privileges (so no sudo). SETUID and SETGID are only for that switch.
  # Idle until config.sh writes .runner, then serve one job from /home/runner.
  docker run -d --rm --name "$name" --network "$net" --label "zephyr.runner=$kind" \
    --cpus "$cpus" --memory "$memory" --cap-drop ALL --cap-add SETUID --cap-add SETGID \
    --security-opt no-new-privileges --user root -e HOME=/home/runner -v "$ci_cache:/cache:ro" "$runner_image" \
    sh -c 'rm -f /.dockerenv; exec setpriv --reuid=runner --regid=runner --init-groups bash -c "until [ -f .runner ]; do sleep 2; done; exec ./run.sh"' \
    >/dev/null || failed "the runner container did not start"
  # --replace takes over the registration the last job's runner left under this name.
  docker exec --user runner -e HOME=/home/runner "$name" ./config.sh --url "https://github.com/$repo" --token "$t" \
    --name "$name" --labels zephyr-local --unattended --replace --ephemeral >/dev/null || failed "registration failed"
  rm -f "$fails"
  echo "$name is waiting for a job"
  docker wait "$name" >/dev/null 2>&1 || true
}

unit() {
  # PATH as now, so systemd finds docker and gh; the repository and the limits as this shell has them.
  cat <<EOF
[Unit]
Description=zephyr-local CI runner %i: one job per container (deploy/runner/start.sh)

[Service]
Environment="PATH=$PATH" "ZEPHYR_GH_REPO=$repo" "ZEPHYR_RUNNER_CPUS=$cpus" "ZEPHYR_RUNNER_MEMORY=$memory"
EnvironmentFile=-$config/env
ExecStart=$self serve %i
ExecStop=-$(command -v docker) rm -f zephyr-runner-%i
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
EOF
}

# Builds main's latest commit, when it moved, into zephyr-ci-cache: the dependencies of what
# ci.yml's lint and test compile, at the paths a runner's job uses (a job rebuilds the workspace's
# own crates every time). The commit is heads/main's, read here with the owner's gh, and its source
# main's tarball for that commit: a branch's code never runs here. It builds in a throwaway
# container from the runner image, started as the runners are (no capability but the switch to
# the runner user, no-new-privileges), from the cache it replaces, into a folder renamed into place
# when done; then /cache/current points at it in one rename, and the build before is kept for a job
# still copying it.
warm() {
  local sha state=${XDG_STATE_HOME:-$HOME/.local/state}/zephyr-runner/warm.sha
  sha=$(gh api "repos/$repo/git/ref/heads/main" --jq .object.sha) || die "could not read main's commit"
  [[ $sha =~ ^[0-9a-f]{40}$ ]] || die "not a commit: $sha"
  if [ "$(cat "$state" 2>/dev/null)" = "$sha" ]; then echo "the cache holds main ($sha)"; return; fi
  warm_tmp=$(mktemp -d)
  trap 'rm -rf "$warm_tmp"' EXIT
  gh api "repos/$repo/tarball/$sha" > "$warm_tmp/main.tar.gz" || die "could not fetch main's $sha"
  chmod 755 "$warm_tmp" && chmod 644 "$warm_tmp/main.tar.gz"
  step "building main $sha into $ci_cache"
  docker volume create "$ci_cache" >/dev/null
  # As root only to remove /.dockerenv and give /cache to the runner user, then that user for good.
  # shellcheck disable=SC2016 # expanded in the container
  docker run --rm --name zephyr-ci-warm --network "$net" --cpus "$warm_cpus" --memory "$warm_memory" \
    --label zephyr.runner=warm --cap-drop ALL --cap-add SETUID --cap-add SETGID --cap-add CHOWN \
    --security-opt no-new-privileges --user root -e HOME=/home/runner -e "SHA=$sha" \
    -v "$ci_cache:/cache" -v "$warm_tmp:/in:ro" "$runner_image" sh -c '
      rm -f /.dockerenv
      [ "$(stat -c %U /cache)" = runner ] || chown -R runner:runner /cache
      exec setpriv --reuid=runner --regid=runner --init-groups bash -c "$1"' sh '
      set -euo pipefail
      if [ "$(cat /cache/current/sha 2>/dev/null)" = "$SHA" ]; then exit 0; fi
      src=/home/runner/_work/zephyr/zephyr
      mkdir -p "$src"
      tar -xzf /in/main.tar.gz --strip-components 1 -C "$src"
      if [ -f /cache/current/sha ]; then
        cp -a /cache/current/target/. "$CARGO_TARGET_DIR/"
        cp -a /cache/current/registry/. "$CARGO_HOME/registry/"
      fi
      cd "$src"
      # Newer than anything copied: the tarball carries the commit time.
      find . -type f -exec touch -c {} +
      cargo nextest run --workspace --profile ci --no-run
      cargo clippy --all-targets -- -D warnings || echo "clippy failed on main; its build is kept"
      new=/cache/build-$SHA.tmp.$$
      rm -rf "$new" && mkdir -p "$new/registry"
      cp -a "$CARGO_TARGET_DIR" "$new/target"
      cp -a "$CARGO_HOME/registry/cache" "$CARGO_HOME/registry/index" "$new/registry/"
      echo "$SHA" > "$new/sha"
      chmod -R a+rX "$new"
      rm -rf "/cache/build-$SHA" && mv -T "$new" "/cache/build-$SHA"
      old=$(readlink /cache/current || true)
      ln -sfn "build-$SHA" /cache/current.new && mv -T /cache/current.new /cache/current
      for d in /cache/build-*; do
        case ${d#/cache/} in "build-$SHA" | "$old") ;; *) rm -rf "$d" ;; esac
      done' || die "main $sha did not build; the cache keeps what it had"
  mkdir -p "$(dirname "$state")"
  echo "$sha" > "$state"
  echo "the cache holds main ($sha)"
}

warm_units() {
  cat > "$units/zephyr-ci-warm.service" <<EOF
[Unit]
Description=build main's latest commit into zephyr-ci-cache for the zephyr-local runners (deploy/runner/start.sh)

[Service]
Type=oneshot
TimeoutStartSec=60min
Environment="PATH=$PATH" "ZEPHYR_GH_REPO=$repo" "ZEPHYR_WARM_CPUS=$warm_cpus" "ZEPHYR_WARM_MEMORY=$warm_memory"
EnvironmentFile=-$config/env
ExecStart=$self warm
ExecStopPost=-$(command -v docker) rm -f zephyr-ci-warm
EOF
  cat > "$units/zephyr-ci-warm.timer" <<EOF
[Unit]
Description=check every 10 minutes whether main moved, and build it for the CI cache

[Timer]
OnBootSec=2min
OnUnitActiveSec=10min

[Install]
WantedBy=timers.target
EOF
}

# The CA, the daemon's certificate (for name zephyr-buildkitd) and image.yml's client certificate,
# made once; the client's three files go to the repo secrets image.yml reads.
certs() {
  if [ ! -f "$tls/client.pem" ]; then
    step "making BuildKit's CA and certificates in $tls"
    rm -rf "$tls"
    (
      set -e
      umask 077
      mkdir -p "$tls/daemon"
      cd "$tls"
      key() { openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$1" -keyout "$2" "${@:3}"; }
      key zephyr-buildkit-ca ca-key.pem -x509 -days 3650 -out ca.pem
      key "$builder" daemon/key.pem -out daemon.csr
      printf 'subjectAltName=DNS:%s\nextendedKeyUsage=serverAuth\n' "$builder" > daemon.ext
      openssl x509 -req -in daemon.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -days 3650 -extfile daemon.ext -out daemon/cert.pem
      key zephyr-image-workflow client-key.pem -out client.csr
      printf 'extendedKeyUsage=clientAuth\n' > client.ext
      openssl x509 -req -in client.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -days 3650 -extfile client.ext -out client.pem
      cp ca.pem daemon/
      rm -f ./*.csr ./*.ext ca.srl
    ) >/dev/null 2>&1 || die "could not make the certificates in $tls (openssl?)"
  fi
  step "giving image.yml the client certificate (repo secrets BUILDKIT_CA, BUILDKIT_CERT, BUILDKIT_KEY)"
  gh secret set BUILDKIT_CA --repo "$repo" < "$tls/ca.pem"
  gh secret set BUILDKIT_CERT --repo "$repo" < "$tls/client.pem"
  gh secret set BUILDKIT_KEY --repo "$repo" < "$tls/client-key.pem"
}

up() {
  local n=$1 i name
  # The rootless builder runs as uid 1000 and reads the daemon's key from the owner's folder.
  [ "$(id -u)" = 1000 ] || die "the BuildKit certificates are read as uid 1000; run this as that user"
  # User units stop at logout without linger.
  if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != yes ] &&
    ! loginctl enable-linger "$USER" 2>/dev/null; then
    echo "runner: the runners stop when you log out; to keep them: sudo loginctl enable-linger $USER" >&2
  fi
  step "building $runner_image"
  docker build -q -t "$runner_image" . >/dev/null
  docker network inspect "$net" >/dev/null 2>&1 || { step "creating network $net"; docker network create --driver bridge "$net" >/dev/null; }
  certs

  step "starting $builder"
  if exists "$builder" && [ "$(label "$builder" zephyr.builder)" != "$builder_kind" ]; then
    until [ "$(gh run list --repo "$repo" --workflow image.yml --status in_progress --json databaseId --jq length)" = 0 ]; do
      echo "waiting for an image build to finish before the builder is replaced"
      sleep 30
    done
    docker rm -f "$builder" >/dev/null
  fi
  if exists "$builder"; then
    docker start "$builder" >/dev/null
  else
    # Each build step in a PID namespace of its own (BuildKit's process sandbox), so no step can
    # see or signal the daemon; rootless, that needs /proc unmasked (systempaths=unconfined).
    docker run -d --name "$builder" --network "$net" --restart unless-stopped --label "zephyr.builder=$builder_kind" \
      --security-opt seccomp=unconfined --security-opt apparmor=unconfined --security-opt systempaths=unconfined \
      --cpus 8 --memory 16g -v "$cache:/home/user/.local/share/buildkit" -v "$tls/daemon:/certs:ro" \
      "$buildkit_image" --addr tcp://0.0.0.0:1234 \
      --tlscacert /certs/ca.pem --tlscert /certs/cert.pem --tlskey /certs/key.pem >/dev/null
  fi
  sleep 3
  [ "$(docker inspect -f '{{.State.Running}}' "$builder")" = true ] ||
    { docker logs --tail 20 "$builder" >&2; die "$builder is not running (user namespaces blocked? see the header); not falling back to --privileged"; }

  step "installing systemd user units zephyr-runner@.service and zephyr-ci-warm.timer"
  install -D -m 755 start.sh "$self"
  mkdir -p "$units"
  unit > "$units/zephyr-runner@.service"
  warm_units
  systemctl --user daemon-reload
  docker volume create "$ci_cache" >/dev/null
  systemctl --user enable --now zephyr-ci-warm.timer
  for i in $(seq "$n"); do
    name=zephyr-runner-$i
    step "$name"
    # A runner started otherwise (before ephemeral runners, or with other options) is replaced
    # once idle, never mid-job, with its old work volume if it had one.
    while exists "$name" && [ "$(label "$name" zephyr.runner)" != "$kind" ]; do
      if [ "$(state "$name" | cut -d' ' -f2)" != true ] && unregister "$name"; then
        echo "replacing it"
        docker rm -f "$name" >/dev/null
        docker volume rm "$name-work" >/dev/null 2>&1 || true
        break
      fi
      echo "waiting for $name to finish its job"
      sleep 10
    done
    systemctl --user enable --now "zephyr-runner@$i"
    for _ in $(seq 60); do
      [ "$(state "$name" | cut -d' ' -f1)" = online ] && exists "$name" &&
        [ "$(label "$name" zephyr.runner)" = "$kind" ] && { echo "$name is online"; continue 2; }
      sleep 2
    done
    die "$name did not come online; see: journalctl --user -u zephyr-runner@$i"
  done

  cat <<EOF

done. CI places jobs by repo variable ZEPHYR_CI (README "CI"):
  everything here:     gh variable set ZEPHYR_CI --body local
  everything on GitHub: gh variable set ZEPHYR_CI --body hosted   (this machine is down)
  both (the default):  gh variable delete ZEPHYR_CI
EOF
}

down() {
  local unit name id
  systemctl --user disable --now zephyr-ci-warm.timer >/dev/null 2>&1 || true
  for unit in $(systemctl --user list-units --all --plain --no-legend 'zephyr-runner@*' | awk '{print $1}'); do
    step "stopping $unit"
    systemctl --user disable --now "$unit" >/dev/null 2>&1
  done
  for name in $(docker ps -a --format '{{.Names}}' --filter name='^zephyr-runner-'); do
    docker rm -f "$name" >/dev/null
  done
  for id in $(gh api "repos/$repo/actions/runners?per_page=100" --jq '.runners[] | select(.name | startswith("zephyr-runner-")) | .id'); do
    gh api -X DELETE "repos/$repo/actions/runners/$id" >/dev/null || die "could not unregister runner $id (running a job?); retry when it is idle"
  done
  step "removing $builder and $net"
  docker rm -f "$builder" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
  echo "done. The caches and certificates stay; delete them with: docker volume rm $cache $ci_cache; rm -r $tls"
}

usage="usage: deploy/runner/start.sh up [N] | down | serve I | warm"
case "${1:-}" in
  up)
    n=${2:-2}
    if [ $# -gt 2 ] || ! [[ $n =~ ^[1-9][0-9]*$ ]]; then die "$usage"; fi
    up "$n" ;;
  down) [ $# -eq 1 ] || die "$usage"; down ;;
  serve)
    if [ $# -ne 2 ] || ! [[ $2 =~ ^[1-9][0-9]*$ ]]; then die "$usage"; fi
    serve "$2" ;;
  warm) [ $# -eq 1 ] || die "$usage"; warm ;;
  *) die "$usage" ;;
esac
