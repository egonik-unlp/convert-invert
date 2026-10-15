# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS build

WORKDIR /usr/local/src/app
COPY . .
# Cache the registry and target dir across builds; the binaries are copied out of the
# cache mount so the next stage can pick them up. Cargo decides freshness by mtime, and a
# cached target can look newer than the copied sources, so the crate's own sources are
# touched to always rebuild it (dependencies stay cached).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/src/app/target \
    find src migrations -type f -exec touch {} + \
    && cargo build --release --bin trigger_server --bin convert-invert \
    && mkdir -p /out \
    && cp target/release/trigger_server target/release/convert-invert /out/

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libpq5 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=build /out/trigger_server /usr/local/bin/trigger_server
COPY --from=build /out/convert-invert /usr/local/bin/convert-invert
COPY assets/infinite-playlist /app/assets/infinite-playlist
ENV NEXTTRACK_MODEL_DIR=/app/assets/infinite-playlist
EXPOSE 3124
# trigger_server runs the embedded migrations on startup.
CMD ["trigger_server"]
