# Build
FROM rust:1.83-bookworm AS builder
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
COPY webui ./webui
RUN cargo build --release

# Runtime
FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/mimo_desktop_bridge /usr/local/bin/mimo_desktop_bridge
ENV MIMO_DESKTOP_BRIDGE_DATA=/data
# Config dir is resolved via directories crate; for container we pass --config-dir
RUN mkdir -p /data
EXPOSE 8787
ENTRYPOINT ["mimo_desktop_bridge"]
CMD ["server", "--host", "0.0.0.0", "--port", "8787", "--config-dir", "/data"]
