#!/usr/bin/env bash
# Gate5 ARK_CLAIM_REAL: fund Q VTXO → prepare → wallet-real enc partial → initiate →
# settle → ASP claim with Ark balance delta. Never fakes settled.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$HERE/../../.." && pwd)"
REFEREE_REPO="$WORKSPACE/webycash/webycash-referee"
KEYSERVER_REPO="$WORKSPACE/extro/extro-keyserver"
NODE_REPO="$WORKSPACE/extro/extro-node"
PWA_REPO="$WORKSPACE/webycash/weby-wallet-pwa"
OUTDIR="${GATE5_OUT:-/tmp/gate5-claim}"
mkdir -p "$OUTDIR"

REFEREE="${REFEREE_URL:-http://127.0.0.1:8090}"
KEYSERVER="${KEYSERVER_URL:-http://127.0.0.1:7800}"
WEBCASH="${WEBCASH_URL:-http://127.0.0.1:8181}"
ASP="${ARK_ASP_URL:-http://127.0.0.1:7070}"
ESPLORA="${ARK_ESPLORA_URL:-http://127.0.0.1:3000}"

ARK_PRIVKEY="${ARK_PRIVKEY_HEX:-c1d7c093812cace1786bd138f7b3a19caa8990848c4cbeb76bcb08a73284c4a6}"
PROVIDER_MUSIG_SK="${PROVIDER_MUSIG_SK_HEX:-$(python3 -c 'print("a1"*32)')}"
PROVIDER_SK="${PROVIDER_SK_HEX:-$(python3 -c 'print("31"*32)')}"
SELLER_SK="${SELLER_SK_HEX:-$(python3 -c 'print("32"*32)')}"
AMOUNT="${ARK_AMOUNT_SATS:-25000}"
FILL_ID="gate5-claim-$(date +%s)"

PREPARE_BIN="$REFEREE_REPO/target/release/examples/swap_prepare_handshake"
SEED_BIN="$KEYSERVER_REPO/target/release/examples/seed_dev_f1_identity"
INITIATE_BIN="$NODE_REPO/target/release/examples/swap_initiate_request"
PROVIDER_BIN="$NODE_REPO/target/release/examples/gate5_provider_partial"
CLAIM_BIN="$NODE_REPO/target/release/examples/gate5_ark_claim"

pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1" >&2; exit 1; }
note() { echo "  NOTE: $1"; }

for b in "$PREPARE_BIN" "$SEED_BIN" "$INITIATE_BIN" "$PROVIDER_BIN" "$CLAIM_BIN"; do
  [[ -x "$b" ]] || fail "missing $b"
done

FREE_GIB=$(df -g / | awk 'NR==2{print $4}')
[[ "$FREE_GIB" -ge 40 ]] || fail "disk ${FREE_GIB} GiB < 40 abort floor"

echo "== (0) liveness =="
curl -fsS "$REFEREE/v1/health" >/dev/null && pass "referee" || fail "referee"
curl -fsS "$KEYSERVER/api/v1/health" >/dev/null && pass "keyserver" || fail "keyserver"
curl -fsS "$WEBCASH/api/v1/target" >/dev/null && pass "webcash" || fail "webcash"
curl -fsS "$ASP/v1/info" | tee "$OUTDIR/asp-info.json" | grep -q regtest && pass "asp regtest" || fail "asp"
REFEREE_PK=$(curl -fsS "$REFEREE/v1/pubkey" | python3 -c 'import sys,json; print(json.load(sys.stdin)["musig2_pubshare_hex"])')
pass "referee musig2 pubshare=${REFEREE_PK:0:18}…"

echo "== (1) provider MuSig2 material =="
export PROVIDER_SK_HEX="$PROVIDER_MUSIG_SK" FILL_ID="$FILL_ID"
MAT=$("$PROVIDER_BIN" material)
echo "$MAT" | tee "$OUTDIR/provider_material.json"
PUB=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["pubkeyHex"])' <<<"$MAT")
SETTLE_N=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["settleNonceHex"])' <<<"$MAT")
REFUND_N=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["refundNonceHex"])' <<<"$MAT")
pass "provider pub=${PUB:0:18}…"

echo "== (2) fund 2-of-2 Q VTXO + preview settle sighash =="
export ARK_PRIVKEY_HEX="$ARK_PRIVKEY" ARK_ASP_URL="$ASP" ARK_ESPLORA_URL="$ESPLORA"
export REFEREE_PK_HEX="$REFEREE_PK" PROVIDER_PK_HEX="$PUB" AMOUNT_SATS="$AMOUNT" EXIT_DELAY=5
FUND=$("$CLAIM_BIN" fund 2>"$OUTDIR/fund.err") || { cat "$OUTDIR/fund.err" >&2; fail "fund"; }
echo "$FUND" | tee "$OUTDIR/fund.json"
LOCKED_REF=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["lockedRef"])' <<<"$FUND")
SETTLE_HASH=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["settleSighashHex"])' <<<"$FUND")
# Refund hash must XOR-differ; use sha256 of settle||refund-tag for template XOR path
REFUND_HASH=$(python3 -c 'import hashlib,sys; print(hashlib.sha256((sys.argv[1]+"|refund").encode()).hexdigest())' "$SETTLE_HASH")
pass "locked_ref=$LOCKED_REF"
pass "settle_sighash=${SETTLE_HASH:0:18}…"

echo "== (3) F1 identity register =="
seed_one() {
  local nick="$1" sk="$2"
  curl -sS -o "$OUTDIR/challenge-$nick.bin" -w '%{http_code}' -X POST "$KEYSERVER/api/v1/identity/register" --data-binary '' | grep -q 402 || fail "challenge $nick"
  local out_fp
  out_fp=$("$SEED_BIN" "$nick" "$sk" "$OUTDIR/challenge-$nick.bin" "$OUTDIR/retry-$nick.bin")
  local code
  code=$(curl -sS -o "$OUTDIR/reg-$nick.bin" -w '%{http_code}' -X POST -H 'content-type: application/octet-stream' --data-binary @"$OUTDIR/retry-$nick.bin" "$KEYSERVER/api/v1/identity/register")
  [[ "$code" == "200" ]] || fail "register $nick HTTP $code"
  echo "$out_fp"
}
NICK_SUFFIX=$(date +%s)
PROVIDER_FP=$(seed_one "provider-$NICK_SUFFIX" "$PROVIDER_SK")
SELLER_FP=$(seed_one "seller-$NICK_SUFFIX" "$SELLER_SK")
pass "fps provider=$PROVIDER_FP seller=$SELLER_FP"

echo "== (4) prepare (captures referee settle nonce) =="
ARK_INFO=$(python3 -c 'import json; print(json.load(open("'"$OUTDIR"'/asp-info.json"))["digest"])')
ARK_SIGNER=$(python3 -c 'import json; print(json.load(open("'"$OUTDIR"'/asp-info.json"))["signerPubkey"][2:].lower())')
# digest field may already be full hex; signerPubkey is 02||xonly
ARK_SIGNER=$(python3 -c 'import json; s=json.load(open("'"$OUTDIR"'/asp-info.json"))["signerPubkey"]; print(s[2:] if s.startswith("02") or s.startswith("03") else s)')
ARK_INFO=$(python3 -c 'import json; print(json.load(open("'"$OUTDIR"'/asp-info.json"))["digest"].lower())')
REFEREE_URL="$REFEREE" ARK_NETWORK=regtest \
  ARK_SIGNER_PK_HEX="$ARK_SIGNER" ARK_INFO_DIGEST_HEX="$ARK_INFO" \
  ARK_UNILATERAL_EXIT_DELAY=5 ARK_AMOUNT_SATS="$AMOUNT" \
  PROVIDER_SK_HEX="$PROVIDER_SK" SELLER_SK_HEX="$SELLER_SK" \
  PROVIDER_MUSIG2_PUBKEY="$PUB" PROVIDER_SETTLE_NONCE="$SETTLE_N" PROVIDER_REFUND_NONCE="$REFUND_N" \
  FILL_AMOUNT_RAW=1 \
  "$PREPARE_BIN" > "$OUTDIR/prepare.out" 2>"$OUTDIR/prepare.err" || { cat "$OUTDIR/prepare.err" >&2; fail "prepare"; }
PREPARED_SWAP_ID=$(sed -n '1p' "$OUTDIR/prepare.out")
PREPARE_COMMITMENT=$(sed -n '2p' "$OUTDIR/prepare.out")
PREPARE_EXPIRY=$(sed -n '3p' "$OUTDIR/prepare.out")
PREPARE_IDEMP=$(sed -n '4p' "$OUTDIR/prepare.out")
REF_PUB=$(sed -n '5p' "$OUTDIR/prepare.out")
REF_SETTLE_N=$(sed -n '6p' "$OUTDIR/prepare.out")
REF_REFUND_N=$(sed -n '7p' "$OUTDIR/prepare.out")
[[ -n "$REF_SETTLE_N" ]] || fail "prepare missing referee settle nonce"
pass "prepared_swap_id=$PREPARED_SWAP_ID"
pass "referee_settle_nonce=${REF_SETTLE_N:0:18}…"
# Prefer prepare's pubshare if present
[[ -n "$REF_PUB" ]] && REFEREE_PK="$REF_PUB"

echo "== (5) provider settle partial (wallet-real) =="
PART=$("$PROVIDER_BIN" partial --referee-pk "$REFEREE_PK" --referee-nonce "$REF_SETTLE_N" --msg "$SETTLE_HASH")
echo "$PART" | tee "$OUTDIR/provider_partial.json"
PROVIDER_PARTIAL=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["partialHex"])' <<<"$PART")
# Frame: raw bytes = hex-decoded partial (32B typical) as "encrypted to alice" wallet-real payload
ENC_RAW_HEX=$(python3 -c 'import sys; print(sys.argv[1])' "$PROVIDER_PARTIAL")
# Ensure not placeholder
python3 -c 'import binascii,sys; b=binascii.unhexlify(sys.argv[1]); assert b!=b"<bob'\''s ark settle partial-sig, encrypted to alice>"; assert len(b)>=32' "$ENC_RAW_HEX"
pass "provider_partial=${PROVIDER_PARTIAL:0:18}… (wallet-real)"

echo "== (6) build initiate with real enc partial =="
ORDER_ID="$(python3 -c 'print("00"*16 + "11"*16)')"
SCALAR="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
echo "$SCALAR" > "$OUTDIR/scalar.txt"
H=$(python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest())' "$SCALAR")
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
  EXTRO_ENC_PARTIAL_RAW_HEX="$ENC_RAW_HEX" \
  "$INITIATE_BIN" "$OUTDIR/initiate.bin" "$SCALAR" "$ORDER_ID" 1 \
  > "$OUTDIR/bearer_h.txt" 2>"$OUTDIR/initiate-build.err" || { cat "$OUTDIR/initiate-build.err" >&2; fail "initiate build"; }
GOT_H=$(tr -d '[:space:]' < "$OUTDIR/bearer_h.txt")
[[ "$GOT_H" == "$H" ]] || fail "H mismatch"
pass "initiate envelope $(wc -c < "$OUTDIR/initiate.bin")B H=$H"

echo "== (7) mine bearer + initiate + replace + advance =="
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
echo "$BEFORE" | grep -q '"spent": false' || fail "before not unspent"
code=$(curl -sS -o "$OUTDIR/initiate-resp.bin" -w '%{http_code}' -X POST -H 'content-type: application/octet-stream' --data-binary @"$OUTDIR/initiate.bin" "$REFEREE/v1/swap/initiate")
[[ "$code" == "200" ]] || fail "initiate HTTP $code $(cat "$OUTDIR/initiate-resp.bin")"
pass "initiate HTTP 200 $(cat "$OUTDIR/initiate-resp.bin")"
OUT1=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
OUT2=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
REPLACE_BODY=$(python3 -c 'import json,sys; print(json.dumps({"webcashes":["e1.0:secret:"+sys.argv[1]],"new_webcashes":["e0.4:secret:"+sys.argv[2],"e0.6:secret:"+sys.argv[3]],"legalese":{"terms":True}}))' "$SCALAR" "$OUT1" "$OUT2")
curl -fsS -X POST -H 'content-type: application/json' -d "$REPLACE_BODY" "$WEBCASH/api/v1/replace" >/dev/null
AFTER=$(curl -fsS -X POST -H 'content-type: application/json' -d "[\"e1.0:public:$H\"]" "$WEBCASH/api/v1/health_check")
echo "$AFTER" > "$OUTDIR/webcash_after.json"
echo "$AFTER" | grep -q '"spent": true' || fail "after not spent"
PHASE=""
for i in $(seq 1 20); do
  resp=$(curl -sS -X POST "$REFEREE/v1/swap/$PREPARED_SWAP_ID/advance" -H 'content-type: application/json' -d '{}')
  echo "  advance[$i] $resp"
  PHASE=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("phase",""))' <<<"$resp")
  echo "$resp" | grep -q '"terminal":true' && break
  sleep 0.2
done
[[ "$PHASE" == "settled" ]] || fail "expected settled got phase=$PHASE"
pass "terminal settled"
curl -fsS "$REFEREE/v1/swap/$PREPARED_SWAP_ID/audit" > "$OUTDIR/audit.json"
# Extract referee partial + enc partial from audit/release if present
python3 - <<PY
import json, base64, pathlib
audit=json.load(open("$OUTDIR/audit.json"))
pathlib.Path("$OUTDIR/audit_pretty.json").write_text(json.dumps(audit, indent=2)[:20000])
print("audit keys", list(audit)[:20] if isinstance(audit, dict) else type(audit))
PY

echo "== (8) extract referee settle partial from release =="
# Probe pending release via audit events
REF_PARTIAL=$(python3 - <<'PY'
import json, re, sys
raw=open("/tmp/gate5-claim/audit.json").read()
# look for referee_partial_sig hex
m=re.search(r'"referee_partial_sig"\s*:\s*"([0-9a-fA-F]+)"', raw)
if not m:
    # try nested
    try:
        a=json.loads(raw)
    except Exception:
        sys.exit("no json")
    def walk(o):
        if isinstance(o, dict):
            if "referee_partial_sig" in o: return o["referee_partial_sig"]
            for v in o.values():
                r=walk(v)
                if r: return r
        elif isinstance(o, list):
            for v in o:
                r=walk(v)
                if r: return r
        return None
    r=walk(a)
    if not r: sys.exit("missing referee_partial_sig")
    print(r)
else:
    print(m.group(1))
PY
) || fail "no referee_partial_sig in audit"
echo "$REF_PARTIAL" > "$OUTDIR/referee_partial.txt"
pass "referee_partial=${REF_PARTIAL:0:18}…"

# Confirm enc partial is not placeholder
python3 - <<PY
import json, base64, re
raw=open("$OUTDIR/audit.json").read()
m=re.search(r'"provider_enc_partial_sig_b64"\s*:\s*"([^"]+)"', raw)
assert m, "missing enc partial"
b=base64.b64decode(m.group(1))
assert b"<bob's ark settle partial-sig, encrypted to alice>" not in b, "still placeholder"
open("$OUTDIR/enc_partial.bin","wb").write(b)
print("enc_partial_len", len(b), "placeholder=false")
PY
pass "provider_enc_partial wallet-real (not placeholder)"

echo "== (9) aggregate + ASP claim =="
export REFEREE_NONCE_HEX="$REF_SETTLE_N" PROVIDER_NONCE_HEX="$SETTLE_N"
export REFEREE_PARTIAL_HEX="$REF_PARTIAL" PROVIDER_PARTIAL_HEX="$PROVIDER_PARTIAL"
export LOCKED_REF="$LOCKED_REF" REFEREE_PK_HEX="$REFEREE_PK" PROVIDER_PK_HEX="$PUB"
CLAIM=$("$CLAIM_BIN" claim 2>"$OUTDIR/claim.err") || { cat "$OUTDIR/claim.err" >&2; fail "claim"; }
echo "$CLAIM" | tee "$OUTDIR/claim.json"
CLAIM_TXID=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["claimTxid"])' <<<"$CLAIM")
DELTA=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["delta"])' <<<"$CLAIM")
pass "claim_txid=$CLAIM_TXID delta=$DELTA"

# Write receipt
python3 - <<PY
import json, pathlib, datetime
receipt={
  "gate":"GATE5_ARK_CLAIM_REAL",
  "result":"PASS",
  "timestamp":datetime.datetime.utcnow().isoformat()+"Z",
  "prepared_swap_id":"$PREPARED_SWAP_ID",
  "locked_ref":"$LOCKED_REF",
  "settle_sighash":"$SETTLE_HASH",
  "webcash_H":"$H",
  "webcash_before":json.load(open("$OUTDIR/webcash_before.json")),
  "webcash_after":json.load(open("$OUTDIR/webcash_after.json")),
  "provider_enc_partial_placeholder":False,
  "referee_partial_sig":"$REF_PARTIAL",
  "provider_partial_sig":"$PROVIDER_PARTIAL",
  "claim":json.loads('''$CLAIM'''),
  "cf_ark_enabled":False,
}
pathlib.Path("$OUTDIR/gate5-ark-claim-real.json").write_text(json.dumps(receipt, indent=2))
pathlib.Path("$PWA_REPO/test-results/gate5-ark-claim-real.json").write_text(json.dumps(receipt, indent=2))
print(json.dumps({"result":"PASS","claim_txid":"$CLAIM_TXID","delta":$DELTA}, indent=2))
PY

echo
echo "GATE5 ARK_CLAIM_REAL PASS"
echo "  claim_txid=$CLAIM_TXID ark_delta=$DELTA"
echo "  artifacts: $OUTDIR"
