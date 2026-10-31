/// <reference types="@cloudflare/workers-types" />
//
// Edge Worker for the RGB20 fungible flavor of webycash-server.
//
// Public surface: every request is forwarded to the Container's hyper server
// (the webycash-server binary with WEBCASH_DB_BACKEND=worker_proxy). There is
// no database route on the public origin.
//
// D1: the binary reaches D1 only through the container-only bridge
// (`../../shared/d1-bridge.ts`), registered below with `outboundByHost` on the
// virtual origin `http://d1.webycash.internal`. Requires the Worker secret
// `WORKER_PROXY_AUTH_TOKEN` (>= 32 chars); without it the bridge fails closed.

import { Container, ContainerProxy, type OutboundHandlerContext } from "@cloudflare/containers";
import { D1_BRIDGE_HOST, D1_BRIDGE_ORIGIN, handleD1Bridge } from "../../shared/d1-bridge.ts";

// Required by @cloudflare/containers for outbound interception.
export { ContainerProxy };

interface Env {
  WEBYCASH_D1: D1Database;
  WEBYCASH_CONTAINER: DurableObjectNamespace<RgbContainer>;
  WORKER_PROXY_AUTH_TOKEN?: string;
  WEBYCASH_ISSUERS?: string;
  WEBYCASH_ISSUER_PGP_CERTS?: string;
}

export class RgbContainer extends Container<Env> {
  defaultPort = 8080;
  sleepAfter = "10m";
  envVars: Record<string, string>;

  constructor(ctx: DurableObjectState<{}>, env: Env) {
    super(ctx, env);
    this.envVars = {
      WORKER_PROXY_URL: D1_BRIDGE_ORIGIN,
      WORKER_PROXY_AUTH_TOKEN: env.WORKER_PROXY_AUTH_TOKEN ?? "",
      WEBYCASH_ISSUERS: env.WEBYCASH_ISSUERS ?? "",
      WEBYCASH_ISSUER_PGP_CERTS: env.WEBYCASH_ISSUER_PGP_CERTS ?? "",
    };
  }
}

// Assigned, not declared as a `static` class field: an ES2022 static field
// defines an own property that shadows the base-class static setter, so the
// handler would never be registered and bridge calls would fall through to the
// internet (this broke the referee container on 2026-10-31).
RgbContainer.outboundByHost = {
  [D1_BRIDGE_HOST]: (request: Request, env: Env, _ctx: OutboundHandlerContext) =>
    handleD1Bridge(request, env.WEBYCASH_D1, env.WORKER_PROXY_AUTH_TOKEN),
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    // One DO per asset namespace keeps the Container instance warm.
    const stub = env.WEBYCASH_CONTAINER.get(env.WEBYCASH_CONTAINER.idFromName("primary"));
    return stub.fetch(request);
  },
};
