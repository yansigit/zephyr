#!/usr/bin/env bash
# What the workflows run (README "CI"), so placement and the checks live in one place:
#   .github/ci.sh plan          where jobs go, from repo variable ZEPHYR_CI (both|local|hosted)
#   .github/ci.sh run CHECK     one check: lint | deny | test K/N (a nextest hash partition)
#   .github/ci.sh rescue        run here what GitHub refused to start on its own runners
# plan and rescue read the Actions API with GH_TOKEN (actions: read; checks: read for the reason).
set -euo pipefail
cd "$(dirname "$0")/.."

LOCAL=zephyr-local
HOSTED=ubuntu-latest
# The checks ci.yml runs; the hosted runner takes the last test shard when it is available, on a push
# to main or the nightly run (a pull request runs wholly on zephyr-local).
SHARDS=3
# How far back a hosted job refused for quota sends everything to zephyr-local; the first run after
# it tries hosted again, so a month's reset (or a raised limit) is picked up by itself.
WINDOW="6 hours"

die() { echo "ci: $*" >&2; exit 1; }
summary() { echo "$*" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"; echo "$*"; }
api() { gh api -H 'X-GitHub-Api-Version: 2022-11-28' "$@"; }

# Jobs GitHub never started: failed with no runner and no step (the spending limit or the quota,
# whose annotation says "The job was not started because ..."). Reads job objects on stdin.
NOT_STARTED='select(.conclusion == "failure" and (.steps | length) == 0 and (.runner_name // "") == "")'

# The id of a run in the last $WINDOW with a hosted job GitHub would not start, if any.
recent_refusal() {
  local since ids id n
  since=$(date -u -d "$WINDOW ago" +%Y-%m-%dT%H:%M:%SZ)
  ids=$(api "repos/$GITHUB_REPOSITORY/actions/runs?status=failure&created=%3E%3D$since&per_page=10" \
    --jq '.workflow_runs[].id')
  # A re-run is in progress, so not listed as failed: its earlier attempts count too.
  [ "${GITHUB_RUN_ATTEMPT:-1}" = 1 ] || ids="$GITHUB_RUN_ID $ids"
  for id in $ids; do
    n=$(api "repos/$GITHUB_REPOSITORY/actions/runs/$id/jobs?filter=all&per_page=100" \
      --jq "[.jobs[] | $NOT_STARTED | select(any(.labels[]; . == \"$LOCAL\") | not)] | length") || return
    if [ "$n" -gt 0 ]; then
      echo "$id"
      return 0
    fi
  done
}

plan() {
  local mode=${ZEPHYR_CI:-both} local=$LOCAL hosted=$HOSTED why run
  case $mode in
    both)
      # An API error must not stop CI: it just means trying hosted.
      run=$(recent_refusal) || { echo "ci: could not read recent runs; trying hosted" >&2; run=; }
      if [ "${GITHUB_EVENT_NAME:-}" = pull_request ]; then
        # A PR is pushed many times in review: it costs no hosted minutes. GitHub's clean 2-CPU
        # runner takes its shard on pushes to main and the nightly run, so its environment is
        # still tested before a release, and so is the path ZEPHYR_CI=hosted falls back on.
        hosted=$LOCAL
        why="a pull request: everything on $LOCAL (GitHub's shard runs on main and nightly)"
      elif [ -n "$run" ]; then
        hosted=$LOCAL
        why="GitHub refused a hosted job within $WINDOW (run $run): everything on $LOCAL"
      else
        why="test $SHARDS/$SHARDS on $HOSTED, the rest on $LOCAL"
      fi ;;
    local) hosted=$LOCAL why="ZEPHYR_CI=local: everything on $LOCAL" ;;
    hosted) local=$HOSTED why="ZEPHYR_CI=hosted: everything on $HOSTED" ;;
    *) die "ZEPHYR_CI is '$mode'; use both, local or hosted" ;;
  esac
  local k include="{\"check\":\"lint\",\"runner\":\"$local\"},{\"check\":\"deny\",\"runner\":\"$local\"}"
  for k in $(seq "$SHARDS"); do
    include+=",{\"check\":\"test $k/$SHARDS\",\"runner\":\"$([ "$k" = "$SHARDS" ] && echo "$hosted" || echo "$local")\"}"
  done
  summary "placement: $why"
  {
    echo "local=$local"
    echo "hosted=$hosted"
    # Rescue is possible only when a share went to hosted and zephyr-local is up.
    echo "rescue=$([ "$hosted" = "$HOSTED" ] && [ "$local" = "$LOCAL" ] && echo true || echo false)"
    echo "matrix={\"include\":[$include]}"
  } >> "${GITHUB_OUTPUT:-/dev/stdout}"
}

check() {
  local what=$1 log
  # Tests leave their homes in the temp dir; the runner empties RUNNER_TEMP before each job, where
  # a persistent runner's /tmp would grow without end.
  export TMPDIR=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
  if [ "${RUNNER_ENVIRONMENT:-}" = self-hosted ] && [ -d "${CARGO_TARGET_DIR:-}" ] &&
    [ "$(du -s --block-size=1G "$CARGO_TARGET_DIR" | cut -f1)" -gt 60 ]; then
    echo "ci: $CARGO_TARGET_DIR is over 60 GB; starting it afresh"
    rm -rf "$CARGO_TARGET_DIR"
  fi
  case $what in
    lint)
      shellcheck deploy/install.sh deploy/runner/start.sh .github/ci.sh
      cargo fmt --all --check
      cargo clippy --all-targets -- -D warnings ;;
    deny) cargo deny check advisories bans ;;
    "test "[1-9]*/[1-9]*)
      log=$TMPDIR/nextest.log
      local status=0
      cargo nextest run --workspace --profile ci --partition "hash:${what#test }" 2>&1 | tee "$log" ||
        status=$?
      # A test that passed only on its retry still shows, here and on the run's page. Both lists
      # come from nextest's closing summary.
      local flaky failed
      flaky=$(awk '/^ +Summary \[/ {s = 1} s && /^ +FLAKY /' "$log" | sed 's/^ */- /')
      failed=$(awk '/^ +Summary \[/ {s = 1} s && /^ +(TRY [0-9]+ )?(FAIL|TIMEOUT|SIG[A-Z]+|ABORT) \[/' "$log" |
        sed 's/^ */- /')
      [ -z "$flaky" ] || summary "$(printf '### Flaky in %s (passed on retry)\n%s' "$what" "$flaky")"
      [ -z "$failed" ] || summary "$(printf '### Failed in %s\n%s' "$what" "$failed")"
      return "$status" ;;
    *) die "unknown check '$what'" ;;
  esac
}

# Runs here each check GitHub refused to start on its runners. A check that started and failed is
# never run again: that is a real failure, and so is anything else that did not pass.
rescue() {
  local jobs refused bad name
  jobs=$(api "repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/${GITHUB_RUN_ATTEMPT:-1}/jobs?per_page=100" \
    --jq '.jobs[] | select(.name | test("^(lint|deny|test [0-9]+/[0-9]+) \\("))')
  bad=$(jq -r "select(.conclusion != \"success\") | select(. as \$j | [\$j | $NOT_STARTED] | length == 0) | .name" <<<"$jobs")
  [ -z "$bad" ] || die "not rescued, these ran (or were cancelled) and did not pass: $(echo "$bad" | paste -sd, -)"
  refused=$(jq -r "$NOT_STARTED | \"\(.id)\t\(.name)\"" <<<"$jobs")
  [ -n "$refused" ] || die "nothing to rescue"
  # fd 3, so nothing the checks run can read the list.
  while IFS=$'\t' read -r -u 3 id name; do
    summary "rescue: GitHub did not start '$name' ($(api "repos/$GITHUB_REPOSITORY/check-runs/$id/annotations" \
      --jq '[.[].message] | join(" ")' 2>/dev/null || echo "no annotation")); running it here"
    check "${name% (*}"
  done 3<<<"$refused"
}

case "${1:-}" in
  plan) plan ;;
  run) [ $# -eq 2 ] || die "usage: ci.sh run CHECK"; check "$2" ;;
  rescue) rescue ;;
  *) die "usage: ci.sh plan | run CHECK | rescue" ;;
esac
