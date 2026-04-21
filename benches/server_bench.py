#!/usr/bin/env python3
"""Benchmark webycash-server replace endpoint — uses aiohttp for real concurrency."""
import asyncio, aiohttp, time, json, hashlib, secrets, os, sys

SERVERS = os.environ.get("SERVERS", "http://server-1:8080,http://server-2:8080,http://server-3:8080").split(",")

def random_hex64():
    return secrets.token_hex(32)

def find_preimage(webcash_str, difficulty=1):
    for nonce in range(100000):
        preimage = json.dumps({
            "webcash": [webcash_str], "subsidy": [],
            "timestamp": int(time.time()),
            "difficulty": difficulty, "nonce": nonce,
        })
        h = hashlib.sha256(preimage.encode()).digest()
        if h[0] == 0:
            return preimage
    return None

async def mine_batch(session, server, n):
    """Mine n tokens on a server. Returns list of (secret, webcash_str)."""
    tokens = []
    for _ in range(n):
        secret = random_hex64()
        wc = f"e200.00000000:secret:{secret}"
        preimage = find_preimage(wc)
        if not preimage:
            continue
        async with session.post(f"{server}/api/v1/mining_report",
                json={"preimage": preimage, "legalese": {"terms": True}}) as resp:
            if resp.status == 200:
                tokens.append((secret, wc))
    return tokens

async def do_replace(session, server, wc, n1, n2):
    async with session.post(f"{server}/api/v1/replace", json={
        "webcashes": [wc],
        "new_webcashes": [f"e100.00000000:secret:{n1}", f"e100.00000000:secret:{n2}"],
        "legalese": {"terms": True},
    }) as resp:
        return resp.status

async def main():
    print("=" * 70)
    print("  Webycash Server Replace Benchmark (aiohttp, inside Docker)")
    print(f"  Servers: {', '.join(SERVERS)}")
    print("=" * 70)

    # Use a single TCP connector with many connections (keep-alive)
    connector = aiohttp.TCPConnector(limit=500, limit_per_host=200, ttl_dns_cache=300)
    async with aiohttp.ClientSession(connector=connector) as session:
        # Wait for servers
        for server in SERVERS:
            for _ in range(30):
                try:
                    async with session.get(f"{server}/api/v1/health") as resp:
                        if resp.status == 200:
                            print(f"  {server}: UP")
                            break
                except:
                    await asyncio.sleep(1)

        # Pre-mine
        N_PER = 3000
        print(f"\n--- Pre-mining {N_PER} tokens per server ---")
        t0 = time.time()
        tasks = [mine_batch(session, s, N_PER) for s in SERVERS]
        all_tokens = await asyncio.gather(*tasks)
        tokens = {s: t for s, t in zip(SERVERS, all_tokens)}
        total = sum(len(t) for t in tokens.values())
        elapsed = time.time() - t0
        print(f"  Mined {total} tokens in {elapsed:.1f}s ({total/elapsed:.0f}/s)\n")

        # Benchmark replace with increasing concurrency
        print("--- Replace benchmark (3 servers, connection-pooled aiohttp) ---")
        for concurrency in [1, 16, 64, 128, 256, 512, 1024]:
            # Build args from all servers
            args = []
            per_server = min(concurrency * 5, min(len(t) for t in tokens.values()))
            for server in SERVERS:
                for i in range(per_server):
                    if i >= len(tokens[server]):
                        break
                    _, wc = tokens[server][i]
                    args.append((server, wc, random_hex64(), random_hex64()))

            N = len(args)
            if N < 10:
                print(f"  c={concurrency}: not enough tokens")
                break

            # Concurrent replace requests with semaphore
            sem = asyncio.Semaphore(concurrency)
            ok_count = 0
            err_count = 0

            async def bounded_replace(s, wc, n1, n2):
                nonlocal ok_count, err_count
                async with sem:
                    status = await do_replace(session, s, wc, n1, n2)
                    if status == 200:
                        ok_count += 1
                    else:
                        err_count += 1

            t0 = time.time()
            await asyncio.gather(*[bounded_replace(s, wc, n1, n2) for s, wc, n1, n2 in args])
            elapsed = time.time() - t0
            tps = N / elapsed
            print(f"  c={concurrency:<5} {N:>6} ops  {elapsed:.2f}s  {tps:>8.0f} TPS  ({ok_count} ok, {err_count} err)")

            # Consume tokens
            for server in SERVERS:
                tokens[server] = tokens[server][per_server:]

        print("\n" + "=" * 70)

asyncio.run(main())
