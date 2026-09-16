#!/usr/bin/env bash
# Gate 4: prepare handshake → initiate against live local stack (regtest).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$HERE/../../.." && pwd)"
REFEREE_REPO="$WORKSPACE/webycash/webycash-referee"
KEYSERVER_REPO="$WORKSPACE/extro/extro-keyserver"
NODE_REPO="$WORKSPACE/extro/extro-node"
OUTDIR="${GATE4_OUT:-/tmp/gate4-prepare}"
mkdir -p "$OUTDIR"

REFEREE="${REFEREE_URL:-http://127.0.0.1:8090}"
KEYSERVER="${KEYSERVER_URL:-http://127.0.0.1:7800}"
WEBCASH="${WEBCASH_URL:-http://127.0.0.1:8181}"

ARK_SIGNER="${ARK_SIGNER_PK_HEX:-e35799157be4b37565bb5afe4d04e6a0fa0a4b6a4f4e48b0d904685d253cdbdb}"
ARK_DIGEST="${ARK_INFO_DIGEST_HEX:-ff60fc5daff9965857bd2b906833342a3f53866c03b645a170829720078e7fbd}"
ARK_DELAY="${ARK_UNILATERAL_EXIT_DELAY:-5}"

PROVIDER_SK="${PROVIDER_SK_HEX:-$(python3 -c 'print("31"*32)')}"
SELLER_SK="${SELLER_SK_HEX:-$(python3 -c 'print("32"*32)')}"

PREPARE_BIN="$REFEREE_REPO/target/release/examples/swap_prepare_handshake"
SEED_BIN="$KEYSERVER_REPO/target/release/examples/seed_dev_f1_identity"
INITIATE_BIN="$NODE_REPO/target/release/examples/swap_initiate_request"
PROVIDER_MAT="$REFEREE_REPO/target/release/examples/provider_material"

pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1" >&2; exit 1; }
note() { echo "  NOTE: $1"; }

[[ -x "$PREPARE_BIN" ]] || fail "missing $PREPARE_BIN"
[[ -x "$SEED_BIN" ]] || fail "missing $SEED_BIN"
[[ -x "$INITIATE_BIN" ]] || fail "missing $INITIATE_BIN"
[[ -x "$PROVIDER_MAT" ]] || fail "missing $PROVIDER_MAT"

echo "== (0) liveness =="
curl -fsS "$REFEREE/v1/health" >/dev/null && pass "referee health" || fail "referee"
curl -fsS "$KEYSERVER/api/v1/health" >/dev/null && pass "keyserver health" || fail "keyserver"
curl -fsS "$WEBCASH/api/v1/target" >/dev/null && pass "webcash target" || fail "webcash"

echo "== (1) F1 identity register (local cache) =="
seed_one() {
  local nick="$1" sk="$2" out_fp code
  curl -sS -o "$OUTDIR/challenge-$nick.bin" -w '%{http_code}' -X POST "$KEYSERVER/api/v1/identity/register" --data-binary '' | grep -q 402 \
    || fail "challenge for $nick"
  out_fp=$("$SEED_BIN" "$nick" "$sk" "$OUTDIR/challenge-$nick.bin" "$OUTDIR/retry-$nick.bin")
  code=$(curl -sS -o "$OUTDIR/reg-$nick.bin" -w '%{http_code}' -X POST \
    -H 'content-type: application/octet-stream' \
    --data-binary @"$OUTDIR/retry-$nick.bin" \
    "$KEYSERVER/api/v1/identity/register")
  [[ "$code" == "200" ]] || fail "register $nick HTTP $code: $(cat "$OUTDIR/reg-$nick.bin")"
  code=$(curl -sS -o "$OUTDIR/record-$nick.bin" -w '%{http_code}' "$KEYSERVER/api/v1/identity/record/$out_fp")
  [[ "$code" == "200" ]] || fail "record $nick HTTP $code"
  echo "$out_fp"
}
NICK_SUFFIX=$(date +%s)
PROVIDER_FP=$(seed_one "provider-$NICK_SUFFIX" "$PROVIDER_SK")
SELLER_FP=$(seed_one "seller-$NICK_SUFFIX" "$SELLER_SK")
pass "provider fp=$PROVIDER_FP record=$(wc -c < "$OUTDIR/record-provider-$NICK_SUFFIX.bin")B"
pass "seller fp=$SELLER_FP record=$(wc -c < "$OUTDIR/record-seller-$NICK_SUFFIX.bin")B"

echo "== (2) provider MuSig2 material =="
"$PROVIDER_MAT" > "$OUTDIR/provider_material.txt"
PUB=$(sed -n '1p' "$OUTDIR/provider_material.txt")
SETTLE_N=$(sed -n '2p' "$OUTDIR/provider_material.txt")
REFUND_N=$(sed -n '3p' "$OUTDIR/provider_material.txt")
pass "provider musig2 pub=${PUB:0:18}…"

echo "== (3) POST /v1/swap/prepare =="
REFEREE_URL="$REFEREE" \
ARK_NETWORK=regtest \
ARK_SIGNER_PK_HEX="$ARK_SIGNER" \
ARK_INFO_DIGEST_HEX="$ARK_DIGEST" \
ARK_UNILATERAL_EXIT_DELAY="$ARK_DELAY" \
ARK_AMOUNT_SATS=25000 \
PROVIDER_SK_HEX="$PROVIDER_SK" \
SELLER_SK_HEX="$SELLER_SK" \
PROVIDER_MUSIG2_PUBKEY="$PUB" \
PROVIDER_SETTLE_NONCE="$SETTLE_N" \
PROVIDER_REFUND_NONCE="$REFUND_N" \
FILL_AMOUNT_RAW=1 \
  "$PREPARE_BIN" > "$OUTDIR/prepare.out" 2>"$OUTDIR/prepare.err" || {
    cat "$OUTDIR/prepare.err" >&2
    fail "prepare binary failed"
  }
PREPARED_SWAP_ID=$(sed -n '1p' "$OUTDIR/prepare.out")
PREPARE_COMMITMENT=$(sed -n '2p' "$OUTDIR/prepare.out")
PREPARE_EXPIRY=$(sed -n '3p' "$OUTDIR/prepare.out")
PREPARE_IDEMP=$(sed -n '4p' "$OUTDIR/prepare.out")
[[ -n "$PREPARED_SWAP_ID" && -n "$PREPARE_COMMITMENT" && -n "$PREPARE_IDEMP" ]] || fail "prepare output incomplete: $(cat "$OUTDIR/prepare.out" "$OUTDIR/prepare.err")"
pass "prepared_swap_id=$PREPARED_SWAP_ID"
pass "prepare_commitment=$PREPARE_COMMITMENT"
pass "expires_at_unix=$PREPARE_EXPIRY"
pass "prepare_idempotency=$PREPARE_IDEMP"
printf '%s\n' "$PREPARED_SWAP_ID" > "$OUTDIR/prepared_swap_id.txt"
printf '%s\n' "$PREPARE_COMMITMENT" > "$OUTDIR/prepare_commitment.txt"
printf '%s\n' "$PREPARE_IDEMP" > "$OUTDIR/prepare_idempotency.txt"

echo "== (4) build prepare-bound initiate =="
LOCKED_REF="${EXTRO_LOCKED_REF:-26732faeb5278819523bf7df06b1d66961d76487161a48373ba18bd191a8a799:0}"
SETTLE_HASH="${EXTRO_TX_SETTLE_HASH_HEX:-ecc25b6f0c01284d7835b90b7a7226eb1d27a584c34da8a2e1ff618eccfa9fa9}"
REFUND_HASH="${EXTRO_TX_REFUND_HASH_HEX:-7081a4a22db95c2c803811d4987437b2e445e83633ec79458c923307c8f726d5}"
ORDER_ID="$(python3 -c 'print("00"*16 + "11"*16)')"
SCALAR="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
echo "$SCALAR" > "$OUTDIR/scalar.txt"
H=$(python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())' "$SCALAR")
echo "$H" > "$OUTDIR/bearer_h_expected.txt"

# EXTRO_PARTY_SK_HEX required as base; PROVIDER/SELLER override dual fps
env -u EXTRON_DATA_DIR -u EXTRON_PASSPHRASE \
  EXTRO_PARTY_SK_HEX="$PROVIDER_SK" \
  EXTRO_PROVIDER_SK_HEX="$PROVIDER_SK" \
  EXTRO_SELLER_SK_HEX="$SELLER_SK" \
  EXTRO_PREPARED_SWAP_ID="$PREPARED_SWAP_ID" \
  EXTRO_PREPARE_REQUEST_COMMITMENT_HEX="$PREPARE_COMMITMENT" \
  EXTRO_IDEMPOTENCY_KEY_HEX="$PREPARE_IDEMP" \
  EXTRO_LOCKED_REF="$LOCKED_REF" \
  EXTRO_TX_SETTLE_HASH_HEX="$SETTLE_HASH" \
  EXTRO_TX_REFUND_HASH_HEX="$REFUND_HASH" \
  EXTRO_PROVIDER_MUSIG2_PUBKEY="$PUB" \
  EXTRO_PROVIDER_SETTLE_NONCE="$SETTLE_N" \
  EXTRO_PROVIDER_REFUND_NONCE="$REFUND_N" \
  "$INITIATE_BIN" "$OUTDIR/initiate.bin" "$SCALAR" "$ORDER_ID" 1 \
  > "$OUTDIR/bearer_h.txt" 2>"$OUTDIR/initiate-build.err" || {
    cat "$OUTDIR/initiate-build.err" >&2
    fail "initiate build failed"
  }
GOT_H=$(tr -d '[:space:]' < "$OUTDIR/bearer_h.txt")
[[ "$GOT_H" == "$H" ]] || fail "H mismatch expected=$H got=$GOT_H ($(cat "$OUTDIR/initiate-build.err"))"
pass "initiate envelope $(wc -c < "$OUTDIR/initiate.bin") bytes H=$H"

echo "== (5) mine bearer webcash =="
SUBSIDY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
read -r NONCE PREIMAGE < <(python3 - "$SCALAR" "$SUBSIDY" <<'PY'
import hashlib, json, sys
secret, subsidy = sys.argv[1], sys.argv[2]
for nonce in range(2_000_000):
    preimage = ('{"webcash":["e1.0:secret:%s"],"subsidy":["e0.5:secret:%s"],"timestamp":1714003200,"difficulty":4,"nonce":%d}' % (secret, subsidy, nonce))
    if hashlib.sha256(preimage.encode()).digest()[0] < 0x10:
        print(nonce, json.dumps(preimage)); break
else:
    raise SystemExit('mine failed')
PY
)
MINE_BODY=$(python3 -c 'import json,sys; print(json.dumps({"preimage": json.loads(sys.argv[1]), "legalese": {"terms": True}}))' "$PREIMAGE")
curl -fsS -X POST -H 'content-type: application/json' -d "$MINE_BODY" "$WEBCASH/api/v1/mining_report" >/dev/null
BEFORE=$(curl -fsS -X POST -H 'content-type: application/json' -d "[\"e1.0:public:$H\"]" "$WEBCASH/api/v1/health_check")
echo "$BEFORE" > "$OUTDIR/webcash_before.json"
echo "$BEFORE" | grep -q '"spent": false' || fail "before not unspent: $BEFORE"
pass "webcash before unspent amount=1"

echo "== (6) POST /v1/swap/initiate =="
code=$(curl -sS -o "$OUTDIR/initiate-resp.bin" -w '%{http_code}' \
  -X POST -H 'content-type: application/octet-stream' \
  --data-binary @"$OUTDIR/initiate.bin" \
  "$REFEREE/v1/swap/initiate")
[[ "$code" == "200" ]] || fail "initiate HTTP $code $(cat "$OUTDIR/initiate-resp.bin")"
pass "initiate HTTP 200 $(cat "$OUTDIR/initiate-resp.bin")"

echo "== (7) replace (spend bearer) + advance to settled =="
OUT1=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
OUT2=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
REPLACE_BODY=$(python3 -c 'import json,sys; print(json.dumps({"webcashes":["e1.0:secret:"+sys.argv[1]],"new_webcashes":["e0.4:secret:"+sys.argv[2],"e0.6:secret:"+sys.argv[3]],"legalese":{"terms":True}}))' "$SCALAR" "$OUT1" "$OUT2")
curl -fsS -X POST -H 'content-type: application/json' -d "$REPLACE_BODY" "$WEBCASH/api/v1/replace" | tee "$OUTDIR/replace.json"
echo
AFTER=$(curl -fsS -X POST -H 'content-type: application/json' -d "[\"e1.0:public:$H\"]" "$WEBCASH/api/v1/health_check")
echo "$AFTER" > "$OUTDIR/webcash_after.json"
echo "$AFTER" | grep -q '"spent": true' || fail "after not spent: $AFTER"
pass "webcash after spent (balance delta proven)"

PHASE=""
for i in $(seq 1 20); do
  resp=$(curl -sS -X POST "$REFEREE/v1/swap/$PREPARED_SWAP_ID/advance" -H 'content-type: application/json' -d '{}')
  echo "  advance[$i] $resp"
  PHASE=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("phase",""))' <<<"$resp")
  echo "$resp" | grep -q '"terminal":true' && break
  sleep 0.2
done
curl -fsS "$REFEREE/v1/swap/$PREPARED_SWAP_ID/audit" > "$OUTDIR/audit.json"
[[ "$PHASE" == "settled" ]] || fail "expected settled got phase=$PHASE"
pass "terminal settled"

echo
echo "GATE4 prepare→initiate→settle PASS"
echo "  prepared_swap_id=$PREPARED_SWAP_ID"
echo "  H=$H"
echo "  artifacts: $OUTDIR"
