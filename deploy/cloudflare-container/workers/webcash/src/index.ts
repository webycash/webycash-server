/// <reference types="@cloudflare/workers-types" />
//
// Edge Worker for the Webcash flavor of webycash-server.
//
// Two routes:
//   1. POST /__d1/batch — internal endpoint the Container calls back to
//      whenever the native LedgerStore needs to talk to D1. Translates
//      {sql, params}[] into D1Database.prepare().bind() and forwards the
//      whole batch atomically via `env.WEBYCASH_D1.batch(stmts)`.
//   2. Anything else — forwarded to the Container's hyper server, which
//      runs the existing webycash-server-webcash binary with
//      WEBCASH_DB_BACKEND=worker_proxy.

import { Container } from "@cloudflare/containers";

interface Env {
  WEBYCASH_D1: D1Database;
  WEBYCASH_CONTAINER: DurableObjectNamespace<WebycashContainer>;
  WEBCASH_MODE?: string;
}

/**
 * Container class. Cloudflare Containers are addressed via a Durable
 * Object namespace; each DO instance owns one running container.
 * The native binary in the image binds to port 8080.
 */
export class WebycashContainer extends Container<Env> {
  defaultPort = 8080;
  sleepAfter = "10m";

  // The container reaches the parent Worker's `/__d1/batch` endpoint
  // through a loopback URL. For `wrangler dev --local` this is the
  // Docker-host-mapped loopback (`host.docker.internal:8787`). In
  // production the same hostname pattern resolves to the Worker's edge
  // address via Cloudflare's internal service mesh.
  envVars = {
    WORKER_PROXY_URL: "http://host.docker.internal:8787",
  };
}

interface BatchStmt {
  sql: string;
  params: unknown[];
}

interface BatchRequest {
  statements: BatchStmt[];
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/__d1/batch" && request.method === "POST") {
      return handleD1Batch(request, env);
    }

    // Forward everything else to the Container. One DO per asset
    // namespace keeps the Container instance warm across requests.
    const id = env.WEBYCASH_CONTAINER.idFromName("primary");
    const stub = env.WEBYCASH_CONTAINER.get(id);
    return stub.fetch(request);
  },
};

async function handleD1Batch(request: Request, env: Env): Promise<Response> {
  let body: BatchRequest;
  try {
    body = (await request.json()) as BatchRequest;
  } catch (e) {
    return jsonResponse({ ok: false, error: `body parse: ${stringifyError(e)}` }, 400);
  }
  if (!Array.isArray(body.statements)) {
    return jsonResponse({ ok: false, error: "statements must be an array" }, 400);
  }

  try {
    const stmts = body.statements.map((s) =>
      env.WEBYCASH_D1.prepare(s.sql).bind(...(s.params ?? [])),
    );
    const results = await env.WEBYCASH_D1.batch(stmts);
    const wire = results.map((r) => ({ rows: r.results ?? [] }));
    return jsonResponse({ ok: true, results: wire }, 200);
  } catch (e) {
    // D1 surfaces SQLite ABORTs (double_spend / input_missing / UNIQUE
    // constraint) as exceptions; the message preserves the RAISE label
    // so the Container's WorkerProxyStore can map back to a
    // ReplaceResult::Failed diagnostic.
    return jsonResponse({ ok: false, error: stringifyError(e) }, 400);
  }
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function stringifyError(e: unknown): string {
  if (e instanceof Error) return e.message;
  if (typeof e === "string") return e;
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}
