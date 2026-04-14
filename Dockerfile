# webycash-server Dockerfile
#
# Build modes:
#   --build-arg ENV=dev   → builds from local source (latest HEAD)
#   --build-arg ENV=prod  → installs from GitHub release binary
#
# Usage:
#   docker build --build-arg ENV=dev -t webycash-server .
#   docker build --build-arg ENV=prod --build-arg VERSION=v0.1.0 -t webycash-server .
#   docker run -p 8080:8080 -e WEBCASH_MODE=testnet webycash-server

ARG ENV=prod
ARG VERSION=v0.1.0

# ── Stage: dev (build from source) ──────────────────────────────────

FROM rust:1.85-alpine AS build-dev
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY . .
RUN cargo build --release --bin webycash-server && \
    strip target/release/webycash-server && \
    cp target/release/webycash-server /webycash-server

# ── Stage: prod (install from release) ──────────────────────────────

FROM alpine:3.21 AS build-prod
ARG VERSION
RUN apk add --no-cache curl tar && \
    ARCH=$(uname -m) && \
    case "$ARCH" in \
      x86_64)  ARTIFACT="webycash-server-linux-x86_64" ;; \
      aarch64) ARTIFACT="webycash-server-linux-aarch64" ;; \
      *)       echo "Unsupported arch: $ARCH" && exit 1 ;; \
    esac && \
    curl -fSL "https://github.com/webycash/webycash-server/releases/download/${VERSION}/${ARTIFACT}.tar.gz" \
      -o /tmp/server.tar.gz && \
    mkdir /tmp/extract && cd /tmp/extract && tar xzf /tmp/server.tar.gz && \
    cp /tmp/extract/webycash-server /webycash-server && \
    chmod +x /webycash-server

# ── Final stage: minimal alpine runtime ─────────────────────────────

FROM alpine:3.21 AS final-dev
COPY --from=build-dev /webycash-server /usr/local/bin/webycash-server
COPY config/ /etc/webycash/
RUN apk add --no-cache ca-certificates
EXPOSE 8080
ENTRYPOINT ["webycash-server"]
CMD ["--config", "/etc/webycash/testnet.toml"]

FROM alpine:3.21 AS final-prod
COPY --from=build-prod /webycash-server /usr/local/bin/webycash-server
COPY config/ /etc/webycash/
RUN apk add --no-cache ca-certificates
EXPOSE 8080
ENTRYPOINT ["webycash-server"]
CMD ["--config", "/etc/webycash/testnet.toml"]

# ── Select final stage based on ENV ─────────────────────────────────

FROM final-${ENV}
