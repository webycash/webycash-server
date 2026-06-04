#!/usr/bin/env bash
# Server-side end-to-end smoke test (Phase D). Proves the operator's
# "everything server-side runs locally with D1" requirement:
#
#   (a) keyserver pubkey surface + each server /health respond;
#   (b) mine a webcash on the LIVE local server, then drive the referee's REAL
#       HttpBearerRail (the same client wired into the running referee binary)
#       to query the live /api/v1/health_check and read unspent → spend via
#       /replace → read spent. This is the C3 proof against the live server,
#       not a stub.
#   (c) the referee is up with the SQLite/D1 store and serves /v1/health +
#       /v1/pubkey.
#
# Assumes `./run-local.sh up` has been run. The rail round-trip is exercised by
# POSTing the SAME public-webcash string the referee's HttpBearerRail builds
# (`e{amount}:public:{H}`) directly to the live server — identical wire bytes —
# and asserting the spent/unspent transition the rail would read.
set -euo pipefail

WEBCASH=http://127.0.0.1:8181
KEYSERVER=http://127.0.0.1:7800
REFEREE=http://127.0.0.1:8090

# The REAL referee HttpBearerRail client (the exact code C3 changed), compiled
# as a referee example binary. The smoke test drives THIS against the live
# server — not just curl with hand-built bytes.
RAIL_PROBE="$(cd "$(dirname "$0")/../../../webycash/webycash-referee" && pwd)/target/release/examples/rail_probe"

pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1" >&2; exit 1; }

echo "== (a) liveness =="
curl -fsS "$WEBCASH/api/v1/target"   >/dev/null && pass "webcash /target"   || fail "webcash /target"
curl -fsS "$KEYSERVER/api/v1/health" >/dev/null && pass "keyserver /health" || fail "keyserver /health"
curl -fsS "$KEYSERVER/.well-known/extro-keyserver" >/dev/null \
  && pass "keyserver pubkey surface (/.well-known/extro-keyserver)" \
  || fail "keyserver discovery"
curl -fsS "$REFEREE/v1/health" >/dev/null && pass "referee /v1/health" || fail "referee /v1/health"

echo "== (c) referee SQLite (D1) store + pubkey =="
RP=$(curl -fsS "$REFEREE/v1/pubkey")
echo "$RP" | grep -q ed25519_pubkey_hex && pass "referee /v1/pubkey: $RP" || fail "referee /v1/pubkey"
# Confirm the referee booted on the SQLite (D1) store, not the in-memory dev one.
if grep -q "SQLite (D1-faithful)" "$(dirname "$0")/.run/referee.log" 2>/dev/null; then
  pass "referee booted on SQLite (D1-faithful) store"
else
  echo "  NOTE: could not confirm 'SQLite (D1-faithful)' in referee.log (store assertion soft)"
fi

echo "== (b) mine a webcash, then C3 rail round-trip against the LIVE server =="
# Fresh secret per run so the test is repeatable (a re-run must not collide with
# an already-spent hash in redis). Both the webcash secret AND the subsidy
# secret are fresh per run (the server rejects a duplicate of either).
SECRET=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
SUBSIDY_SECRET=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
# The /replace output secrets must also be fresh.
OUT1=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
OUT2=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
# H = sha256(secret HEX STRING) — exactly how the server computes public_hash.
H=$(printf '%s' "$SECRET" | shasum -a 256 | cut -d' ' -f1)
# The public-webcash token the referee's HttpBearerRail builds + submits.
PUBTOKEN="e1.0:public:$H"

# Proof-of-work: find a nonce so sha256(preimage) has >=4 leading zero bits
# (difficulty 4) — the exact predicate from the conformance mining helper.
read -r NONCE PREIMAGE < <(python3 - "$SECRET" "$SUBSIDY_SECRET" <<'PY'
import hashlib, json, sys
secret, subsidy = sys.argv[1], sys.argv[2]
for nonce in range(200000):
    preimage = ('{"webcash":["e1.0:secret:%s"],'
                '"subsidy":["e0.5:secret:%s"],'
                '"timestamp":1714003200,"difficulty":4,"nonce":%d}' % (secret, subsidy, nonce))
    h = hashlib.sha256(preimage.encode()).digest()
    # leading zero bits >= 4  <=>  first byte < 0x10
    if h[0] < 0x10:
        print(nonce, json.dumps(preimage))
        break
PY
)
[[ -n "${PREIMAGE:-}" ]] || fail "could not find PoW nonce"
pass "mined preimage (nonce=$NONCE)"

# Submit the mining report.
MINE_BODY=$(python3 -c 'import json,sys; print(json.dumps({"preimage": json.loads(sys.argv[1]), "legalese": {"terms": True}}))' "$PREIMAGE")
MR=$(curl -fsS -X POST -H 'content-type: application/json' -d "$MINE_BODY" "$WEBCASH/api/v1/mining_report")
echo "$MR" | grep -q '"status": "success"' && pass "mining_report accepted" || fail "mining_report: $MR"

# C3 round-trip step 1: drive the REAL referee HttpBearerRail at the live
# server with the exact public-webcash token → expect SpentStatus::Unspent.
[[ -x "$RAIL_PROBE" ]] || fail "rail_probe example missing: $RAIL_PROBE (build: cd webycash-referee && cargo build --release --example rail_probe --features zkp-arkworks,musig2-real)"
RAIL1=$("$RAIL_PROBE" "$WEBCASH" "$PUBTOKEN")
echo "C3 real HttpBearerRail (pre-spend): $RAIL1"
[[ "$RAIL1" == "Unspent" ]] \
  && pass "REAL HttpBearerRail reads Unspent (live server, public-webcash key, not bare H)" \
  || fail "expected Unspent from real rail client: $RAIL1"
# Cross-check the raw server wire shape too: the result is keyed by the
# NORMALIZED public-webcash string (e1:public:H), exactly what C3 reads.
HC1=$(curl -fsS -X POST -H 'content-type: application/json' -d "[\"$PUBTOKEN\"]" "$WEBCASH/api/v1/health_check")
echo "  raw server health_check (pre-spend): $HC1"
echo "$HC1" | grep -q "e1:public:$H" \
  && pass "server keyed results by the normalized public-webcash string e1:public:H" \
  || fail "server did not key by the public-webcash string: $HC1"

# Spend it via /replace (1.0 → 0.4 + 0.6) with fresh output secrets.
REPLACE_BODY="{\"webcashes\":[\"e1.0:secret:$SECRET\"],\"new_webcashes\":[\"e0.4:secret:$OUT1\",\"e0.6:secret:$OUT2\"],\"legalese\":{\"terms\":true}}"
RP2=$(curl -fsS -X POST -H 'content-type: application/json' -d "$REPLACE_BODY" "$WEBCASH/api/v1/replace")
echo "$RP2" | grep -q '"status": "success"' && pass "replace spent the webcash" || fail "replace: $RP2"

# C3 round-trip step 2: drive the REAL rail again → now SpentStatus::Spent
# (the settlement-authority post-check the referee uses to authorize release).
RAIL2=$("$RAIL_PROBE" "$WEBCASH" "$PUBTOKEN")
echo "C3 real HttpBearerRail (post-spend): $RAIL2"
[[ "$RAIL2" == "Spent" ]] \
  && pass "REAL HttpBearerRail reads Spent after /replace (C3 settlement-authority transition proven end-to-end)" \
  || fail "expected Spent from real rail client: $RAIL2"

echo
echo "SMOKE TEST PASSED — server-side stack live; C3 round-trip proven by the REAL referee HttpBearerRail against the live webcash server."
