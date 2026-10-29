# syntax=docker/dockerfile:1.7
# Zephyr in one container (DESIGN §4.4), built from pinned sources: `zephyr up --build` runs
#   docker build -t <image> -f deploy/Dockerfile --secret id=gh_token,env=GH_TOKEN .
# The gh_token secret (from `gh auth token`) is only needed while a source repo is private; it is
# mounted for the clone steps and never stored in a layer.
#
# The provider branches are set by build args. Until a branch is pushed, point the build at a local
# checkout instead, which replaces the clone stage of the same name:
#   --build-context rpi-package=../rpi-package--codex --build-context provider-plugins=../rpi-provider-plugins--claude

ARG RPI_VERSION=0.12.0
ARG RPI_SHA256_AMD64=e091b849d678e961c95abc924fec68ad6079a658eb54ca33b6fdcd2e9aa09229
ARG RPI_SHA256_ARM64=d8fb531c46d280acd61e4a58e824f1e0e9d996b4535838d7df812c89da9fec2a
ARG RPI_PACKAGE_REPO=https://github.com/yansigit/rpi-package.git
ARG RPI_PACKAGE_REF=feat/providers-openai-codex
ARG PLUGINS_REPO=https://github.com/yansigit/rpi-provider-plugins.git
ARG PLUGINS_REF=feat/claude-agent-sdk

# Rust plus node 24 (the plugin stage scripts use both). Bookworm's glibc (2.36) is older than
# the final image's (2.39), so what is built here runs there.
FROM rust:1.95-bookworm AS toolchain
COPY --from=node:24-bookworm-slim /usr/local/ /usr/local/

# Fetches REPO at REF (a branch, tag or commit) into /src.
FROM toolchain AS fetch
RUN cat > /usr/local/bin/fetch <<'EOF' && chmod +x /usr/local/bin/fetch
#!/bin/sh
set -eu
token="$(cat /run/secrets/gh_token 2>/dev/null || true)"
if [ -n "$token" ]; then
  # Only in this process's environment: never written to .git/config or a layer.
  export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0="url.https://x-access-token:$token@github.com/.insteadOf" \
    GIT_CONFIG_VALUE_0=https://github.com/
fi
git init -q /src && cd /src && git fetch -q --depth 1 "$1" "$2" && git checkout -q FETCH_HEAD
EOF

FROM fetch AS rpi-package-git
ARG RPI_PACKAGE_REPO RPI_PACKAGE_REF
RUN --mount=type=secret,id=gh_token fetch "$RPI_PACKAGE_REPO" "$RPI_PACKAGE_REF"
FROM scratch AS rpi-package
COPY --from=rpi-package-git /src /

FROM fetch AS provider-plugins-git
ARG PLUGINS_REPO PLUGINS_REF
RUN --mount=type=secret,id=gh_token fetch "$PLUGINS_REPO" "$PLUGINS_REF"
FROM scratch AS provider-plugins
COPY --from=provider-plugins-git /src /

# zephyr, zephyr-policy and librpi_providers.so.
FROM toolchain AS rust
COPY Cargo.toml Cargo.lock /src/zephyr/
COPY crates /src/zephyr/crates
COPY --from=rpi-package / /src/rpi-package
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cd /src/zephyr && cargo build --release --locked -p zephyr -p zephyr-policy \
    && cd /src/rpi-package && cargo build --release --locked -p rpi-providers \
    && mkdir -p /out/bin /out/lib /out/ext \
    && cp /src/zephyr/target/release/zephyr /out/bin/ \
    && cp /src/zephyr/target/release/libzephyr_policy.so /out/lib/ \
    && cp /src/rpi-package/target/release/librpi_providers.so /out/ext/

# rpi-cursor-lite and rpi-claude-agent: each package's stage.sh builds its cdylib and worker and
# copies the lockfile-pinned node_modules (npm ci --ignore-scripts first, with the lockfiles' hashes).
FROM toolchain AS plugins
COPY --from=provider-plugins / /src/plugins
WORKDIR /src/plugins
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/root/.npm \
    for d in packages/rpi-cursor-sdk/worker packages/rpi-cursor-sdk/runtime packages/rpi-claude-agent/worker; do \
      [ ! -f "$d/package-lock.json" ] || (cd "$d" && npm ci --ignore-scripts --no-audit --no-fund); \
    done \
    && packages/rpi-cursor-lite/scripts/stage.sh /out/ext/rpi-cursor-lite \
    && packages/rpi-claude-agent/scripts/stage.sh /out/ext/rpi-claude-agent

# The pinned upstream rpi release, checked against its published sha256.
FROM ubuntu:24.04 AS rpi
ARG RPI_VERSION RPI_SHA256_AMD64 RPI_SHA256_ARM64 TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && case "$TARGETARCH" in arm64) t=aarch64 s=$RPI_SHA256_ARM64 ;; *) t=x86_64 s=$RPI_SHA256_AMD64 ;; esac \
    && f="rpi-v$RPI_VERSION-$t-unknown-linux-gnu.tar.gz" \
    && curl -fsSLo "/tmp/$f" "https://github.com/bigfish1913/pi-rust/releases/download/v$RPI_VERSION/$f" \
    && echo "$s  /tmp/$f" | sha256sum -c - \
    && tar -xzf "/tmp/$f" -C /usr/local/bin ./rpi

FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates git curl python3 build-essential \
    && rm -rf /var/lib/apt/lists/*
COPY --from=node:24-bookworm-slim /usr/local/bin/node /usr/local/bin/node
COPY --from=rpi /usr/local/bin/rpi /usr/local/bin/rpi
COPY --from=rust /out/bin/zephyr /usr/local/bin/zephyr
COPY --from=rust /out/lib/libzephyr_policy.so /usr/local/lib/libzephyr_policy.so
COPY --from=rust /out/ext/ /opt/zephyr/ext/
COPY --from=plugins /out/ext/ /opt/zephyr/ext/
# The zephyr binary runs its daemon commands here, and the host commands outside.
ENV ZEPHYR_IN_CONTAINER=1
CMD ["zephyr", "run"]
