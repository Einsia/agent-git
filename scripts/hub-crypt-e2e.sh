#!/usr/bin/env bash
# Live end-to-end proof of the encryption-recipients TEAM flow against a REAL agit-hub.
#
# This closes the gap wave 3 left: the team-default keybox flow was proven only by unit tests. Here a
# real hub (SQLite + filesystem — the Team KEK is DB-only, no S3 needed) drives the whole chain with the
# real `agit` client:
#
#   1. An org `acme` with two members (alice, bob) and a NON-member (carol), each on their OWN isolated
#      $AGIT_HOME and machine identity.
#   2. alice + bob both `agit identity enroll` (publish their machine X25519 to the hub registry).
#   3. an admin (alice) `agit hub team rekey acme` — mints TK gen 1, sealed to alice + bob.
#   4. alice creates a session, pushes it, then `agit a encrypt` with NO reader flags — the WAVE-4
#      ZERO-CONFIG TEAM DEFAULT: an org-owned session wraps its content key under the org's Team KEK
#      (a team stanza), "readable to the team, not the public". alice pushes the ciphertext.
#   5. bob CLONEs over HTTP (an org member, so the hub serves him the bytes), `agit crypt unlock`, and
#      SUCCESSFULLY reads the plaintext transcript.
#   6. carol (NOT a member) is FAIL-CLOSED on BOTH axes: the hub REFUSES her the bytes over git
#      (axis-1), and even handed the raw ciphertext out of band she cannot decrypt — `agit crypt unlock`
#      fails, the working tree stays ciphertext, and `agit crypt-smudge` REFUSES rather than leak (axis-2).
#   7. after a SECOND `agit hub team rekey acme` (carol was never in), bob STILL reads — his gen-1
#      envelope + the gen-1 team stanza survive the rotation.
#
# SELF-CONTAINED: it builds its own temp workdir, starts its own hub, and exits non-zero on ANY failed
# assertion. It references no path outside the repo and no developer scratch dir. It uses the
# isolate-$AGIT_HOME discipline: every per-user env var is its OWN export, never `export A=x B=$A/y`.
#
# ── Configuration (all overridable via env) ──
#   AGIT_BIN / AGIT       path to the `agit` client binary
#   AGIT_HUB_BIN / HUB    path to the `agit-hub` binary
#   PORT                  hub listen port (default 8573)
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

AGIT="${AGIT_BIN:-${AGIT:-}}"
if [ -z "$AGIT" ]; then
  for c in "$REPO_ROOT/target/release/agit" "$REPO_ROOT/target/debug/agit"; do [ -x "$c" ] && AGIT="$c" && break; done
fi
AGIT="${AGIT:-agit}"
HUB="${AGIT_HUB_BIN:-${HUB:-}}"
if [ -z "$HUB" ]; then
  for c in "$REPO_ROOT/target/release/agit-hub" "$REPO_ROOT/target/debug/agit-hub"; do [ -x "$c" ] && HUB="$c" && break; done
fi
HUB="${HUB:-agit-hub}"

PORT="${PORT:-8573}"
BASE="http://127.0.0.1:$PORT"
MARKER="MARKER_TEAM_PLAINTEXT_7fa31c"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/agit-hub-crypt-e2e.XXXXXX")"
ROOT="$WORKDIR/hubroot"

PASS=0; FAIL=0
ok(){ echo "  PASS: $1"; PASS=$((PASS+1)); }
no(){ echo "  FAIL: $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$2" = "$3" ]; then ok "$1 ($2)"; else no "$1 — expected [$3] got [$2]"; fi; }
GIT="git -c user.name=e2e -c user.email=e2e@local -c protocol.version=2 -c commit.gpgsign=false"

wait_ready(){ for i in $(seq 1 200); do curl -s -o /dev/null "$1/" && return 0; sleep 0.1; done; return 1; }
cleanup(){ [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null; rm -rf "$WORKDIR" 2>/dev/null; }
trap cleanup EXIT

for t in git curl; do command -v "$t" >/dev/null 2>&1 || { echo "FATAL: missing tool: $t" >&2; exit 2; }; done
[ -x "$AGIT" ] || command -v "$AGIT" >/dev/null 2>&1 || { echo "FATAL: agit not found: $AGIT" >&2; exit 2; }
[ -x "$HUB" ]  || command -v "$HUB"  >/dev/null 2>&1 || { echo "FATAL: agit-hub not found: $HUB" >&2; exit 2; }

echo "############ agit team-encryption LIVE e2e (SQLite/fs hub) ############"
echo "  agit:    $AGIT"
echo "  hub:     $HUB"
echo "  workdir: $WORKDIR"

# ── isolated $AGIT_HOME + $HOME per user (each its OWN export — never export A=x B=\$A/y) ──
export ALICE_AGIT="$WORKDIR/alice/agit"
export ALICE_HOME="$WORKDIR/alice/home"
export ALICE_CODE="$WORKDIR/alice/code"
export BOB_AGIT="$WORKDIR/bob/agit"
export BOB_HOME="$WORKDIR/bob/home"
export BOB_CODE="$WORKDIR/bob/code"
export CAROL_AGIT="$WORKDIR/carol/agit"
export CAROL_HOME="$WORKDIR/carol/home"
export CAROL_CODE="$WORKDIR/carol/code"
mkdir -p "$ALICE_CODE" "$BOB_CODE" "$CAROL_CODE"
$GIT -C "$ALICE_CODE" init -q -b main; $GIT -C "$BOB_CODE" init -q -b main; $GIT -C "$CAROL_CODE" init -q -b main

echo "== 1. hub users (password on stdin), tokens =="
printf 'alice-e2e-pass-1\n' | $HUB user add alice --admin --root "$ROOT" >/dev/null 2>&1
printf 'bob-e2e-pass-12\n'  | $HUB user add bob  --root "$ROOT" >/dev/null 2>&1
printf 'carol-e2e-pass-3\n' | $HUB user add carol --root "$ROOT" >/dev/null 2>&1
TA=$($HUB token add ta --user alice --write --root "$ROOT" 2>/dev/null | awk '/token:/{print $2}')
TB=$($HUB token add tb --user bob   --write --root "$ROOT" 2>/dev/null | awk '/token:/{print $2}')
TC=$($HUB token add tc --user carol --write --root "$ROOT" 2>/dev/null | awk '/token:/{print $2}')
[ -n "$TA" ] && [ -n "$TB" ] && [ -n "$TC" ] && ok "issued tokens for alice, bob, carol" || no "token issuance"

echo "== 2. start the hub (SQLite + filesystem) =="
$HUB serve --port "$PORT" --root "$ROOT" >"$WORKDIR/serve.log" 2>&1 &
SRV=$!
wait_ready "$BASE" || { no "hub did not come up"; echo "===== PASS=$PASS FAIL=$((FAIL+1)) ====="; exit 1; }
ok "hub is serving on $BASE"

# per-user hub URLs (token in the password field) — again, each its own export.
export ALICE_URL="http://alice:$TA@127.0.0.1:$PORT"
export BOB_URL="http://bob:$TB@127.0.0.1:$PORT"
export CAROL_URL="http://carol:$TC@127.0.0.1:$PORT"

echo "== 3. org acme = {alice(admin), bob}; carol is NOT a member =="
curl -s -c "$WORKDIR/ajar" -X POST "$BASE/api/login" -d '{"username":"alice","password":"alice-e2e-pass-1"}' >/dev/null
check "create org acme -> 201" "$(curl -s -b "$WORKDIR/ajar" -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orgs" -d '{"name":"acme"}')" "201"
check "add bob to acme -> 200"  "$(curl -s -b "$WORKDIR/ajar" -o /dev/null -w '%{http_code}' -X POST "$BASE/api/orgs/acme/members" -d '{"username":"bob","role":"member"}')" "200"
MEM=$(curl -s -b "$WORKDIR/ajar" "$BASE/api/orgs/acme")
echo "$MEM" | grep -q '"username":"bob"' && ok "bob is an acme member" || no "bob membership"
echo "$MEM" | grep -q '"username":"carol"' && no "carol must NOT be an acme member!!" || ok "carol is NOT an acme member"
check "create org agent acme/proj -> owner org:acme" "$(curl -s -b "$WORKDIR/ajar" -X POST "$BASE/api/agents" -d '{"name":"proj","org":"acme"}' | grep -o '"owner":"org:acme"')" '"owner":"org:acme"'

echo "== 4. alice + bob enroll their machine identities (own \$AGIT_HOME each) =="
( cd "$ALICE_CODE" && HOME="$ALICE_HOME" AGIT_HOME="$ALICE_AGIT" AGIT_HUB_URL="$ALICE_URL" "$AGIT" identity enroll >"$WORKDIR/a-enroll.out" 2>&1 )
grep -q "enrolled alice" "$WORKDIR/a-enroll.out" && ok "alice enrolled" || no "alice enroll: $(cat "$WORKDIR/a-enroll.out")"
( cd "$BOB_CODE" && HOME="$BOB_HOME" AGIT_HOME="$BOB_AGIT" AGIT_HUB_URL="$BOB_URL" "$AGIT" identity enroll >"$WORKDIR/b-enroll.out" 2>&1 )
grep -q "enrolled bob" "$WORKDIR/b-enroll.out" && ok "bob enrolled" || no "bob enroll: $(cat "$WORKDIR/b-enroll.out")"
# alice and bob must have DISTINCT machine identities (isolated homes).
AX=$(grep x25519 "$WORKDIR/a-enroll.out" | awk '{print $2}')
BX=$(grep x25519 "$WORKDIR/b-enroll.out" | awk '{print $2}')
[ -n "$AX" ] && [ "$AX" != "$BX" ] && ok "alice and bob have distinct machine identities" || no "identities not isolated ($AX vs $BX)"

echo "== 5. admin rotates the org Team KEK (gen 1, sealed to alice + bob) =="
( cd "$ALICE_CODE" && HOME="$ALICE_HOME" AGIT_HOME="$ALICE_AGIT" AGIT_HUB_URL="$ALICE_URL" "$AGIT" hub team rekey acme >"$WORKDIR/rekey1.out" 2>&1 )
grep -q "generation 1, sealed to 2 member" "$WORKDIR/rekey1.out" && ok "TK gen 1 sealed to 2 members" || no "rekey1: $(cat "$WORKDIR/rekey1.out")"

echo "== 6. alice: init the store, seed a session, push plaintext =="
( cd "$ALICE_CODE" && HOME="$ALICE_HOME" AGIT_HOME="$ALICE_AGIT" "$AGIT" a init proj >/dev/null 2>&1 )
STORE=$(HOME="$ALICE_HOME" AGIT_HOME="$ALICE_AGIT" AGIT_AGENT=proj "$AGIT" a rev-parse --show-toplevel 2>/dev/null)
[ -n "$STORE" ] && ok "alice store resolved" || no "alice store not resolved"
$GIT -C "$STORE" config user.email a@e2e; $GIT -C "$STORE" config user.name alice
$GIT -C "$STORE" remote add origin "$ALICE_URL/acme/proj.git"
mkdir -p "$STORE/sessions/web/claude-code"
printf '{"role":"user","content":"%s please refactor the parser"}\n' "$MARKER" > "$STORE/sessions/web/claude-code/s1.jsonl"
$GIT -C "$STORE" add -A && $GIT -C "$STORE" commit -qm "seed session"
( cd "$ALICE_CODE" && HOME="$ALICE_HOME" AGIT_HOME="$ALICE_AGIT" AGIT_HUB_URL="$ALICE_URL" AGIT_AGENT=proj "$AGIT" a push origin HEAD:main >/dev/null 2>"$WORKDIR/push1.err" )
check "alice push (plaintext) rc" "$?" "0"

echo "== 7. WAVE-4 ZERO-CONFIG: 'agit a encrypt' (no reader flags) -> TEAM stanza =="
( cd "$ALICE_CODE" && HOME="$ALICE_HOME" AGIT_HOME="$ALICE_AGIT" AGIT_HUB_URL="$ALICE_URL" AGIT_AGENT=proj "$AGIT" a encrypt --yes >"$WORKDIR/encrypt.out" 2>&1 )
check "zero-config encrypt rc" "$?" "0"
grep -q "team) encryption enabled" "$WORKDIR/encrypt.out" && ok "encrypt reports the TEAM default" || no "encrypt: $(cat "$WORKDIR/encrypt.out")"
KB="$STORE/.agit/keybox.jsonl"
grep -q '"t":"team"' "$KB" && ok "keybox has a team stanza" || no "no team stanza in keybox"
grep -q '"t":"public"' "$KB" && no "zero-config must NOT be public!!" || ok "zero-config is NOT public"
grep -q '"t":"user"' "$KB" && no "zero-config must NOT wrap to an individual owner!!" || ok "zero-config is team-only (no owner stanza)"
grep -q '"org":"acme"' "$KB" && ok "the team stanza names org acme" || no "team stanza org"
# The committed session blob is now ciphertext (AGITCRYPT magic), not plaintext.
$GIT -C "$STORE" cat-file -p HEAD:sessions/web/claude-code/s1.jsonl | head -c 9 | grep -q "AGITCRYPT" && ok "committed session blob is ciphertext" || no "session blob not encrypted"
( cd "$ALICE_CODE" && HOME="$ALICE_HOME" AGIT_HOME="$ALICE_AGIT" AGIT_HUB_URL="$ALICE_URL" AGIT_AGENT=proj "$AGIT" a push origin HEAD:main >/dev/null 2>"$WORKDIR/push2.err" )
check "alice push (ciphertext + keybox through the hub gate) rc" "$?" "0"
# The keybox is wrap-ciphertext; the hub secret gate must NOT refuse it.
grep -qi "REFUSED" "$WORKDIR/push2.err" && no "hub refused the keybox push!! $(cat "$WORKDIR/push2.err")" || ok "hub accepted the keybox (not flagged as a secret)"

echo "== 8. bob (org member) clones over HTTP, unlocks, and READS the plaintext =="
( cd "$BOB_CODE" && HOME="$BOB_HOME" AGIT_HOME="$BOB_AGIT" AGIT_HUB_URL="$BOB_URL" "$AGIT" a clone "$BOB_URL/acme/proj.git" >"$WORKDIR/bclone.out" 2>&1 )
grep -q "cloned proj" "$WORKDIR/bclone.out" && ok "bob cloned the org agent over HTTP" || no "bob clone: $(cat "$WORKDIR/bclone.out")"
BSTORE=$(HOME="$BOB_HOME" AGIT_HOME="$BOB_AGIT" AGIT_AGENT=proj "$AGIT" a rev-parse --show-toplevel 2>/dev/null)
BSESS="$BSTORE/sessions/web/claude-code/s1.jsonl"
# Before unlock: the working tree carries ciphertext (no filter wired, no key).
head -c 9 "$BSESS" | grep -q "AGITCRYPT" && ok "bob's pre-unlock working tree is ciphertext" || no "bob pre-unlock not ciphertext"
grep -q "$MARKER" "$BSESS" && no "bob read plaintext WITHOUT unlocking!!" || ok "bob cannot read before unlocking"
( cd "$BSTORE" && HOME="$BOB_HOME" AGIT_HOME="$BOB_AGIT" AGIT_HUB_URL="$BOB_URL" AGIT_AGENT=proj "$AGIT" crypt unlock >"$WORKDIR/bunlock.out" 2>&1 )
check "bob crypt unlock rc" "$?" "0"
grep -q "$MARKER" "$BSESS" && ok "BOB READS THE PLAINTEXT TRANSCRIPT after unlock" || no "bob failed to decrypt: $(cat "$WORKDIR/bunlock.out")"

echo "== 9. carol (NON-member) is FAIL-CLOSED on BOTH axes =="
# axis-1: the hub REFUSES carol the bytes over git (she is not an acme member).
check "carol git-fetch of the org agent -> 403 (hub refuses the bytes)" \
  "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$TC" "$BASE/acme/proj.git/info/refs?service=git-upload-pack")" "403"
# axis-2: even handed the raw ciphertext out of band (a hub-bytes-leak / operator copy), carol cannot
# decrypt. Clone the bare repo straight off the hub's filesystem, bypassing the ACL entirely.
( cd "$CAROL_CODE" && HOME="$CAROL_HOME" AGIT_HOME="$CAROL_AGIT" AGIT_HUB_URL="$CAROL_URL" "$AGIT" a clone "$ROOT/acme/proj.git" >"$WORKDIR/cclone.out" 2>&1 )
grep -q "cloned proj" "$WORKDIR/cclone.out" && ok "carol obtained the raw ciphertext bytes out of band" || no "carol clone: $(cat "$WORKDIR/cclone.out")"
CSTORE=$(HOME="$CAROL_HOME" AGIT_HOME="$CAROL_AGIT" AGIT_AGENT=proj "$AGIT" a rev-parse --show-toplevel 2>/dev/null)
CSESS="$CSTORE/sessions/web/claude-code/s1.jsonl"
( cd "$CSTORE" && HOME="$CAROL_HOME" AGIT_HOME="$CAROL_AGIT" AGIT_HUB_URL="$CAROL_URL" AGIT_AGENT=proj "$AGIT" crypt unlock >"$WORKDIR/cunlock.out" 2>&1 )
CU_RC=$?
[ "$CU_RC" -ne 0 ] && ok "carol crypt unlock FAILS (not a reader / no envelope)" || no "carol unlock must fail closed, rc=$CU_RC"
head -c 9 "$CSESS" | grep -q "AGITCRYPT" && ok "carol's working tree stays ciphertext" || no "carol working tree not ciphertext"
grep -q "$MARKER" "$CSESS" && no "carol READ THE PLAINTEXT!! fail-closed breached" || ok "carol CANNOT read the plaintext"
# The smudge filter itself must REFUSE (nonzero) rather than emit plaintext, given carol's (empty) keyring.
$GIT -C "$CSTORE" cat-file -p HEAD:sessions/web/claude-code/s1.jsonl > "$WORKDIR/cipher.blob"
( cd "$CSTORE" && HOME="$CAROL_HOME" AGIT_HOME="$CAROL_AGIT" "$AGIT" crypt-smudge <"$WORKDIR/cipher.blob" >"$WORKDIR/csmudge.out" 2>/dev/null )
SM_RC=$?
[ "$SM_RC" -ne 0 ] && ok "crypt-smudge REFUSES carol's ciphertext (nonzero exit)" || no "smudge did not refuse, rc=$SM_RC"
grep -q "$MARKER" "$WORKDIR/csmudge.out" && no "smudge leaked plaintext to carol!!" || ok "smudge emitted no plaintext for carol"

echo "== 10. a second team rekey (carol never in) — bob STILL reads =="
( cd "$ALICE_CODE" && HOME="$ALICE_HOME" AGIT_HOME="$ALICE_AGIT" AGIT_HUB_URL="$ALICE_URL" "$AGIT" hub team rekey acme >"$WORKDIR/rekey2.out" 2>&1 )
grep -q "generation 2, sealed to 2 member" "$WORKDIR/rekey2.out" && ok "TK rotated to gen 2 (still alice + bob)" || no "rekey2: $(cat "$WORKDIR/rekey2.out")"
# Force bob to RE-DERIVE from scratch: drop his repo-local keyring AND his cached gen-1 TK, so the
# re-unlock must refetch his gen-1 envelope from the hub (which the rotation did NOT delete).
[ -n "$BSTORE" ] && [ -n "$BOB_AGIT" ] && rm -rf "$BSTORE/.git/agit-crypt" "$BOB_AGIT/crypt/tk"
$GIT -C "$BSTORE" checkout -q -- sessions 2>/dev/null || true
( cd "$BSTORE" && HOME="$BOB_HOME" AGIT_HOME="$BOB_AGIT" AGIT_HUB_URL="$BOB_URL" AGIT_AGENT=proj "$AGIT" crypt unlock >"$WORKDIR/bunlock2.out" 2>&1 )
check "bob re-unlock after the rotation rc" "$?" "0"
grep -q "$MARKER" "$BSESS" && ok "bob STILL reads after the team rekey (gen-1 envelope survived)" || no "bob lost access after rekey: $(cat "$WORKDIR/bunlock2.out")"

echo
echo "===== PASS=$PASS FAIL=$FAIL ====="
[ "$FAIL" -eq 0 ]
