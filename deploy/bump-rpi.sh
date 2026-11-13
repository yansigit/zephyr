#!/usr/bin/env bash
# Moves every rpi pin to another upstream release (default: the newest on crates.io), the way
# .github/workflows/rpi-bump.yml does each day:
#   deploy/bump-rpi.sh            # the latest stable release
#   deploy/bump-rpi.sh 0.22.0     # a given one
# It edits the Dockerfile (version and both sha256s, from the release's own .sha256 files, checked
# against the downloaded tarball), RPI_PIN, the Cargo pins and Cargo.lock, fake-rpi's --version, and
# the fixed path the ignored tests and release.sh look in. It downloads the x86_64 binary to
# $RPI_BUMP_DIR (default target/rpi-<version>) so the ignored tests can run against it:
#   ZEPHYR_REAL_RPI=1 RPI_BIN=target/rpi-<version>/rpi cargo test --workspace -- --ignored
# Prose that says what rpi 0.N does is left alone: it was checked against that version.
# rpi-bump.yml pushes only what these edits make of each file: keep `pinned` and ALLOWED in
# .github/bump-check.py in step with every sed below.
# With GITHUB_OUTPUT set it writes version=, old= and rpi= there; already current, it writes nothing.
set -euo pipefail
cd "$(dirname "$0")/.."
die() { echo "bump-rpi: $*" >&2; exit 1; }

old=$(sed -n 's/^ARG RPI_VERSION=//p' deploy/Dockerfile)
[ -n "$old" ] || die "no ARG RPI_VERSION in deploy/Dockerfile"
new=${1:-}
if [ -z "$new" ]; then
  new=$(curl -fsS -A 'zephyr-rpi-bump' https://crates.io/api/v1/crates/rpi-tui \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["crate"]["max_stable_version"])')
fi
[[ $new =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not a release version: $new"
if [ "$new" = "$old" ]; then echo "rpi $old is the latest"; exit 0; fi
# Never move backwards: a yanked or re-tagged release must not downgrade the pins.
[ "$(printf '%s\n%s\n' "$old" "$new" | sort -V | tail -1)" = "$new" ] || die "$new is older than $old"

dir=${RPI_BUMP_DIR:-target/rpi-$new}
mkdir -p "$dir"
url=https://github.com/bigfish1913/pi-rust/releases/download/v$new
declare -A sha
for t in x86_64 aarch64; do
  f=rpi-v$new-$t-unknown-linux-gnu.tar.gz
  # The release publishes one <tarball>.sha256 per asset; a release still uploading has none yet.
  sha[$t]=$(curl -fsSL "$url/$f.sha256" | awk '{print $1}') || die "no $f.sha256 in the v$new release (yet?)"
  [[ ${sha[$t]} =~ ^[0-9a-f]{64}$ ]] || die "$f.sha256 is not a sha256"
  curl -fsSLo "$dir/$f" "$url/$f"
  echo "${sha[$t]}  $dir/$f" | sha256sum -c --quiet - || die "$f does not match its .sha256"
done
tar -xzf "$dir/rpi-v$new-x86_64-unknown-linux-gnu.tar.gz" --no-same-owner -C "$dir" ./rpi
[ "$("$dir/rpi" --version)" = "rpi $new" ] || die "the v$new binary reports $("$dir/rpi" --version)"

minor=${new%.*}
sed -i -e "s/^ARG RPI_VERSION=.*/ARG RPI_VERSION=$new/" \
  -e "s/^ARG RPI_SHA256_AMD64=.*/ARG RPI_SHA256_AMD64=${sha[x86_64]}/" \
  -e "s/^ARG RPI_SHA256_ARM64=.*/ARG RPI_SHA256_ARM64=${sha[aarch64]}/" deploy/Dockerfile
sed -i "s/^const RPI_PIN: &str = \"rpi [0-9.]*\";/const RPI_PIN: \&str = \"rpi $minor.\";/" crates/zephyr/src/main.rs
grep -q "RPI_PIN: &str = \"rpi $minor.\"" crates/zephyr/src/main.rs || die "RPI_PIN was not updated"
# rpi-tui, rpi-plugin-sdk and rpi-ai, each pinned with `=`.
sed -i -E "s/^(rpi-[a-z-]+ = (\{ version = )?)\"=$old\"/\1\"=$new\"/" crates/*/Cargo.toml
! grep -En "^rpi-[a-z-]+ = .*\"=$old\"" crates/*/Cargo.toml || die "a Cargo pin was not updated"
sed -i "s/print(\"rpi $old\")/print(\"rpi $new\")/" crates/zephyr/tests/fake-rpi.py
grep -rlF "rpi-$old/bin/rpi" crates deploy CLAUDE.md | xargs -r sed -i "s|rpi-$old/bin/rpi|rpi-$new/bin/rpi|g"
sed -i "s/the upstream \`rpi\` $old release/the upstream \`rpi\` $new release/" README.md
sed -i -e "s/rpi is pinned to [0-9.]*x/rpi is pinned to $minor.x/" -e "s/\"=$old\"/\"=$new\"/g" CLAUDE.md
# The `=` pins leave one version to pick; this also moves rpi-telemetry, which rpi-tui pins.
cargo update -q -p rpi-tui -p rpi-plugin-sdk -p rpi-ai

echo "rpi $old -> $new; binary at $dir/rpi"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  { echo "version=$new"; echo "old=$old"; echo "rpi=$(realpath "$dir/rpi")"; } >> "$GITHUB_OUTPUT"
fi
