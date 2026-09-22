# syntax=docker/dockerfile:1
# Multi-stage build with cargo-chef: dependencies are compiled in their own layer, so a code-only change
# rebuilds in seconds. Railway always prefers a Dockerfile when one is present.

FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json --bin gum-server
COPY . .
RUN cargo build --release --bin gum-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 gum
WORKDIR /app
COPY --from=builder /app/target/release/gum-server /usr/local/bin/gum-server
COPY config/default.toml config/default.toml
USER gum
ENV RUST_LOG=info,sqlx=warn,hyper=warn
EXPOSE 8080
CMD ["gum-server"]
