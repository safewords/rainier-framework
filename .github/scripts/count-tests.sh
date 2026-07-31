#!/usr/bin/env bash
# Count the tests that actually pass, and write the shields.io endpoint the
# README's badge reads.
#
# `--list` would be faster, but it counts `ignore`d doc examples as tests —
# and this repo has eighty of them, so the badge would claim eighty tests that
# never run. Summing what the run reports is the number that means something.
set -euo pipefail

output=$(cargo test --workspace --all-features 2>&1)
count=$(printf '%s' "$output" | grep -oE '[0-9]+ passed' | awk '{s+=$1} END {print s+0}')

if [ "$count" -eq 0 ]; then
    echo "counted no passing tests; refusing to write a badge that says so" >&2
    printf '%s\n' "$output" | tail -40 >&2
    exit 1
fi

mkdir -p .github/badges
cat > .github/badges/tests.json <<JSON
{
  "schemaVersion": 1,
  "label": "tests",
  "message": "${count} passing",
  "color": "brightgreen"
}
JSON

echo "$count passing"
