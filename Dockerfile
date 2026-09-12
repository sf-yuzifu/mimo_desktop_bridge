# Build
FROM rust:1.83-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY webui ./webui
RUN cargo build --release

# Runtime
FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/mimo_desktop_bridge /usr/local/bin/mimo_desktop_bridge
# Config dir is resolved via directories crate; for container we pass --config-dir
RUN mkdir -p /data
EXPOSE 8787
# Liveness via the unauthenticated /healthz endpoint (HTTP; override the
# check if you terminate TLS inside the container).
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8787/healthz || exit 1
ENTRYPOINT ["mimo_desktop_bridge"]
CMD ["server", "--host", "0.0.0.0", "--port", "8787", "--config-dir", "/data"]
