# Build stage
FROM rust:1.80-slim as builder

WORKDIR /usr/src/app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifest files
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src/ src/

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd --create-home --shell /bin/bash nntp-proxy

# Create directories
RUN mkdir -p /etc/nntp-proxy /var/log/nntp-proxy
RUN chown nntp-proxy:nntp-proxy /var/log/nntp-proxy

# Copy the binary from builder stage
COPY --from=builder /usr/src/app/target/release/nntp-proxy /usr/local/bin/nntp-proxy

# Copy default config
COPY config.toml /etc/nntp-proxy/config.toml
RUN chown -R nntp-proxy:nntp-proxy /etc/nntp-proxy

# Switch to non-root user
USER nntp-proxy

# Expose port
EXPOSE 8119

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD nc -z localhost 8119 || exit 1

# Run the application
CMD ["/usr/local/bin/nntp-proxy", "--config", "/etc/nntp-proxy/config.toml"]
