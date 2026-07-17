# syntax=docker/dockerfile:1

# ─────────────────────────────────────────────────────────────────────────────
# Build stage — compile the agit-hub binary.
#
# The React frontend (hub-ui/dist) is committed and embedded into the binary at
# compile time via include_str!, so this stage needs no Node/npm — only the Rust
# toolchain. The dependency set is pure-Rust (argon2, sha2, serde, chrono, …):
# there is no OpenSSL / native-tls, so the runtime image needs no extra shared
# libraries beyond libc.
# ─────────────────────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
# BuildKit cache mounts keep the crate registry and target/ warm across rebuilds
# without baking them into an image layer. The binary must be copied OUT of the
# cached target/ within the SAME RUN, because the cache mount is not part of the
# resulting image.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin agit-hub \
 && cp /src/target/release/agit-hub /agit-hub

# ─────────────────────────────────────────────────────────────────────────────
# Runtime stage — minimal Debian with git and ca-certificates.
#
# The hub shells out to git for the smart-http machinery (receive-pack, rev-list,
# cat-file) so git must be present. It runs as a non-root user and keeps all
# state under a VOLUME.
# ─────────────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Non-root service account. Its home IS the data root: agit-hub's default --root
# is $HOME/.agit-hub, so `serve` and every admin subcommand you later run with
# `docker exec` (user add / add / token …) resolve to the same directory without
# anyone having to remember a --root flag.
RUN groupadd --system --gid 10001 agithub \
 && useradd  --system --uid 10001 --gid agithub \
             --home-dir /data --shell /usr/sbin/nologin agithub \
 && install -d -o agithub -g agithub -m 0700 /data

COPY --from=build /agit-hub /usr/local/bin/agit-hub

ENV HOME=/data
USER agithub
WORKDIR /data

# All hub state — users.json, agents.json, auth.json, audit.log and the bare
# repos — lives under $HOME/.agit-hub. Persist it.
VOLUME ["/data"]

# agit-hub serve defaults to --port 8177.
EXPOSE 8177

# A container is only reachable if it binds beyond loopback, and agit-hub refuses
# a non-loopback PLAINTEXT bind. --tls is the "TLS is terminated in front of me"
# promise the bind guard asks for: it does NOT make the hub speak TLS (it never
# does) — it relaxes the guard and marks the session cookie Secure, which is
# correct because the real client connection is HTTPS at the proxy. Put a
# TLS-terminating reverse proxy in front (see deploy/docker-compose.yml) and add
# --trusted-proxy <proxy-ip> so the per-IP rate limit keys on the real client IP.
#
# For a throwaway plaintext LAN/local look with no proxy, override the command:
#   docker run --rm -p 8177:8177 agit-hub serve --host 0.0.0.0 --insecure
ENTRYPOINT ["agit-hub"]
CMD ["serve", "--host", "0.0.0.0", "--port", "8177", "--tls"]
