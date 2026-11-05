#!/usr/bin/env bash
# `preturn-check --only "test <args>"`: one `cargo test`, and whether it really tested something.
set -euo pipefail

# The words reach cargo as written: split on spaces, with no shell expansion.
read -r -a words <<< "$AGIT_PRETURN_ONLY"
cargo test --locked "${words[@]:1}" 2>&1 | tee preturn-test.log
# The checks below read the log without colour codes: cargo's own lines carry them.
sed 's/\x1b\[[0-9;]*m//g' preturn-test.log > preturn-test.plain
# cargo's own status decides a failure. A filter that matches nothing still exits 0, so a pass
# also needs a test to have run: a libtest summary with at least one pass, or a target with its own
# harness (`harness = false`), which prints its own lines and no summary.
grep -E 'test result: ok\. [1-9]' preturn-test.plain >/dev/null && exit 0
ran=()
while read -r name; do
  grep -F "Running tests/$name.rs " preturn-test.plain >/dev/null && ran+=("$name")
done < <(awk '
  function flush() { if (block && harness && name != "") print name; block = 0; harness = 0; name = "" }
  /^\[\[test\]\]/ { flush(); block = 1; next }
  /^\[/ { flush(); next }
  block && /^name *=/ { name = $0; sub(/^name *= *"/, "", name); sub(/".*/, "", name) }
  block && /^harness *= *false/ { harness = 1 }
  END { flush() }' Cargo.toml)
if [ "${#ran[@]}" -gt 0 ]; then
  echo "Ran with their own harness: ${ran[*]}. They print no summary, so cargo's exit status is their result."
  exit 0
fi
echo "No test ran; check the filter: $AGIT_PRETURN_ONLY" >&2
exit 1
