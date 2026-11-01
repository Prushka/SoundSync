#!/usr/bin/env bash
# Prints release notes (Markdown) for a CI release: download links, then every commit since the
# previous release, newest first, with its message body (trailers removed).
# Usage: release-notes.sh <tag> <previous tag, or "" for the whole history> <repository URL> [head]
set -euo pipefail

TAG="$1"
PREV="$2"
REPO_URL="$3"
HEAD="${4:-HEAD}"
RANGE="$HEAD"
if [ -n "$PREV" ]; then
  RANGE="$PREV..$HEAD"
fi

cat <<EOF
## Downloads

- **Windows:** [SoundSync-windows-x64.zip]($REPO_URL/releases/download/$TAG/SoundSync-windows-x64.zip): extract it and run \`install-windows.ps1\` (updating works the same way).
- **macOS:** [SoundSync-macos-universal.zip]($REPO_URL/releases/download/$TAG/SoundSync-macos-universal.zip): unzip it and move \`SoundSync.app\` to Applications; the first time, right-click it and choose **Open**.

Every device in a group should run the same release: versions with different network protocols cannot talk to each other.

## Changes

EOF

count=0
for h in $(git rev-list --no-merges "$RANGE"); do
  count=$((count + 1))
  echo "- **$(git show -s --format=%s "$h")** ([$(git show -s --format=%h "$h")]($REPO_URL/commit/$h))"
  # Body under the bullet: trailers dropped, trailing blank lines trimmed, indented into the item.
  git show -s --format=%b "$h" | awk '
    tolower($0) !~ /^(co-authored-by|signed-off-by):/ { lines[n++] = $0 }
    END {
      while (n > 0 && lines[n - 1] ~ /^[[:space:]]*$/) n--
      for (i = 0; i < n; i++) print (lines[i] ~ /^[[:space:]]*$/ ? "" : "  " lines[i])
    }'
done
if [ "$count" -eq 0 ]; then
  echo "- No changes since $PREV."
fi

if [ -n "$PREV" ]; then
  printf '\n**All changes:** %s/compare/%s...%s\n' "$REPO_URL" "$PREV" "$TAG"
fi
