#!/usr/bin/env bash
# Alice↔Bob swap round-trip driver — sell webcash for ARK, then back, for a TINY
# amount, against the LIVE local stack (`./run-local.sh up`).
#
# This drives the REAL running referee over HTTP (POST /v1/swap/initiate →
# /advance), the REAL webcash server (mine the bearer leg, then the rail
# spent-status pre/post-check via the referee's own HttpBearerRail), and a real
# two-proof rkyv InitiateRequest built by extro-node's `swap_initiate_request`
# example (real Groth16/BN254 bearer proof). It is NOT a wrapper around an
# in-process cargo test — every step hits a listening service.
#
# ╔═ SAFETY ═╗
#  Default network = SIGNET (a valueless test network). MAINNET is refused unless
#  EXTRO_SWAP_NETWORK=mainnet is set explicitly AND the run is interactive AND the
#  operator types the confirmation phrase. Even then this script NEVER moves real
#  funds itself — the live ARK leg (the actual VTXO settlement on a real ASP) is
#  the OPERATOR's, performed with the operator's wallet + ASP credentials (see the
#  ./SWAP-ROUNDTRIP-RUNBOOK.md). This script prepares + drives every leg it can
#  locally and prints the exact remaining operator step.
# ╚══════════╝
#
#   ./swap-roundtrip.sh            # signet-default dry run against the local stack
#   EXTRO_SWAP_NETWORK=signet ...  # explicit signet (same as default)
#   EXTRO_SWAP_NETWORK=mainnet ... # REFUSED non-interactively; operator-only
#
# Honest scope (what runs live here vs the operator's mainnet/signet ARK leg):
#   LIVE here:  referee /v1/health, /v1/pubkey; real two-proof InitiateRequest
#               built + POSTed (decodes + reaches the real verifier); webcash
#               bearer leg mined on the live server; the referee's REAL
#               HttpBearerRail Unspent→Spent read MECHANISM (the settlement-
#               authority observation) driven against the live server. NOTE: no
#               swap completes live (initiate 422s at the verifier, below); the
#               full post-check→settle/refund path is proven IN-PROCESS under the
#               real BIP327 signer + MockArkSigner (referee `alice_bob_roundtrip`).
#   GAP (B2c):  the POSTed envelope DECODES and reaches the REAL arkworks
#               verifier, which returns 422 ZkpRejected. This is CONSISTENT with
#               the ConditionalLeg/B2c gap: the referee recomputes the CONDITIONAL
#               binding with settlement_tx_hash=Some (7 public inputs), but the
#               extro-circuits dev ceremony fixes a 6-input BearerPayloadCircuit,
#               so a real conditional proof cannot verify. The generic 422 does
#               not itself isolate the leg; the referee test
#               `conditional_leg_real_proof_is_rejected_documents_b2c_gap` does
#               (in-process: real bearer proof TRUE, real conditional REJECTED).
#               Closing it = a 7-input conditional ceremony (full in-circuit
#               MuSig2 is the long-term B2c target).
#   OPERATOR:   the ARK 2-of-2 VTXO settlement on a real signet (then, separately,
#               mainnet) ASP — extro-node `ArkHandle` ↔ the operator's ASP.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # …/webycash/webycash-server/local-stack
WORKSPACE="$(cd "$HERE/../../.." && pwd)"               # …/workspace
NODE_REPO="$WORKSPACE/extro/extro-node"
REFEREE_REPO="$WORKSPACE/webycash/webycash-referee"

WEBCASH="${WEBCASH_URL:-http://127.0.0.1:8181}"
REFEREE="${REFEREE_URL:-http://127.0.0.1:8090}"
NETWORK="${EXTRO_SWAP_NETWORK:-signet}"
AMOUNT="${SWAP_AMOUNT:-1}"            # TINY: 1 webcash unit
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

pass() { echo "  PASS: $1"; }
note() { echo "  NOTE: $1"; }
fail() { echo "  FAIL: $1" >&2; exit 1; }

# ── Mainnet guard (loud; operator-only; refuses non-interactive) ────────────
if [[ "$NETWORK" == "mainnet" || "$NETWORK" == "bitcoin" ]]; then
  echo "════════════════════════════════════════════════════════════════"
  echo " MAINNET REQUESTED — REAL FUNDS. This script will NOT run a mainnet"
  echo " swap. The mainnet ARK leg is the OPERATOR's, performed manually with"
  echo " the operator's wallet + ASP credentials per the RUNBOOK. This driver"
  echo " refuses to proceed against mainnet."
  echo "════════════════════════════════════════════════════════════════"
  if [[ ! -t 0 ]]; then
    fail "mainnet refused: non-interactive shell (no human at the keyboard)"
  fi
  read -r -p 'Type exactly "I AM THE OPERATOR AND WILL RUN MAINNET MANUALLY" to acknowledge: ' ACK
  [[ "$ACK" == "I AM THE OPERATOR AND WILL RUN MAINNET MANUALLY" ]] \
    || fail "mainnet acknowledgement phrase not matched — aborting"
  echo
  echo "Acknowledged. This script still does NOT broadcast or move mainnet funds."
  echo "Follow ./SWAP-ROUNDTRIP-RUNBOOK.md 'Tiny mainnet run' section. Exiting."
  exit 0
fi

if [[ "$NETWORK" != "signet" && "$NETWORK" != "regtest" && "$NETWORK" != "testnet" ]]; then
  fail "EXTRO_SWAP_NETWORK=$NETWORK unrecognized (signet|regtest|testnet|mainnet)"
fi
echo "network: $NETWORK (valueless test network — safe)"

# Prefer the release example (real Groth16 proving is ~10× faster in release;
# 4 provings/run otherwise makes the "one command" impractical). Fall back to
# debug if only that is built.
INITIATE_BIN="$NODE_REPO/target/release/examples/swap_initiate_request"
[[ -x "$INITIATE_BIN" ]] || INITIATE_BIN="$NODE_REPO/target/debug/examples/swap_initiate_request"
RAIL_PROBE="$REFEREE_REPO/target/release/examples/rail_probe"

require_bin() {
  [[ -x "$1" ]] || fail "missing $2: $1
  build it (ONE cargo at a time — check the OOM ps-guard first):
    $3"
}

echo "== (0) prerequisites =="
require_bin "$INITIATE_BIN" "swap_initiate_request example" \
  "cd $NODE_REPO && cargo build --release --example swap_initiate_request --features scheme402"
require_bin "$RAIL_PROBE" "rail_probe example" \
  "cd $REFEREE_REPO && cargo build --release --example rail_probe --features zkp-arkworks,musig2-real"
curl -fsS "$REFEREE/v1/health" >/dev/null && pass "referee /v1/health (live)" || fail "referee down — run ./run-local.sh up"
curl -fsS "$WEBCASH/api/v1/target" >/dev/null && pass "webcash /target (live)" || fail "webcash server down"
RP=$(curl -fsS "$REFEREE/v1/pubkey")
echo "$RP" | grep -q ed25519_pubkey_hex && pass "referee /v1/pubkey: $RP" || fail "referee /v1/pubkey"

# Drive ONE swap leg: build a real-proof InitiateRequest for `scalar`, mine the
# matching webcash bearer leg on the live server, POST initiate, and exercise the
# rail spent-status transition. Prints whether the live initiate passed or hit
# the documented B2c gap.
run_leg() {
  local label="$1" scalar="$2"
  echo
  echo "== leg: $label (webcash → ARK, $NETWORK) =="

  # 1) Build the real two-proof InitiateRequest. Its bearer H = SHA-256(ascii(hex(scalar)))
  #    — the SAME H the webcash server computes for the secret string `scalar`.
  local out="$TMP/$label.bin"
  local H
  H=$("$INITIATE_BIN" "$out" "$scalar" 2>/dev/null) || fail "initiate-request build failed"
  pass "built real two-proof InitiateRequest ($(wc -c <"$out" | tr -d ' ') bytes), bearer H=$H"

  # 2) Mine the matching webcash on the LIVE server so H exists + is unspent
  #    (the bearer leg the seller offers). `scalar` IS the webcash secret string.
  local SUBSIDY OUT1 OUT2 NONCE PREIMAGE
  SUBSIDY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
  read -r NONCE PREIMAGE < <(python3 - "$scalar" "$SUBSIDY" <<'PY'
import hashlib, json, sys
secret, subsidy = sys.argv[1], sys.argv[2]
for nonce in range(400000):
    preimage = ('{"webcash":["e1.0:secret:%s"],'
                '"subsidy":["e0.5:secret:%s"],'
                '"timestamp":1714003200,"difficulty":4,"nonce":%d}' % (secret, subsidy, nonce))
    if hashlib.sha256(preimage.encode()).digest()[0] < 0x10:
        print(nonce, json.dumps(preimage)); break
PY
)
  [[ -n "${PREIMAGE:-}" ]] || fail "could not find PoW nonce"
  local MINE_BODY MR
  MINE_BODY=$(python3 -c 'import json,sys; print(json.dumps({"preimage": json.loads(sys.argv[1]), "legalese": {"terms": True}}))' "$PREIMAGE")
  MR=$(curl -fsS -X POST -H 'content-type: application/json' -d "$MINE_BODY" "$WEBCASH/api/v1/mining_report")
  echo "$MR" | grep -q '"status": "success"' && pass "mined the bearer webcash on the live server" || fail "mining_report: $MR"

  local PUBTOKEN="e1.0:public:$H"

  # 3) Rail PRE-CHECK via the referee's REAL HttpBearerRail → Unspent.
  local R1
  R1=$("$RAIL_PROBE" "$WEBCASH" "$PUBTOKEN")
  [[ "$R1" == "Unspent" ]] && pass "referee HttpBearerRail pre-check: Unspent (live)" || fail "pre-check expected Unspent, got $R1"

  # 4) POST the real InitiateRequest to the LIVE referee. The referee runs the
  #    REAL arkworks verifier on BOTH proofs. The bearer proof verifies; the
  #    conditional proof (7 inputs) hits the B2c gap → 422. We capture + report
  #    honestly; we do NOT swap the referee to a mock to force a pass.
  local CODE BODY
  BODY=$(curl -s -o "$TMP/$label.resp" -w '%{http_code}' \
    -X POST -H 'content-type: application/octet-stream' \
    --data-binary "@$out" "$REFEREE/v1/swap/initiate") || true
  CODE="$BODY"
  echo "  POST /v1/swap/initiate → HTTP $CODE"
  local RESP
  RESP="$(cat "$TMP/$label.resp" 2>/dev/null)"
  if [[ "$CODE" == "200" ]]; then
    local SWAP_ID
    SWAP_ID=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1])).get("swap_id",""))' "$TMP/$label.resp" 2>/dev/null || true)
    pass "LIVE initiate accepted BOTH proofs — swap_id=$SWAP_ID (B2c gap CLOSED — a 7-input conditional ceremony must have landed)"
    # Spend the bearer leg (recipient takes ownership), then advance the swap so
    # the post-check sees Spent and the referee releases the settle.
    local O1 O2
    O1=$(python3 -c 'import secrets;print(secrets.token_hex(32))')
    O2=$(python3 -c 'import secrets;print(secrets.token_hex(32))')
    curl -fsS -X POST -H 'content-type: application/json' \
      -d "{\"webcashes\":[\"e1.0:secret:$scalar\"],\"new_webcashes\":[\"e0.4:secret:$O1\",\"e0.6:secret:$O2\"],\"legalese\":{\"terms\":true}}" \
      "$WEBCASH/api/v1/replace" | grep -q success && pass "bearer leg spent (recipient took ownership)"
    curl -fsS -X POST "$REFEREE/v1/swap/$SWAP_ID/advance" -d '{}' >/dev/null && pass "referee advanced the swap (post-check → release)"
    return 0
  elif [[ "$CODE" == "422" ]]; then
    # The envelope DECODED and reached the REAL arkworks verifier; a ZKP was
    # rejected. This is consistent with the ConditionalLeg/B2c gap (the
    # conditional proof has 7 public inputs; the dev ceremony VK is 6-input).
    # The generic 422 does not by itself isolate WHICH leg — the referee test
    # `conditional_leg_real_proof_is_rejected_documents_b2c_gap` isolates it.
    note "LIVE initiate → 422 ZkpRejected (decoded + reached the REAL verifier)."
    echo "       referee response: $RESP"
    note "consistent with the ConditionalLeg/B2c gap (7-input conditional proof"
    note "vs the 6-input dev VK); leg isolation is in the referee's B2c test."
  elif [[ "$CODE" == "400" ]]; then
    fail "LIVE initiate → 400 BadRequest (the envelope did not even decode): $RESP"
  else
    fail "LIVE initiate → unexpected HTTP $CODE: $RESP"
  fi

  # 5) Even on the B2c branch, prove the rail POST-CHECK transition (the
  #    settlement authority): spend the bearer leg, then the real rail reads Spent.
  local O1 O2
  O1=$(python3 -c 'import secrets;print(secrets.token_hex(32))')
  O2=$(python3 -c 'import secrets;print(secrets.token_hex(32))')
  curl -fsS -X POST -H 'content-type: application/json' \
    -d "{\"webcashes\":[\"e1.0:secret:$scalar\"],\"new_webcashes\":[\"e0.4:secret:$O1\",\"e0.6:secret:$O2\"],\"legalese\":{\"terms\":true}}" \
    "$WEBCASH/api/v1/replace" | grep -q success && pass "bearer leg spent on the live server" || fail "replace failed"
  local R2
  R2=$("$RAIL_PROBE" "$WEBCASH" "$PUBTOKEN")
  [[ "$R2" == "Spent" ]] \
    && pass "referee HttpBearerRail reads Spent after /replace — the rail-client Unspent→Spent mechanism (the settlement-authority observation) proven LIVE. NOTE: no swap exists at this point (initiate 422'd at the verifier); the full post-check→settle path is proven in-process (alice_bob_roundtrip)." \
    || fail "expected Spent, got $R2"
}

# ── The round-trip: webcash→ARK, then ARK→webcash (roles swapped) ───────────
# Fresh 32-byte hex scalars per leg so re-runs never collide with a spent H.
LEG1_SCALAR="$(python3 -c 'import secrets;print(secrets.token_hex(32))')"
LEG2_SCALAR="$(python3 -c 'import secrets;print(secrets.token_hex(32))')"

run_leg "leg1-alice-sells-webcash" "$LEG1_SCALAR"
run_leg "leg2-bob-sells-webcash"   "$LEG2_SCALAR"

echo
echo "════════════════════════════════════════════════════════════════"
echo "ROUND-TRIP DRIVER COMPLETE ($NETWORK)."
echo "  Proven LIVE: referee health/pubkey; real two-proof InitiateRequest built,"
echo "    POSTed, DECODED, and reached the REAL arkworks verifier; webcash bearer"
echo "    leg mined; the referee's REAL HttpBearerRail Unspent→Spent read"
echo "    mechanism (the settlement-authority observation)."
echo "  NOT proven live: no swap completes (initiate 422s at the verifier). The"
echo "    full post-check→settle/refund path is proven IN-PROCESS under the real"
echo "    BIP327 signer + MockArkSigner (referee alice_bob_roundtrip)."
echo "  Remaining: (build) the B2c 7-input conditional ceremony before live"
echo "    initiate accepts; (operator) the ARK 2-of-2 VTXO settlement on a real"
echo "    ASP. See ./SWAP-ROUNDTRIP-RUNBOOK.md."
echo "════════════════════════════════════════════════════════════════"

# ─────────────────────────────────────────────────────────────────────────────
# RUNBOOK (read; the operator performs the ARK + mainnet steps)
# ─────────────────────────────────────────────────────────────────────────────
# See ./SWAP-ROUNDTRIP-RUNBOOK.md for the full operator runbook:
#  - how to fund a signet ARK wallet (extro-node `ark board`) + signet faucet;
#  - the exact signet dry-run, then the tiny-amount mainnet steps;
#  - the ARK_ASP_URL / ARK_NETWORK config for the referee's read-only validation.
