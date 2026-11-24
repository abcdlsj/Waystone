# syntax=docker/dockerfile:1
#
# Build from the repository root (the workspace manifest lives there):
#
#   docker build -f crates/waystoned/Dockerfile -t waystoned .
#
# The binary is linked against musl, so it is fully static and the runtime
# image only adds a shell for `docker exec` and an unprivileged user.

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# The pinned toolchain file would make rustup download another toolchain;
# the image's stable compiler is what we want here.
RUN rm -f rust-toolchain.toml
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p waystoned \
    && install -D -m 0755 target/release/waystoned /out/waystoned

FROM alpine:3.20
RUN adduser -D -H -u 10001 waystone \
    && mkdir /data \
    && chown waystone:waystone /data \
    && chmod 0700 /data
COPY --from=build /out/waystoned /usr/local/bin/waystoned
USER waystone
ENV WAYSTONE_DATA_DIR=/data
VOLUME /data
# Panel + signaling, and STUN/TURN. Relayed allocations use extra UDP ports,
# which is why the README runs the container with host networking.
EXPOSE 8443/tcp 3478/udp
ENTRYPOINT ["waystoned"]
