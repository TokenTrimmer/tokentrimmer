# Multi-stage build for the `tt` CLI binary (Gateway + Inspect + Plan).
#
# Layers:
#   1. chef    — installs cargo-chef and computes a dependency recipe.
#   2. planner — emits recipe.json from the current source tree.
#   3. builder — cooks dependencies (cached), then builds the release binary.
#   4. runtime — distroless/cc; just the binary + CA certs. ~30 MB final.
#
# Build:
#   docker build -t ghcr.io/tokentrimmer/tt-cli:dev .
# Run:
#   docker run --rm -p 8080:8080 ghcr.io/tokentrimmer/tt-cli:dev tt gateway

# --- 1. chef ----------------------------------------------------------------
FROM rust:1.88-bookworm AS chef
RUN cargo install --locked cargo-chef
WORKDIR /app

# --- 2. planner -------------------------------------------------------------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- 3. builder -------------------------------------------------------------
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cook dependencies — this layer is cached unless deps change.
RUN cargo chef cook --release --recipe-path recipe.json
# Now copy the source and build only what's not in the dep cache.
COPY . .
RUN cargo build --release -p tt-cli
RUN strip /app/target/release/tt

# --- 4. runtime -------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
WORKDIR /app
COPY --from=builder /app/target/release/tt /usr/local/bin/tt
# Migrations are embedded into the binary via sqlx::migrate!(); no runtime copy
# is required. If we ever move to runtime-loaded migrations, also COPY
# crates/core/migrations /app/migrations here.

USER nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/tt"]
CMD ["gateway"]
