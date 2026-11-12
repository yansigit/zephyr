#!/usr/bin/env bash
# Installs the zephyr CLI on a fresh Linux machine as this user, then runs `zephyr init` (DESIGN §4.8):
#   deploy/install.sh [--from-image | --from-source] [--no-init] [-- INIT ARGS..]
# Run it from a checkout. It refuses an existing install (upgrade with `zephyr upgrade`), never runs
# sudo, curl or wget, installs no Docker, Rust or gh (it names their pages), and writes only the CLI.
set -euo pipefail

usage() { echo "usage: deploy/install.sh [--from-image | --from-source] [--no-init] [-- INIT ARGS..]" >&2; exit 2; }
die() { echo "install: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

from='' init=1
while [ $# -gt 0 ]; do
  case "$1" in
    --from-image | --from-source) [ -z "$from" ] || usage; from=${1#--from-} ;;
    --no-init) init= ;;
    --) shift; break ;;
    *) usage ;;
  esac
  shift
done
# Arguments for `zephyr init` with no init to give them to would be dropped.
[ -n "$init" ] || [ $# -eq 0 ] || usage

repo=${ZEPHYR_REPO:-ghcr.io/yansigit/zephyr}
container=${ZEPHYR_CONTAINER:-zephyr}
bin=$HOME/.local/bin
arch=$(uname -m)

step "checking this machine"
[ "$(uname -s)" = Linux ] || die "zephyr runs on Linux only"
[ "$(id -u)" -ne 0 ] || die "run this as your own user, not root: the CLI and zephyr's files live in your home"
docker version >/dev/null 2>&1 || die "docker does not answer for $(id -un). Install Docker (https://docs.docker.com/engine/install/)
  and let this user run it, as your own user, no sudo: https://docs.docker.com/engine/install/linux-postinstall/"
# A new CLI followed by init would upgrade a live container, so an existing install is left alone:
# the container, either data dir (also after `zephyr down`), or a CLI this would replace.
if docker inspect --type container "$container" >/dev/null 2>&1 ||
  [ -e "$HOME/.zephyr-daemon" ] || [ -e "$HOME/.zephyr" ] || [ -e "$bin/zephyr" ] || [ -L "$bin/zephyr" ]; then
  die "zephyr is installed here; upgrade with zephyr upgrade"
fi
if root=$(docker info -f '{{.DockerRootDir}}' 2>/dev/null) &&
  free=$(df -Pk "$root" 2>/dev/null | awk 'NR == 2 { print $4 }') && [[ $free =~ ^[0-9]+$ ]]; then
  [ "$free" -ge $((20 * 1024 * 1024)) ] ||
    echo "warning: $root has $((free / 1024 / 1024)) GB free; the image and its builds want 20 GB" >&2
fi

# The workspace's rust-version, and BuildKit, whose secrets `zephyr up --build` uses.
source_checks() {
  if ! [[ $(cargo --version 2>/dev/null) =~ ^cargo\ ([0-9]+)\.([0-9]+) ]] ||
    ! ((BASH_REMATCH[1] > 1 || (BASH_REMATCH[1] == 1 && BASH_REMATCH[2] >= 91))); then
    die "--from-source needs cargo 1.91 or newer: https://rustup.rs"
  fi
  docker buildx version >/dev/null 2>&1 ||
    die "--from-source needs docker buildx (zephyr up --build uses it): https://docs.docker.com/build/install-buildx/"
}

# A private ghcr.io package takes a login as the gh user, its token on stdin; no other registry gets it.
pull() {
  step "pulling $repo:latest"
  docker pull "$repo:latest" && return
  local reg=${repo%%/*} user
  case $repo in
    ghcr.io/*) ;;
    *) # As docker reads it: a first part with a dot or a port names a registry, else Docker Hub.
      case $reg in *.* | *:* | localhost) [ "$reg" != "$repo" ] || reg=docker.io ;; *) reg=docker.io ;; esac
      echo "could not pull $repo:latest: docker login $reg first, if it needs a login" >&2
      return 1 ;;
  esac
  if user=$(gh api user --jq .login) && gh auth token | docker login ghcr.io -u "$user" --password-stdin &&
    docker pull "$repo:latest"; then
    return
  fi
  echo "could not pull $repo:latest; if the package is private, sign in with gh auth login (https://cli.github.com) first" >&2
  return 1
}

# The image's own CLI, through a temp file beside the target so the `mv` is atomic. It must say
# what the image's `zephyr --version` says, as `zephyr upgrade` checks the CLI it fetches.
from_image() {
  local want got
  want=$(docker run --rm --entrypoint zephyr "$repo:latest" --version | head -n 1) ||
    die "could not run zephyr --version in $repo:latest"
  mkdir -p "$bin"
  tmp=$(mktemp "$bin/.zephyr-install.XXXXXX")
  trap 'rm -f "$tmp"' EXIT
  docker run --rm --entrypoint cat "$repo:latest" /usr/local/bin/zephyr >"$tmp" ||
    die "could not copy the CLI out of $repo:latest"
  chmod 755 "$tmp"
  got=$("$tmp" --version | head -n 1) || die "the CLI from $repo:latest does not run on this machine"
  if [ -z "$want" ] || [ "$got" != "$want" ]; then
    die "the CLI copied out of $repo:latest says '$got', not '$want' as the image does"
  fi
  # -n: never over a CLI that appeared since the check above.
  mv -n "$tmp" "$bin/zephyr" || true
  [ ! -e "$tmp" ] || die "$bin/zephyr appeared meanwhile; not replacing it"
  installed=$bin/zephyr
  # That CLI upgrades from the repository its image was built for (ZEPHYR_BUILD_REPO), else the
  # default: another repository has to stay named for `zephyr upgrade`.
  [ -z "${ZEPHYR_REPO:-}" ] ||
    echo "note: the CLI came from $repo; unless that image was built with ZEPHYR_BUILD_REPO=$repo," \
      "keep ZEPHYR_REPO=$repo set (in your shell's rc file) for zephyr upgrade" >&2
}

from_source() {
  step "building the CLI"
  # Built to upgrade from the repository it was installed for, when that is not the default.
  if [ -n "${ZEPHYR_REPO:-}" ]; then
    (cd "$(dirname "$0")/.." && ZEPHYR_BUILD_REPO=$repo cargo install --locked --path crates/zephyr)
  else
    (cd "$(dirname "$0")/.." && cargo install --locked --path crates/zephyr)
  fi
  installed=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin/zephyr
}

case $from in
  image)
    [ "$arch" = x86_64 ] || die "the image is built for x86_64 only and this is $arch: use --from-source"
    pull || exit 1
    from_image ;;
  source) source_checks; from_source ;;
  *)
    if [ "$arch" = x86_64 ] && pull; then
      from_image
    elif command -v cargo >/dev/null; then
      source_checks; from_source
    else
      die "could not install the CLI either way:
  from the image (x86_64 only): gh auth login (https://cli.github.com) if $repo is private, then deploy/install.sh --from-image
  from source: install Rust (https://rustup.rs), then deploy/install.sh --from-source"
    fi ;;
esac

dir=$(dirname "$installed")
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "warning: $dir is not on PATH; add it in your shell's rc file to run zephyr by name" >&2 ;;
esac
first=$(command -v zephyr || true)
[ -z "$first" ] || [ "$first" = "$installed" ] || echo "warning: $first comes first on PATH, so it runs instead; remove it" >&2

[ -n "$init" ] || { echo "done: $installed is installed; next, zephyr init"; exit 0; }
step "zephyr init"
exec "$installed" init "$@"
