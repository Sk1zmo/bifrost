# Bifrost in a container.
#
#   docker build -t bifrost .
#   docker run --rm --network host bifrost
#
# `--network host` is not optional: Bifrost discovers peers by broadcasting on
# the LAN, and Docker's default bridge network puts the container on its own
# private subnet where no broadcast reaches the real network. On Docker Desktop
# for macOS and Windows, host networking is limited — run Bifrost natively
# there instead; the container is for Linux servers.

# ── Build ────────────────────────────────────────────────────────────────────
FROM rust:1.83-slim AS builder

WORKDIR /src

# Copy manifests first so dependencies stay cached when only sources change.
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/
COPY crates/web/Cargo.toml crates/web/
COPY crates/cli/Cargo.toml crates/cli/

# A stub tree lets `cargo build` resolve and compile every dependency before
# the real sources exist.
RUN mkdir -p crates/core/src crates/web/src crates/cli/src \
    && echo 'fn main() {}' > crates/cli/src/main.rs \
    && touch crates/core/src/lib.rs crates/web/src/lib.rs \
    && cargo build --release -p bifrost-cli 2>/dev/null || true

COPY . .

# Touch the real entry points so cargo does not reuse the stub artefacts.
RUN touch crates/core/src/lib.rs crates/web/src/lib.rs crates/cli/src/main.rs \
    && cargo build --release -p bifrost-cli

# ── Runtime ──────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# An unprivileged user: Bifrost binds ports 9000+, none of which need root.
RUN useradd --create-home --uid 10001 bifrost
USER bifrost
WORKDIR /home/bifrost

COPY --from=builder /src/target/release/bifrost /usr/local/bin/bifrost

ENV BIFROST_HOME=/home/bifrost/.bifrost \
    BIFROST_LOG=info

VOLUME ["/home/bifrost/.bifrost"]

EXPOSE 9000/tcp 9001/udp 9002/tcp 9003/tcp 9004/tcp 9005/udp

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD ["/usr/local/bin/bifrost", "peers"]

ENTRYPOINT ["/usr/local/bin/bifrost"]
CMD ["serve"]
