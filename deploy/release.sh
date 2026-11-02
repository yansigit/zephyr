#!/usr/bin/env bash
# One-command release (README "Releasing"), run from a clean, pushed `main`:
#   deploy/release.sh 0.2.1   bump, tag, wait for ghcr, install the CLI, upgrade
#   deploy/release.sh --cli   a host-only change: test and install the CLI, nothing published
set -euo pipefail
cd "$(dirname "$0")/.."

die() { echo "release: $*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }
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

step "testing"
cargo fmt --check
cargo clippy -q --all-targets -- -D warnings
cargo test -q

if [ -n "$version" ]; then
  tag=v$version
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "$tag already exists"
  current=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
  step "releasing $current -> $version"
  # The CLI pulls ghcr.io/yansigit/zephyr:<crate version>, so the version and the tag move together.
  sed -i "0,/^version = \"$current\"/s//version = \"$version\"/" Cargo.toml
  cargo update -q --workspace
  git commit -q -am "chore(release): $version"
  git tag -a "$tag" -m "$tag"
  git push -q origin main "$tag"

  step "waiting for ghcr.io/yansigit/zephyr:$version (.github/workflows/image.yml, ~20 min)"
  run=
  for _ in $(seq 60); do
    run=$(gh run list --workflow image.yml --branch "$tag" --limit 1 --json databaseId --jq '.[0].databaseId // empty')
    [ -n "$run" ] && break
    sleep 5
  done
  [ -n "$run" ] || die "no image workflow run started for $tag"
  gh run watch "$run" --exit-status >/dev/null || die "the image workflow failed: gh run view $run --log-failed
  if GitHub's runners are out of minutes: gh variable set ZEPHYR_RUNNER --body zephyr-local && gh run rerun $run
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
