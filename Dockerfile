# Build stage
FROM rust:1.85-slim as builder

WORKDIR /usr/src/app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifest files first for better layer caching
COPY Cargo.toml Cargo.lock ./

# Create dummy source to build dependencies only
RUN mkdir -p src/bin && \
    echo "fn main() {}" > src/bin/nntp-proxy.rs && \
    echo "fn main() {}" > src/bin/nntp-cache-proxy.rs && \
    mkdir -p src && \
    echo "" > src/lib.rs && \
    cargo build --release --bin nntp-proxy --bin nntp-cache-proxy && \
    rm -rf src

# Copy source code
COPY src/ src/

# Build the application (dependencies are already cached)
RUN cargo build --release --bin nntp-proxy

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies and netcat for health checks
RUN apt-get update && apt-get install -y \
    ca-certificates \
    netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd --create-home --shell /bin/bash --uid 1000 nntp-proxy

# Create directories
RUN mkdir -p /etc/nntp-proxy /var/log/nntp-proxy && \
    chown -R nntp-proxy:nntp-proxy /var/log/nntp-proxy /etc/nntp-proxy

# Copy the binary from builder stage
COPY --from=builder /usr/src/app/target/release/nntp-proxy /usr/local/bin/nntp-proxy

# Switch to non-root user
USER nntp-proxy

# Expose default proxy port
EXPOSE 8119

# Environment variables with defaults
ENV NNTP_PROXY_PORT=8119 \
    NNTP_PROXY_ROUTING_MODE=hybrid \
    NNTP_PROXY_CONFIG=/etc/nntp-proxy/config.toml \
    RUST_LOG=info

# Health check using netcat
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD nc -z localhost ${NNTP_PROXY_PORT} || exit 1

# Run the application
# The config file is optional - if not present and NNTP_SERVER_0_* env vars are set,
# the proxy will use environment variable configuration
CMD ["/usr/local/bin/nntp-proxy"]
