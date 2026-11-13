#!/usr/bin/env bash
# rpi-bump.yml's build runs the bump and the tests here: a throwaway container, with this checkout
# mounted at /src and its .git read only. Nothing the new rpi binary or a crate's build script starts
# can see the runner's processes, whose environment holds the Actions runtime token (it can write
# the cache), and nothing outlives the command.
#   .github/sandbox.sh image                                build zephyr-sandbox:local
#   SANDBOX_ENV="NAME NAME=value ..." .github/sandbox.sh run CMD [ARG...]
# The image is the final image's Ubuntu (its glibc, which the rpi release needs) with the image's
# own Rust toolchain, both by deploy/Dockerfile's digests, and the tools the tests use, as on
# zephyr-local's runners (deploy/runner/Dockerfile). A command runs as this user, without
# /.dockerenv, so the tests see a host as on any runner. The workflow builds the image and copies
# this script before anything untrusted runs, and calls the copy.
set -euo pipefail
cd "$(dirname "$0")/.."
image=zephyr-sandbox:local

case "${1:-}" in
  image)
    pinned() { sed -n "s/^FROM \($1:[^ ]*@sha256:[0-9a-f]\{64\}\) AS $2\$/\1/p" deploy/Dockerfile; }
    ubuntu=$(pinned ubuntu ubuntu-24.04) rust=$(pinned rust toolchain)
    if [ -z "$ubuntu" ] || [ -z "$rust" ]; then
      echo "sandbox: deploy/Dockerfile pins no ubuntu or rust image" >&2
      exit 1
    fi
    docker build -q -t "$image" - >/dev/null <<EOF
FROM $ubuntu
COPY --from=$rust /usr/local/rustup /usr/local/rustup
COPY --from=$rust /usr/local/cargo /usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup PATH=/usr/local/cargo/bin:\$PATH
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config \
      libssl-dev python3 tmux git curl ca-certificates procps \
    && rm -rf /var/lib/apt/lists/* && rustup component add rustfmt clippy
EOF
    ;;
  run)
    shift
    envs=()
    for e in ${SANDBOX_ENV:-}; do envs+=(-e "$e"); done
    exec docker run --rm -v "$PWD:/src" -v "$PWD/.git:/src/.git:ro" -w /src "${envs[@]}" \
      -e "HOST_UID=$(id -u)" -e "HOST_GID=$(id -g)" "$image" bash -c '
        set -e
        rm -f /.dockerenv
        groupadd -o -g "$HOST_GID" host
        useradd -o -u "$HOST_UID" -g "$HOST_GID" -m host
        exec setpriv --reuid="$HOST_UID" --regid="$HOST_GID" --init-groups \
          env HOME=/home/host CARGO_HOME=/home/host/.cargo "$@"' sandbox "$@" ;;
  *) echo "usage: .github/sandbox.sh image | run CMD [ARG...]" >&2; exit 1 ;;
esac
