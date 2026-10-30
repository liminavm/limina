#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Marcelo Jorge Vieira

# Refuse a range of commits unless each one is signed off by its author.
#
# CONTRIBUTING.md asks every commit for a Signed-off-by trailer, the contributor's certification
# of the Developer Certificate of Origin, and CI runs this over every pull request. Only a trailer
# in the message's last paragraph counts, and it must carry the commit author's email, since the
# sign-off is the author's own statement. Merge commits are not checked: GitHub's "Update branch"
# writes them without one. A bot's sign-off is accepted as it comes, because Dependabot authors
# as <id>+dependabot[bot]@users.noreply.github.com and signs off as support@github.com.
#
# Usage: scripts/check-signoff.sh <base> [<head>]   checks <base>..<head>; <head> is HEAD by default
set -euo pipefail

base=${1:?usage: scripts/check-signoff.sh <base> [<head>]}
head=${2:-HEAD}
commits=$(git rev-list --no-merges --reverse "$base..$head")

lower() { tr '[:upper:]' '[:lower:]'; }

unsigned=0
for commit in $commits; do
  author=$(git log -1 --format=%ae "$commit" | lower)
  signers=$(git log -1 --format='%(trailers:key=Signed-off-by,valueonly)' "$commit" \
    | sed -n 's/.*<\(.*\)>.*/\1/p' | lower)
  case $author in
    *'[bot]@users.noreply.github.com') [ -n "$signers" ] && continue ;;
    *) printf '%s\n' "$signers" | grep -qxF "$author" && continue ;;
  esac
  echo "$(git log -1 --format='%h %s' "$commit"): no Signed-off-by from its author <$author>" >&2
  unsigned=$((unsigned + 1))
done

if [ "$unsigned" -gt 0 ]; then
  echo "$unsigned commit(s) not signed off; sign the branch off with:" >&2
  echo "  git rebase --signoff $(git merge-base "$base" "$head") && git push --force-with-lease" >&2
  exit 1
fi
echo "check-signoff: every commit in $base..$head is signed off by its author"
