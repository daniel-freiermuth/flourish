#!/usr/bin/env bash
# Every git command the daemon runs is spelled in src/clones.rs: one that
# may change a clone through a `CloneHold` (held under the clone's
# permit), one that changes none as an `Unheld` variant. A `"git"`
# literal anywhere else in src/ would be a git command neither type
# vouches for, so a change to a clone could run without its permit.
#
# `just lint` and the CI "Git encapsulation" step both run this script.
# The pipeline-status handling is check-sql.sh's; see there for why a
# bare `if pipeline` cannot tell "no match" from "grep failed".
set -uo pipefail

cd "$(dirname "$0")" || exit 1

comment_line='^[^:]*:[0-9]+:[[:space:]]*//'

echo "Checking git encapsulation..."
grep -rn '"git"' src/ | grep -v '^src/clones\.rs:' | grep -Ev "$comment_line"
status=("${PIPESTATUS[@]}")
for st in "${status[@]}"; do
  if ((st > 1)); then
    echo "✗ git encapsulation: grep exited $st, so this check did not run" >&2
    exit 2
  fi
done
if ((status[2] == 0)); then
  echo "✗ a git command spelled outside src/clones.rs"
  exit 1
fi
echo "✓ git commands only in src/clones.rs"
