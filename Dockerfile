# syntax=docker/dockerfile:1

# ---------------------------------------------------------------- builder ---
# The toolchain is pinned by rust-toolchain.toml; this base only needs to be
# new enough to honour it.
FROM rust:1.97-bookworm AS builder

# BoringSSL, which `wreq` links for TLS/JA3 impersonation, is built from source
# by `boring-sys2`, and none of its build tooling ships in the slim Rust image.
# Two separate failures, in this order, if you leave them out:
#
#   cmake / perl / golang-go   the cmake build itself, which reports only
#                              "failed to execute command: No such file or
#                              directory (os error 2)" — it is failing to exec
#                              a build tool, not missing a source file
#   libclang-dev               `bindgen` generating the Rust bindings, which
#                              says so plainly: "Unable to find libclang"
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake perl golang-go clang libclang-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Dependency layer first: manifests only, so a source edit does not invalidate
# the (long) dependency build.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ ./crates/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p browser-server -p browser-mcp \
 && mkdir -p /out \
 && cp target/release/brs-serve target/release/brs-mcp /out/

# ---------------------------------------------------------------- runtime ---
FROM debian:bookworm-slim AS runtime

# Chromium plus the fonts and libraries it will not start without. The CDP
# crate drives a real browser; a runtime without Chromium silently breaks every
# `browse` call while `fetch` keeps working, which is a confusing failure.
RUN apt-get update && apt-get install -y --no-install-recommends \
      chromium \
      ca-certificates \
      fonts-liberation fonts-noto-color-emoji fonts-unifont \
      libnss3 libnspr4 libdbus-1-3 libatk1.0-0 libatk-bridge2.0-0 \
      libcups2 libdrm2 libxkbcommon0 libxcomposite1 libxdamage1 \
      libxfixes3 libxrandr2 libgbm1 libasound2 libpango-1.0-0 \
      tini curl \
 && rm -rf /var/lib/apt/lists/*

# Non-root. The profile root is a volume so logins survive `docker restart`.
RUN useradd --create-home --uid 10001 brs \
 && mkdir -p /var/lib/browser-rs/profiles \
 && chown -R brs:brs /var/lib/browser-rs

COPY --from=builder /out/brs-serve /usr/local/bin/brs-serve
COPY --from=builder /out/brs-mcp   /usr/local/bin/brs-mcp

USER brs
WORKDIR /home/brs

# `chromiumoxide`'s detection honours CHROME; see resolve_executable().
ENV CHROME=/usr/bin/chromium \
    BROWSER_RS_BIND=0.0.0.0:8080 \
    BROWSER_RS_PROFILE_ROOT=/var/lib/browser-rs/profiles \
    RUST_LOG=info

VOLUME ["/var/lib/browser-rs/profiles"]
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/api/health || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/brs-serve"]
