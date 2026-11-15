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
# Where zephyr-local's runners find main's last build, read only (deploy/runner/start.sh warm).
CACHE=/cache/current

die() { echo "ci: $*" >&2; exit 1; }
summary() { echo "$*" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"; echo "$*"; }
api() { gh api -H 'X-GitHub-Api-Version: 2022-11-28' "$@"; }

# Jobs GitHub never started: failed with no runner and no step (the spending limit or the quota,
# whose annotation says "The job was not started because ..."). Reads job objects on stdin.
NOT_STARTED='select(.conclusion == "failure" and (.steps | length) == 0 and (.runner_name // "") == "")'

# Runs that can have tried GitHub's runner while ZEPHYR_CI is `both`: the nightly and manual runs
# (ci.yml's nightly test, rpi-bump.yml, image.yml's manual builds) and a release tag's image build.
# Reads run objects on stdin.
MAY_BE_HOSTED='select(.event == "schedule" or .event == "workflow_dispatch" or (.event == "push" and (.head_branch // "" | test("^v[0-9]"))))'

# The id of the run whose hosted job GitHub would not start, when that is this month's last word on
# GitHub's runners: the newest run of this UTC month with a hosted job that GitHub either started
# or refused. GitHub's included minutes and spending limit are monthly, so one refusal keeps every
# hosted job here for the rest of the month instead of being refused again each night, and the
# first run of the next month tries hosted again. A hosted job that started since (a manual run with
# ZEPHYR_TRY_HOSTED, after a raised limit) ends it sooner.
recent_refusal() {
  local since ids id verdict
  since=$(date -u +%Y-%m-01T00:00:00Z)
  ids=$(api "repos/$GITHUB_REPOSITORY/actions/runs?created=%3E%3D$since&per_page=100" \
    --jq "[.workflow_runs[] | select(.created_at >= \"$since\") | $MAY_BE_HOSTED | .id] | .[:20][]") || return
  # A re-run's earlier attempts count too (jobs?filter=all lists them).
  [ "${GITHUB_RUN_ATTEMPT:-1}" = 1 ] || ids="$GITHUB_RUN_ID $ids"
  for id in $ids; do
    verdict=$(api "repos/$GITHUB_REPOSITORY/actions/runs/$id/jobs?filter=all&per_page=100" \
      --jq "[.jobs[] | select(any(.labels[]; . == \"$LOCAL\") | not)] |
        if any(.[]; [$NOT_STARTED] | length > 0) then \"refused\"
        elif any(.[]; (.runner_name // \"\") != \"\") then \"started\"
        else \"\" end") || return
    case $verdict in
      refused) echo "$id"; return 0 ;;
      started) return 0 ;;
    esac
  done
}

# Two checks, each compiling once: lint and test. The test job goes to GitHub's runner only on
# the nightly run (its minutes are scarce, and a refused job costs a rescue), so its clean 2-CPU
# environment, and the path ZEPHYR_CI=hosted falls back on, are still tested daily. `hosted` is
# where a job that wants GitHub's runner goes (image.yml and rpi-bump.yml read it), unless GitHub
# refused one this month. ZEPHYR_TRY_HOSTED=true (ci.yml's manual input `hosted`) tries it whatever
# was refused, with the test job, so a raised limit is picked up by one manual run.
plan() {
  local mode=${ZEPHYR_CI:-both} local=$LOCAL hosted=$HOSTED test=$LOCAL why run=
  case $mode in
    both)
      # An API error must not stop CI: it just means trying hosted.
      if [ "${ZEPHYR_TRY_HOSTED:-}" != true ] && ! run=$(recent_refusal); then
        echo "ci: could not read recent runs; trying hosted" >&2
        run=
      fi
      if [ "${ZEPHYR_TRY_HOSTED:-}" = true ]; then
        test=$HOSTED
        why="asked to try $HOSTED: test there, lint on $LOCAL"
      elif [ -n "$run" ]; then
        hosted=$LOCAL
        why="GitHub refused a hosted job this month (run $run), and started none since: everything on $LOCAL"
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

# The files this run changes, every path a rename touches on both its sides; fails when it cannot
# tell. A PR's checkout is its merge into the base, so HEAD^1 is the base. A push compares the
# commit before it (BEFORE, github.event.before) with this one through the API, so every commit it
# brought counts, and fails for a new branch (all zeros), a force push (BEFORE not an ancestor) or a
# comparison GitHub cuts at 300 files.
changed() {
  local out
  case ${GITHUB_EVENT_NAME:-} in
    pull_request) git diff --name-only --no-renames HEAD^1 HEAD ;;
    push)
      [[ ${BEFORE:-} =~ ^[0-9a-f]{40}$ ]] && [[ $BEFORE =~ [1-9a-f] ]] || return 1
      out=$(api "repos/$GITHUB_REPOSITORY/compare/$BEFORE...$GITHUB_SHA" \
        --jq '[.status, (.files | length | tostring)] + [.files[] | .filename, (.previous_filename // empty)] | .[]') ||
        return 1
      [ "$(sed -n 1p <<<"$out")" = ahead ] && [ "$(sed -n 2p <<<"$out")" -lt 300 ] || return 1
      sed 1,2d <<<"$out" ;;
    *) return 1 ;;
  esac
}

# Whether ci.yml itself passed on commit $1 for a pull request: a successful ci.yml run for that
# head, and a ci-ok check-run made by GitHub Actions in that run's own check suite. A check-run of
# the same name from any other workflow or app counts for nothing.
passed() {
  local suite
  for suite in $(api "repos/$GITHUB_REPOSITORY/actions/workflows/ci.yml/runs?head_sha=$1&event=pull_request&status=success&per_page=20" \
    --jq ".workflow_runs[] | select(.head_sha == \"$1\" and .event == \"pull_request\" and .conclusion == \"success\" and .path == \".github/workflows/ci.yml\") | .check_suite_id"); do
    [[ $suite =~ ^[0-9]+$ ]] || continue
    [ "$(api "repos/$GITHUB_REPOSITORY/commits/$1/check-runs?check_name=ci-ok&per_page=100" \
      --jq "[.check_runs[] | select(.name == \"ci-ok\" and .conclusion == \"success\" and .app.slug == \"github-actions\" and .check_suite.id == $suite)] | length")" -gt 0 ] &&
      return 0
  done
  return 1
}

# Prints why this run needs no checks, or nothing. Only a pull request or a push: the nightly and
# manual runs always test everything.
why_skip() {
  local files f tree pr head
  case ${GITHUB_EVENT_NAME:-} in pull_request | push) ;; *) return 0 ;; esac
  if files=$(changed) && [ -n "$files" ]; then
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
  # main's merge of a PR rebased onto it is the PR's head tree, which its ci.yml run already tested.
  [ "${GITHUB_EVENT_NAME:-}" = push ] && [ "${GITHUB_REF:-}" = refs/heads/main ] || return 0
  tree=$(git rev-parse 'HEAD^{tree}')
  for pr in $(api "repos/$GITHUB_REPOSITORY/commits/$GITHUB_SHA/pulls" --jq '.[].number'); do
    [[ $pr =~ ^[0-9]+$ ]] || continue
    head=$(api "repos/$GITHUB_REPOSITORY/pulls/$pr" --jq '.head.sha') || continue
    [[ $head =~ ^[0-9a-f]{40}$ ]] || continue
    [ "$(api "repos/$GITHUB_REPOSITORY/git/commits/$head" --jq '.tree.sha')" = "$tree" ] || continue
    if passed "$head"; then
      echo "PR #$pr's head $head passed ci.yml's ci-ok on this same tree"
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
