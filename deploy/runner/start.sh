#!/usr/bin/env bash
# Self-hosted fallback for .github/workflows/image.yml when Actions minutes run out (README "Releasing"):
#   deploy/runner/start.sh up [N]   start zephyr-buildkitd and N runners (default 1), registered as zephyr-local
#   deploy/runner/start.sh down     unregister and remove them; the build cache volume stays
# Everything sits on the zephyr-ci bridge network, never the host's: jobs get no Docker socket, no bind
# mounts and no route to 127.0.0.1, so the live `zephyr` container (--network host) is out of reach.
# Known risk: rootless BuildKit needs unprivileged user namespaces, which Ubuntu's AppArmor restriction
# (sysctl kernel.apparmor_restrict_unprivileged_userns=1) can block. If the builder fails to start,
# stop there: never switch to a privileged builder.
set -euo pipefail

repo=yansigit/zephyr
net=zephyr-ci
builder=zephyr-buildkitd
cache=zephyr-buildkit-cache
buildkit_image=moby/buildkit:v0.34.1-rootless
runner_image=ghcr.io/actions/actions-runner:2.338.2

die() { echo "runner: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
exists() { docker container inspect "$1" >/dev/null 2>&1; }
# Short-lived (1 h) tokens go straight into `docker exec` args, never to disk or the container's env.
token() { gh api -X POST "repos/$repo/actions/runners/$1" --jq .token; }

up() {
  local n=$1 i name
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

  for i in $(seq "$n"); do
    name=zephyr-runner-$i
    step "starting $name"
    # The image's workdir is /home/runner (user runner); idle until config.sh writes .runner, then serve jobs.
    exists "$name" || docker run -d --name "$name" --network "$net" --restart unless-stopped \
      --cpus 2 --memory 4g "$runner_image" \
      bash -c 'until [ -f .runner ]; do sleep 2; done; exec ./run.sh' >/dev/null
    docker start "$name" >/dev/null
    if docker exec "$name" test -f .runner; then
      echo "already registered"
    else
      docker exec "$name" ./config.sh --url "https://github.com/$repo" --token "$(token registration-token)" \
        --name "$name" --labels zephyr-local --unattended --replace
    fi
  done

  cat <<EOF

done. Build on this runner:   gh variable set ZEPHYR_RUNNER --body zephyr-local
back to GitHub-hosted:        gh variable delete ZEPHYR_RUNNER
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
  echo "done. The build cache stays; delete it with: docker volume rm $cache"
}

case "${1:-}" in
  up)
    n=${2:-1}
    [ $# -le 2 ] && [[ $n =~ ^[1-9][0-9]*$ ]] || die "usage: deploy/runner/start.sh up [N] | down"
    up "$n" ;;
  down) [ $# -eq 1 ] || die "usage: deploy/runner/start.sh up [N] | down"; down ;;
  *) die "usage: deploy/runner/start.sh up [N] | down" ;;
esac
