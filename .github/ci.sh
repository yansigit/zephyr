#!/usr/bin/env bash
# What the workflows run (README "CI"), so placement and the checks live in one place:
#   .github/ci.sh plan          where jobs go, from repo variable ZEPHYR_CI (both|local|hosted)
#   .github/ci.sh skip          why this ci run needs no checks (docs only, or main's merge of a
#                               PR that already passed on the same tree); nothing when it does
#   .github/ci.sh run CHECK     one check: lint (shellcheck, fmt, clippy, cargo deny) | test
#   .github/ci.sh rescue-plan   which checks GitHub refused to start on its own runners
#   .github/ci.sh rescue        run those (CHECKS) here, with no GitHub token
# plan, skip and rescue-plan read the Actions API with GH_TOKEN (actions, checks and contents: read).
set -euo pipefail
cd "$(dirname "$0")/.."

LOCAL=zephyr-local
HOSTED=ubuntu-latest
# How far back a hosted job refused for quota sends everything to zephyr-local; the first run after
# it tries hosted again, so a month's reset (or a raised limit) is picked up by itself.
WINDOW="6 hours"
# Where zephyr-local's runners find main's last build, read only (deploy/runner/start.sh warm).
CACHE=/cache/current

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

# Two checks, each compiling once: lint and test. The test job goes to GitHub's runner only on
# the nightly run (its minutes are scarce, and a refused job costs a rescue), so its clean 2-CPU
# environment, and the path ZEPHYR_CI=hosted falls back on, are still tested daily. `hosted` is
# where a job that wants GitHub's runner goes (image.yml reads it), after the refusal window.
plan() {
  local mode=${ZEPHYR_CI:-both} local=$LOCAL hosted=$HOSTED test=$LOCAL why run
  case $mode in
    both)
      # An API error must not stop CI: it just means trying hosted.
      run=$(recent_refusal) || { echo "ci: could not read recent runs; trying hosted" >&2; run=; }
      if [ -n "$run" ]; then
        hosted=$LOCAL
        why="GitHub refused a hosted job within $WINDOW (run $run): everything on $LOCAL"
      elif [ "${GITHUB_EVENT_NAME:-}" = schedule ]; then
        test=$HOSTED
        why="the nightly run: test on $HOSTED, lint on $LOCAL"
      else
        why="everything on $LOCAL (GitHub's runner tests the nightly run)"
      fi ;;
    local) hosted=$LOCAL why="ZEPHYR_CI=local: everything on $LOCAL" ;;
    hosted) local=$HOSTED test=$HOSTED why="ZEPHYR_CI=hosted: everything on $HOSTED" ;;
    *) die "ZEPHYR_CI is '$mode'; use both, local or hosted" ;;
  esac
  summary "placement: $why"
  {
    echo "local=$local"
    echo "hosted=$hosted"
    # Rescue is possible only when a check went to hosted and zephyr-local is up.
    echo "rescue=$([ "$test" = "$HOSTED" ] && [ "$local" = "$LOCAL" ] && echo true || echo false)"
    echo "matrix={\"include\":[{\"check\":\"lint\",\"runner\":\"$local\"},{\"check\":\"test\",\"runner\":\"$test\"}]}"
  } >> "${GITHUB_OUTPUT:-/dev/stdout}"
}

# Prints why this run needs no checks, or nothing. Only a pull request or a push: the nightly and
# manual runs always test everything. Needs the checkout's parent (fetch-depth 2).
why_skip() {
  local files f tree pr head
  case ${GITHUB_EVENT_NAME:-} in pull_request | push) ;; *) return 0 ;; esac
  # A PR's checkout is its merge into the base, so HEAD^1 is the base for both events.
  files=$(git diff --name-only HEAD^1 HEAD) || return 0
  if [ -n "$files" ]; then
    local docs=yes
    while IFS= read -r f; do
      # Markdown outside the code: .github/ holds workflows, crates/ embeds files (assets/web) and
      # deploy/ ships the starter skills' SKILL.md in the image. No test reads README.md,
      # DESIGN.md or CLAUDE.md.
      case $f in .github/* | crates/* | deploy/*) docs= ;; *.md) ;; *) docs= ;; esac
      [ -n "$docs" ] || break
    done <<<"$files"
    if [ -n "$docs" ]; then
      echo "only Markdown outside the code changed ($(echo "$files" | wc -l) files)"
      return 0
    fi
  fi
  # main's merge of a PR rebased onto it is the PR's head tree, which its ci-ok already tested.
  [ "${GITHUB_EVENT_NAME:-}" = push ] && [ "${GITHUB_REF:-}" = refs/heads/main ] || return 0
  tree=$(git rev-parse 'HEAD^{tree}')
  for pr in $(api "repos/$GITHUB_REPOSITORY/commits/$GITHUB_SHA/pulls" --jq '.[].number') ; do
    head=$(api "repos/$GITHUB_REPOSITORY/pulls/$pr" --jq '.head.sha') || continue
    [ "$(api "repos/$GITHUB_REPOSITORY/git/commits/$head" --jq '.tree.sha')" = "$tree" ] || continue
    if [ "$(api "repos/$GITHUB_REPOSITORY/commits/$head/check-runs?check_name=ci-ok&per_page=100" \
      --jq '[.check_runs[] | select(.conclusion == "success")] | length')" -gt 0 ]; then
      echo "PR #$pr's head $head passed ci-ok on this same tree"
      return 0
    fi
  done
}

skip() {
  local why
  # Any error is a reason to run the checks, never to skip them.
  why=$(why_skip 2>/dev/null) || why=
  if [ -z "$why" ]; then summary "checks: all"; else summary "checks: none, $why"; fi
  echo "skip=$why" >> "${GITHUB_OUTPUT:-/dev/stdout}"
}

# zephyr-local's runners start with an empty target dir: copy main's last build in (read only at
# $CACHE, written by start.sh's warm unit, never by a job), then make every file of this checkout
# newer than anything copied, so cargo rebuilds whatever the branch changed and trusts the copy only
# for what it did not.
seed() {
  local from s=$SECONDS
  [ "${RUNNER_ENVIRONMENT:-}" = self-hosted ] && [ -f "$CACHE/sha" ] || return 0
  from=$(readlink -f "$CACHE")
  if cp -a "$from/target/." "$CARGO_TARGET_DIR/" && cp -a "$from/registry/." "${CARGO_HOME:?}/registry/"; then
    find . -path ./.git -prune -o -type f -exec touch -c {} +
    echo "ci: seeded from main's build of $(cat "$from/sha") in $((SECONDS - s)) s"
  else
    echo "ci: could not copy main's build; building from cold"
    rm -rf "${CARGO_TARGET_DIR:?}"/*
  fi
}

check() {
  local what=$1 log
  # Tests leave their homes in the temp dir; the runner empties RUNNER_TEMP before each job.
  export TMPDIR=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
  case $what in
    lint)
      shellcheck deploy/install.sh deploy/runner/start.sh deploy/release.sh deploy/bump-rpi.sh .github/ci.sh .github/sandbox.sh
      cargo fmt --all --check
      cargo deny check advisories bans
      seed
      cargo clippy --all-targets -- -D warnings ;;
    test)
      seed
      log=$TMPDIR/nextest.log
      local status=0
      cargo nextest run --workspace --profile ci 2>&1 | tee "$log" || status=$?
      # A test that passed only on its retry still shows, here and on the run's page. Both lists
      # come from nextest's closing summary.
      local flaky failed
      flaky=$(awk '/^ +Summary \[/ {s = 1} s && /^ +FLAKY /' "$log" | sed 's/^ */- /')
      failed=$(awk '/^ +Summary \[/ {s = 1} s && /^ +(TRY [0-9]+ )?(FAIL|TIMEOUT|SIG[A-Z]+|ABORT) \[/' "$log" |
        sed 's/^ */- /')
      [ -z "$flaky" ] || summary "$(printf '### Flaky (passed on retry)\n%s' "$flaky")"
      [ -z "$failed" ] || summary "$(printf '### Failed\n%s' "$failed")"
      return "$status" ;;
    *) die "unknown check '$what'" ;;
  esac
}

# Which checks GitHub refused to start on its runners, one per line in output `checks`. A check that
# started and failed is never run again: that is a real failure, and so is anything else that did
# not pass. Its step holds GH_TOKEN and runs only these API calls; `rescue`, the next step, runs
# the checks with no token in its environment at all.
rescue_plan() {
  local jobs refused bad id name list=
  jobs=$(api "repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/${GITHUB_RUN_ATTEMPT:-1}/jobs?per_page=100" \
    --jq '.jobs[] | select(.name | test("^(lint|test) \\("))')
  bad=$(jq -r "select(.conclusion != \"success\") | select(. as \$j | [\$j | $NOT_STARTED] | length == 0) | .name" <<<"$jobs")
  [ -z "$bad" ] || die "not rescued, these ran (or were cancelled) and did not pass: $(echo "$bad" | paste -sd, -)"
  refused=$(jq -r "$NOT_STARTED | \"\(.id)\t\(.name)\"" <<<"$jobs")
  [ -n "$refused" ] || die "nothing to rescue"
  while IFS=$'\t' read -r id name; do
    summary "rescue: GitHub did not start '$name' ($(api "repos/$GITHUB_REPOSITORY/check-runs/$id/annotations" \
      --jq '[.[].message] | join(" ")' 2>/dev/null || echo "no annotation")); running it here"
    list+="${name% (*}"$'\n'
  done <<<"$refused"
  { echo "checks<<EOF"; printf '%s' "$list"; echo "EOF"; } >> "${GITHUB_OUTPUT:-/dev/stdout}"
}

# Runs the checks rescue_plan chose, given in CHECKS. They build and run the branch's code, so
# this step starts with no GitHub token in its environment.
rescue() {
  local what
  [ -z "${GH_TOKEN:-}${GITHUB_TOKEN:-}" ] || die "rescue runs the branch's code: run it without a GitHub token"
  [ -n "${CHECKS:-}" ] || die "nothing to rescue"
  # fd 3, so nothing the checks run can read the list.
  while IFS= read -r -u 3 what; do
    [ -z "$what" ] || check "$what"
  done 3<<<"$CHECKS"
}

case "${1:-}" in
  plan) plan ;;
  skip) skip ;;
  run) [ $# -eq 2 ] || die "usage: ci.sh run CHECK"; check "$2" ;;
  rescue-plan) rescue_plan ;;
  rescue) rescue ;;
  *) die "usage: ci.sh plan | skip | run CHECK | rescue-plan | rescue" ;;
esac
