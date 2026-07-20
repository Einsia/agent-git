#!/usr/bin/env bash
# Live end-to-end QA of agit-hub against REAL Postgres + Garage (S3).
#
# This is the CI counterpart to demo/hub-e2e.sh (which exercises the SQLite/fs
# default). Every check here is BACKEND-APPROPRIATE for the PRODUCTION stack: no
# hub.db byte-greps, no local blobs-dir stat, no "blobs: filesystem" banner.
# Instead it proves persistence by querying Postgres, proves object storage by
# inspecting the Garage bucket over S3, and proves durability by restarting the
# hub process against the SAME Postgres + Garage.
#
# It covers the full behavioral surface (login/session, ACL over git smart-http,
# tokens, registration, organizations, blob PUT/GET/rename/purge) PLUS the
# live-only proofs:
#   1. Postgres persistence across a process restart.
#   2. No plaintext password/token in Postgres (argon2id + sha256 digests only).
#   3. Blobs really land in the Garage bucket (object count + key listing);
#      rename moves the object's prefix, purge physically deletes it.
#   4. is_admin round-trips through Postgres, private-agent 404 non-disclosure.
#
# SELF-CONTAINED: it takes the hub binary, the Postgres URL and the Garage S3
# endpoint/keys from the environment (with CI defaults matching the workflow's
# services), creates its own temp workdir, and exits non-zero on ANY failed
# check. It references no path outside the repo and no developer scratch dir.
#
# ── Configuration (all overridable via env) ──────────────────────────────────
#   AGIT_HUB_BIN / HUB       path to the agit-hub binary to test
#   AGIT_HUB_DB              postgres://... connection URL (the hub reads this)
#   AGIT_HUB_S3_ENDPOINT     Garage/S3 endpoint URL
#   AGIT_HUB_S3_BUCKET       bucket name (must already exist)
#   AGIT_HUB_S3_REGION       S3 region (Garage layout region)
#   AGIT_HUB_S3_ACCESS_KEY   S3 access key id
#   AGIT_HUB_S3_SECRET_KEY   S3 secret key
#   PG_CONTAINER/PG_USER/PG_DB   fallback docker-exec psql target when no host
#                                `psql` client is on PATH (local dev boxes)
set -u

# ── locate the binary ────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HUB="${AGIT_HUB_BIN:-${HUB:-}}"
if [ -z "$HUB" ]; then
  for cand in "$REPO_ROOT/target/release/agit-hub" "$REPO_ROOT/target/debug/agit-hub"; do
    [ -x "$cand" ] && HUB="$cand" && break
  done
fi
HUB="${HUB:-agit-hub}"

# ── live backends (all overridable; CI defaults match the hub-live workflow) ──
export AGIT_HUB_DB="${AGIT_HUB_DB:-postgres://agithub:agithub@127.0.0.1:5433/agithub}"
export AGIT_HUB_S3_ENDPOINT="${AGIT_HUB_S3_ENDPOINT:-http://127.0.0.1:13900}"
export AGIT_HUB_S3_BUCKET="${AGIT_HUB_S3_BUCKET:-agit-blobs}"
export AGIT_HUB_S3_REGION="${AGIT_HUB_S3_REGION:-garage}"
# NOTE: these are placeholder defaults, NOT real credentials. CI injects a fresh
# ephemeral Garage key via $GITHUB_ENV; local dev must export its own. Never bake
# a real S3 secret into a committed file.
export AGIT_HUB_S3_ACCESS_KEY="${AGIT_HUB_S3_ACCESS_KEY:-GKplaceholderaccesskey000}"
export AGIT_HUB_S3_SECRET_KEY="${AGIT_HUB_S3_SECRET_KEY:-0000000000000000000000000000000000000000000000000000000000000000}"
BUCKET="$AGIT_HUB_S3_BUCKET"

# docker-exec psql fallback target (used only when no host `psql` is present)
PG_CONTAINER="${PG_CONTAINER:-agit-pg}"
PG_USER="${PG_USER:-agithub}"
PG_DB="${PG_DB:-agithub}"

# aws CLI for object-level Garage inspection (present in CI and on the dev box)
export AWS_ACCESS_KEY_ID="$AGIT_HUB_S3_ACCESS_KEY"
export AWS_SECRET_ACCESS_KEY="$AGIT_HUB_S3_SECRET_KEY"
export AWS_DEFAULT_REGION="$AGIT_HUB_S3_REGION"
AWS="aws --endpoint-url $AGIT_HUB_S3_ENDPOINT"

PORT_MAIN="${PORT_MAIN:-8611}"
PORT_REG="${PORT_REG:-8612}"
BASE="http://127.0.0.1:$PORT_MAIN"
BREG="http://127.0.0.1:$PORT_REG"

# ── self-owned temp workdir (cleaned up on exit; never a scratch path) ───────
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/agit-hub-live-e2e.XXXXXX")"
ROOT_MAIN=$(mktemp -d "$WORKDIR/e2e-main.XXXX")
ROOT_REG=$(mktemp -d "$WORKDIR/e2e-reg.XXXX")

PASS=0; FAIL=0
ok(){ echo "  PASS: $1"; PASS=$((PASS+1)); }
no(){ echo "  FAIL: $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$2" = "$3" ]; then ok "$1 ($2)"; else no "$1 — expected [$3] got [$2]"; fi; }
GIT="git -c user.name=live -c user.email=live@local -c protocol.version=2"

# psql: prefer a host client over TCP (CI installs postgresql-client and points
# it at the Postgres service via AGIT_HUB_DB); fall back to docker exec into a
# named container for local dev boxes without a psql on PATH.
if command -v psql >/dev/null 2>&1; then
  psqlq(){ psql "$AGIT_HUB_DB" -tAc "$1" 2>/dev/null; }
  psqlx(){ psql "$AGIT_HUB_DB" -c "$1" >/dev/null 2>&1; }
else
  psqlq(){ docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAc "$1" 2>/dev/null; }
  psqlx(){ docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -c "$1" >/dev/null 2>&1; }
fi

# Garage object count == number of keys physically in the bucket (S3-portable;
# no dependency on the `garage` admin binary or a named container).
gcount(){ $AWS s3 ls "s3://$BUCKET" --recursive 2>/dev/null | grep -c . ; }
gls(){ $AWS s3 ls "s3://$BUCKET" --recursive 2>/dev/null; }
wait_ready(){ for i in $(seq 1 200); do curl -s -o /dev/null "$1/" && return 0; sleep 0.1; done; return 1; }
cleanup(){
  [ -n "${SRV_MAIN:-}" ] && kill "$SRV_MAIN" 2>/dev/null
  [ -n "${SRV_REG:-}" ] && kill "$SRV_REG" 2>/dev/null
  rm -rf "$WORKDIR" 2>/dev/null
}
trap cleanup EXIT

# preflight: fail loudly and early if a required tool or backend is missing
for t in git curl aws; do
  command -v "$t" >/dev/null 2>&1 || { echo "FATAL: required tool not found: $t" >&2; exit 2; }
done
[ -x "$HUB" ] || command -v "$HUB" >/dev/null 2>&1 || { echo "FATAL: agit-hub binary not found: $HUB" >&2; exit 2; }

echo "############ agit-hub LIVE e2e — Postgres + Garage ############"
echo "  DB:      $AGIT_HUB_DB"
echo "  S3:      $AGIT_HUB_S3_ENDPOINT  bucket=$BUCKET  region=$AGIT_HUB_S3_REGION"
echo "  binary:  $HUB"
echo "  workdir: $WORKDIR"

echo "== 0. reset Postgres schema + empty Garage bucket (clean baseline) =="
psqlx "DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;"
$AWS s3 rm "s3://$BUCKET" --recursive >/dev/null 2>&1
G_BASE=$(gcount)
check "Garage bucket starts empty" "$G_BASE" "0"
[ -z "$(psqlq "SELECT to_regclass('public.users')")" ] && ok "Postgres schema starts clean (no users table)" || no "schema not clean"

echo "== 1. create users via CLI (password on stdin, never argv) =="
APW='alice-live-passphrase-77'; BPW='bob-live-passphrase-88'
printf '%s\n' "$APW" | $HUB user add alice --admin --root "$ROOT_MAIN" >"$ROOT_MAIN/ua.out" 2>&1
printf '%s\n' "$BPW"  | $HUB user add bob   --root "$ROOT_MAIN" >/dev/null 2>&1
printf 'short\n'      | $HUB user add shorty --root "$ROOT_MAIN" >/dev/null 2>&1 && no "short password should be rejected" || ok "short password rejected"
$HUB user list --root "$ROOT_MAIN" | grep -q "alice.*admin" && ok "alice created as admin" || no "alice admin"
grep -q "stored in Postgres" "$ROOT_MAIN/ua.out" && ok "CLI says stored in Postgres (not users.json)" || no "CLI backend message: $(cat "$ROOT_MAIN/ua.out")"
grep -q "users.json" "$ROOT_MAIN/ua.out" && no "CLI still claims users.json on Postgres" || ok "CLI no longer claims users.json"

echo "== 2. Postgres has NO plaintext password; kdf=argon2id; is_admin round-trips =="
DUMP=$(psqlq "SELECT username||'|'||pw_hash||'|'||salt||'|'||kdf||'|'||is_admin FROM users ORDER BY username")
echo "$DUMP" | sed 's/^/    users-row: /'
echo "$DUMP" | grep -qF "$APW" && no "alice plaintext password found in Postgres!!" || ok "alice plaintext password absent from Postgres"
echo "$DUMP" | grep -qF "$BPW" && no "bob plaintext password found in Postgres!!"   || ok "bob plaintext password absent from Postgres"
NBAD=$(psqlq "SELECT count(*) FROM users WHERE kdf NOT LIKE 'argon2id%'")
check "every user kdf is argon2id" "$NBAD" "0"
check "alice.is_admin round-trips as 1 (admin decodes on Postgres)" "$(psqlq "SELECT is_admin FROM users WHERE username='alice'")" "1"
check "bob.is_admin round-trips as 0 (non-admin decodes on Postgres)" "$(psqlq "SELECT is_admin FROM users WHERE username='bob'")" "0"

echo "== 3. create agents (default private) =="
$HUB add secretproj --owner alice --root "$ROOT_MAIN" >/dev/null 2>&1
$HUB add openproj --owner alice --public --root "$ROOT_MAIN" >/dev/null 2>&1
$HUB add bobproj --owner bob --root "$ROOT_MAIN" >/dev/null 2>&1
$HUB list --root "$ROOT_MAIN" | grep -Eq "secretproj .*private" && ok "default visibility is private" || no "default visibility"
check "agents persisted to Postgres" "$(psqlq "SELECT count(*) FROM agents")" "3"

echo "== 4. issue tokens; Postgres stores digests only (no plaintext token) =="
T_ALICE_W=$($HUB token add ci   --user alice --agent alice/secretproj --write --root "$ROOT_MAIN" 2>/dev/null | awk '/token:/{print $2}')
T_ALICE_R=$($HUB token add ro   --user alice --agent alice/secretproj --read  --root "$ROOT_MAIN" 2>/dev/null | awk '/token:/{print $2}')
T_BOB_W=$($HUB   token add bobci --user bob   --agent bob/bobproj    --write --root "$ROOT_MAIN" 2>/dev/null | awk '/token:/{print $2}')
[ -n "$T_ALICE_W" ] && ok "token issued (plaintext returned once)" || no "token add"
TDUMP=$(psqlq "SELECT id||'|'||name||'|'||scope||'|'||hash FROM tokens ORDER BY name")
echo "$TDUMP" | sed 's/^/    tokens-row: /'
LEAK=0
for t in "$T_ALICE_W" "$T_ALICE_R" "$T_BOB_W"; do echo "$TDUMP" | grep -qF "$t" && LEAK=$((LEAK+1)); done
check "no plaintext token in Postgres (only sha256 digests)" "$LEAK" "0"
NHASH=$(psqlq "SELECT count(*) FROM tokens WHERE hash ~ '^[0-9a-f]{64}$'")
check "every token stored as a 64-hex sha256 digest" "$NHASH" "3"

echo "== 5. start MAIN hub (invite-only) against Postgres + Garage =="
$HUB serve --port "$PORT_MAIN" --root "$ROOT_MAIN" >"$ROOT_MAIN/serve.log" 2>&1 &
SRV_MAIN=$!
wait_ready "$BASE" || { no "main hub did not come up"; echo "===== PASS=$PASS FAIL=$((FAIL+1)) ====="; exit 1; }
grep -q "store:   Postgres" "$ROOT_MAIN/serve.log" && ok "banner shows store: Postgres" || no "store banner: $(grep -i store "$ROOT_MAIN/serve.log")"
grep -Eq "blobs:   s3 .*/$BUCKET" "$ROOT_MAIN/serve.log" && ok "banner shows blobs: s3 (Garage)" || no "blobs banner: $(grep -i blobs "$ROOT_MAIN/serve.log")"
grep -q "signup:  invite-only" "$ROOT_MAIN/serve.log" && ok "banner shows signup: invite-only" || no "signup banner"

echo "== 6. login / session (admin bit over the wire from Postgres) =="
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/login" -d "{\"username\":\"alice\",\"password\":\"wrong\"}")
check "wrong password -> 401" "$code" "401"
curl -s -c "$ROOT_MAIN/jar" -X POST "$BASE/api/login" -d "{\"username\":\"alice\",\"password\":\"$APW\"}" >/dev/null
grep -q agit_session "$ROOT_MAIN/jar" && ok "login sets session cookie" || no "cookie"
me=$(curl -s -b "$ROOT_MAIN/jar" "$BASE/api/me")
echo "$me" | grep -q '"username":"alice"' && ok "/api/me identifies alice" || no "/api/me: $me"
echo "$me" | grep -q '"is_admin":true' && ok "/api/me carries is_admin (decoded from Postgres)" || no "is_admin: $me"
check "anonymous /api/me -> 401" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/me")" "401"
curl -s -c "$ROOT_MAIN/jarbob" -X POST "$BASE/api/login" -d "{\"username\":\"bob\",\"password\":\"$BPW\"}" >/dev/null

echo "== 7. /api/agents visibility + private-agent non-disclosure =="
anon=$(curl -s "$BASE/api/agents")
echo "$anon" | grep -q openproj && ok "anon sees public agent" || no "public visible"
echo "$anon" | grep -q secretproj && no "anon sees private agent!!" || ok "anon cannot see private agent"
echo "$(curl -s -b "$ROOT_MAIN/jar" "$BASE/api/agents")" | grep -q '"role":"owner"' && ok "owner listing carries role=owner" || no "role"
echo "$(curl -s -b "$ROOT_MAIN/jarbob" "$BASE/api/agents")" | grep -q secretproj && no "bob sees alice private agent!!" || ok "bob cannot see alice private agent"
check "bob GET private agent detail -> 404 (non-disclosure)" "$(curl -s -o /dev/null -w '%{http_code}' -b "$ROOT_MAIN/jarbob" "$BASE/api/agent/alice/secretproj")" "404"

echo "== 8. git smart-http ACL (must precede http-backend) =="
check "anon pull private -> 401" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/alice/secretproj.git/info/refs?service=git-upload-pack")" "401"
check "anon pull public -> 200"  "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/alice/openproj.git/info/refs?service=git-upload-pack")" "200"
check "bound token cross-agent pull -> 403" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_W" "$BASE/bob/bobproj.git/info/refs?service=git-upload-pack")" "403"
check "bound token own-agent pull -> 200"   "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_W" "$BASE/alice/secretproj.git/info/refs?service=git-upload-pack")" "200"
check "stranger token on private -> 403"     "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_BOB_W"   "$BASE/alice/secretproj.git/info/refs?service=git-upload-pack")" "403"
check "read token probes receive-pack -> 403" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_R" "$BASE/alice/secretproj.git/info/refs?service=git-receive-pack")" "403"
check "write token probes receive-pack -> 200" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_W" "$BASE/alice/secretproj.git/info/refs?service=git-receive-pack")" "200"

echo "== 9. real clone / push / read-only push blocked (populates aid) =="
WORK=$(mktemp -d "$WORKDIR/e2e-work.XXXX")
( cd "$WORK" && $GIT clone -q "http://git:$T_ALICE_W@127.0.0.1:$PORT_MAIN/alice/secretproj.git" c1 2>/dev/null ) && ok "write token can clone" || no "clone"
if cd "$WORK/c1" 2>/dev/null; then
  mkdir -p sessions/myenv/codex
  printf '{"payload":{"model":"gpt-5"}}\n' > sessions/myenv/codex/s1.jsonl
  printf '[agent]\nid = "agt_11112222-3333-4444-5555-666677778888"\nname = "secretproj"\n' > agent.toml
  $GIT add -A >/dev/null && $GIT commit -qm "first session" >/dev/null
  $GIT push -q origin HEAD:main 2>/dev/null && ok "write token can push" || no "push"
  cd "$WORKDIR"
fi
( cd "$WORK" && $GIT clone -q "http://git:$T_ALICE_R@127.0.0.1:$PORT_MAIN/alice/secretproj.git" c2 2>/dev/null )
if cd "$WORK/c2" 2>/dev/null; then
  echo x >> agent.toml && $GIT add -A >/dev/null && $GIT commit -qm x >/dev/null
  $GIT push -q origin HEAD:main 2>/dev/null && no "read-only token pushed!!" || ok "read-only token cannot push"
  cd "$WORKDIR"
fi
d=$(curl -s -b "$ROOT_MAIN/jar" "$BASE/api/agent/alice/secretproj")
echo "$d" | grep -q '"aid":"agt_11112222-3333-4444-5555-666677778888"' && ok "aid read from pushed agent.toml" || no "aid: $(echo "$d" | head -c 200)"

echo "== 10. blobs really land in Garage (bucket count + key listing) =="
G0=$(gcount)
head -c 204800 /dev/urandom > "$WORKDIR/e2e-big.bin"
LOCAL_SHA=$(sha256sum "$WORKDIR/e2e-big.bin" | awk '{print $1}')
code=$(curl -s -o "$WORKDIR/e2e-put.json" -w '%{http_code}' -u "git:$T_ALICE_W" -X PUT --data-binary @"$WORKDIR/e2e-big.bin" "$BASE/api/agent/alice/secretproj/blob")
check "write token PUT 200KB blob -> 201" "$code" "201"
SHA=$(sed -n 's/.*"sha256":"\([0-9a-f]*\)".*/\1/p' "$WORKDIR/e2e-put.json")
check "server-side sha256 == local sha256" "$SHA" "$LOCAL_SHA"
G1=$(gcount)
check "Garage object count increased after PUT" "$G1" "$((G0+1))"
$AWS s3 ls "s3://$BUCKET/blobs/alice/secretproj/$SHA" | grep -q "$SHA" && ok "object present in Garage under blobs/alice/secretproj/<sha>" || no "object not found in Garage"
echo "    garage-objects: $G0 -> $G1 (after PUT)"
curl -s -u "git:$T_ALICE_W" -o "$WORKDIR/e2e-got.bin" "$BASE/api/agent/alice/secretproj/blob/$SHA"
check "GET round-trips identical bytes" "$(sha256sum "$WORKDIR/e2e-got.bin" | awk '{print $1}')" "$SHA"
hdr=$(curl -s -D - -o /dev/null -u "git:$T_ALICE_W" "$BASE/api/agent/alice/secretproj/blob/$SHA")
echo "$hdr" | grep -qi 'X-Content-Type-Options: nosniff' && ok "blob response has nosniff" || no "nosniff"
echo "$hdr" | grep -qi "Content-Security-Policy: default-src 'none'; sandbox" && ok "blob response has sandbox CSP" || no "CSP"
check "re-PUT identical bytes -> 201 (idempotent)" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_W" -X PUT --data-binary @"$WORKDIR/e2e-big.bin" "$BASE/api/agent/alice/secretproj/blob")" "201"
check "re-PUT did not duplicate the object in Garage" "$(gcount)" "$G1"
check "read token GET blob -> 200" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_R" "$BASE/api/agent/alice/secretproj/blob/$SHA")" "200"
check "read token PUT blob -> 403" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_R" -X PUT --data-binary @"$WORKDIR/e2e-big.bin" "$BASE/api/agent/alice/secretproj/blob")" "403"
WRONG=$(printf '%064d' 0)
check "PUT wrong claimed sha256 -> 409" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_W" -X PUT --data-binary @"$WORKDIR/e2e-big.bin" "$BASE/api/agent/alice/secretproj/blob?sha256=$WRONG")" "409"
check "stranger GET private blob -> 404 (non-disclosure)" "$(curl -s -o /dev/null -w '%{http_code}' -b "$ROOT_MAIN/jarbob" "$BASE/api/agent/alice/secretproj/blob/$SHA")" "404"
check "anon GET private blob -> 401" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/agent/alice/secretproj/blob/$SHA")" "401"
check "same digest under another agent -> 404 (per-agent namespace)" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_BOB_W" "$BASE/api/agent/bob/bobproj/blob/$SHA")" "404"
check "malformed digest -> 404" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_W" "$BASE/api/agent/alice/secretproj/blob/nothex")" "404"

echo "== 11. blob rename migration in Garage (object physically moves prefix) =="
J="$ROOT_MAIN/jar"
curl -s -b "$J" -X POST "$BASE/api/agents" -d '{"name":"renmem"}' >/dev/null
printf 'rename-me-bytes' > "$WORKDIR/e2e-rb.bin"
GR0=$(gcount)
RSHA=$(curl -s -b "$J" -X PUT --data-binary @"$WORKDIR/e2e-rb.bin" "$BASE/api/agent/alice/renmem/blob" | sed -n 's/.*"sha256":"\([0-9a-f]*\)".*/\1/p')
[ -n "$RSHA" ] && ok "renmem blob uploaded (session auth)" || no "renmem blob put"
$AWS s3 ls "s3://$BUCKET/blobs/alice/renmem/$RSHA" | grep -q "$RSHA" && ok "object present under blobs/alice/renmem/ before rename" || no "pre-rename object missing"
check "rename renmem -> renmem2 -> 200" "$(curl -s -o /dev/null -w '%{http_code}' -b "$J" -X PATCH "$BASE/api/agent/alice/renmem" -d '{"name":"renmem2"}')" "200"
check "blob served under NEW name -> 200" "$(curl -s -o /dev/null -w '%{http_code}' -b "$J" "$BASE/api/agent/alice/renmem2/blob/$RSHA")" "200"
check "blob under OLD name -> 404" "$(curl -s -o /dev/null -w '%{http_code}' -b "$J" "$BASE/api/agent/alice/renmem/blob/$RSHA")" "404"
$AWS s3 ls "s3://$BUCKET/blobs/alice/renmem/$RSHA" | grep -q "$RSHA" && no "stale object left under old prefix in Garage!!" || ok "Garage: old prefix blobs/alice/renmem/ is gone"
$AWS s3 ls "s3://$BUCKET/blobs/alice/renmem2/$RSHA" | grep -q "$RSHA" && ok "Garage: object now under new prefix blobs/alice/renmem2/" || no "new-prefix object missing"
check "rename kept object count stable (moved, not lost/duplicated)" "$(gcount)" "$((GR0+1))"

echo "== 12. blob purge in Garage (object physically disappears) =="
curl -s -b "$J" -X POST "$BASE/api/agents" -d '{"name":"purgeme"}' >/dev/null
printf 'previous-owner-secret' > "$WORKDIR/e2e-pb.bin"
GP0=$(gcount)
PSHA=$(curl -s -b "$J" -X PUT --data-binary @"$WORKDIR/e2e-pb.bin" "$BASE/api/agent/alice/purgeme/blob" | sed -n 's/.*"sha256":"\([0-9a-f]*\)".*/\1/p')
[ -n "$PSHA" ] && ok "purgeme blob uploaded" || no "purgeme blob put"
check "Garage count +1 after purgeme PUT" "$(gcount)" "$((GP0+1))"
curl -s -b "$J" -o /dev/null -X DELETE "$BASE/api/agent/alice/purgeme"                 # soft delete
check "purge purgeme -> 204" "$(curl -s -o /dev/null -w '%{http_code}' -b "$J" -X DELETE "$BASE/api/agent/alice/purgeme?purge=true")" "204"
$AWS s3 ls "s3://$BUCKET/blobs/alice/purgeme/$PSHA" | grep -q "$PSHA" && no "purged object still in Garage!!" || ok "Garage: purged object physically gone"
check "Garage count back to pre-PUT after purge" "$(gcount)" "$GP0"
curl -s -b "$J" -X POST "$BASE/api/agents" -d '{"name":"purgeme"}' >/dev/null       # recreate same name
check "recycled name cannot read prior owner blob -> 404" "$(curl -s -o /dev/null -w '%{http_code}' -b "$J" "$BASE/api/agent/alice/purgeme/blob/$PSHA")" "404"

echo "== 13. self-service registration + organizations (fresh usernames, reg hub) =="
CPW='carol-live-passphrase-99'; DPW='dave-live-passphrase-11'; EPW='erin-live-passphrase-22'
AGIT_HUB_REGISTRATION=1 $HUB serve --port "$PORT_REG" --root "$ROOT_REG" >"$ROOT_REG/serve.log" 2>&1 &
SRV_REG=$!
wait_ready "$BREG" || no "reg hub did not come up"
grep -q "signup:  open" "$ROOT_REG/serve.log" && ok "reg hub banner shows signup: open" || no "signup open banner"
tokf(){ sed -n 's/.*"token":"\([^"]*\)".*/\1/p'; }
check "register carol -> 200" "$(curl -s -c "$ROOT_REG/cjar" -o /dev/null -w '%{http_code}' -X POST "$BREG/api/register" -d "{\"username\":\"carol\",\"password\":\"$CPW\"}")" "200"
grep -q agit_session "$ROOT_REG/cjar" && ok "register issues a session cookie" || no "register cookie"
echo "$(curl -s -b "$ROOT_REG/cjar" "$BREG/api/me")" | grep -q '"is_admin":false' && ok "registered user is not admin" || no "register is_admin"
check "duplicate username -> 409 (not 500)" "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BREG/api/register" -d "{\"username\":\"carol\",\"password\":\"another-pass-1\"}")" "409"
echo "$(curl -s -X POST "$BREG/api/register" -d '{"username":"mallory","password":"mallory-pass-1","is_admin":true}')" | grep -q '"is_admin":false' && ok "cannot self-grant admin at register" || no "register admin escalation"
check "short password -> 400" "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BREG/api/register" -d '{"username":"tiny","password":"short"}')" "400"
check "illegal username -> 400" "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BREG/api/register" -d '{"username":"Bad Name","password":"password-123"}')" "400"
check "create org acme -> 201" "$(curl -s -b "$ROOT_REG/cjar" -o /dev/null -w '%{http_code}' -X POST "$BREG/api/orgs" -d '{"name":"acme"}')" "201"
check "duplicate org name -> 409" "$(curl -s -o /dev/null -w '%{http_code}' -b "$ROOT_REG/cjar" -X POST "$BREG/api/orgs" -d '{"name":"acme"}')" "409"
curl -s -c "$ROOT_REG/djar" -X POST "$BREG/api/register" -d "{\"username\":\"dave\",\"password\":\"$DPW\"}" >/dev/null
check "org admin adds member -> 200" "$(curl -s -b "$ROOT_REG/cjar" -o /dev/null -w '%{http_code}' -X POST "$BREG/api/orgs/acme/members" -d '{"username":"dave","role":"member"}')" "200"
check "non-admin member adds member -> 403" "$(curl -s -b "$ROOT_REG/djar" -o /dev/null -w '%{http_code}' -X POST "$BREG/api/orgs/acme/members" -d '{"username":"carol","role":"admin"}')" "403"
check "add non-existent user -> 400" "$(curl -s -b "$ROOT_REG/cjar" -o /dev/null -w '%{http_code}' -X POST "$BREG/api/orgs/acme/members" -d '{"username":"ghost","role":"member"}')" "400"
check "remove last admin -> 409" "$(curl -s -b "$ROOT_REG/cjar" -o /dev/null -w '%{http_code}' -X DELETE "$BREG/api/orgs/acme/members/carol")" "409"
curl -s -c "$ROOT_REG/ejar" -X POST "$BREG/api/register" -d "{\"username\":\"erin\",\"password\":\"$EPW\"}" >/dev/null
check "non-member views org -> 404 (non-disclosure)" "$(curl -s -b "$ROOT_REG/ejar" -o /dev/null -w '%{http_code}' "$BREG/api/orgs/acme")" "404"
check "member views org -> 200" "$(curl -s -b "$ROOT_REG/djar" -o /dev/null -w '%{http_code}' "$BREG/api/orgs/acme")" "200"
echo "$(curl -s -b "$ROOT_REG/cjar" -X POST "$BREG/api/agents" -d '{"name":"shared","org":"acme"}')" | grep -q '"owner":"org:acme"' && ok "create agent under org (owner=org:acme)" || no "org agent"
check "non-admin member creates org agent -> 403" "$(curl -s -b "$ROOT_REG/djar" -o /dev/null -w '%{http_code}' -X POST "$BREG/api/agents" -d '{"name":"shared2","org":"acme"}')" "403"
check "org member reads org private agent -> 200" "$(curl -s -b "$ROOT_REG/djar" -o /dev/null -w '%{http_code}' "$BREG/api/agent/acme/shared")" "200"
check "non-member reads org private agent -> 404" "$(curl -s -b "$ROOT_REG/ejar" -o /dev/null -w '%{http_code}' "$BREG/api/agent/acme/shared")" "404"
DT=$(curl -s -b "$ROOT_REG/djar" -X POST "$BREG/api/tokens" -d '{"name":"dt","scope":"read"}' | tokf)
check "org member git-pulls org agent -> 200 (smart-http folding)" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$DT" "$BREG/acme/shared.git/info/refs?service=git-upload-pack")" "200"
ET=$(curl -s -b "$ROOT_REG/ejar" -X POST "$BREG/api/tokens" -d '{"name":"et","scope":"read"}' | tokf)
check "non-member git-pulls org agent -> 403" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$ET" "$BREG/acme/shared.git/info/refs?service=git-upload-pack")" "403"
kill "$SRV_REG" 2>/dev/null; SRV_REG=""

echo "== 14. registration is closed by default on the main (invite-only) hub =="
check "POST /api/register on invite-only hub -> 403" "$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/register" -d '{"username":"walkin","password":"password-123"}')" "403"

echo "== 15. POSTGRES + GARAGE PERSISTENCE ACROSS A HUB RESTART =="
# Snapshot durable state, then STOP the hub process and START a brand-new one
# against the SAME Postgres + Garage + root, and prove everything survives.
U_BEFORE=$(psqlq "SELECT count(*) FROM users")
A_BEFORE=$(psqlq "SELECT count(*) FROM agents")
T_BEFORE=$(psqlq "SELECT count(*) FROM tokens")
G_BEFORE=$(gcount)
echo "    before restart: users=$U_BEFORE agents=$A_BEFORE tokens=$T_BEFORE garage_objects=$G_BEFORE"
kill "$SRV_MAIN" 2>/dev/null; wait "$SRV_MAIN" 2>/dev/null; SRV_MAIN=""
# prove the old process is really gone
curl -s -o /dev/null -w '' "$BASE/" 2>/dev/null && no "old hub still answering after kill" || ok "old hub process stopped"
$HUB serve --port "$PORT_MAIN" --root "$ROOT_MAIN" >"$ROOT_MAIN/serve2.log" 2>&1 &
SRV_MAIN=$!
wait_ready "$BASE" || no "restarted hub did not come up"
# user survived: login with the ORIGINAL password works on the cold process
curl -s -c "$ROOT_MAIN/jar2" -X POST "$BASE/api/login" -d "{\"username\":\"alice\",\"password\":\"$APW\"}" >/dev/null
me2=$(curl -s -b "$ROOT_MAIN/jar2" "$BASE/api/me")
echo "$me2" | grep -q '"username":"alice"' && ok "RESTART: user survived (login works on cold process)" || no "user lost after restart: $me2"
echo "$me2" | grep -q '"is_admin":true' && ok "RESTART: is_admin survived + decodes from Postgres" || no "admin bit lost after restart"
# token survived: the pre-restart token still authenticates git smart-http
check "RESTART: pre-restart token still authenticates -> 200" "$(curl -s -o /dev/null -w '%{http_code}' -u "git:$T_ALICE_W" "$BASE/alice/secretproj.git/info/refs?service=git-upload-pack")" "200"
# agent + pushed repo survived: aid still resolves from the repo on disk
echo "$(curl -s -b "$ROOT_MAIN/jar2" "$BASE/api/agent/alice/secretproj")" | grep -q '"aid":"agt_11112222-3333-4444-5555-666677778888"' && ok "RESTART: agent + repo survived (aid resolves)" || no "agent/aid lost after restart"
# blob survived in Garage: GET the pre-restart blob back byte-identical
curl -s -u "git:$T_ALICE_W" -o "$WORKDIR/e2e-got2.bin" "$BASE/api/agent/alice/secretproj/blob/$SHA"
check "RESTART: pre-restart blob still in Garage (bytes match)" "$(sha256sum "$WORKDIR/e2e-got2.bin" | awk '{print $1}')" "$SHA"
# counts identical across the restart (nothing was in-memory-only)
check "RESTART: Postgres user count unchanged" "$(psqlq "SELECT count(*) FROM users")" "$U_BEFORE"
check "RESTART: Postgres agent count unchanged" "$(psqlq "SELECT count(*) FROM agents")" "$A_BEFORE"
check "RESTART: Postgres token count unchanged" "$(psqlq "SELECT count(*) FROM tokens")" "$T_BEFORE"
check "RESTART: Garage object count unchanged" "$(gcount)" "$G_BEFORE"

echo
echo "===== live evidence ====="
echo "  final Postgres users:  $(psqlq "SELECT string_agg(username||'(admin='||is_admin||')', ', ') FROM users")"
echo "  final Garage objects:  $(gcount)"
gls | sed 's/^/    /'
echo
echo "===== PASS=$PASS FAIL=$FAIL ====="
kill "$SRV_MAIN" 2>/dev/null
[ "$FAIL" -eq 0 ]
