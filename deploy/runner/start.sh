#!/usr/bin/env bash
# The owner's self-hosted CI runners, label zephyr-local, which .github/workflows run on beside
# GitHub's (README "CI"):
#   deploy/runner/start.sh up [N]   build zephyr-runner:local (deploy/runner/Dockerfile), start
#                                   zephyr-buildkitd and N runner slots (default 2) as systemd user
#                                   units zephyr-runner@<i>; run again after changing the
#                                   Dockerfile or the limits
#   deploy/runner/start.sh down     stop the slots and unregister them; the BuildKit cache stays
#   deploy/runner/start.sh serve I  what unit zephyr-runner@I runs: one runner for one job
# Ephemeral: each container registers with --ephemeral, takes one job and exits, and is removed
# (--rm); systemd then starts a fresh one from the image. No volume, no home, no ~/.cargo, no work
# tree or target dir outlives a job, so nothing one job's code leaves behind reaches the next. A new
# image or new limits apply from each slot's next job. Each runner gets ZEPHYR_RUNNER_CPUS (default
# 8) CPUs and ZEPHYR_RUNNER_MEMORY (default 16g).
# Everything sits on the zephyr-ci bridge network, never the host's: jobs get no Docker socket, no bind
# mounts and no route to 127.0.0.1, so the live `zephyr` container (--network host) is out of reach.
# Known risk: rootless BuildKit needs unprivileged user namespaces, which Ubuntu's AppArmor restriction
# (sysctl kernel.apparmor_restrict_unprivileged_userns=1) can block. If the builder fails to start,
# stop there: never switch to a privileged builder.
set -euo pipefail
cd "$(dirname "$0")"

# The repository the runners register with; a fork names its own.
repo=${ZEPHYR_GH_REPO:-yansigit/zephyr}
cpus=${ZEPHYR_RUNNER_CPUS:-8}
memory=${ZEPHYR_RUNNER_MEMORY:-16g}
net=zephyr-ci
builder=zephyr-buildkitd
cache=zephyr-buildkit-cache
buildkit_image=moby/buildkit:v0.34.1-rootless
runner_image=zephyr-runner:local
units=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user
# The copy of this script the units run, so they do not depend on the checkout `up` ran from.
self=${XDG_DATA_HOME:-$HOME/.local/share}/zephyr-runner/start.sh

die() { echo "runner: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
exists() { docker container inspect "$1" >/dev/null 2>&1; }
# Short-lived (1 h) tokens go straight into `docker exec` args, never to disk or the container's env.
token() { gh api -X POST "repos/$repo/actions/runners/$1" --jq .token; }
# online|offline and busy (true|false) of a registered runner; empty when it is not registered.
state() { gh api "repos/$repo/actions/runners?per_page=100" --jq ".runners[] | select(.name == \"$1\") | \"\(.status) \(.busy)\""; }

# One runner for one job, in the foreground: unit zephyr-runner@<i> runs this, and systemd starts
# it again when it returns.
serve() {
  local name=zephyr-runner-$1
  docker rm -f "$name" >/dev/null 2>&1 || true
  # The image's workdir is /home/runner (user runner); idle until config.sh writes .runner, then serve
  # one job. Without /.dockerenv, which Docker makes at each start, the zephyr tests see a host, as on
  # GitHub's runners, not the container (host::in_container).
  docker run -d --rm --name "$name" --network "$net" --label zephyr.runner=ephemeral \
    --cpus "$cpus" --memory "$memory" "$runner_image" \
    bash -c 'sudo rm -f /.dockerenv; until [ -f .runner ]; do sleep 2; done; exec ./run.sh' >/dev/null
  # --replace takes over the registration the last job's runner left under this name.
  docker exec "$name" ./config.sh --url "https://github.com/$repo" --token "$(token registration-token)" \
    --name "$name" --labels zephyr-local --unattended --replace --ephemeral >/dev/null
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
ExecStart=$self serve %i
ExecStop=-$(command -v docker) rm -f zephyr-runner-%i
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
EOF
}

up() {
  local n=$1 i name
  step "building $runner_image"
  docker build -q -t "$runner_image" . >/dev/null
  docker network inspect "$net" >/dev/null 2>&1 || { step "creating network $net"; docker network create --driver bridge "$net" >/dev/null; }

  step "starting $builder"
  if exists "$builder"; then
    docker start "$builder" >/dev/null
  else
    docker run -d --name "$builder" --network "$net" --restart unless-stopped \
      --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
      --cpus 8 --memory 16g -v "$cache:/home/user/.local/share/buildkit" \
      "$buildkit_image" --addr tcp://0.0.0.0:1234 --oci-worker-no-process-sandbox >/dev/null
  fi
  sleep 3
  [ "$(docker inspect -f '{{.State.Running}}' "$builder")" = true ] ||
    { docker logs --tail 20 "$builder" >&2; die "$builder is not running (user namespaces blocked? see the header); not falling back to --privileged"; }

  step "installing systemd user unit zephyr-runner@.service"
  install -D -m 755 start.sh "$self"
  mkdir -p "$units"
  unit > "$units/zephyr-runner@.service"
  systemctl --user daemon-reload
  for i in $(seq "$n"); do
    name=zephyr-runner-$i
    step "$name"
    # A runner from before ephemeral runners: kept volume and all. Replaced once idle, so no job is
    # cut off, and its volume goes with it.
    if exists "$name" && [ "$(docker inspect -f '{{index .Config.Labels "zephyr.runner"}}' "$name")" != ephemeral ]; then
      # Only once idle; once removed it is given no job.
      until [ "$(state "$name" | cut -d' ' -f2)" = false ] && gh api -X DELETE \
        "repos/$repo/actions/runners/$(gh api "repos/$repo/actions/runners?per_page=100" --jq ".runners[] | select(.name == \"$name\") | .id")" \
        >/dev/null 2>&1; do
        echo "waiting for $name to finish its job"
        sleep 10
      done
      echo "replacing it with an ephemeral runner"
      docker rm -f "$name" >/dev/null
      docker volume rm "$name-work" >/dev/null 2>&1 || true
    fi
    systemctl --user enable --now "zephyr-runner@$i"
    for _ in $(seq 30); do
      [ "$(state "$name" | cut -d' ' -f1)" = online ] && { echo "$name is online"; continue 2; }
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
  echo "done. The BuildKit cache stays; delete it with: docker volume rm $cache"
}

usage="usage: deploy/runner/start.sh up [N] | down | serve I"
case "${1:-}" in
  up)
    n=${2:-2}
    if [ $# -gt 2 ] || ! [[ $n =~ ^[1-9][0-9]*$ ]]; then die "$usage"; fi
    up "$n" ;;
  down) [ $# -eq 1 ] || die "$usage"; down ;;
  serve)
    if [ $# -ne 2 ] || ! [[ $2 =~ ^[1-9][0-9]*$ ]]; then die "$usage"; fi
    serve "$2" ;;
  *) die "$usage" ;;
esac
