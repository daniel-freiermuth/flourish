#!/usr/bin/env bash
# The SQL encapsulation guarantees, in one place: `just lint` and the CI
# "SQL encapsulation" step both run this script. They used to spell the same
# three greps out twice in different shell dialects, which only ever agreed
# by hand.
#
# Every check is a grep pipeline, and grep's exit codes carry three meanings:
# 0 matched, 1 nothing matched, anything above that grep itself failed
# (unreadable path, invalid regex). A bare `if pipeline; then` collapses the
# last two into "clean", so a check that never ran reads as a check that
# passed. `pipefail` does not rescue that: it yields the *last* nonzero
# status, so a downstream no-match (1) overwrites an upstream failure (2).
# Only `${PIPESTATUS[@]}`, captured on the line after the pipeline, can tell
# the three apart, which is what `matched` below does. pipefail stays on for
# the benefit of any future pipeline whose failure is an ordinary failure.
set -uo pipefail

cd "$(dirname "$0")" || exit 1

# The comment filter must be anchored: an unanchored `//` drops any line
# that merely *contains* a comment marker, so a real violation with a
# trailing comment (or a URL in a string literal) escapes the gate. Match
# `//` only where the code starts, i.e. right after grep's `path:line:`
# prefix, allowing indentation.
comment_line='^[^:]*:[0-9]+:[[:space:]]*//'

# Classify one pipeline from its per-stage statuses. Returns success when the
# pipeline matched, i.e. when there is a violation to report; aborts the whole
# script if any stage failed outright, because a failed stage means the
# guarantee was not tested at all.
matched() {
  local name=$1 st last=1
  shift
  for st in "$@"; do
    if ((st > 1)); then
      echo "✗ $name: grep exited $st, so this check did not run" >&2
      exit 2
    fi
    last=$st
  done
  ((last == 0))
}

echo "Checking pool() encapsulation..."
grep -rn '\.pool()' src/ | grep -v 'store\.rs' | grep -Ev "$comment_line"
status=("${PIPESTATUS[@]}")
if matched "pool() encapsulation" "${status[@]}"; then
  echo "✗ pool() leaked outside store.rs"
  exit 1
fi
echo "✓ pool() only in store.rs"

# `pr_state`'s column groups each have their own writers, and its derived
# attention columns are settled in the same transaction as every write to
# their inputs (src/attention.rs). src/store/pr_state.rs is where that is
# kept: SQL writing the table anywhere else could bypass the settle.
#
# Each file is matched whole (`-z`), not line by line: SQL in a `query!`
# literal is routinely split with a `\` continuation, and a write spelled
# `"UPDATE \` on one line and `pr_state SET ..."` on the next is just as
# much a write. The price is that comments count too, so prose elsewhere
# names the table without the SQL verb in front of it.
#
# SQL keywords and identifiers are case-insensitive, and an identifier may
# be quoted or carry a schema: `update "pr_state"`, `INSERT OR REPLACE INTO
# main.pr_state` and `UPDATE OR IGNORE [pr_state]` are all writes.
pr_state_write='\b(?:INTO|UPDATE(?:\s+OR\s+\w+)?|DELETE\s+FROM)(?:\s|\\)+(?:["`\[]?\w+["`\]]?\.)?["`\[]?pr_state\b'
echo "Checking pr_state writes..."
grep -rlziP "$pr_state_write" src/ | tr '\0' '\n' | grep -vx 'src/store/pr_state\.rs'
status=("${PIPESTATUS[@]}")
if matched "pr_state writes" "${status[@]}"; then
  echo "✗ pr_state written outside src/store/pr_state.rs"
  exit 1
fi
echo "✓ pr_state written only in src/store/pr_state.rs"

# A trigger writes behind every statement that fires it, so one writing
# `pr_state` would be a writer outside src/store/pr_state.rs that no settle
# follows. A migration's own backfill runs once and is not covered: only
# trigger bodies are checked.
#
# A body ends at the `END` that closes its `BEGIN`, not at the first `END;`:
# a `CASE ... END` closes statements inside a body too, so the body is
# scanned with `CASE`/`END` nesting counted. Comments, string literals and
# quoted identifiers are blanked (to spaces of the same length, so offsets
# hold) before the scan, so an `END` inside one of them ends nothing; only
# comments and literals are blanked before the write check, which has to
# see a quoted `"pr_state"`. `TEMP`/`TEMPORARY` triggers count as triggers.
echo "Checking pr_state writes in triggers..."
PR_STATE_WRITE=$pr_state_write perl -0777 -ne '
  my ($code, $scan) = ($_, $_);
  while (/(--[^\n]*|\/\*.*?(?:\*\/|\z)|\x27(?:[^\x27]|\x27\x27)*\x27?)|("(?:[^"]|"")*"?|`[^`]*`?|\[[^\]]*\]?)/sg) {
    my ($at, $len) = ($-[0], $+[0] - $-[0]);
    substr($scan, $at, $len) = " " x $len;
    substr($code, $at, $len) = " " x $len if defined $1;
  }
  while ($scan =~ /\bCREATE\s+(?:TEMP(?:ORARY)?\s+)?TRIGGER\b/gi) {
    my $start = $-[0];
    next unless $scan =~ /\G.*?\bBEGIN\b/sgci;
    my ($depth, $end) = (0, undef);
    while ($scan =~ /\b(CASE|END)\b/gi) {
      if (uc $1 eq "CASE") { $depth++ }
      elsif ($depth) { $depth-- }
      else { $end = $+[0]; last }
    }
    $end //= length $scan;
    pos($scan) = $end;
    my $trigger = substr($code, $start, $end - $start);
    if ($trigger =~ /$ENV{PR_STATE_WRITE}/i) {
      my ($name) = $trigger =~ /\bTRIGGER\s+(?:IF\s+NOT\s+EXISTS\s+)?(\S+)/i;
      print "$ARGV: $name\n";
      $found = 1;
    }
  }
  END { exit($found ? 0 : 1) }
' migrations/*.sql
status=("${PIPESTATUS[@]}")
if matched "pr_state writes in triggers" "${status[@]}"; then
  echo "✗ a migration trigger writes pr_state"
  exit 1
fi
echo "✓ no trigger writes pr_state"

echo "Checking compile-time checked queries..."
grep -rn 'sqlx::query(' src/ | grep -v 'query!\|query_as!\|query_scalar!\|capacity\.rs' | grep -Ev "$comment_line"
status=("${PIPESTATUS[@]}")
if matched "compile-time checked queries" "${status[@]}"; then
  echo "✗ unchecked sqlx::query found"
  exit 1
fi
echo "✓ all queries compile-time checked"

echo "Checking no QueryBuilder..."
grep -rn 'QueryBuilder' src/ | grep -Ev "$comment_line"
status=("${PIPESTATUS[@]}")
if matched "no QueryBuilder" "${status[@]}"; then
  echo "✗ QueryBuilder found"
  exit 1
fi
echo "✓ no QueryBuilder"

echo "All SQL guarantees hold."
