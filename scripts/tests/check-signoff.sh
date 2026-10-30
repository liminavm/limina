#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Marcelo Jorge Vieira
#
# Test scripts/check-signoff.sh, the sign-off check every pull request runs.
#
# A sign-off counts only as a trailer carrying the commit author's email, so: an unsigned commit,
# one signed off by someone else, or one whose sign-off sits above the last paragraph fails the
# range; merge commits are not checked; a bot passes on its own sign-off; and the rebase the
# check prints on failure is enough to make the range pass.
#
# Run it directly: scripts/tests/check-signoff.sh
set -uo pipefail
# Run from a git hook, GIT_DIR and GIT_INDEX_FILE point at the repository being committed to;
# clear them so the scratch repository below is the one every git command sees.
# shellcheck disable=SC2046
unset $(git rev-parse --local-env-vars 2>/dev/null)
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_COMMITTER_NAME="A U Thor" GIT_COMMITTER_EMAIL=a@example.com

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CHECK="$REPO/scripts/check-signoff.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
BOT='49699333+dependabot[bot]@users.noreply.github.com'
SOB=$'\n\nSigned-off-by: A U Thor <a@example.com>'

fails=0
check() { # <name> <want> <got>
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: wanted [$2], got [$3]"; fails=$((fails + 1)); fi
}
commit() { # <author email> <message>
  GIT_AUTHOR_NAME="A U Thor" GIT_AUTHOR_EMAIL=$1 git commit -q --allow-empty -m "$2"
}
verdict() { # <base> <head>
  if "$CHECK" "$1" "$2" > /dev/null 2>&1; then echo pass; else echo fail; fi
}
one() { # <name> <want> <author email> <message>
  git checkout -q --detach "$base"
  commit "$3" "$4"
  check "$1" "$2" "$(verdict "$base" HEAD)"
}

git init -q "$TMP/repo"
cd "$TMP/repo" || exit 1
commit a@example.com "base"
base=$(git rev-parse HEAD)

one "a commit signed off by its author passes" pass a@example.com "subject$SOB"
one "a commit with no sign-off fails" fail a@example.com $'subject\n\nbody'
one "a sign-off by someone else fails" fail a@example.com $'subject\n\nSigned-off-by: B <b@example.com>'
one "the author's email matches in any case" pass A@Example.com $'subject\n\nSigned-off-by: A <a@EXAMPLE.com>'
one "a sign-off above the last paragraph does not count" fail a@example.com "subject$SOB"$'\n\nprose'
one "a bot passes on its own sign-off" pass "$BOT" $'subject\n\nSigned-off-by: dependabot[bot] <support@github.com>'
one "a bot without a sign-off fails" fail "$BOT" "subject"

check "an empty range passes" pass "$(verdict "$base" "$base")"
check "an unknown base fails" fail "$(verdict 0123456789abcdef0123456789abcdef01234567 HEAD)"

git checkout -q --detach "$base"
commit a@example.com "first$SOB"
git checkout -q -b side
commit a@example.com "side$SOB"
git checkout -q --detach HEAD~1
git merge -q --no-ff --no-edit side
check "an unsigned merge of signed commits passes" pass "$(verdict "$base" HEAD)"

git checkout -q --detach "$base"
commit a@example.com "first$SOB"
commit a@example.com "second"
commit a@example.com "third$SOB"
check "one unsigned commit among signed ones fails" fail "$(verdict "$base" HEAD)"
hint=$("$CHECK" "$base" HEAD 2>&1 | grep -o 'git rebase --signoff [0-9a-f]*')
$hint > /dev/null 2>&1
check "the printed rebase signs the range off" pass "$(verdict "$base" HEAD)"

[ "$fails" -eq 0 ] && echo "check-signoff: all cases pass" || { echo "check-signoff: $fails case(s) FAILED"; exit 1; }
