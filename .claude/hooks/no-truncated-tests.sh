#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# PreToolUse/Bash guard: refuse to run the test suite piped into `head` or `tail`.
#
# Why this exists. A pipeline's exit status is the *last* command's, so
#
#     bash scripts/test-boot.sh debug 2>&1 | tail -45
#
# reports tail's status (0, always) no matter how the suite ended, and shows only the final
# fragment of the log. Both halves of that are traps: the exit code looks green when it isn't,
# and the visible tail can be all-passing while an earlier test binary failed. This has produced
# a false "suite green" claim more than once.
#
# The fix is to keep the suite's own exit status and read the whole log:
#
#     bash scripts/test-boot.sh debug > /tmp/limina-suite.log 2>&1; echo "EXIT=$?"
#     grep -E "^test result:|FAILED|panicked" /tmp/limina-suite.log
#
# Reads the hook JSON on stdin; emits a deny decision (and exits 0 — the decision is carried by
# the JSON, not the exit code). Anything that is not a truncated test run passes through silently.
set -uo pipefail

command=$(jq -r '.tool_input.command // empty')
[[ -z $command ]] && exit 0

# Heredoc BODIES are data, not commands — strip them before matching. Without this the hook
# blocks its own documentation: a `git commit -F - <<'EOF'` whose message quotes the very
# pipeline being warned about looks exactly like running it. (That is not hypothetical; it
# happened on the commit that introduced this hook.) The opener line is kept, since the real
# command lives there.
scan=$(printf '%s\n' "$command" | awk '
  inhd { if ($0 ~ "^[[:space:]]*" delim "[[:space:]]*$") inhd = 0; next }
  {
    print
    if (match($0, /<<-?[[:space:]]*("[^"]+"|'"'"'[^'"'"']+'"'"'|[A-Za-z_][A-Za-z0-9_]*)/)) {
      delim = substr($0, RSTART, RLENGTH)
      sub(/^<<-?[[:space:]]*/, "", delim)
      gsub(/["'"'"']/, "", delim)
      inhd = 1
    }
  }')

# Does this command run the test suite? Covers the cargo runners and the boot-suite script.
runs_tests='(^|[;&|[:space:]])(cargo([[:space:]]+\+[^[:space:]]+)?[[:space:]]+(test|nextest)|cargo([[:space:]]+\+[^[:space:]]+)?[[:space:]]+xtask[[:space:]]+test|(bash[[:space:]]+)?[^[:space:]]*scripts/test-boot\.sh)'
# ...and is it piped into a truncating pager? `|&` is bash's shorthand for `2>&1 |`.
truncates='\|&?[[:space:]]*(head|tail)([[:space:]]|$)'

if [[ $scan =~ $runs_tests ]] && [[ $scan =~ $truncates ]]; then
  jq -n --arg cmd "$command" '{
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      permissionDecision: "deny",
      permissionDecisionReason: (
        "Blocked: this pipes the test suite into head/tail.\n\n" +
        "A pipeline exits with the LAST command'"'"'s status, so the suite'"'"'s own pass/fail is " +
        "discarded and you only see a fragment of the log — a green-looking tail can sit above " +
        "an earlier failed test binary.\n\n" +
        "Run it redirected instead, keeping the exit code, then read the whole log:\n" +
        "  <suite command> > /tmp/limina-suite.log 2>&1; echo \"EXIT=$?\"\n" +
        "  grep -E \"^test result:|FAILED|panicked\" /tmp/limina-suite.log\n\n" +
        "Blocked command was: " + $cmd
      )
    }
  }'
  exit 0
fi

exit 0
