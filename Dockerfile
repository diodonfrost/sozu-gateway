# Multi-stage build for the Sōzu gateway controller (control plane only).
# The data plane (Sōzu) runs as a separate container from its own image.

FROM rust:1-bookworm AS builder
# sozu-command-lib's build.rs runs prost-build, which needs protoc.
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release -p sozu-gw-controller \
    && strip target/release/sozu-gw-controller

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/sozu-gw-controller /usr/local/bin/sozu-gw-controller
# Run as the same unprivileged uid as the Sōzu sidecar so both can share the
# command socket created on the emptyDir volume.
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/sozu-gw-controller"]
