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

# Dependency caching layer: copy manifests and build deps first
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    cargo build --release && \
    rm -rf src

# Copy real source and rebuild
COPY src/ src/
RUN touch src/main.rs src/lib.rs && cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/fynd /usr/local/bin/fynd

EXPOSE 3000 9898

ENTRYPOINT ["/usr/local/bin/fynd", "serve"]
