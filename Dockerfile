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
COPY bench-harness/Cargo.toml bench-harness/
COPY fynd-core/Cargo.toml fynd-core/
COPY fynd-rpc/Cargo.toml fynd-rpc/
COPY fynd-rpc-types/Cargo.toml fynd-rpc-types/
COPY clients/rust/Cargo.toml clients/rust/
COPY tools/benchmark/Cargo.toml tools/benchmark/
COPY tools/common/Cargo.toml tools/common/
COPY tools/fynd-swap-cli/Cargo.toml tools/fynd-swap-cli/
COPY tools/erc20-overrides/Cargo.toml tools/erc20-overrides/
COPY tools/fynd-gas-audit/Cargo.toml tools/fynd-gas-audit/
COPY tools/record-market/Cargo.toml tools/record-market/
COPY tools/hindsight/Cargo.toml tools/hindsight/
COPY test-fixtures/Cargo.toml test-fixtures/
RUN mkdir -p src fynd-core/src bench-harness/src bench-harness/benches fynd-rpc/src \
        fynd-rpc-types/src \
        clients/rust/src tools/benchmark/src tools/common/src tools/fynd-swap-cli/src \
        tools/erc20-overrides/src tools/fynd-gas-audit/src \
        tools/record-market/src tools/hindsight/src test-fixtures/src && \
    echo "" > bench-harness/src/lib.rs && \
    echo "fn main() {}" > bench-harness/benches/algorithm_bench.rs && \
    echo "fn main() {}" > bench-harness/benches/profile.rs && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    echo "" > fynd-core/src/lib.rs && \
    echo "" > fynd-rpc/src/lib.rs && \
    echo "" > fynd-rpc-types/src/lib.rs && \
    echo "" > clients/rust/src/lib.rs && \
    echo "fn main() {}" > tools/benchmark/src/main.rs && \
    echo "" > tools/common/src/lib.rs && \
    echo "fn main() {}" > tools/fynd-swap-cli/src/main.rs && \
    echo "" > tools/erc20-overrides/src/lib.rs && \
    echo "fn main() {}" > tools/fynd-gas-audit/src/main.rs && \
    echo "fn main() {}" > tools/record-market/src/main.rs && \
    echo "fn main() {}" > tools/hindsight/src/main.rs && \
    echo "" > test-fixtures/src/lib.rs && \
    cargo build --release --package fynd --features fynd-rpc/experimental --package fynd-swap-cli --package hindsight && \
    rm -rf src fynd-core/src bench-harness/src bench-harness/benches fynd-rpc/src \
        fynd-rpc-types/src \
        clients/rust/src tools/benchmark/src tools/common/src tools/fynd-swap-cli/src \
        tools/erc20-overrides/src tools/fynd-gas-audit/src \
        tools/record-market/src tools/hindsight/src test-fixtures/src

# Copy real source and rebuild
COPY src/ src/
COPY fynd-core/src/ fynd-core/src/
COPY fynd-rpc/src/ fynd-rpc/src/
COPY fynd-rpc-types/src/ fynd-rpc-types/src/
COPY clients/rust/src/ clients/rust/src/
COPY tools/fynd-swap-cli/src/ tools/fynd-swap-cli/src/
COPY tools/erc20-overrides/src/ tools/erc20-overrides/src/
COPY tools/common/src/ tools/common/src/
COPY tools/hindsight/src/ tools/hindsight/src/
RUN mkdir -p tools/benchmark/src tools/fynd-gas-audit/src \
        tools/record-market/src test-fixtures/src bench-harness/src bench-harness/benches && \
    echo "" > bench-harness/src/lib.rs && \
    echo "fn main() {}" > bench-harness/benches/algorithm_bench.rs && \
    echo "fn main() {}" > bench-harness/benches/profile.rs && \
    echo "fn main() {}" > tools/benchmark/src/main.rs && \
    echo "fn main() {}" > tools/fynd-gas-audit/src/main.rs && \
    echo "fn main() {}" > tools/record-market/src/main.rs && \
    echo "" > test-fixtures/src/lib.rs && \
    touch src/main.rs src/lib.rs fynd-core/src/lib.rs fynd-rpc/src/lib.rs \
        fynd-rpc-types/src/lib.rs clients/rust/src/lib.rs \
        tools/fynd-swap-cli/src/main.rs tools/erc20-overrides/src/lib.rs \
        tools/common/src/lib.rs tools/hindsight/src/main.rs && \
    cargo build --release --package fynd --features fynd-rpc/experimental --package fynd-swap-cli --package hindsight

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/fynd /usr/local/bin/fynd
COPY --from=builder /app/target/release/fynd-swap-cli /usr/local/bin/fynd-swap-cli
COPY --from=builder /app/target/release/hindsight /usr/local/bin/hindsight

EXPOSE 3000 9898

ENTRYPOINT ["/usr/local/bin/fynd"]
