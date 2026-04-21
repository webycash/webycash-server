#!/usr/bin/env python3
"""Raw Redis pipeline benchmark — inside Docker network."""
import redis, time, json, hashlib, secrets, concurrent.futures, os, sys

HOSTS = os.environ.get("REDIS_HOSTS", "redis-1,redis-2,redis-3").split(",")

LUA_REPLACE = """
local ni=tonumber(ARGV[1]) local no=tonumber(ARGV[2]) local now=ARGV[3]
for i=1,ni do local k=KEYS[i] local j=redis.call('GET',k) if not j then return 'ERR:nf' end local r=cjson.decode(j) if r.spent then return 'ERR:sp' end end
for i=1,ni do local k=KEYS[i] local j=redis.call('GET',k) local r=cjson.decode(j) r.spent=true r.spent_at=now redis.call('SET',k,cjson.encode(r)) end
for i=1,no do local k=KEYS[ni+i] if redis.call('EXISTS',k)==1 then return 'ERR:ex' end redis.call('SET',k,ARGV[4+i]) end
redis.call('SET',ARGV[4],ARGV[5]) return 'ok'
"""

def run_pipeline(args):
    host, start_idx, count, sha = args
    r = redis.Redis(host=host, port=6379)
    pipe = r.pipeline(transaction=False)
    for i in range(start_idx, start_idx + count):
        inp = hashlib.sha256(f"s{host}_{i}".encode()).hexdigest()
        out = secrets.token_hex(32)
        out_rec = json.dumps({"public_hash":out,"amount_wats":200,"spent":False,"created_at":"x","spent_at":None,"origin":"replaced"})
        pipe.evalsha(sha, 2, f"token:{inp}", f"token:{out}", "1", "1", "now", f"a:{host}:{i}", json.dumps({"id":str(i)}), out_rec)
    t0 = time.time()
    results = pipe.execute(raise_on_error=False)
    elapsed = time.time() - t0
    ok = sum(1 for r in results if r == b"ok")
    return count, elapsed, ok

def precreate(host, N):
    r = redis.Redis(host=host, port=6379)
    r.flushdb()
    sha = r.script_load(LUA_REPLACE)
    pipe = r.pipeline(transaction=False)
    for i in range(N):
        h = hashlib.sha256(f"s{host}_{i}".encode()).hexdigest()
        rec = json.dumps({"public_hash":h,"amount_wats":200,"spent":False,"created_at":"x","spent_at":None,"origin":"mined"})
        pipe.set(f"token:{h}", rec)
    pipe.execute()
    return sha

print("=" * 70)
print("  Redis Replace Pipeline — Inside Docker Network")
print("=" * 70)

# --- Test 1: Single pipeline, single node ---
N = 100000
sha = precreate(HOSTS[0], N)
_, elapsed, ok = run_pipeline((HOSTS[0], 0, N, sha))
print(f"\n1 pipe, 1 node: {N:,} ops in {elapsed:.3f}s = {N/elapsed:,.0f} TPS")

# --- Test 2: 16 concurrent pipelines, single node ---
N_PER = 10000
CONNS = 16
N_TOTAL = N_PER * CONNS
sha = precreate(HOSTS[0], N_TOTAL)
args = [(HOSTS[0], i * N_PER, N_PER, sha) for i in range(CONNS)]
t0 = time.time()
with concurrent.futures.ThreadPoolExecutor(max_workers=CONNS) as ex:
    results = list(ex.map(run_pipeline, args))
elapsed = time.time() - t0
total_ok = sum(r[2] for r in results)
print(f"{CONNS} pipes, 1 node: {N_TOTAL:,} ops in {elapsed:.3f}s = {N_TOTAL/elapsed:,.0f} TPS ({total_ok:,} ok)")

# --- Test 3: 16 concurrent pipelines × 3 nodes ---
print(f"\n--- 3 nodes × {CONNS} connections each ---")
shas = {}
for h in HOSTS:
    shas[h] = precreate(h, N_TOTAL)

all_args = []
for h in HOSTS:
    for c in range(CONNS):
        all_args.append((h, c * N_PER, N_PER, shas[h]))

t0 = time.time()
with concurrent.futures.ThreadPoolExecutor(max_workers=CONNS * len(HOSTS)) as ex:
    results = list(ex.map(run_pipeline, all_args))
elapsed = time.time() - t0
total_ops = sum(r[0] for r in results)
total_ok = sum(r[2] for r in results)
print(f"3 × {CONNS} = {CONNS*3} pipes: {total_ops:,} ops in {elapsed:.3f}s = {total_ops/elapsed:,.0f} TPS ({total_ok:,} ok)")

# --- Test 4: Raw SET (no Lua) for throughput ceiling ---
print("\n--- Raw SET throughput (no Lua, 3 nodes) ---")
def raw_set(args):
    host, N = args
    r = redis.Redis(host=host, port=6379)
    r.flushdb()
    pipe = r.pipeline(transaction=False)
    for i in range(N):
        pipe.set(f"k:{host}:{i}", f"v{i}")
    t0 = time.time()
    pipe.execute()
    return N, time.time() - t0

t0 = time.time()
with concurrent.futures.ThreadPoolExecutor(max_workers=3) as ex:
    results = list(ex.map(raw_set, [(h, 500000) for h in HOSTS]))
elapsed = time.time() - t0
total_ops = sum(r[0] for r in results)
for h, (n, e) in zip(HOSTS, results):
    print(f"  {h}: {n:,} SETs in {e:.3f}s = {n/e:,.0f} ops/s")
print(f"  TOTAL: {total_ops:,} SETs in {elapsed:.3f}s = {total_ops/elapsed:,.0f} ops/s")

print("\n" + "=" * 70)
