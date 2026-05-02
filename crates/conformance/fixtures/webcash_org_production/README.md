# webcash.org production fixtures

Captures from `https://webcash.org` taken on 2026-04-25 (UTC). Each fixture is a
captured request/response pair with full headers and body. The Webcash flavor
of the new server MUST produce byte-identical bodies for matching inputs;
header set must include the documented CORS, HSTS, and Content-Type values.

## Capture environment

- Tool: `curl 8.x` (no flags that alter byte content)
- User-Agent: `webycash-conformance-harness/0.1`
- TLS: HTTP/1.1 over HTTPS, no upgrade

## Quirks observed (must preserve)

1. **Content-Type for JSON bodies is `text/html; charset=UTF-8`** — production runs
   on Python Tornado which defaults to `text/html` even for JSON payloads. Our
   server currently emits `application/json`; M1 must add a Webcash-only
   override OR document the divergence as acceptable. Decision deferred to M1.
2. **Amount normalization** — request token `e1.0:public:...` is echoed back as
   `e1:public:...` in `health_check` response keys. Trailing `.0` is stripped.
3. **`/api/v1/stats` does NOT exist on production** — returns 404. Our existing
   server exposes it; preserving compat means EITHER hiding it on Webcash flavor
   OR documenting as a webycash extension.
4. **Empty replace returns success** — `/api/v1/replace` with empty input and
   output arrays returns `{"status": "success"}`. Likely an unintended trivial
   case but we must not break it.
5. **Malformed bodies return bare HTML 500** — no JSON envelope on parse errors.
6. **Terms endpoint is `/terms/text`** (not `/terms`).
