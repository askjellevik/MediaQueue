FROM rust:1-slim-bookworm AS build
WORKDIR /src
ENV SQLX_OFFLINE=true
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/media-requests /media-requests

FROM gcr.io/distroless/cc-debian12:nonroot
# Large allocations (each Argon2 login takes 19 MiB) go straight to mmap and
# back to the OS when freed. Without a fixed threshold glibc starts keeping
# them in per-thread arenas, RSS ratchets up with every blocking thread that
# ever hashed, and the container hits its memory limit under login load.
ENV GLIBC_TUNABLES=glibc.malloc.mmap_threshold=131072
COPY --from=build /media-requests /app/media-requests
USER 65532:65532
ENTRYPOINT ["/app/media-requests"]
CMD ["serve"]
