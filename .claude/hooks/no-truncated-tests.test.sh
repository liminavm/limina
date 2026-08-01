#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Cases for no-truncated-tests.sh. Run: bash .claude/hooks/no-truncated-tests.test.sh
#
# The cases live in this FILE rather than on a command line on purpose: a shell command
# containing them is itself a truncated test run, so the hook (correctly) blocks any attempt to
# test it inline.
set -uo pipefail
cd "$(dirname "$0")"

HOOK=./no-truncated-tests.sh
fails=0

# decide <expected: deny|allow> <description> <command>
decide() {
  local expect=$1 desc=$2 cmd=$3 got
  # A pass-through emits NOTHING at all (no JSON = no decision), and `jq` given empty input
  # prints nothing rather than applying its `//` default — so the empty case is normalized here.
  got=$(printf '{"tool_input":{"command":%s}}' "$(jq -Rn --arg c "$cmd" '$c')" \
        | bash "$HOOK" | jq -r '.hookSpecificOutput.permissionDecision // "allow"')
  got=${got:-allow}
  if [[ $got == "$expect" ]]; then
    printf '  ok   %-58s %s\n' "$desc" "$got"
  else
    printf '  FAIL %-58s want=%s got=%s\n' "$desc" "$expect" "$got"
    fails=$((fails + 1))
  fi
}

TAIL='| tail'
HEAD='| head'
SUITE='bash scripts/test-boot.sh debug'

echo "must deny — the suite truncated:"
decide deny "boot suite into tail"        "$SUITE 2>&1 $TAIL -45"
decide deny "xtask test into tail"        "cargo xtask test 2>&1 $TAIL -40"
decide deny "cargo test into head"        "cargo test -p limina 2>&1 $HEAD -20"
decide deny "no space before tail"        "cargo test |tail"
decide deny "|& shorthand"                "cargo test |& tail -5"
decide deny "nextest"                     "cargo nextest run 2>&1 $TAIL"
decide deny "+toolchain form"             "cargo +nightly test 2>&1 $TAIL -3"
decide deny "real run after a heredoc"    "$(printf 'cat <<%sE%s >/tmp/x\nhi\nE\ncargo test 2>&1 | tail -3' "'" "'")"

echo "must allow — not a truncated suite run:"
decide allow "suite redirected to a log"  "$SUITE > /tmp/x.log 2>&1; echo \$?"
decide allow "suite with no pipe"         "cargo test -p limina --bin limina window::fit"
decide allow "grepping a saved log"       "grep -E '^test result:' /tmp/x.log $TAIL -20"
decide allow "a build, not tests"         "cargo build 2>&1 $TAIL -5"
decide allow "git log"                    "git log --oneline $HEAD -5"
decide allow "clippy"                     "cargo clippy --workspace 2>&1 $HEAD -30"
decide allow "plain tail of a file"       "tail -40 /tmp/limina-suite.log"
# The regression that made heredoc stripping necessary: the commit message for this very hook
# quotes the pipeline it forbids, which is prose, not an invocation.
decide allow "heredoc body quotes it"     "$(printf 'git commit -F - <<%sEOF%s\nfix: stop doing\n\n    %s 2>&1 | tail -45\n\nEOF' "'" "'" "$SUITE")"
decide allow "unquoted heredoc body"      "$(printf 'cat > /tmp/n.md <<EOF\nrun: cargo test | tail -5\nEOF')"
# The runner and the truncating pipe must be in the SAME pipeline. These are the false denials
# from matching the two patterns independently across a whole command line: the suite's status
# and output go to a log, and the pipe belongs to some unrelated later command.
decide allow "redirected run, later pipe" "cargo xtask test > /tmp/x.log 2>&1; pgrep -f 'xtask test' $HEAD -3"
decide allow "run then tail the log"      "$SUITE > /tmp/x.log 2>&1; echo done $TAIL -1"
decide allow "run, then && an echo"       "cargo test > /tmp/x.log; echo ok $TAIL -1"
# ...but a runner anywhere on the left of that pipe still swallows its status.
decide deny  "runner upstream in a pipe"  "cargo test 2>&1 | grep -v warning $TAIL -20"
decide deny  "second statement truncated" "echo starting; cargo test 2>&1 $TAIL -5"

echo
if ((fails)); then
  echo "$fails case(s) FAILED"
  exit 1
fi
echo "all cases passed"
