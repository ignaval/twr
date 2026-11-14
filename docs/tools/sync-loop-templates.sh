#!/usr/bin/env bash
# Compares twr's copies of the loop skills' reviewer templates and findings schema with a
# checkout of the skills repo (`ignaval/skills`), and prints the differences
# (docs/design/loops.md, "Templates and the skills repo"). A dev tool: twr never reads the
# skills repo at run time.
#
#   docs/tools/sync-loop-templates.sh <path to a checkout of the skills repo>
#
# It compares, as unified diffs (the skill's text first, twr's second):
#   - implementation-loop/SKILL.md's "Plan-review template" block with
#     crates/twr-hub/src/loops/plan-review.md;
#   - its "Impl-review template" block with crates/twr-hub/src/loops/impl-review.md;
#   - review-loop/SKILL.md's "Review prompt template" block with
#     crates/twr-hub/src/loops/review.md;
#   - implementation-loop/findings.schema.json with `FINDINGS_SCHEMA` in
#     crates/twr-core/src/loops/contract.rs, both as normalized JSON.
# twr's templates fill in placeholders (`{{plan_path}}`, `{{diffs}}`, ...) where the skill
# shows `<absolute $SCRATCH path>/...`, and say the reviewer runs read-only, so those lines
# always differ; read the rest. Exit status: 0 when nothing differs, 1 when something does,
# 2 on a usage error or a file that is missing.
set -euo pipefail

if [ "$#" -ne 1 ] || [ ! -d "$1" ]; then
  echo "usage: $0 <path to a checkout of the skills repo>" >&2
  exit 2
fi
skills=$(cd "$1" && pwd)
twr=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

need() {
  if [ ! -f "$1" ]; then
    echo "missing: $1" >&2
    exit 2
  fi
}

# The first fenced block after the heading line that starts with "$2" in markdown file "$1".
block_after() {
  awk -v heading="$2" '
    index($0, heading) == 1 { found = 1; next }
    found && /^```/ { if (inside) exit; inside = 1; next }
    inside { print }
  ' "$1"
}

impl="$skills/implementation-loop/SKILL.md"
review="$skills/review-loop/SKILL.md"
schema="$skills/implementation-loop/findings.schema.json"
contract="$twr/crates/twr-core/src/loops/contract.rs"
for f in "$impl" "$review" "$schema" "$contract" \
  "$twr/crates/twr-hub/src/loops/plan-review.md" \
  "$twr/crates/twr-hub/src/loops/impl-review.md" \
  "$twr/crates/twr-hub/src/loops/review.md"; do
  need "$f"
done

status=0
compare() {
  # compare <label> <skill text file> <twr file>
  if [ ! -s "$2" ]; then
    echo "== $1: the skill's block was not found" >&2
    status=1
    return
  fi
  if diff -u --label "skills: $1" --label "twr: $1" "$2" "$3" >"$work/out"; then
    echo "== $1: the same"
  else
    echo "== $1: differs"
    cat "$work/out"
    status=1
  fi
}

block_after "$impl" "### Plan-review template" >"$work/plan-review.md"
compare "plan-review" "$work/plan-review.md" "$twr/crates/twr-hub/src/loops/plan-review.md"

block_after "$impl" "### Impl-review template" >"$work/impl-review.md"
compare "impl-review" "$work/impl-review.md" "$twr/crates/twr-hub/src/loops/impl-review.md"

block_after "$review" "## Review prompt template" >"$work/review.md"
compare "review" "$work/review.md" "$twr/crates/twr-hub/src/loops/review.md"

# The schema: the JSON between `FINDINGS_SCHEMA: &str = r#"` and `"#;`, normalized like the
# skill's file.
awk '
  /FINDINGS_SCHEMA: &str = r#"/ { inside = 1; sub(/.*r#"/, ""); if (length($0)) print; next }
  inside && /^"#;/ { exit }
  inside { print }
' "$contract" >"$work/twr-schema.json"
normalize() { python3 -I -c 'import json, sys; print(json.dumps(json.load(open(sys.argv[1])), indent=2, sort_keys=True))' "$1"; }
normalize "$schema" >"$work/a.json"
normalize "$work/twr-schema.json" >"$work/b.json"
compare "findings.schema.json" "$work/a.json" "$work/b.json"

exit "$status"
