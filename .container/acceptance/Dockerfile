FROM rust:1.96-bookworm AS build

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y \
    build-essential pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=compute-acceptance-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=compute-acceptance-cargo-target,target=/src/target \
    cargo build --locked --release -p compute-cli && \
    cp target/release/compute /tmp/compute

FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y \
    ca-certificates curl tar unzip xz-utils && \
    rm -rf /var/lib/apt/lists/*
COPY --from=build /tmp/compute /usr/local/bin/compute
ENTRYPOINT ["/usr/local/bin/compute"]
