#!/usr/bin/env bash
# Cold-start the LOCAL server-side stack: every backend running as its native
# self against a faithful Cloudflare emulation (D1 = SQLite file, ledger =
# redis container). Launches the NATIVE host binaries (built with
# `cargo build --release` — never docker-built) and the one infra container the
# server-side path needs (redis).
#
#   ./run-local.sh up      # build-check binaries, start redis, launch servers
#   ./run-local.sh down     # stop everything
#   ./run-local.sh status   # show what is up
#
# Ports:
#   server-webcash    :8181   (redis-backed ledger)
#   server-rgb        :8182
#   server-voucher    :8183
#   extro-keyserver   :7800   (in-memory identity store, dev operator key)
#   webycash-referee  :8090   (SQLite = D1, real HttpBearerRail → :8181)
#
# This script does NOT run cargo build itself (the OOM ps-guard owns that) — it
# asserts the binaries already exist and tells you the exact build command if
# one is missing.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVER_REPO="$(cd "$HERE/.." && pwd)"                       # webycash-server
WORKSPACE="$(cd "$SERVER_REPO/../.." && pwd)"               # …/workspace
KEYSERVER_REPO="$WORKSPACE/extro/extro-keyserver"
REFEREE_REPO="$WORKSPACE/webycash/webycash-referee"

RUNDIR="$HERE/.run"           # pids, sqlite db, dev keys, logs (gitignored)
mkdir -p "$RUNDIR"

SRV_WEBCASH_BIN="$SERVER_REPO/target/release/webycash-server-webcash"
SRV_RGB_BIN="$SERVER_REPO/target/release/webycash-server-rgb"
SRV_VOUCHER_BIN="$SERVER_REPO/target/release/webycash-server-voucher"
KEYSERVER_BIN="$KEYSERVER_REPO/target/release/extro-keyserver"
REFEREE_BIN="$REFEREE_REPO/target/release/webycash-referee"

COMPOSE="docker compose -f $HERE/docker-compose.local.yml"

# ── dev key material (deterministic, NEVER for production) ──────────────────
# Ed25519 operator seed for the keyserver (64 hex) + its 40-hex fingerprint,
# and a secp256k1 MuSig2 secret for the referee (32-byte hex). All 0600.
ensure_dev_keys() {
  if [[ ! -f "$RUNDIR/operator_seed.hex" ]]; then
    printf '%s' "1111111111111111111111111111111111111111111111111111111111111111" > "$RUNDIR/operator_seed.hex"
    chmod 600 "$RUNDIR/operator_seed.hex"
  fi
  # Fingerprint is operator-chosen for local dev; any 40-hex value is accepted.
  if [[ ! -f "$RUNDIR/operator_fp.hex" ]]; then
    printf '%s' "00112233445566778899aabbccddeeff00112233" > "$RUNDIR/operator_fp.hex"
    chmod 600 "$RUNDIR/operator_fp.hex"
  fi
  if [[ ! -f "$RUNDIR/musig2_secret.hex" ]]; then
    printf '%s' "2222222222222222222222222222222222222222222222222222222222222222" > "$RUNDIR/musig2_secret.hex"
    chmod 600 "$RUNDIR/musig2_secret.hex"
  fi
}

require_bin() {
  local bin="$1" hint="$2"
  if [[ ! -x "$bin" ]]; then
    echo "MISSING native binary: $bin" >&2
    echo "  build it (one cargo at a time — check the OOM ps-guard first):" >&2
    echo "    $hint" >&2
    exit 1
  fi
}

wait_http() {  # url, name, tries
  local url="$1" name="$2" tries="${3:-40}"
  for _ in $(seq 1 "$tries"); do
    if curl -fsS "$url" >/dev/null 2>&1; then echo "  $name up ($url)"; return 0; fi
    sleep 0.25
  done
  echo "  $name FAILED to come up at $url" >&2
  return 1
}

launch() {  # name, logfile, binary, [env KEY=VAL ...]
  local name="$1"; shift
  local log="$1"; shift
  local bin="$1"; shift
  ( env "$@" "$bin" >"$log" 2>&1 & echo $! > "$RUNDIR/$name.pid" )
  echo "  launched $name (pid $(cat "$RUNDIR/$name.pid"), log $log)"
}

up() {
  require_bin "$SRV_WEBCASH_BIN" "cd $SERVER_REPO && cargo build --release -p webycash-server-webcash"
  require_bin "$KEYSERVER_BIN"   "cd $KEYSERVER_REPO && cargo build --release --bin extro-keyserver"
  require_bin "$REFEREE_BIN"     "cd $REFEREE_REPO && cargo build --release --features zkp-arkworks,musig2-real"
  ensure_dev_keys

  echo "[1/4] redis (ledger backend)…"
  $COMPOSE up -d redis
  for _ in $(seq 1 40); do
    if docker exec "$($COMPOSE ps -q redis)" redis-cli ping 2>/dev/null | grep -q PONG; then
      echo "  redis up (:6391)"; break
    fi; sleep 0.25
  done
  local REDIS_URL="redis://127.0.0.1:6391"

  # Issuer registry for POST /api/v1/issue. The rail servers verify the issuing
  # wallet's `X-Issuer-Signature` against registered `fingerprint:ed25519_pubkey`
  # issuers; with none registered the server rejects every issue with HTTP 500
  # ("issuer registry not configured"). These defaults are the dev wallet's
  # public issuer identities, derived via the extro-node `DeriveIssuer` command
  # for the Voucher / RGB families at slot 0 (40-hex fp = first 20 bytes of the
  # 64-hex Ed25519 verifying key). Authorize a different issuing wallet by
  # exporting WEBYCASH_VOUCHER_ISSUERS / WEBYCASH_RGB_ISSUERS (`fp:pubkey_hex`,
  # comma-separated for multiple issuers).
  local VOUCHER_ISSUERS="${WEBYCASH_VOUCHER_ISSUERS:-1215372420e2a867e122fd82c1728f10e05f0219:1215372420e2a867e122fd82c1728f10e05f02199a289a2ec57d7b308c4603f8}"
  local RGB_ISSUERS="${WEBYCASH_RGB_ISSUERS:-695027fb31478d3f7d715582820bd6e8c4528110:695027fb31478d3f7d715582820bd6e8c45281102e98aa3eed3893a367c6e137}"

  echo "[2/4] webcash/rgb/voucher servers (native, redis-backed)…"
  launch server-webcash "$RUNDIR/server-webcash.log" "$SRV_WEBCASH_BIN" \
    WEBCASH_BIND_ADDR=127.0.0.1:8181 WEBCASH_MODE=testnet WEBCASH_DB_BACKEND=redis \
    WEBYCASH_DIFFICULTY=4 REDIS_URL="$REDIS_URL" RUST_LOG=warn
  if [[ -x "$SRV_RGB_BIN" ]]; then
    launch server-rgb "$RUNDIR/server-rgb.log" "$SRV_RGB_BIN" \
      WEBCASH_BIND_ADDR=127.0.0.1:8182 WEBCASH_MODE=testnet WEBCASH_DB_BACKEND=redis \
      WEBYCASH_MINING_REQUIRED_FOR_ISSUANCE=false WEBYCASH_ISSUERS="$RGB_ISSUERS" \
      REDIS_URL="$REDIS_URL" RUST_LOG=warn
  fi
  if [[ -x "$SRV_VOUCHER_BIN" ]]; then
    launch server-voucher "$RUNDIR/server-voucher.log" "$SRV_VOUCHER_BIN" \
      WEBCASH_BIND_ADDR=127.0.0.1:8183 WEBCASH_MODE=testnet WEBCASH_DB_BACKEND=redis \
      WEBYCASH_MINING_REQUIRED_FOR_ISSUANCE=false WEBYCASH_ISSUERS="$VOUCHER_ISSUERS" \
      REDIS_URL="$REDIS_URL" RUST_LOG=warn
  fi
  wait_http "http://127.0.0.1:8181/api/v1/target" server-webcash

  echo "[3/4] extro-keyserver (in-memory identity store, dev operator key)…"
  launch keyserver "$RUNDIR/keyserver.log" "$KEYSERVER_BIN" \
    EXTRO_BIND_ADDR=127.0.0.1:7800 EXTRO_ENV=dev \
    EXTRO_OPERATOR_DEV_KEY="$(cat "$RUNDIR/operator_seed.hex")" \
    EXTRO_OPERATOR_FINGERPRINT="$(cat "$RUNDIR/operator_fp.hex")" \
    EXTRO_OPERATOR_DOMAIN=keyserver.local RUST_LOG=info
  wait_http "http://127.0.0.1:7800/api/v1/health" keyserver

  echo "[4/4] webycash-referee (SQLite = D1, real HttpBearerRail → webcash :8181)…"
  # WEBYCASH_REFEREE_DB_PATH points at a local SQLite file: D1 IS SQLite, so
  # this is the exact store/audit code path that runs on Cloudflare D1.
  # EXTRO_ZKP_VK_PATH points at the extro-circuits dev verifying key. NOTE: the
  # arkworks verifier ALREADY defaults to this same dev VK when the env var is
  # unset (ArkworksVerifier::from_env_bearer → default_vk_path =
  # extro-circuits/circuits-dev/vk_bearer_payload.bin), so the referee boots
  # with a REAL arkworks Groth16/BN254 verifier here (not fail-closed). It only
  # fails closed if neither the env path nor the dev VK file is readable. The
  # smoke test does not exercise swap-initiate, so the VK is not load-bearing
  # for Phase D — but it IS for Phase E, and is wired explicitly for clarity.
  local DEV_VK="$WORKSPACE/extro/extro-circuits/circuits-dev/vk_bearer_payload.bin"
  launch referee "$RUNDIR/referee.log" "$REFEREE_BIN" \
    WEBYCASH_REFEREE_BIND=127.0.0.1:8090 \
    WEBYCASH_REFEREE_DB_PATH="$RUNDIR/referee.sqlite" \
    WEBYCASH_REFEREE_RAIL_BASE_URL=http://127.0.0.1:8181 \
    WEBYCASH_REFEREE_MUSIG2_KEY_PATH="$RUNDIR/musig2_secret.hex" \
    EXTRO_ZKP_VK_PATH="$DEV_VK" \
    RUST_LOG=info
  wait_http "http://127.0.0.1:8090/v1/health" referee

  echo
  echo "stack up. endpoints:"
  echo "  webcash   http://127.0.0.1:8181/api/v1/target"
  echo "  rgb       http://127.0.0.1:8182/api/v1/target   (if built)"
  echo "  voucher   http://127.0.0.1:8183/api/v1/target   (if built)"
  echo "  keyserver http://127.0.0.1:7800/api/v1/health  + /.well-known/extro-keyserver"
  echo "  referee   http://127.0.0.1:8090/v1/health      + /v1/pubkey"
  echo
  echo "smoke test:  $HERE/smoke-test.sh"
  echo "tear down:   $HERE/run-local.sh down"
}

down() {
  for name in referee keyserver server-voucher server-rgb server-webcash; do
    if [[ -f "$RUNDIR/$name.pid" ]]; then
      kill "$(cat "$RUNDIR/$name.pid")" 2>/dev/null || true
      rm -f "$RUNDIR/$name.pid"
      echo "  stopped $name"
    fi
  done
  $COMPOSE down 2>/dev/null || true
  echo "stack down."
}

status() {
  echo "native processes:"
  for name in server-webcash server-rgb server-voucher keyserver referee; do
    if [[ -f "$RUNDIR/$name.pid" ]] && kill -0 "$(cat "$RUNDIR/$name.pid")" 2>/dev/null; then
      echo "  $name: RUNNING (pid $(cat "$RUNDIR/$name.pid"))"
    else
      echo "  $name: down"
    fi
  done
  echo "containers:"; $COMPOSE ps 2>/dev/null || true
}

case "${1:-up}" in
  up) up ;;
  down) down ;;
  status) status ;;
  *) echo "usage: $0 {up|down|status}"; exit 1 ;;
esac
