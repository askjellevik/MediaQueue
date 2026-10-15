FROM rust:1-slim-bookworm AS build
WORKDIR /src
ENV SQLX_OFFLINE=true
# Extra cargo features, e.g. `--build-arg FEATURES=cf-access`.
ARG FEATURES=""
# Set by buildx; keeps amd64 and arm64 builds out of each other's target dir.
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=target-$TARGETARCH \
    cargo build --release --locked --features "$FEATURES" \
 && cp target/release/mediaqueue /mediaqueue

FROM gcr.io/distroless/cc-debian12:nonroot
ENV BIND_ADDR=0.0.0.0:8000 DATABASE_PATH=/data/db.sqlite3
# Large allocations (each Argon2 login takes 19 MiB) go straight to mmap and
# back to the OS when freed. Without a fixed threshold glibc starts keeping
# them in per-thread arenas, RSS ratchets up with every blocking thread that
# ever hashed, and the container hits its memory limit under login load.
ENV GLIBC_TUNABLES=glibc.malloc.mmap_threshold=131072
COPY --from=build /mediaqueue /app/mediaqueue
USER 65532:65532
ENTRYPOINT ["/app/mediaqueue"]
CMD ["serve"]
