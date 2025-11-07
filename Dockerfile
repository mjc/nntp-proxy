# Stage 1: build the Rust binary using nightly
FROM rustlang/rust:nightly AS builder
WORKDIR /usr/src/app

# Install dependencies for building
RUN apt-get update && apt-get install -y pkg-config libssl-dev ca-certificates && rm -rf /var/lib/apt/lists/*

# Copy source code
COPY Cargo.toml ./
COPY src/ src/

# Build release
RUN cargo build --release --bins

# Stage 2: final lightweight image
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /usr/src/app/target/release/nntp-proxy /usr/local/bin/nntp-proxy

# Working directory for mounted config
WORKDIR /config

# Run nntp-proxy
CMD ["nntp-proxy", "-c", "/config/config.toml"]
