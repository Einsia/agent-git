#!/usr/bin/env bash
# End-to-end acceptance for AgitHub's permission model, against a REAL running server and REAL git.
#
# Every check asserts an exact outcome and fails loudly. This exists because inline shell pipelines
# silently produced empty tokens during development and nearly reported "push accepted" on a push that
# had in fact failed — a green that would have been a lie.
#
#   ./demo/hub-e2e.sh          (needs: cargo build --release --bin agit-hub, git, curl)

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HUB="$ROOT/target/release/agit-hub"
PORT="${PORT:-8194}"
G=$'\033[32m'; R=$'\033[31m'; B=$'\033[1m'; N=$'\033[0m'
PASS=0; FAIL=0
ok(){ echo "  ${G}✓${N} $*"; PASS=$((PASS+1)); }
bad(){ echo "  ${R}✗${N} $*"; FAIL=$((FAIL+1)); }
is(){ # is <label> <expected> <actual>
  [[ "$2" == "$3" ]] && ok "$1 ($3)" || bad "$1 — expected $2, got $3"
}

[[ -x "$HUB" ]] || { echo "build first: cargo build --release --bin agit-hub"; exit 1; }
H="$(mktemp -d)/hub"; mkdir -p "$H"
TMP="$(mktemp -d)"
cleanup(){ pkill -f "agit-hub serve --root $H" 2>/dev/null; rm -rf "$TMP" "$(dirname "$H")"; }
trap cleanup EXIT

code(){ curl -s -o /dev/null -w '%{http_code}' "$@"; }
tok(){ awk '/token:/{print $2}' "$1" | head -1; }

echo "${B}Act 0 · the server refuses to be unsafe${N}"
out="$("$HUB" serve --root "$H" --host 0.0.0.0 --port "$PORT" 2>&1)"; rc=$?
is "non-loopback bind without TLS is refused" 2 "$rc"
echo "$out" | grep -qi "plaintext" && ok "and it says why" || bad "the refusal should explain itself"

echo "${B}Act 1 · users and password storage${N}"
"$HUB" user add alice --root "$H" >/dev/null 2>&1 <<< $'short\nshort\n'; rc=$?
is "a short password is rejected" 1 "$rc"
[[ -f "$H/users.json" ]] && bad "a rejected user must not be persisted" || ok "nothing persisted on rejection"

"$HUB" user add alice --root "$H" >/dev/null 2>&1 <<< $'pw-alice-123\npw-alice-123\n'
"$HUB" user add bob   --root "$H" >/dev/null 2>&1 <<< $'pw-bob-12345\npw-bob-12345\n'
[[ -f "$H/users.json" ]] && ok "users created" || bad "users.json missing"
grep -q "argon2id" "$H/users.json" && ok "passwords use argon2id, not a bare sha256" || bad "no argon2id in users.json"
grep -q "pw-alice-123" "$H/users.json" && bad "PLAINTEXT PASSWORD ON DISK" || ok "plaintext never hits disk"
is "users.json is 0600" 600 "$(stat -c '%a' "$H/users.json")"

echo "${B}Act 2 · agents, tokens${N}"
"$HUB" add payments --owner alice --root "$H" >/dev/null 2>&1
"$HUB" add other    --owner bob   --root "$H" >/dev/null 2>&1
"$HUB" token add alice-write --user alice --agent payments --write --root "$H" > "$TMP/w.txt" 2>&1
"$HUB" token add alice-read --user alice --agent payments --read --root "$H" > "$TMP/r.txt" 2>&1
WTOK="$(tok "$TMP/w.txt")"; RTOK="$(tok "$TMP/r.txt")"
[[ ${#WTOK} -ge 32 ]] && ok "write token issued (${#WTOK} chars)" || bad "no write token parsed"
[[ ${#RTOK} -ge 32 ]] && ok "read token issued" || bad "no read token parsed"
if [[ ${#WTOK} -ge 32 ]]; then
  grep -q "$WTOK" "$H"/*.json 2>/dev/null && bad "TOKEN STORED IN PLAINTEXT" || ok "only the token digest is stored"
else
  bad "skipped the plaintext-token check: no token to look for"
fi

# an unregistered bare repo: GIT_HTTP_EXPORT_ALL=1 would have served this to anyone
git init -q --bare "$H/sneaky.git"
"$HUB" serve --root "$H" --port "$PORT" >"$TMP/hub.log" 2>&1 &
sleep 1.5
U="http://127.0.0.1:$PORT"

echo "${B}Act 3 · the git-http gate (the bypass this all rests on)${N}"
is "anonymous fetch of a private agent"      401 "$(code "$U/payments.git/info/refs?service=git-upload-pack")"
is "anonymous fetch of an UNREGISTERED repo" 401 "$(code "$U/sneaky.git/info/refs?service=git-upload-pack")"
is "write token fetches its own agent"       200 "$(code -u "x:$WTOK" "$U/payments.git/info/refs?service=git-upload-pack")"
is "the same token on another agent"         403 "$(code -u "x:$WTOK" "$U/other.git/info/refs?service=git-upload-pack")"

echo "${B}Act 4 · existence must not leak${N}"
a="$(code "$U/api/agent/payments")"; b="$(code "$U/api/agent/nope-does-not-exist")"
is "private and nonexistent are indistinguishable" "$a" "$b"

echo "${B}Act 5 · real git push, and the read/write split${N}"
R1="$TMP/repo"; mkdir -p "$R1"; cd "$R1"
git init -q -b main .; echo hi > f.txt
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm init
if git push -q "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main 2>"$TMP/p1"; then
  ok "write token pushes"
else
  bad "write token push failed: $(head -1 "$TMP/p1")"
fi
echo more >> f.txt; git -c user.name=a -c user.email=a@e.com commit -qam second
if git push -q "http://x:$RTOK@127.0.0.1:$PORT/payments.git" main 2>"$TMP/p2"; then
  bad "SECURITY: a READ token was allowed to push"
else
  ok "read token cannot push"
fi
if git ls-remote "http://x:$RTOK@127.0.0.1:$PORT/payments.git" 2>/dev/null | grep -q main; then
  ok "read token can still fetch (the split is real, not a blanket deny)"
else
  bad "read token cannot fetch either — the grant is not a read grant"
fi
cd "$ROOT"

echo "${B}Act 6 · audit${N}"
A="$H/audit.log"
if [[ -f "$A" ]]; then
  n="$(wc -l < "$A")"
  [[ "$n" -gt 0 ]] && ok "audit recorded $n events" || bad "audit log is empty"
  grep -qi "user" "$A" && ok "user creation audited" || bad "user creation missing from audit"
  grep -qi "agent" "$A" && ok "agent creation audited" || bad "agent creation missing"
  grep -qi "token" "$A" && ok "token issuance audited" || bad "token issuance missing"
  # a denial must be recorded too: exposure control without accountability is theatre
  grep -qi "deny\|denied" "$A" && ok "denials are audited" || bad "no denial rows — the 403s above went unrecorded"
else
  bad "no audit.log at $A"
fi

echo
if [[ $FAIL -eq 0 ]]; then echo "${G}${B}hub e2e: $PASS checks passed.${N}"; else echo "${R}${B}hub e2e: $FAIL failed, $PASS passed.${N}"; fi
exit $FAIL
