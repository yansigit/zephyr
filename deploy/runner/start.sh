#!/usr/bin/env bash
# The owner's self-hosted CI runners, label zephyr-local, which .github/workflows run on beside
# GitHub's (README "CI"):
#   deploy/runner/start.sh up [N]   build zephyr-runner:local (deploy/runner/Dockerfile), start
#                                   zephyr-buildkitd and N runners (default 2); run again after
#                                   changing the Dockerfile or the limits: a runner whose image or
#                                   limits differ is replaced once idle, one at a time
#   deploy/runner/start.sh down     unregister and remove them; the cache volumes stay
# Each runner gets ZEPHYR_RUNNER_CPUS (default 8) CPUs and ZEPHYR_RUNNER_MEMORY (default 16g), and
# keeps its work dir (checkouts, the cargo target dir) on its own volume zephyr-runner-<i>-work.
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

die() { echo "runner: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
exists() { docker container inspect "$1" >/dev/null 2>&1; }
# Short-lived (1 h) tokens go straight into `docker exec` args, never to disk or the container's env.
token() { gh api -X POST "repos/$repo/actions/runners/$1" --jq .token; }
# online|offline and busy (true|false) of a registered runner; empty when it is not registered.
state() { gh api "repos/$repo/actions/runners?per_page=100" --jq ".runners[] | select(.name == \"$1\") | \"\(.status) \(.busy)\""; }

start_runner() {
  local name=$1 want=$2
  # The image's workdir is /home/runner (user runner); idle until config.sh writes .runner, then serve
  # jobs. Without /.dockerenv, which Docker makes at each start, the zephyr tests see a host, as on
  # GitHub's runners, not the container (host::in_container).
  docker run -d --name "$name" --network "$net" --restart unless-stopped --label "zephyr.want=$want" \
    --cpus "$cpus" --memory "$memory" -v "$name-work:/home/runner/_work" "$runner_image" \
    bash -c 'sudo rm -f /.dockerenv; until [ -f .runner ]; do sleep 2; done; exec ./run.sh' >/dev/null
  # --replace takes over the registration a replaced container left under this name.
  docker exec "$name" ./config.sh --url "https://github.com/$repo" --token "$(token registration-token)" \
    --name "$name" --labels zephyr-local --unattended --replace >/dev/null
  local i
  for i in $(seq 30); do
    [ "$(state "$name")" = "online false" ] && { echo "$name is online"; return; }
    sleep 2
  done
  die "$name did not come online; see: docker logs $name"
}

up() {
  local n=$1 i name want
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

  # What a runner must have been started with to be kept: this image and these limits.
  want="$(docker image inspect -f '{{.Id}}' "$runner_image") $cpus $memory"
  # New runners first, so one is online before an old one is replaced.
  local order=()
  for i in $(seq "$n"); do
    if exists "zephyr-runner-$i"; then order+=("$i"); else order=("$i" "${order[@]}"); fi
  done
  for i in "${order[@]}"; do
    name=zephyr-runner-$i
    step "$name"
    if exists "$name" && [ "$(docker inspect -f '{{index .Config.Labels "zephyr.want"}}' "$name")" = "$want" ]; then
      docker start "$name" >/dev/null
      if docker exec "$name" test -f .runner; then echo "up to date"; continue; fi
      docker rm -f "$name" >/dev/null
    elif exists "$name"; then
      # One at a time and only once idle, so the others keep serving and no job is cut off.
      until [ "$(state "$name" | cut -d' ' -f2)" != true ]; do echo "waiting for $name to finish its job"; sleep 10; done
      echo "replacing it (new image or limits)"
      docker rm -f "$name" >/dev/null
    fi
    start_runner "$name" "$want"
  done

  cat <<EOF

done. CI places jobs by repo variable ZEPHYR_CI (README "CI"):
  everything here:     gh variable set ZEPHYR_CI --body local
  everything on GitHub: gh variable set ZEPHYR_CI --body hosted   (this machine is down)
  both (the default):  gh variable delete ZEPHYR_CI
EOF
}

down() {
  local name
  for name in $(docker ps -a --format '{{.Names}}' --filter name='^zephyr-runner-'); do
    step "removing $name"
    docker start "$name" >/dev/null # a stopped one can't run config.sh remove
    if docker exec "$name" test -f .runner; then
      docker exec "$name" ./config.sh remove --token "$(token remove-token)" ||
        die "could not unregister $name (running a job?); retry when it is idle"
    fi
    docker rm -f "$name" >/dev/null
  done
  step "removing $builder and $net"
  docker rm -f "$builder" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
  echo "done. The caches stay; delete them with: docker volume rm $cache \$(docker volume ls -qf name=zephyr-runner-)"
}

usage="usage: deploy/runner/start.sh up [N] | down"
case "${1:-}" in
  up)
    n=${2:-2}
    if [ $# -gt 2 ] || ! [[ $n =~ ^[1-9][0-9]*$ ]]; then die "$usage"; fi
    up "$n" ;;
  down) [ $# -eq 1 ] || die "$usage"; down ;;
  *) die "$usage" ;;
esac
