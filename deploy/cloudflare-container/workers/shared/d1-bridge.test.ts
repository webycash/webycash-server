// Run: node --experimental-strip-types --test (see package.json "test").
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { D1_BRIDGE_ORIGIN, D1_BRIDGE_PATH, handleD1Bridge, type D1Like } from "./d1-bridge.ts";

const TOKEN = "t".repeat(40);
const URL_OK = D1_BRIDGE_ORIGIN + D1_BRIDGE_PATH;
const body = JSON.stringify({ statements: [{ sql: "SELECT ?1", params: [7] }] });

function fakeDb(fail?: string): D1Like & { executed: Array<{ sql: string; params: unknown[] }> } {
  const executed: Array<{ sql: string; params: unknown[] }> = [];
  return {
    executed,
    prepare: (sql: string) => ({ bind: (...params: unknown[]) => ({ sql, params }) }),
    async batch(stmts: unknown[]) {
      if (fail) throw new Error(fail);
      for (const s of stmts as Array<{ sql: string; params: unknown[] }>) executed.push(s);
      return stmts.map(() => ({ results: [{ x: 1 }] }));
    },
  };
}
const post = (url: string, init: RequestInit = {}) => new Request(url, { method: "POST", ...init });
const authed = { body, headers: { authorization: `Bearer ${TOKEN}` } };

test("authenticated batch on the container-only host executes", async () => {
  const db = fakeDb();
  const r = await handleD1Bridge(post(URL_OK, authed), db, TOKEN);
  assert.equal(r.status, 200);
  assert.deepEqual(await r.json(), { ok: true, results: [{ rows: [{ x: 1 }] }] });
  assert.deepEqual(db.executed, [{ sql: "SELECT ?1", params: [7] }]);
});

test("public worker origin is refused even with the token", async () => {
  const db = fakeDb();
  const r = await handleD1Bridge(
    post("https://webycash-cf-container-webcash.example.workers.dev" + D1_BRIDGE_PATH, authed),
    db,
    TOKEN,
  );
  assert.equal(r.status, 404);
  assert.equal(db.executed.length, 0);
});

test("fails closed without a configured token, and on missing/wrong bearer", async () => {
  const db = fakeDb();
  assert.equal((await handleD1Bridge(post(URL_OK, authed), db, undefined)).status, 503);
  assert.equal((await handleD1Bridge(post(URL_OK, authed), db, "short")).status, 503);
  assert.equal((await handleD1Bridge(post(URL_OK, { body }), db, TOKEN)).status, 401);
  const wrong = { body, headers: { authorization: `Bearer ${"u".repeat(40)}` } };
  assert.equal((await handleD1Bridge(post(URL_OK, wrong), db, TOKEN)).status, 401);
  assert.equal(db.executed.length, 0);
});

test("non-POST and malformed bodies are rejected without touching D1", async () => {
  const db = fakeDb();
  assert.equal((await handleD1Bridge(new Request(URL_OK), db, TOKEN)).status, 405);
  const bad = (b: string) => post(URL_OK, { body: b, headers: { authorization: `Bearer ${TOKEN}` } });
  assert.equal((await handleD1Bridge(bad("{"), db, TOKEN)).status, 400);
  assert.equal((await handleD1Bridge(bad('{"statements":1}'), db, TOKEN)).status, 400);
  assert.equal((await handleD1Bridge(bad('{"statements":[{"sql":1}]}'), db, TOKEN)).status, 400);
  assert.equal((await handleD1Bridge(bad('{"statements":[{"sql":"x","params":1}]}'), db, TOKEN)).status, 400);
  assert.equal(db.executed.length, 0);
});

test("D1 RAISE labels are surfaced for ledger diagnostics", async () => {
  const r = await handleD1Bridge(post(URL_OK, authed), fakeDb("D1_ERROR: double_spend"), TOKEN);
  assert.equal(r.status, 400);
  assert.deepEqual(await r.json(), { ok: false, error: "D1_ERROR: double_spend" });
});

// Structural guard for every flavor Worker (importing them needs the Workers
// runtime): no public DB route, bridge registered by assignment (an ES2022
// `static outboundByHost` field shadows the base setter), ContainerProxy
// exported, container pointed at the virtual origin, never a loopback/public URL.
for (const flavor of ["webcash", "rgb", "rgb-collectible", "voucher"]) {
  test(`${flavor} worker: container-only bridge, no public /__d1 route`, () => {
    const src = readFileSync(new URL(`../${flavor}/src/index.ts`, import.meta.url), "utf8");
    const code = src.replace(/^\s*\/\/.*$/gm, "");
    assert.doesNotMatch(code, /__d1/, "public fetch must not route /__d1/*");
    assert.doesNotMatch(code, /static\s+outboundByHost/);
    assert.match(code, /^\w+Container\.outboundByHost\s*=\s*\{/m);
    assert.match(code, /export \{ ContainerProxy \}/);
    assert.match(code, /WORKER_PROXY_URL:\s*D1_BRIDGE_ORIGIN/);
    assert.doesNotMatch(code, /host\.docker\.internal|workers\.dev|https?:\/\//);
  });
}
