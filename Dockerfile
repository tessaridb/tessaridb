# TessariDB, as a container.
#
# Two stages, because the toolchain that builds this is two orders of magnitude
# larger than the binary it produces: the builder carries a Rust toolchain, a C++
# compiler and libclang, and the image somebody pulls carries one statically
# linked executable and the C++ runtime it needs.
#
# The build is slow, and it is slow for one reason worth stating up front: the
# storage engine compiles a vendored RocksDB — and lz4 and zstd with it — from
# source, which dominates everything else here. There is no dependency-caching
# stage below because a nineteen-crate workspace of path dependencies cannot be
# pre-warmed without stubbing out nineteen crates' sources, and that scaffolding
# breaks silently the first time a crate is added. The cost is paid on a rebuild
# instead, where it is visible.

# ── build ───────────────────────────────────────────────────────────────────
FROM rust:1.98-slim-bookworm AS build

# `clang` and `libclang-dev` because the storage engine's bindings are generated
# by bindgen, which loads libclang at build time and panics without it. The C++
# toolchain because the same crate compiles RocksDB itself.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      clang libclang-dev build-essential \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# `--locked` rather than a plain build: the lock file is committed, and a build
# that silently resolved a different dependency graph than the one the tests ran
# against is a build nobody can reproduce. `-p tessari-cli --bin tessaridb`
# because the workspace holds nineteen crates and the image wants one binary.
RUN cargo build --release --locked -p tessari-cli --bin tessaridb \
 && /src/target/release/tessaridb --version

# ── run ─────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# `libstdc++6` because RocksDB is C++ and the binary links it dynamically; it is
# usually already present in this base, and naming it is what stops that from
# being an assumption. `bash` for the health check, which speaks HTTP over
# `/dev/tcp` rather than adding an HTTP client to a database image.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libstdc++6 bash \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /var/lib/tessaridb --shell /usr/sbin/nologin tessaridb \
 && mkdir -p /var/lib/tessaridb \
 && chown tessaridb:tessaridb /var/lib/tessaridb

COPY --from=build /src/target/release/tessaridb /usr/local/bin/tessaridb
COPY docker/entrypoint.sh /usr/local/bin/tessaridb-entrypoint
COPY docker/healthcheck.sh /usr/local/bin/tessaridb-healthcheck
COPY LICENSE /usr/share/doc/tessaridb/LICENSE
RUN chmod +x /usr/local/bin/tessaridb-entrypoint /usr/local/bin/tessaridb-healthcheck

# What the entrypoint reads. Every one of these has a default that makes
# `docker run tessaridb/tessaridb` a working node, and every one can be replaced
# with `-e`.
#
#   TESSARIDB_STORE           where the store lives. Set it empty for a store
#                             held in memory and lost when the container stops.
#   TESSARIDB_ADDRESS         the wire protocol's address. Empty turns it off.
#   TESSARIDB_HTTP_ADDRESS    the HTTP surface's address. Empty turns it off.
#   TESSARIDB_LOG             how much the node reports: error, warn, info,
#                             debug or trace.
#
# `0.0.0.0` rather than a loopback address, because a container's loopback is
# reachable from nothing outside it and a node bound there would answer no
# published port — which looks exactly like a node that failed to start.
ENV TESSARIDB_STORE=/var/lib/tessaridb/store \
    TESSARIDB_ADDRESS=0.0.0.0:9080 \
    TESSARIDB_HTTP_ADDRESS=0.0.0.0:8000 \
    TESSARIDB_LOG=info

# What the node itself reads, and what this image deliberately does NOT default.
#
#   TESSARIDB_INITIAL_USER      declare this user as a store-wide owner, once,
#   TESSARIDB_INITIAL_PASSWORD  when the store has no users at all.
#
# A store with no users is OPEN: it runs anything for anybody who reaches the
# port. Setting both of these on first start is what closes it. They are left
# unset here on purpose — a published image carrying a default password would
# close every store that pulls it with a credential the whole internet knows,
# which is worse than the open store it looks like it is fixing. Half of the
# pair is refused rather than started, so a misspelled variable stops the node
# instead of quietly leaving it open.
#
#   TESSARIDB_PASSWORD          the password for `--user`, when this image is
#                               run as a client rather than as a node.

VOLUME ["/var/lib/tessaridb"]
EXPOSE 9080/tcp 8000/tcp

USER tessaridb
WORKDIR /var/lib/tessaridb

# No init process. The node installs its own SIGTERM and SIGINT handlers before
# any surface is serving and forks nothing, so it is correct as PID 1 — the two
# jobs an init would do here, reaping orphans and giving PID 1 a signal
# disposition, are respectively unnecessary and already done. `docker stop` then
# reaches the node directly and closes the store rather than killing it.
ENTRYPOINT ["/usr/local/bin/tessaridb-entrypoint"]
CMD ["serve"]

# What this proves: the HTTP surface answers `/health`, which needs no
# credential precisely so that a supervisor can tell a live node from a dead one
# without holding one. What it does NOT prove is that any particular namespace
# exists or that the store's users have been declared — a health route that
# needed a password would be a health route nobody configures.
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
  CMD ["/usr/local/bin/tessaridb-healthcheck"]

LABEL org.opencontainers.image.title="TessariDB" \
      org.opencontainers.image.description="TessariDB — a database with the query language, the storage engine and the wire protocol in one binary." \
      org.opencontainers.image.source="https://github.com/TessariDB/TessariDB" \
      org.opencontainers.image.documentation="https://docs.tessaridb.com" \
      org.opencontainers.image.url="https://tessaridb.com" \
      org.opencontainers.image.licenses="BUSL-1.1" \
      org.opencontainers.image.version="0.0.3-alpha"
