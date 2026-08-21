# syntax=docker/dockerfile:1
#
# Two stages: build against the full Rust toolchain, ship a binary on a base with
# no shell and no package manager.
#
#   docker build -t netcluster-server .
#   docker run --rm -p 8080:8080 netcluster-server
#
# 9.6 MB to pull, 37 MB on disk, of which the binary is 1.3 MB -- the rest is the
# distroless base. There is nothing to mount and nothing to persist: the index is a
# materialised view of your position stream, so a container that dies is a
# container you restart.
#
# A cold build takes about 50 s; changing source and rebuilding takes about 5 s,
# because the dependency graph is compiled in its own cached layer below.

# ---------------------------------------------------------------- build ------
FROM rust:1.98-slim-bookworm AS builder
WORKDIR /build

# Dependencies first, from manifests alone, so editing source does not re-download
# and re-compile the dependency graph on every build.
COPY Cargo.toml Cargo.lock ./
COPY crates/netcluster/Cargo.toml crates/netcluster/Cargo.toml
COPY crates/netcluster-server/Cargo.toml crates/netcluster-server/Cargo.toml
RUN mkdir -p crates/netcluster/src crates/netcluster-server/src crates/netcluster-server/demo \
 && : > crates/netcluster/src/lib.rs \
 && : > crates/netcluster-server/src/lib.rs \
 && : > crates/netcluster-server/demo/index.html \
 && echo 'fn main() {}' > crates/netcluster-server/src/main.rs \
 && cargo build --release --bin netcluster-server \
 && rm -rf crates/netcluster/src crates/netcluster-server/src

COPY crates crates
# Cargo keys off mtime; the stub artifacts above are newer than the real sources
# we just copied over them, so say plainly that these changed.
RUN touch crates/netcluster/src/lib.rs \
          crates/netcluster-server/src/lib.rs \
          crates/netcluster-server/src/main.rs \
 && cargo build --release --bin netcluster-server \
 && strip target/release/netcluster-server \
 && mkdir -p /emptydir

# -------------------------------------------------------------- runtime ------
# distroless/cc carries glibc and nothing else: no shell, no apt, no busybox. That
# is why the health check is a flag on the binary rather than a curl invocation.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/netcluster-server /usr/local/bin/netcluster-server

# /data must exist in the image, owned by the user the server runs as. Docker
# copies a directory's ownership into an empty named volume mounted over it, so
# without this the volume arrives owned by root and every snapshot fails with
# EACCES -- at shutdown, which is the worst possible moment to find out.
COPY --from=builder --chown=65532:65532 /emptydir /data

ENV NETCLUSTER_ADDR=0.0.0.0:8080 \
    NETCLUSTER_SWEEP_SECONDS=10 \
    NETCLUSTER_AUTO_CREATE=1 \
    NETCLUSTER_SNAPSHOT_SECONDS=60

# Persistence is off unless NETCLUSTER_DATA_DIR points somewhere writable. Point it
# at a mounted volume, not a path inside the image: this runs happily with a
# read-only root filesystem and should keep doing so.
#
#   docker run -v ncdata:/data -e NETCLUSTER_DATA_DIR=/data ...
VOLUME ["/data"]

EXPOSE 8080
USER nonroot

# Kubernetes and ALB probe /healthz over HTTP themselves and ignore this; it is
# here so `docker run` and compose report health without a shell in the image.
HEALTHCHECK --interval=10s --timeout=3s --start-period=3s --retries=3 \
  CMD ["/usr/local/bin/netcluster-server", "--health"]

ENTRYPOINT ["/usr/local/bin/netcluster-server"]
