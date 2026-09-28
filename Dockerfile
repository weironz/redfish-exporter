FROM rust:1.98.1-slim-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/redfish-exporter /usr/local/bin/redfish-exporter
ENV REDFISH_EXPORTER_BIND=127.0.0.1:19626
ENV REDFISH_EXPORTER_TARGETS_FILE=/run/secrets/redfish-targets.json
ENTRYPOINT ["/usr/local/bin/redfish-exporter"]
