#!/usr/bin/env bash
# One-command release (README "Releasing"), run from a clean, pushed `main`:
#   deploy/release.sh 0.2.1   bump, tag, wait for ghcr, install the CLI, upgrade
#   deploy/release.sh --cli   a host-only change: test and install the CLI, nothing published
set -euo pipefail
cd "$(dirname "$0")/.."
# The owner's release: the CLI it installs and the container it upgrades are the defaults, never
# a side install this shell may point at (or a repository `cargo install` would bake in).
unset ZEPHYR_REPO ZEPHYR_BUILD_REPO ZEPHYR_IMAGE ZEPHYR_CONTAINER

die() { echo "release: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
# Whether version $1 is above $2, compared as bump-rpi.sh compares rpi's: a release never goes back.
newer() { [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | tail -1)" = "$1" ]; }
case "${1:-}" in
  --cli) version= ;;
  [0-9]*.[0-9]*.[0-9]*) version=$1 ;;
  *) die "usage: deploy/release.sh <version> | --cli" ;;
esac
[ $# -eq 1 ] || die "usage: deploy/release.sh <version> | --cli"

step "checking the checkout"
[ "$(git branch --show-current)" = main ] || die "not on main"
# `cargo install --path` builds the working tree, so anything uncommitted would ship.
[ -z "$(git status --porcelain)" ] || { git status --short >&2; die "the working tree has changes; commit or stash them"; }
git fetch -q origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "main differs from origin/main; pull or push first"
current=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -z "$version" ] || newer "$version" "$current" || die "$version is not above the current $current"

step "testing"
cargo fmt --check
cargo clippy -q --all-targets -- -D warnings
cargo test -q

step "testing against the pinned rpi"
# The ignored tests drive the real binary: $RPI_BIN, else this fixed path (the tests' RPI consts).
rpi=${RPI_BIN:-/home/user/Projects/.always-on/rpi-0.31.0/bin/rpi}
if [ ! -x "$rpi" ] && [ "$(uname -m)" = x86_64 ]; then
  # The pin moves with each rpi-bump merge: fetch the Dockerfile's release, checked as it checks it.
  v=$(sed -n 's/^ARG RPI_VERSION=//p' deploy/Dockerfile)
  s=$(sed -n 's/^ARG RPI_SHA256_AMD64=//p' deploy/Dockerfile)
  f=rpi-v$v-x86_64-unknown-linux-gnu.tar.gz
  t=$(mktemp -d)
  { curl -fsSLo "$t/$f" "https://github.com/bigfish1913/pi-rust/releases/download/v$v/$f" \
    && echo "$s  $t/$f" | sha256sum -c --quiet - \
    && mkdir -p "$(dirname "$rpi")" && tar -xzf "$t/$f" --no-same-owner -C "$(dirname "$rpi")" ./rpi; } \
    || echo "release: could not fetch rpi $v" >&2
  rm -rf "$t"
fi
# Nothing is released without the real-rpi suite: RPI_BIN=<path> is the one way past a missing binary.
[ -x "$rpi" ] || die "$rpi is missing, so the real-rpi tests cannot run; RPI_BIN=<path> names another copy"
export RPI_BIN="$rpi"
ZEPHYR_REAL_RPI=1 cargo test --workspace -- --ignored

if [ -n "$version" ]; then
  tag=v$version
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "$tag already exists"
  step "releasing $current -> $version"
  # The CLI pulls ghcr.io/yansigit/zephyr:<crate version>, so the version and the tag move together.
  sed -i "0,/^version = \"$current\"/s//version = \"$version\"/" Cargo.toml
  cargo update -q --workspace
  git commit -q -am "chore(release): $version"
  git tag -a "$tag" -m "$tag"
  # Both or neither: a tag pushed without its commit on main (or the reverse) is half a release.
  git push --atomic -q origin main "$tag"

  step "waiting for ghcr.io/yansigit/zephyr:$version (.github/workflows/image.yml, ~20 min)"
  run=
  for _ in $(seq 60); do
    run=$(gh run list --workflow image.yml --branch "$tag" --limit 1 --json databaseId --jq '.[0].databaseId // empty')
    [ -n "$run" ] && break
    sleep 5
  done
  [ -n "$run" ] || die "no image workflow run started for $tag"
  gh run watch "$run" --exit-status >/dev/null || die "the image workflow failed: gh run view $run --log-failed
  if GitHub refused it for minutes: gh run rerun $run (the whole run plans again and builds on zephyr-local)
  once the image is published, finish by hand: cargo install --locked --path crates/zephyr && zephyr upgrade"
fi

step "installing the CLI"
cargo install -q --locked --path crates/zephyr
first=$(command -v zephyr)
[ "$first" = "$HOME/.cargo/bin/zephyr" ] || echo "warning: $first comes first on PATH, so it runs instead; remove it" >&2
[ -n "$version" ] || { echo "done: the CLI is installed; the container is unchanged"; exit 0; }

step "upgrading the live install"
zephyr=$HOME/.cargo/bin/zephyr
# It waits until the agent is idle and backs up state.db first; the version need not be :latest yet.
"$zephyr" upgrade "$version"
echo "done: $tag is published and running"
