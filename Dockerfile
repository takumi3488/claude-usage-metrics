# Chef stage: Install cargo-chef
FROM lukemathwalker/cargo-chef:latest-rust-1.92-alpine@sha256:5a1e9cdd661f398f81646ae6f44c2663562ccefd3deeb74d06677b7e1dbfb730 AS chef

WORKDIR /app

RUN cargo install cargo-chef

# Planner stage: Prepare recipe.json
FROM chef AS planner

COPY . .

RUN cargo chef prepare --recipe-path recipe.json

# Builder stage: Build dependencies and application
FROM chef AS builder

# Install required dependencies for musl build
RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static

COPY --from=planner /app/recipe.json recipe.json

# Build dependencies (this layer will be cached)
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

# Copy source code and build application
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release && \
  cp /app/target/release/claude-usage-metrics /tmp/claude-usage-metrics

# Runtime stage: Create minimal production image with static binary
FROM gcr.io/distroless/static-debian12:nonroot@sha256:cba10d7abd3e203428e86f5b2d7fd5eb7d8987c387864ae4996cf97191b33764

WORKDIR /app

COPY --from=builder /tmp/claude-usage-metrics .

CMD ["./claude-usage-metrics"]
