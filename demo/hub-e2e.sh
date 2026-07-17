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

echo "${B}Act 7 · the aid is the identity; the name is only a label${N}"
J="$TMP/jar"; JB="$TMP/jarb"
curl -s -c "$J"  -H 'content-type: application/json' -d '{"username":"alice","password":"pw-alice-123"}' -o /dev/null "$U/api/login"
curl -s -c "$JB" -H 'content-type: application/json' -d '{"username":"bob","password":"pw-bob-12345"}'   -o /dev/null "$U/api/login"
api(){   curl -s                          -b "$J" -H 'content-type: application/json' "$@"; }
acode(){ curl -s -o /dev/null -w '%{http_code}' -b "$J" -H 'content-type: application/json' "$@"; }
bapi(){  curl -s                          -b "$JB" -H 'content-type: application/json' "$@"; }
bcode(){ curl -s -o /dev/null -w '%{http_code}' -b "$JB" -H 'content-type: application/json' "$@"; }
is "alice gets a login session" "alice" "$(api "$U/api/me" | jq -r .username)"

# The aid is minted CLIENT-side and committed into the store; the hub only ever reads it.
AID="agt_e2e-payments-0001"
cd "$R1"
mkdir -p sessions/claude-code
cat > agent.toml <<EOF
[agent]
id = "$AID"
name = "payments"
EOF
cat > sessions/claude-code/s1.jsonl <<'EOF'
{"type":"user","cwd":"/home/user/proj","gitBranch":"main","message":{"content":"investigate the flaky checkout timeout"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"The retry loop in the billing client was the culprit."},{"type":"tool_use","name":"Edit","input":{"file_path":"src/checkout.rs"}}]}}
EOF
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm "identity + a session"
git push -q "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main 2>"$TMP/p3" || bad "pushing agent.toml failed: $(head -1 "$TMP/p3")"
cd "$ROOT"

d="$(api "$U/api/agent/payments")"
is "the hub reads the aid out of the store"   "$AID"       "$(echo "$d" | jq -r .aid)"
is "and says where it read it from"           "agent.toml" "$(echo "$d" | jq -r .aid_source)"
is "a first sighting is reported as learned"  "learned"    "$(echo "$d" | jq -r .aid_status)"
is "the second read comes off the cache"      "ok"         "$(api "$U/api/agent/payments" | jq -r .aid_status)"
grep -q "$AID" "$H/agents.json" && ok "the aid is stored in the agent metadata" || bad "agents.json never learned the aid"
is "by-aid resolves the identity to its name" "payments"   "$(api "$U/api/agent/by-aid/$AID" | jq -r .name)"

# The point of the whole exercise: renaming is a metadata edit, not a new agent.
r="$(api -X PATCH -d '{"name":"billing"}' "$U/api/agent/payments")"
is "rename reports the new name"              "billing"  "$(echo "$r" | jq -r .name)"
is "A RENAME MUST NOT CHANGE THE IDENTITY"    "$AID"     "$(echo "$r" | jq -r .aid)"
is "by-aid follows the rename"                "billing"  "$(api "$U/api/agent/by-aid/$AID" | jq -r .name)"
is "the old name stops resolving"             404        "$(acode "$U/api/agent/payments")"
is "and tokens follow the rename too"         200        "$(code -u "x:$WTOK" "$U/billing.git/info/refs?service=git-upload-pack")"
api -X PATCH -d '{"name":"payments"}' "$U/api/agent/billing" >/dev/null
is "renamed back, same identity throughout"   "payments" "$(api "$U/api/agent/by-aid/$AID" | jq -r .name)"

is "an unknown aid is a 404"                  404 "$(acode "$U/api/agent/by-aid/agt_nobody-has-this")"
is "a malformed aid is refused outright"      400 "$(acode "$U/api/agent/by-aid/not-an-aid")"
is "by-aid hands anonymous nothing"           401 "$(code "$U/api/agent/by-aid/$AID")"
is "by-aid is not an oracle for strangers"    404 "$(bcode "$U/api/agent/by-aid/$AID")"

# Two agents may never share one aid: push the same store under a second name.
api -X POST -d '{"name":"payments-copy"}' "$U/api/agents" >/dev/null
CTOK="$(api -X POST -d '{"name":"copy","scope":"write","agent":"payments-copy"}' "$U/api/tokens" | jq -r .token)"
cd "$R1"; git push -q "http://x:$CTOK@127.0.0.1:$PORT/payments-copy.git" main 2>/dev/null; cd "$ROOT"
is "a second store claiming a taken aid is flagged" "conflict" "$(api "$U/api/agent/payments-copy" | jq -r .aid_status)"
is "and the aid still resolves to the holder"       "payments" "$(api "$U/api/agent/by-aid/$AID" | jq -r .name)"
grep -q "agent.aid.conflict" "$A" && ok "the aid conflict is audited" || bad "no agent.aid.conflict row"

echo "${B}Act 8 · merge requests${N}"
api -X POST -d '{"name":"feature-work"}' "$U/api/agents" >/dev/null
mr="$(api -X POST -d '{"title":"reconcile the checkout memory","source":"feature-work","dialogue_transcript":"claude-code: the retry loop is the cause\ncodex: agreed, and the 30s timeout is the real bug"}' "$U/api/agent/payments/mrs")"
is "an MR opens against the target"             1              "$(echo "$mr" | jq -r .id)"
is "it records who opened it"                   "alice"        "$(echo "$mr" | jq -r .author)"
is "it starts open"                             "open"         "$(echo "$mr" | jq -r .state)"
is "it names the source agent"                  "feature-work" "$(echo "$mr" | jq -r .source.agent)"
is "and snapshots the target's IDENTITY, not just its name" "$AID" "$(echo "$mr" | jq -r .target.aid)"
api "$U/api/agent/payments/mrs/1" | jq -r .dialogue_transcript | grep -q "the retry loop is the cause" \
  && ok "the MR carries the dialogue transcript for review" || bad "the transcript is missing from the detail view"
is "the list shows it"                          1 "$(api "$U/api/agent/payments/mrs" | jq '.mrs | length')"
is "a comment lands"                            1 "$(api -X POST -d '{"body":"agreed — the 30s timeout is the real bug"}' "$U/api/agent/payments/mrs/1/comments" | jq -r .id)"
is "and shows up on the MR"                     1 "$(api "$U/api/agent/payments/mrs/1" | jq '.comments | length')"

# Every MR route goes through acl::decide, on the TARGET agent.
is "a stranger cannot list another agent's MRs" 404 "$(bcode "$U/api/agent/payments/mrs")"
is "a stranger cannot open one"                 404 "$(bcode -X POST -d '{"title":"x","source":"other"}' "$U/api/agent/payments/mrs")"
is "a READ token cannot open an MR"             403 "$(curl -s -o /dev/null -w '%{http_code}' -u "x:$RTOK" -H 'content-type: application/json' -X POST -d '{"title":"x","source":"feature-work"}' "$U/api/agent/payments/mrs")"
is "a read token can still read them"           200 "$(curl -s -o /dev/null -w '%{http_code}' -u "x:$RTOK" "$U/api/agent/payments/mrs")"
is "an MR against itself is refused"            400 "$(acode -X POST -d '{"title":"x","source":"payments"}' "$U/api/agent/payments/mrs")"
is "an unreadable source cannot be proposed"    404 "$(acode -X POST -d '{"title":"x","source":"other"}' "$U/api/agent/payments/mrs")"

api -X POST -d '{"title":"second opinion","source":"feature-work"}' "$U/api/agent/payments/mrs" >/dev/null
is "the next MR gets the next id"               2 "$(api "$U/api/agent/payments/mrs" | jq '[.mrs[].id] | max')"
is "a bogus state is refused"                   400 "$(acode -X POST -d '{"state":"reopened"}' "$U/api/agent/payments/mrs/2/close")"
is "closing works"                              "closed" "$(api -X POST -d '{}' "$U/api/agent/payments/mrs/2/close" | jq -r .state)"
is "a merge can be RECORDED (the hub runs none)" "merged" "$(api -X POST -d '{"state":"merged"}' "$U/api/agent/payments/mrs/1/close" | jq -r .state)"
is "a settled MR takes no more comments"        409 "$(acode -X POST -d '{"body":"late"}' "$U/api/agent/payments/mrs/1/comments")"
is "and cannot be closed twice"                 409 "$(acode -X POST -d '{}' "$U/api/agent/payments/mrs/1/close")"
is "an unknown MR is a 404"                     404 "$(acode "$U/api/agent/payments/mrs/99")"
grep -q '"action":"mr.open"'   "$A" && ok "opening an MR is audited"      || bad "no mr.open row"
grep -q '"action":"mr.merged"' "$A" && ok "recording a merge is audited"  || bad "no mr.merged row"

api -X PATCH -d '{"name":"billing"}' "$U/api/agent/payments" >/dev/null
is "MRs follow their agent across a rename" 2 "$(api "$U/api/agent/billing/mrs" | jq '.mrs | length')"
api -X PATCH -d '{"name":"payments"}' "$U/api/agent/billing" >/dev/null

echo "${B}Act 8b · an MR must not leak the private side it came from${N}"
# alice owns both. The MR is opened INTO a public agent, so its audience is everyone — and the
# opener's permission on the source is not the audience's. Existence is itself a secret.
api -X POST -d '{"name":"pubagent"}'  "$U/api/agents" >/dev/null
api -X POST -d '{"name":"privagent"}' "$U/api/agents" >/dev/null
api -X PATCH -d '{"visibility":"public"}' "$U/api/agent/pubagent" >/dev/null
is "pubagent is public"      "public"  "$(api "$U/api/agent/pubagent"  | jq -r .visibility)"
is "privagent stays private" "private" "$(api "$U/api/agent/privagent" | jq -r .visibility)"

# Give privagent a REAL identity to leak. Without this the aid checks below pass whether or not the
# redaction works — an aid that was never learned is null either way, which is a green that means
# nothing.
PAID="agt_e2e-privagent-0002"
PTOK="$(api -X POST -d '{"name":"ptok","scope":"write","agent":"privagent"}' "$U/api/tokens" | jq -r .token)"
[[ ${#PTOK} -ge 32 ]] && ok "a write token for privagent" || bad "no privagent token parsed"
R2="$TMP/privrepo"; mkdir -p "$R2"; cd "$R2"; git init -q -b main .
printf '[agent]\nid = "%s"\nname = "privagent"\n' "$PAID" > agent.toml
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm "identity"
git push -q "http://x:$PTOK@127.0.0.1:$PORT/privagent.git" main 2>"$TMP/pp" \
  || bad "could not push privagent's store: $(head -1 "$TMP/pp")"
cd "$ROOT"
is "privagent has a real aid to leak" "$PAID" "$(api "$U/api/agent/privagent" | jq -r .aid)"

SECRETLINE="the internal rotation schedule is quarterly"
p="$(api -X POST -d "{\"title\":\"from the private side\",\"source\":\"privagent\",\"dialogue_transcript\":\"privagent said: $SECRETLINE\"}" "$U/api/agent/pubagent/mrs")"
is "the MR opens against the public target"        1           "$(echo "$p" | jq -r .id)"
is "and alice, who can read both, sees the source" "privagent" "$(api "$U/api/agent/pubagent/mrs" | jq -r '.mrs[0].source.agent')"
# The MR really does carry the private aid — so a null for bob below is a redaction, not an absence.
is "the MR snapshotted the private aid"            "$PAID"     "$(api "$U/api/agent/pubagent/mrs" | jq -r '.mrs[0].source.aid')"

# bob has no grant on privagent whatsoever: his agent list does not even contain it.
is "bob cannot reach the private agent directly" 404 "$(bcode "$U/api/agent/privagent")"
bapi "$U/api/agents" | jq -r '.agents[].name' | grep -qx privagent \
  && bad "privagent is in bob's agent list" || ok "privagent is not in bob's agent list"
is "BOB MUST NOT SEE THE PRIVATE SOURCE'S NAME" "null" "$(bapi "$U/api/agent/pubagent/mrs" | jq -r '.mrs[0].source.agent')"
is "nor its aid"                                "null" "$(bapi "$U/api/agent/pubagent/mrs" | jq -r '.mrs[0].source.aid')"
is "nor its ref"                                "null" "$(bapi "$U/api/agent/pubagent/mrs" | jq -r '.mrs[0].source.ref')"
is "and the redaction is declared, not silent"  "true" "$(bapi "$U/api/agent/pubagent/mrs" | jq -r '.mrs[0].source.redacted')"
is "anonymous is told no more than bob"         "null" "$(curl -s "$U/api/agent/pubagent/mrs" | jq -r '.mrs[0].source.agent')"
is "the DETAIL view redacts it too"             "null" "$(bapi "$U/api/agent/pubagent/mrs/1" | jq -r '.source.agent')"
is "the TARGET side is not redacted — bob may read that one" "pubagent" "$(bapi "$U/api/agent/pubagent/mrs/1" | jq -r '.target.agent')"
# The transcript is the dialogue BETWEEN the two sides, so it quotes the private one by construction.
is "the transcript goes with the source"            "null" "$(bapi "$U/api/agent/pubagent/mrs/1" | jq -r '.dialogue_transcript')"
is "and says so, rather than pretending there is none" "true" "$(bapi "$U/api/agent/pubagent/mrs/1" | jq -r '.transcript_redacted')"
bapi "$U/api/agent/pubagent/mrs/1" | grep -q "$SECRETLINE" \
  && bad "THE PRIVATE SIDE'S WORDS LEAKED TO BOB THROUGH THE TRANSCRIPT" || ok "the private side's words never reach bob"
curl -s "$U/api/agent/pubagent/mrs/1" | grep -q "$SECRETLINE" \
  && bad "THE TRANSCRIPT LEAKED TO ANONYMOUS" || ok "nor anonymous"
is "the list view withholds it too" "false" "$(bapi "$U/api/agent/pubagent/mrs" | jq -r '.mrs[0].has_transcript')"
# ...and the redaction must not eat the transcript of someone who may read it.
is "alice's own view is untouched" "false" "$(api "$U/api/agent/pubagent/mrs/1" | jq -r '.transcript_redacted')"
api "$U/api/agent/pubagent/mrs/1" | jq -r .dialogue_transcript | grep -q "$SECRETLINE" \
  && ok "and her transcript is intact" || bad "the redaction ate the owner's own transcript"

echo "${B}Act 8c · a comment is a WRITE of hub state, whichever tier let you in${N}"
# Commenting is gated at Read on purpose — anyone who may read a review may join it. That is a
# statement about WHO may take part, not a licence to mutate hub state anonymously or on a read token.
is "an ANONYMOUS comment on a public agent's MR is refused" 401 \
  "$(code -X POST -H 'content-type: application/json' -d '{"body":"anon was here"}' "$U/api/agent/pubagent/mrs/1/comments")"
is "and it left nothing behind" 0 "$(api "$U/api/agent/pubagent/mrs/1" | jq '.comments | length')"
grep -q '"actor":"","action":"mr.comment"' "$A" \
  && bad "an unauthenticated mutation is attributed to nobody" || ok "no hub mutation is authored by the empty string"

o="$(api -X POST -d '{"title":"third opinion","source":"feature-work"}' "$U/api/agent/payments/mrs")"
MR3="$(echo "$o" | jq -r .id)"
is "a fresh OPEN MR to comment on" 3 "$MR3"
# acl.rs: a read-only token in an admin's hands still only reads — intersection, not maximum.
is "a READ token cannot comment" 403 \
  "$(code -u "x:$RTOK" -H 'content-type: application/json' -X POST -d '{"body":"read token wrote this"}' "$U/api/agent/payments/mrs/$MR3/comments")"
is "and the write token still can (not a blanket deny)" 201 \
  "$(code -u "x:$WTOK" -H 'content-type: application/json' -X POST -d '{"body":"write token comment"}' "$U/api/agent/payments/mrs/$MR3/comments")"

# The capability that is the whole reason the route is gated at Read rather than at Write.
api -X POST -d '{"name":"reviewclub"}' "$U/api/agents" >/dev/null
api -X POST -d '{"username":"bob","role":"read"}' "$U/api/agent/reviewclub/members" >/dev/null
api -X POST -d '{"title":"for review","source":"feature-work"}' "$U/api/agent/reviewclub/mrs" >/dev/null
is "a READ MEMBER can still comment on what he may read" 201 \
  "$(bcode -X POST -d '{"body":"bob reviews"}' "$U/api/agent/reviewclub/mrs/1/comments")"
is "but a read member still cannot OPEN one" 403 \
  "$(bcode -X POST -d '{"title":"nope","source":"feature-work"}' "$U/api/agent/reviewclub/mrs")"

echo "${B}Act 8d · a comment thread has a ceiling${N}"
# One comment was bounded at 8KiB; how MANY was not — and update_mrs re-serializes the whole of
# mrs.json per comment, so an unbounded thread is quadratic on disk as well as unbounded on it.
# Charged to a session, which the token rate-limiter does not touch: every 429 below is COMMENTS_MAX.
urls=(); for _ in $(seq 1 520); do urls+=("$U/api/agent/reviewclub/mrs/1/comments"); done
out="$(curl -s -o /dev/null -w '%{http_code}\n' -b "$J" -H 'content-type: application/json' -X POST -d '{"body":"flood"}' "${urls[@]}")"
n201="$(echo "$out" | grep -c 201)"; n429="$(echo "$out" | grep -c 429)"
[[ "$n201" -gt 0 ]] && ok "comments are accepted up to the ceiling ($n201 × 201)" || bad "not one comment was accepted"
[[ "$n429" -gt 0 ]] && ok "and past it they are refused ($n429 × 429)"            || bad "520 comments on one MR were never refused"
is "the thread stops exactly at COMMENTS_MAX" 500 "$(api "$U/api/agent/reviewclub/mrs/1" | jq '.comments | length')"

echo "${B}Act 9 · the server-side secret gate${N}"
before="$(git ls-remote "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main | awk '{print $1}')"
[[ -x "$H/payments.git/hooks/pre-receive" ]] && ok "a pre-receive hook is installed server-side" || bad "no pre-receive hook in the bare repo"
cd "$R1"
echo 'aws_key = "AKIAIOSFODNN7EXAMPLE"' > leak.txt
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm "oops"
if git push "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main >"$TMP/s1" 2>&1; then
  bad "SECURITY: a push carrying an AWS key was accepted"
else
  ok "a push carrying a secret is refused"
fi
grep -qi "aws-access-key-id" "$TMP/s1" && ok "and the refusal names the rule that fired" || bad "the rejection never said what it found"
grep -qi "leak.txt" "$TMP/s1" && ok "and the file it is in" || bad "the rejection never said where"
# --no-verify skips the CLIENT hook. That is exactly why the gate cannot live there.
if git push --no-verify "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main >"$TMP/s2" 2>&1; then
  bad "SECURITY: --no-verify walked the secret straight past the server"
else
  ok "--no-verify does not reach the server-side gate"
fi
cd "$ROOT"
is "the refused push moved no ref" "$before" "$(git ls-remote "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main | awk '{print $1}')"
grep -q "git.push.rejected" "$A" && ok "the refused push is audited" || bad "no git.push.rejected row"
# ...and the gate must not be a blanket deny.
cd "$R1"
git reset -q --hard "$before"
echo "a clean note" > ok.txt
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm "clean"
if git push -q "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main 2>"$TMP/s3"; then
  ok "a clean push still goes through"
else
  bad "the gate refused a CLEAN push: $(head -3 "$TMP/s3")"
fi
cd "$ROOT"

# Every check below pushes from a known-good ref. An empty capture here would make them vacuous.
clean="$(git ls-remote "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main | awk '{print $1}')"
[[ ${#clean} -eq 40 ]] && ok "the clean ref is readable, so the checks below are real" \
                       || bad "could not read the clean ref ('$clean') — everything below would be vacuous"

echo "${B}Act 9b · ONE NUL BYTE used to be the entire gate${N}"
cd "$R1"
git reset -q --hard "$clean"
# The identical file WITHOUT the leading NUL is refused (Act 9). With it, the blob was skipped whole,
# `truncated` was never set, the hook printed nothing and exited 0 — and the key went live.
printf '\000' > bin.dat
echo 'aws_access_key_id = AKIAIOSFODNN7EXAMPLE' >> bin.dat
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm "nul-prefixed key"
if git push "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main >"$TMP/n1" 2>&1; then
  bad "SECURITY: one NUL byte walked a live AWS key straight past the gate"
else
  ok "a NUL-prefixed AWS key is REFUSED (the strings pass reads binary now)"
fi
grep -qi "aws-access-key-id" "$TMP/n1" && ok "and the refusal names the rule that fired" || bad "the NUL rejection never named the rule"
grep -q  "bin.dat"           "$TMP/n1" && ok "and the file it is in"                      || bad "the NUL rejection never named the file"
cd "$ROOT"
is "and no ref moved" "$clean" "$(git ls-remote "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main | awk '{print $1}')"
# The server is the only real gate, so --no-verify must not help here either.
cd "$R1"
git push --no-verify "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main >"$TMP/n2" 2>&1 \
  && bad "SECURITY: --no-verify walked the NUL-hidden key past the server" || ok "--no-verify does not reach it"
cd "$ROOT"

echo "${B}Act 9c · the scan FAILS CLOSED on its own bounds${N}"
cd "$R1"
git reset -q --hard "$clean"
# Past the per-blob bound the scan cannot read the blob at all. It used to skip it and ACCEPT the
# push, warning about a bound that had not fired and naming no file.
head -c 17000000 /dev/zero | tr '\0' 'A' > huge.txt
echo 'aws_access_key_id = AKIAIOSFODNN7EXAMPLE' >> huge.txt
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm "a key hidden past the scan bound"
if git push "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main >"$TMP/o1" 2>&1; then
  bad "SECURITY: a key past the per-blob bound was accepted — the gate cleared what it never read"
else
  ok "a blob the scan could not read is REFUSED, not waved through"
fi
grep -q "NOT SCANNED" "$TMP/o1" && ok "and it says the push was not scanned in full"  || bad "the refusal never admitted the scan was incomplete"
grep -q "huge.txt"    "$TMP/o1" && ok "and names the offending file"                  || bad "the refusal never named the file"
grep -q "16777216"    "$TMP/o1" && ok "and the REAL bound that fired (per-blob)"      || bad "the refusal never named the real bound"
grep -q "2000 blobs"  "$TMP/o1" && bad "it still blames a bound that never fired"     || ok "and does not blame a bound that never fired"
cd "$ROOT"
is "the unscannable push moved no ref" "$clean" "$(git ls-remote "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main | awk '{print $1}')"
grep -q "unscanned huge.txt" "$A" && ok "and it is audited, naming the file" || bad "no audit row naming the unscanned file"

# Fail-closed has to stay OPERABLE, or it is just an outage with a security story.
echo "huge.txt" > "$H/payments.git/.agit-scan-skip"
cd "$R1"
if git push -q "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main 2>"$TMP/o2"; then
  ok "the documented escape hatch (.agit-scan-skip) lets a judged path through"
else
  bad "fail-closed with no way out: $(head -3 "$TMP/o2")"
fi
cd "$ROOT"
rm -f "$H/payments.git/.agit-scan-skip"
clean2="$(git ls-remote "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main | awk '{print $1}')"
[[ ${#clean2} -eq 40 && "$clean2" != "$clean" ]] && ok "and that push really did move the ref" \
                                                 || bad "the escape-hatch push did not move the ref ('$clean2')"

echo "${B}Act 9d · padding a key past the OLD bound no longer hides it${N}"
cd "$R1"
git reset -q --hard "$clean2"
# 1.2MB of padding used to carry this blob past the old 1MiB per-blob bound; the key went live. The
# bound is now generous enough that the blob is SCANNED — so the key itself is what refuses the push.
{ echo 'aws_access_key_id = AKIAIOSFODNN7EXAMPLE'; head -c 1200000 /dev/zero | tr '\0' 'A'; } > padded.txt
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm "padded key"
if git push "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main >"$TMP/d1" 2>&1; then
  bad "SECURITY: 1.2MB of padding walked an AWS key past the gate"
else
  ok "a 1.2MB-padded AWS key is REFUSED"
fi
grep -qi "aws-access-key-id" "$TMP/d1" && ok "and the KEY is what was found — the blob was scanned, not skipped" \
                                       || bad "the padded push was refused, but not for the key"
grep -q "padded.txt" "$TMP/d1" && ok "and the file is named" || bad "the padded rejection never named the file"
cd "$ROOT"

echo "${B}Act 9e · fail-closed must not break ordinary work${N}"
cd "$R1"
git reset -q --hard "$clean2"
# A real binary asset with no secret in it. Deterministic on purpose — a security suite must not flake.
# The 0..255 cycle is genuinely binary (NUL-bearing) and carries a 95-char printable run per cycle,
# so the strings pass has plenty to chew on and must still find nothing.
mkdir -p assets
python3 -c "import sys; sys.stdout.buffer.write(bytes(range(256))*8000)" > assets/blob.bin
printf '\211PNG\r\n\032\n' > logo.png
python3 -c "import sys; sys.stdout.buffer.write(bytes(range(256))*400)" >> logo.png
echo "a perfectly ordinary note" > note.txt
is "the binary asset is a real 2MB one" 2048000 "$(stat -c '%s' assets/blob.bin)"
git -c user.name=a -c user.email=a@e.com add -A
git -c user.name=a -c user.email=a@e.com commit -qm "a real binary asset and a note"
if git push -q "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main 2>"$TMP/c1"; then
  ok "a clean push carrying real binary files still goes through"
else
  bad "the hardened gate refused a CLEAN push: $(head -5 "$TMP/c1")"
fi
newhead="$(git rev-parse HEAD)"
cd "$ROOT"
is "and the clean push really moved the ref" "$newhead" "$(git ls-remote "http://x:$WTOK@127.0.0.1:$PORT/payments.git" main | awk '{print $1}')"

echo "${B}Act 10 · cross-agent search${N}"
s="$(api "$U/api/search?q=checkout+timeout")"
n="$(echo "$s" | jq '.hits | length')"
[[ "$n" -ge 1 ]] && ok "search finds the session ($n hit(s))" || bad "search found nothing"
echo "$s" | jq -r '.hits[].agent' | grep -qx payments && ok "and names the agent it came from" || bad "payments is not among the hits"
echo "$s" | jq -r '.hits[0].matched[]' | grep -q prompt && ok "it says the match was in a prompt" || bad "no field attribution on the hit"
is "a file match is found too"           "payments" "$(api "$U/api/search?q=checkout.rs" | jq -r '.hits[0].agent')"
is "the scan bound is reported honestly" "false"    "$(echo "$s" | jq -r .scan_capped)"
is "SEARCH NEVER CROSSES AN ACL"         0          "$(bapi "$U/api/search?q=checkout+timeout" | jq '.hits | length')"
is "anonymous search sees nothing"       0          "$(curl -s "$U/api/search?q=checkout+timeout" | jq '.hits | length')"
is "a one-character query is refused"    400        "$(acode "$U/api/search?q=x")"
# the per-agent search reports its own bound rather than passing a capped count off as a total
api "$U/api/agent/payments?q=checkout" | jq -e 'has("scan_capped") and has("scanned")' >/dev/null \
  && ok "the per-agent search reports what it scanned" || bad "no scan bound reported"

echo "${B}Act 11 · a token has its own budget (not the per-IP cap)${N}"
# Burst past the token's allowance in one curl invocation (one connection, many requests).
urls=(); for _ in $(seq 1 300); do urls+=("$U/api/agent/payments"); done
out="$(curl -s -o /dev/null -w '%{http_code}\n' -u "x:$RTOK" "${urls[@]}")"
n429="$(echo "$out" | grep -c 429)"
n200="$(echo "$out" | grep -c 200)"
[[ "$n200" -gt 0 ]]  && ok "the token's burst is served ($n200 × 200)"       || bad "the token was throttled from the very first request"
[[ "$n429" -gt 0 ]]  && ok "and past its budget it is refused ($n429 × 429)" || bad "300 requests on one token were never throttled"
# The budget is charged to the CREDENTIAL: the same address on a session must be unaffected.
is "the same address on a session is unaffected" "alice" "$(api "$U/api/me" | jq -r .username)"
is "and a different token has its own budget"    200     "$(code -u "x:$WTOK" "$U/payments.git/info/refs?service=git-upload-pack")"

echo
if [[ $FAIL -eq 0 ]]; then echo "${G}${B}hub e2e: $PASS checks passed.${N}"; else echo "${R}${B}hub e2e: $FAIL failed, $PASS passed.${N}"; fi
exit $FAIL
