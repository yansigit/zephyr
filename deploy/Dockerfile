# syntax=docker/dockerfile:1.7
# Zephyr in one container (DESIGN §4.4), built from pinned sources: `zephyr up --build` runs
#   docker build -t <image> -f deploy/Dockerfile --secret id=gh_token,env=GH_TOKEN .
# The gh_token secret (from `gh auth token`) is only needed while a source repo is private; it is
# mounted for the clone steps and never stored in a layer.
#
# The provider sources are pinned to commit SHAs by the build args below. To build against
# working trees instead, point the build at local checkouts, which replace the clone stage of the
# same name:
#   --build-context rpi-package=../rpi-package--codex --build-context provider-plugins=../rpi-provider-plugins--claude

ARG RPI_VERSION=0.15.0
ARG RPI_SHA256_AMD64=5762028e9e3a91229e4440c0dfd1260da55a9c4c35ce1b880f521fcc22386ebf
ARG RPI_SHA256_ARM64=59b21b680bb677f51d308080d8ee0688d0c2f3b2936ce061d3bbfd70a9c4a178
ARG RPI_PACKAGE_REPO=https://github.com/yansigit/rpi-package.git
ARG RPI_PACKAGE_REF=11e8ac183cf582144b8562ea19ec6f2620553cc3 # feat/providers-openai-codex: main cabdb15 + Antigravity, OpenCode Go, Codex
ARG PLUGINS_REPO=https://github.com/yansigit/rpi-provider-plugins.git
ARG PLUGINS_REF=9e8abac4ace5bc02cda5d6586b59eed12252cade # main as of 2026-10-31: claude-agent on Claude Code's own prompt and sessions
# The agent's browsers (DESIGN §4.11). fingerprint-chromium ships no arm64 build; Playwright's
# Chromium stands in there.
ARG FPC_VERSION=150.0.7871.186
ARG FPC_SHA256_AMD64=48eff860053efd9ad498d51370f0a1f59f983df39c02a72ad2a8682cdead4055
ARG AGENT_BROWSER_VERSION=0.41.0
ARG AGENT_BROWSER_SHA256_AMD64=399c7e9f06bd29aa933fdf25c0bd958eea06927fc0ce6f536ba6347aabd64628
ARG AGENT_BROWSER_SHA256_ARM64=e196e9cabe6cb1b01bd5912973628361b9f40a332683a12426abad007f0d3589
ARG PLAYWRIGHT_VERSION=1.64.1

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

# zephyr, zephyr-policy, zephyr-cursor, librpi_providers.so and librpi_mcp_adapter.so.
FROM toolchain AS rust
COPY Cargo.toml Cargo.lock /src/zephyr/
COPY crates /src/zephyr/crates
COPY --from=rpi-package / /src/rpi-package
# sharing=locked: the rust and plugins stages run at once and would unpack the same crate into this cache together.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cd /src/zephyr && cargo build --release --locked -p zephyr -p zephyr-policy -p zephyr-cursor \
    && cd /src/rpi-package && cargo build --release --locked -p rpi-providers -p rpi-mcp-adapter \
    && mkdir -p /out/bin /out/lib /out/ext \
    && cp /src/zephyr/target/release/zephyr /out/bin/ \
    && cp /src/zephyr/target/release/libzephyr_policy.so /out/lib/ \
    && cp /src/zephyr/target/release/libzephyr_cursor.so /out/ext/ \
    && cp /src/rpi-package/target/release/librpi_providers.so /out/ext/ \
    && cp /src/rpi-package/target/release/librpi_mcp_adapter.so /out/ext/

# rpi-claude-agent: its stage.sh builds the cdylib and worker and installs the lockfile-pinned
# node_modules (npm ci --ignore-scripts, checked against the lockfiles' hashes). Cursor is not a
# staged package any more: crates/zephyr-cursor builds libzephyr_cursor.so in the rust stage.
FROM toolchain AS plugins
COPY --from=provider-plugins / /src/plugins
WORKDIR /src/plugins
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked --mount=type=cache,target=/root/.npm \
    packages/rpi-claude-agent/scripts/stage.sh /out/ext/rpi-claude-agent

# The pinned upstream rpi release, checked against its published sha256.
FROM ubuntu:24.04 AS rpi
ARG RPI_VERSION RPI_SHA256_AMD64 RPI_SHA256_ARM64 TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && case "$TARGETARCH" in arm64) t=aarch64 s=$RPI_SHA256_ARM64 ;; *) t=x86_64 s=$RPI_SHA256_AMD64 ;; esac \
    && f="rpi-v$RPI_VERSION-$t-unknown-linux-gnu.tar.gz" \
    && curl -fsSLo "/tmp/$f" "https://github.com/bigfish1913/pi-rust/releases/download/v$RPI_VERSION/$f" \
    && echo "$s  /tmp/$f" | sha256sum -c - \
    && tar -xzf "/tmp/$f" -C /usr/local/bin ./rpi

# The default browser and agent-browser, which drives it, both checked against pinned sha256s.
FROM toolchain AS browser
ARG TARGETARCH FPC_VERSION FPC_SHA256_AMD64 AGENT_BROWSER_VERSION AGENT_BROWSER_SHA256_AMD64 \
    AGENT_BROWSER_SHA256_ARM64 PLAYWRIGHT_VERSION
RUN set -eu; mkdir -p /out/browser /out/bin; \
    case "$TARGETARCH" in \
    arm64) a=arm64 s=$AGENT_BROWSER_SHA256_ARM64; \
           PLAYWRIGHT_BROWSERS_PATH=/tmp/pw npx -y "playwright@$PLAYWRIGHT_VERSION" install chromium; \
           cp -a /tmp/pw/chromium-*/chrome-linux*/. /out/browser/ ;; \
    *) a=x64 s=$AGENT_BROWSER_SHA256_AMD64; \
       f="ungoogled-chromium-$FPC_VERSION-1-x86_64_linux.tar.xz"; \
       curl -fsSLo "/tmp/$f" "https://github.com/adryfish/fingerprint-chromium/releases/download/$FPC_VERSION/$f"; \
       echo "$FPC_SHA256_AMD64  /tmp/$f" | sha256sum -c -; \
       tar -xJf "/tmp/$f" -C /out/browser --strip-components 1 ;; \
    esac; \
    rm -f /out/browser/chromedriver; find /out/browser/locales -type f ! -name en-US.pak -delete; \
    curl -fsSLo /out/bin/agent-browser \
      "https://github.com/vercel-labs/agent-browser/releases/download/v$AGENT_BROWSER_VERSION/agent-browser-linux-$a"; \
    echo "$s  /out/bin/agent-browser" | sha256sum -c -; chmod 755 /out/bin/agent-browser

# The agent's Python: deploy/python's locked packages (Camoufox among them) in a venv on the final
# image's python3, plus uv for the rest. Camoufox, the fallback browser, spoofs Linux only
# (`os="linux"`, which matches the real GPU), so its macOS- and Windows-only font groups go.
# Firefox writes a fontconfig file into its cache dir on each launch, hence the 1777 dir.
FROM ubuntu:24.04 AS python
COPY --from=ghcr.io/astral-sh/uv:0.13.6@sha256:af22a52f5eafef3d78e551599d405c02df61919d18028f0cea8eda93bc9986d0 /uv /usr/local/bin/uv
COPY deploy/python/pyproject.toml deploy/python/uv.lock /src/python/
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates python3 \
    && UV_PROJECT_ENVIRONMENT=/opt/zephyr/py UV_PYTHON=/usr/bin/python3.12 UV_PYTHON_DOWNLOADS=never \
       uv sync --project /src/python --frozen --no-install-project --no-cache --link-mode copy \
    && XDG_CACHE_HOME=/opt/zephyr/camoufox/cache /opt/zephyr/py/bin/python -m camoufox fetch \
    && rm -rf /opt/zephyr/camoufox/cache/camoufox/browsers/official/*/fonts/M \
              /opt/zephyr/camoufox/cache/camoufox/browsers/official/*/fonts/MW \
              /opt/zephyr/camoufox/cache/camoufox/browsers/official/*/fonts/W \
    && mkdir -m 1777 /opt/zephyr/camoufox/cache/camoufox/fontconfig \
    && printf '#!/bin/sh\nXDG_CACHE_HOME=/opt/zephyr/camoufox/cache exec /opt/zephyr/py/bin/python "$@"\n' \
       > /opt/zephyr/camoufox/camoufox-python && chmod 755 /opt/zephyr/camoufox/camoufox-python

FROM ubuntu:24.04
# Chromium's and Firefox's libraries, Vulkan/EGL for the GPU, and the agent's desktop (§4.11).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates git curl python3 build-essential \
      libnss3 libatk1.0-0t64 libatk-bridge2.0-0t64 libcups2t64 libdrm2 libxkbcommon0 libxcomposite1 \
      libxdamage1 libxfixes3 libxrandr2 libgbm1 libpango-1.0-0 libcairo2 libasound2t64 libxshmfence1 \
      libgtk-3-0t64 libdbus-glib-1-2 libx11-xcb1 libegl1 libgl1 libvulkan1 fonts-dejavu-core \
      xvfb openbox xdotool x11-utils imagemagick x11vnc \
    && rm -rf /var/lib/apt/lists/*
COPY --from=node:24-bookworm-slim /usr/local/bin/node /usr/local/bin/node
COPY --from=rpi /usr/local/bin/rpi /usr/local/bin/rpi
COPY --from=rust /out/bin/zephyr /usr/local/bin/zephyr
COPY --from=rust /out/lib/libzephyr_policy.so /usr/local/lib/libzephyr_policy.so
COPY --from=rust /out/ext/ /opt/zephyr/ext/
COPY --from=plugins /out/ext/ /opt/zephyr/ext/
COPY --from=browser /out/browser/ /opt/zephyr/browser/
COPY --from=browser /out/bin/agent-browser /usr/local/bin/agent-browser
COPY --from=python /opt/zephyr/py/ /opt/zephyr/py/
COPY --from=python /opt/zephyr/camoufox/ /opt/zephyr/camoufox/
COPY --from=python /usr/local/bin/uv /usr/local/bin/uv
RUN ln -s /opt/zephyr/camoufox/camoufox-python /usr/local/bin/camoufox-python
# The zephyr binary runs its daemon commands here, and the host commands outside.
ENV ZEPHYR_IN_CONTAINER=1
# The daemon starts an X display here for the agent (§4.11); every child inherits these.
# Chromium's own sandbox cannot start under --cap-drop ALL and no-new-privileges: the container
# is its sandbox. Vulkan reaches the GPU headless and on Xvfb alike (gl-egl falls back to llvmpipe).
ENV PATH=/opt/zephyr/py/bin:$PATH \
    ZEPHYR_DESKTOP=:99 \
    AGENT_BROWSER_EXECUTABLE_PATH=/opt/zephyr/browser/chrome \
    AGENT_BROWSER_PROFILE=/zephyr/browser \
    AGENT_BROWSER_HEADED=1 \
    AGENT_BROWSER_ARGS=--no-sandbox,--disable-dev-shm-usage,--use-angle=vulkan,--enable-features=Vulkan,--ignore-gpu-blocklist
# `docker run --init` makes docker-init PID 1; supervise starts `zephyr run` again if it crashes,
# so the agent's background commands (kept and reaped by PID 1) outlive a daemon crash.
CMD ["zephyr", "supervise"]
