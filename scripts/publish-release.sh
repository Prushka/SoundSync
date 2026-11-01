#!/usr/bin/env bash
# Publishes this CI build as a GitHub release "SoundSync <version> (build <n>)", tagged
# v<version>-build.<n>, with the Windows and macOS zips and notes listing the commits since the
# previous release. Re-running the same build updates its release.
# CI only: needs GH_TOKEN (contents: write), GITHUB_REPOSITORY, GITHUB_SERVER_URL, GITHUB_SHA,
# GITHUB_RUN_NUMBER, the full git history, and both zips in dist/.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
TAG="v$VERSION-build.$GITHUB_RUN_NUMBER"
TITLE="SoundSync $VERSION (build $GITHUB_RUN_NUMBER)"
REPO_URL="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY"
ASSETS=(dist/SoundSync-windows-x64.zip dist/SoundSync-macos-universal.zip)
ls -l "${ASSETS[@]}"

# The newest published release other than this one; its commit must be in our history.
PREV="$(gh release list --exclude-drafts --limit 20 --json tagName --jq "[.[].tagName | select(. != \"$TAG\")][0] // \"\"")"
if [ -n "$PREV" ] && ! git rev-parse -q --verify "refs/tags/$PREV" >/dev/null; then
  echo "previous release $PREV is not in this history; listing every commit"
  PREV=""
fi

NOTES="$(mktemp)"
bash scripts/release-notes.sh "$TAG" "$PREV" "$REPO_URL" "$GITHUB_SHA" > "$NOTES"

if gh release view "$TAG" >/dev/null 2>&1; then
  gh release upload "$TAG" "${ASSETS[@]}" --clobber
  gh release edit "$TAG" --title "$TITLE" --notes-file "$NOTES"
else
  gh release create "$TAG" "${ASSETS[@]}" --target "$GITHUB_SHA" --title "$TITLE" --notes-file "$NOTES" --latest
fi
echo "Published $TITLE: $REPO_URL/releases/tag/$TAG"
