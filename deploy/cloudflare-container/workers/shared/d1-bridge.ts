// Container-only D1 bridge shared by every webycash-server flavor Worker
// (webcash, rgb, rgb-collectible, voucher).
//
// The native ledger binary (`WEBCASH_DB_BACKEND=worker_proxy`,
// `crates/storage/src/worker_proxy_backend.rs`) POSTs SQL batches to
// `{WORKER_PROXY_URL}/__d1/batch`. That URL is the virtual origin
// `http://d1.webycash.internal`, which only exists inside the container: the
// Worker registers it with `@cloudflare/containers` `outboundByHost`, so the
// handler runs in the Workers runtime with the D1 binding and is not routable
// from the internet. The Worker's public `fetch()` has no database route.
//
// Defence in depth: the bridge also requires `Authorization: Bearer <token>`
// (`WORKER_PROXY_AUTH_TOKEN`, >= 32 chars) and fails closed without it.

export const D1_BRIDGE_HOST = "d1.webycash.internal";
export const D1_BRIDGE_ORIGIN = `http://${D1_BRIDGE_HOST}`;
export const D1_BRIDGE_PATH = "/__d1/batch";
export const MIN_TOKEN_LEN = 32;

/** The subset of `D1Database` the bridge uses (lets tests supply a fake). */
export interface D1Like {
  prepare(sql: string): { bind(...values: unknown[]): unknown };
  batch(stmts: unknown[]): Promise<Array<{ results?: unknown[] }>>;
}

interface BatchStmt {
  sql: string;
  params?: unknown[];
}

/**
 * Serve one bridge request. `token` is the Worker secret the container must
 * present. Wire: `{"statements":[{"sql","params"}]}` →
 * `{"ok":true,"results":[{"rows":[…]}]}`; D1 errors (SQLite RAISE labels such
 * as double_spend / input_missing) come back as `{"ok":false,"error":…}` 400 so
 * the store maps them to ledger diagnostics.
 */
export async function handleD1Bridge(
  request: Request,
  db: D1Like,
  token: string | undefined,
): Promise<Response> {
  const url = new URL(request.url);
  if (url.hostname !== D1_BRIDGE_HOST || url.pathname !== D1_BRIDGE_PATH) {
    return json({ ok: false, error: "not found" }, 404);
  }
  if (request.method !== "POST") {
    return json({ ok: false, error: "method not allowed" }, 405);
  }
  const want = token ?? "";
  if (want.length < MIN_TOKEN_LEN) {
    return json({ ok: false, error: "d1 bridge disabled (no token configured)" }, 503);
  }
  const auth = request.headers.get("authorization") ?? "";
  const got = auth.startsWith("Bearer ") ? auth.slice(7) : "";
  if (!timingSafeEqualStr(got, want)) {
    return json({ ok: false, error: "unauthorized" }, 401);
  }
  let statements: BatchStmt[];
  try {
    const body = (await request.json()) as { statements?: unknown };
    if (!Array.isArray(body.statements)) {
      return json({ ok: false, error: "statements must be an array" }, 400);
    }
    statements = body.statements as BatchStmt[];
  } catch (e) {
    return json({ ok: false, error: `body parse: ${str(e)}` }, 400);
  }
  try {
    const stmts = statements.map((s) => {
      if (typeof s?.sql !== "string") throw new Error("statement sql must be a string");
      if (s.params !== undefined && !Array.isArray(s.params)) {
        throw new Error("statement params must be an array");
      }
      return db.prepare(s.sql).bind(...(s.params ?? []));
    });
    const results = await db.batch(stmts);
    return json({ ok: true, results: results.map((r) => ({ rows: r.results ?? [] })) }, 200);
  } catch (e) {
    return json({ ok: false, error: str(e) }, 400);
  }
}

/** Constant-time string compare (length leak only). */
export function timingSafeEqualStr(a: string, b: string): boolean {
  const ea = new TextEncoder().encode(a);
  const eb = new TextEncoder().encode(b);
  let diff = ea.length ^ eb.length;
  const n = Math.max(ea.length, eb.length);
  for (let i = 0; i < n; i++) diff |= (ea[i] ?? 0) ^ (eb[i] ?? 0);
  return diff === 0;
}

function json(body: { ok: boolean; error?: string; results?: unknown }, status: number): Response {
  if (status >= 400 && status !== 400) {
    // Operator signal for refusals (never SQL params or tokens).
    console.error(`d1-bridge: ${status} ${body.error ?? ""}`);
  }
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function str(e: unknown): string {
  if (e instanceof Error) return e.message;
  if (typeof e === "string") return e;
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}
