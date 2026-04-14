FROM rust:1.85-slim AS builder
WORKDIR /app

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY config/ config/

# Build
RUN cargo build --release --bin webycash-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/webycash-server /usr/local/bin/
COPY --from=builder /app/config/ /etc/webycash/

EXPOSE 8080
CMD ["webycash-server", "--config", "/etc/webycash/testnet.toml"]
