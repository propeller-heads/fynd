# Stage 1: Build
FROM rust:1.92-bookworm AS builder

# Install system dependencies needed by aws-lc-sys, openssl-sys, etc.
RUN apt-get update && apt-get install -y \
    cmake \
    pkg-config \
    libssl-dev \
    build-essential \
    perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Dependency caching layer: copy all workspace manifests and build deps first
COPY Cargo.toml Cargo.lock ./
COPY fynd-core/Cargo.toml fynd-core/
COPY fynd-rpc/Cargo.toml fynd-rpc/
COPY fynd-rpc-types/Cargo.toml fynd-rpc-types/
COPY clients/rust/Cargo.toml clients/rust/
COPY tools/benchmark/Cargo.toml tools/benchmark/
COPY tools/fynd-swap-cli/Cargo.toml tools/fynd-swap-cli/
RUN mkdir -p src fynd-core/src fynd-rpc/src fynd-rpc-types/src \
        clients/rust/src tools/benchmark/src tools/fynd-swap-cli/src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    echo "" > fynd-core/src/lib.rs && \
    echo "" > fynd-rpc/src/lib.rs && \
    echo "" > fynd-rpc-types/src/lib.rs && \
    echo "" > clients/rust/src/lib.rs && \
    echo "fn main() {}" > tools/benchmark/src/main.rs && \
    echo "fn main() {}" > tools/fynd-swap-cli/src/main.rs && \
    cargo build --release --package fynd && \
    rm -rf src fynd-core/src fynd-rpc/src fynd-rpc-types/src \
        clients/rust/src tools/benchmark/src tools/fynd-swap-cli/src

# Copy real source and rebuild
COPY src/ src/
COPY fynd-core/src/ fynd-core/src/
COPY fynd-rpc/src/ fynd-rpc/src/
COPY fynd-rpc-types/src/ fynd-rpc-types/src/
RUN touch src/main.rs src/lib.rs fynd-core/src/lib.rs fynd-rpc/src/lib.rs \
        fynd-rpc-types/src/lib.rs && \
    cargo build --release --package fynd

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/fynd /usr/local/bin/fynd

EXPOSE 3000 9898

ENTRYPOINT ["/usr/local/bin/fynd", "serve"]
