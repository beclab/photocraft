# syntax=docker/dockerfile:1
# photocraft-agent-server: the HTTP service agents call to run engine commands on images.
# Build from the repository root: docker build -f packaging/agent/Dockerfile .
ARG RUST_VERSION=1.99
ARG DEBIAN_VERSION=bookworm

FROM rust:${RUST_VERSION}-slim-${DEBIAN_VERSION} AS builder
ARG PHOTOCRAFT_BUILD_SHA=
ARG PHOTOCRAFT_BUILD_DATE=
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p photocraft-agent-server --features heif \
    && install -Dm755 target/release/photocraft-agent-server /out/photocraft-agent-server \
    && /out/photocraft-agent-server --version

FROM debian:${DEBIAN_VERSION}-slim AS runtime
COPY --from=builder /out/photocraft-agent-server /usr/local/bin/photocraft-agent-server
COPY LICENSE-MIT LICENSE-APACHE NOTICE ATTRIBUTION.md /usr/share/doc/photocraft/
USER 1000:1000
ENV PHOTOCRAFT_AGENT_ADDR=0.0.0.0:8090 \
    TMPDIR=/tmp
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/photocraft-agent-server"]
