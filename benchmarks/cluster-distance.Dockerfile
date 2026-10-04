# syntax=docker/dockerfile:1
# The build `benchmarks/cluster-distance.sh` measures in: a release node and the
# release `serving` test binary, on Linux, with `tc` to inject the distance.
FROM rust:1.98-slim-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang libclang-dev build-essential iproute2 \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . /src/
# The target cache outlives the sources copied over it, so every crate is
# touched newer than its last build: a stale fingerprint would measure old code.
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    find /src/crates -type f -exec touch {} + \
 && CARGO_INCREMENTAL=0 cargo build --release -p tessari-cli --bin tessaridb \
 && CARGO_INCREMENTAL=0 cargo test --release -p tessari-cli --test serving --no-run \
 && mkdir -p /out \
 && cp target/release/tessaridb /out/tessaridb \
 && cp "$(ls -t target/release/deps/serving-* | grep -v '\.d$' | head -1)" /out/serving
