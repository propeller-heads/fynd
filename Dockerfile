# Stage 1: Generate dependency recipe
FROM lukemathwalker/cargo-chef:latest-rust-1.92-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 2: Build dependencies + binary
FROM chef AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    pkg-config \
    libssl-dev \
    build-essential \
    perl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build -p fynd --locked --release --features experimental && \
    cp /app/target/release/fynd /usr/local/bin/fynd

# Stage 3: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -r -s /bin/false -u 1000 fynd

COPY --from=builder /usr/local/bin/fynd /usr/local/bin/fynd

USER fynd

EXPOSE 3000 9898

ENTRYPOINT ["/usr/local/bin/fynd", "serve"]
