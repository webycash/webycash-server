/// <reference types="@cloudflare/workers-types" />
//
// Edge Worker for the RGB20 fungible flavor of webycash-server.
// Same shape as the webcash Worker — only the Container class name and
// the D1 binding differ. See workers/webcash/src/index.ts for inline
// docs on the /__d1/batch wire protocol.

import { Container } from "@cloudflare/containers";

interface Env {
  WEBYCASH_D1: D1Database;
  WEBYCASH_CONTAINER: DurableObjectNamespace<RgbContainer>;
  WEBYCASH_ISSUERS?: string;
  WEBYCASH_ISSUER_PGP_CERTS?: string;
}

export class RgbContainer extends Container<Env> {
  defaultPort = 8080;
  sleepAfter = "10m";
  envVars: Record<string, string>;

  constructor(ctx: DurableObjectState<{}>, env: Env) {
    super(ctx, env);
    // Container reads these at boot: WORKER_PROXY_URL is the loopback
    // back to this Worker's `/__d1/batch`; the issuer env-vars populate
    // the IssuerRegistry for /api/v1/issue.
    this.envVars = {
      WORKER_PROXY_URL: "http://host.docker.internal:8787",
      WEBYCASH_ISSUERS: env.WEBYCASH_ISSUERS ?? "",
      WEBYCASH_ISSUER_PGP_CERTS: env.WEBYCASH_ISSUER_PGP_CERTS ?? "",
    };
  }
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
