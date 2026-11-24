#!/usr/bin/env bash
# Usage: publish.sh <dir-with-clips> <subdir-name>
# Adds every file in <dir-with-clips> (not subdirectories) under <subdir-name>/ on the
# e2e-recordings branch of origin, using plumbing so no worktree or index changes, then
# prints the URLs to use in PR comments.
set -euo pipefail
src=$1 sub=$2 branch=e2e-recordings
# Name the repository the push goes to, not whatever gh defaults to.
repo=$(gh repo view "$(git remote get-url origin)" --json nameWithOwner --jq .nameWithOwner)

entries=""
for f in "$src"/*; do
  [ -f "$f" ] || continue
  entries+="100644 blob $(git hash-object -w "$f")	$(basename "$f")"$'\n'
done
subtree=$(printf '%s' "$entries" | git mktree)

if git fetch -q origin "$branch" 2>/dev/null; then
  parent=$(git rev-parse FETCH_HEAD)
  root=$( (git ls-tree "$parent" | awk -F '\t' -v s="$sub" '$2 != s'; printf '040000 tree %s\t%s\n' "$subtree" "$sub") | git mktree)
  commit=$(git commit-tree "$root" -p "$parent" -m "docs: demo recordings for $sub")
else
  readme=$(printf 'Demo recordings referenced from PR comments.\n' | git hash-object -w --stdin)
  root=$(printf '100644 blob %s\tREADME.md\n040000 tree %s\t%s\n' "$readme" "$subtree" "$sub" | git mktree)
  commit=$(git commit-tree "$root" -m "docs: demo recordings for $sub")
fi
git push -q origin "$commit:refs/heads/$branch"

for f in "$src"/*; do
  [ -f "$f" ] || continue
  n=$(basename "$f")
  case $n in
    *.gif) echo "gif  https://raw.githubusercontent.com/$repo/$branch/$sub/$n" ;;
    *.mp4) echo "mp4  https://github.com/$repo/blob/$branch/$sub/$n" ;;
  esac
done
