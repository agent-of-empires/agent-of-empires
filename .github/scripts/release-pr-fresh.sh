#!/usr/bin/env bash
set -euo pipefail

# release-pr-fresh.sh: in the merge queue, fail a release-plz PR when commits
# landed on main after its last refresh, since they would ship without
# CHANGELOG.md entries. Other PRs pass. ci.yml's `Release PR Up To Date` job runs
# it with HEAD_REF and GROUP_HEAD from the merge_group event, plus GH_TOKEN.
#
#   --self-test   Checks the decision logic against a throwaway repo.

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

# pr_number <ref>: refs/heads/gh-readonly-queue/main/pr-<N>-<sha> to <N>.
pr_number() {
  local pr="${1##*/pr-}"
  pr="${pr%%-*}"
  [[ $pr =~ ^[0-9]+$ ]] || { echo "::error::Cannot parse PR from $1" >&2; return 1; }
  echo "$pr"
}

# gate <branch> <same repo> <pr head> <group head>
# Same rule as release-plz.yml's Tag release job.
gate() {
  if [[ $2 != true || $1 != release-plz-* ]]; then
    echo "Not a release PR."
    return 0
  fi
  if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
    git fetch -q --no-tags --unshallow origin "$3" "$4" || return 2
  fi
  local missed count
  # Explicit: callers in `||` context run this with errexit off.
  missed="$(git log --oneline "$3..$4~1")" || return 2
  if [ -z "$missed" ]; then
    echo "Release PR is up to date."
    return 0
  fi
  count="$(echo "$missed" | wc -l | tr -d ' ')"
  # Indented, not fenced: a subject with backticks cannot break the block.
  { echo '### Would ship without CHANGELOG.md entries'; echo; echo "$missed" | sed 's/^/    /'; } >>"${GITHUB_STEP_SUMMARY:-/dev/null}"
  echo "::error title=Release PR is stale::$count commit(s) landed after the release PR's last refresh and would ship without CHANGELOG.md entries. Dispatch release-plz.yml to refresh the PR, then queue it again. See the job summary."
  return 1
}

run() {
  local pr branch same_repo head
  pr="$(pr_number "$HEAD_REF")"
  IFS=$'\t' read -r branch release head < <(
    gh pr view "$pr" -R "$GITHUB_REPOSITORY" --json headRefName,headRefOid,isCrossRepository \
      --jq '[.headRefName, (.isCrossRepository | not | tostring), .headRefOid] | @tsv'
  )
  gate "$branch" "$same_repo" "$head" "$GROUP_HEAD"
}

self_test() {
  local dir base main_tip tree rel fresh stale out
  export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
  dir="$(mktemp -d)"
  trap 'rm -rf "$dir"' EXIT
  cd "$dir"
  git init -q
  git commit -q --allow-empty -m base
  base="$(git rev-parse HEAD)"
  git commit -q --allow-empty -m 'fix: one'
  git commit -q --allow-empty -m 'fix: ```two```'
  main_tip="$(git rev-parse HEAD)"
  tree="$(git rev-parse 'HEAD^{tree}')"
  # A release PR refreshed at base, and queue squashes on top of base / main.
  rel="$(git commit-tree "$tree" -p "$base" -m 'chore: release')"
  fresh="$(git commit-tree "$tree" -p "$base" -m squash)"
  stale="$(git commit-tree "$tree" -p "$main_tip" -m squash)"
  export GITHUB_STEP_SUMMARY="$dir/summary.md"

  [ "$(pr_number refs/heads/gh-readonly-queue/main/pr-5570-abc123)" = 5570 ] ||
    fail "did not parse the PR number"
  pr_number refs/heads/weird 2>/dev/null && fail "accepted a ref without a PR number"
  ok "parses merge-queue refs and rejects others"

  gate release-plz-x true "$rel" "$fresh" >/dev/null || fail "failed a refreshed release PR"
  gate release-plz-x true "$rel" deadbeef 2>/dev/null && fail "passed when git log failed"
  ok "passes a refreshed release PR"

  out="$(gate release-plz-x true "$rel" "$stale")" && fail "passed a stale release PR"
  [[ $out == *"2 commit(s)"* ]] || fail "wrong count: $out"
  grep -q '^    [0-9a-f]* fix: ```two```$' summary.md || fail "summary lines not indented"
  ok "fails a stale release PR and lists its missed commits"

  gate feature/x true "$rel" "$stale" >/dev/null || fail "gated a non-release branch"
  gate release-plz-x false "$rel" "$stale" >/dev/null || fail "gated a fork PR"
  ok "skips non-release PRs"
  trap - EXIT
  rm -rf "$dir"
  echo "self-test passed."
}

case "${1:-}" in
  --self-test) self_test ;;
  "") run ;;
  *)
    echo "usage: $0 [--self-test]" >&2
    exit 2
    ;;
esac
