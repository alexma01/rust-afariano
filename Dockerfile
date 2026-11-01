# syntax=docker/dockerfile:1.7
# rust-afariano server image. The Rust build runs natively on the build machine and
# cross-compiles to the target platform with zig (no QEMU for the compile); the
# final image is Alpine + git (the server shells out to git for every operation).
#
#   docker buildx build --platform linux/amd64 -t rust-afariano:latest --load .
#   docker buildx build --platform linux/amd64 --target binaries -o dist .   # binaries only

FROM --platform=$BUILDPLATFORM rust:1.98-alpine AS build
ARG TARGETARCH
RUN apk add --no-cache musl-dev zig
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked cargo-zigbuild
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    case "$TARGETARCH" in \
      amd64) T=x86_64-unknown-linux-musl ;; \
      arm64) T=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
    esac && \
    cargo zigbuild --release --locked -p rust-afariano -p rust-afariano-runner --target "$T" && \
    mkdir -p /out && cp "target/$T/release/rust-afariano" "target/$T/release/rust-afariano-runner" /out/

FROM scratch AS binaries
COPY --from=build /out/ /

FROM alpine:3.22
LABEL org.opencontainers.image.title="rust-afariano" \
      org.opencontainers.image.description="Self-hosted Git and GitLab-style CI in one small Rust binary" \
      org.opencontainers.image.source="https://github.com/frank10gm/rust-afariano" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
RUN apk add --no-cache git ca-certificates tini \
    && adduser -D -h /data -u 10001 rust-afariano
COPY --from=build /out/rust-afariano /usr/local/bin/rust-afariano
USER rust-afariano
ENV RUST_AFARIANO_DATA_DIR=/data \
    RUST_AFARIANO_HTTP_ADDR=0.0.0.0:8080 \
    RUST_AFARIANO_SSH_ADDR=0.0.0.0:2222
VOLUME /data
EXPOSE 8080 2222
HEALTHCHECK --interval=30s --timeout=3s CMD wget -qO- http://127.0.0.1:8080/healthz >/dev/null || exit 1
ENTRYPOINT ["/sbin/tini", "--", "rust-afariano"]
