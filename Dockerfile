# Build stage - use nightly for let-chains support
FROM rustlang/rust:nightly-slim AS builder

WORKDIR /usr/src/app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy everything and build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

# Build the application
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

# Environment variables
# Proxy settings
ENV NNTP_PROXY_PORT=8119 \
    NNTP_PROXY_ROUTING_MODE=hybrid \
    NNTP_PROXY_CONFIG=/etc/nntp-proxy/config.toml \
    NNTP_PROXY_THREADS="" \
    RUST_LOG=info

# Backend server configuration (example - override these)
# Server 0 (required if no config file)
# ENV NNTP_SERVER_0_HOST=news.example.com
# ENV NNTP_SERVER_0_PORT=119
# ENV NNTP_SERVER_0_NAME="News Server 1"
# ENV NNTP_SERVER_0_USERNAME=""
# ENV NNTP_SERVER_0_PASSWORD=""
# ENV NNTP_SERVER_0_MAX_CONNECTIONS=10

# Server 1 (optional - for load balancing)
# ENV NNTP_SERVER_1_HOST=news2.example.com
# ENV NNTP_SERVER_1_PORT=119
# ENV NNTP_SERVER_1_NAME="News Server 2"
# ENV NNTP_SERVER_1_USERNAME=""
# ENV NNTP_SERVER_1_PASSWORD=""
# ENV NNTP_SERVER_1_MAX_CONNECTIONS=10

# Health check using netcat
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD sh -c "nc -z localhost ${NNTP_PROXY_PORT} || exit 1"

# Run the application
CMD ["/usr/local/bin/nntp-proxy"]
