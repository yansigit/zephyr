# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
# Zephyr in one container (DESIGN §4.4), built from pinned sources: `zephyr up --build` runs
#   docker build -t <image> -f deploy/Dockerfile --secret id=gh_token,env=GH_TOKEN .
# The gh_token secret (from `gh auth token`) is only needed while a source repo is private; it is
# mounted for the clone steps and never stored in a layer.
#
# The provider sources are pinned to commit SHAs by the build args below. To build against
# working trees instead, point the build at local checkouts, which replace the clone stage of the
# same name:
#   --build-context rpi-package=<dir> --build-context provider-plugins=<dir>
# An empty repo arg leaves that source out (DESIGN §4.4 item 1): with
#   --build-arg RPI_PACKAGE_REPO= --build-arg PLUGINS_REPO=
# the image has zephyr, the policy plugin, Cursor and rpi's built-in providers only.

ARG RPI_VERSION=0.25.0
ARG RPI_SHA256_AMD64=a387c57af04a6834d951ee823c73d8293f53e651c0c3adb113a9356532535786
ARG RPI_SHA256_ARM64=2098cb6d7658fcd7f96b413ef39994e13c04626a5b0cf63b12ef8f6781c4d644
ARG RPI_PACKAGE_REPO=https://github.com/yansigit/rpi-package.git
ARG RPI_PACKAGE_REF=11e8ac183cf582144b8562ea19ec6f2620553cc3 # feat/providers-openai-codex: main cabdb15 + Antigravity, OpenCode Go, Codex
ARG PLUGINS_REPO=https://github.com/yansigit/rpi-provider-plugins.git
ARG PLUGINS_REF=9e8abac4ace5bc02cda5d6586b59eed12252cade # main as of 2026-10-31: claude-agent on Claude Code's own prompt and sessions
# The agent's browsers (DESIGN §4.11). fingerprint-chromium ships no arm64 build; Playwright's
# Chromium stands in there: the Chrome for Testing build Playwright 1.64.1 installs (its
# browsers.json), from Playwright's CDN.
ARG FPC_VERSION=150.0.7871.186
ARG FPC_SHA256_AMD64=48eff860053efd9ad498d51370f0a1f59f983df39c02a72ad2a8682cdead4055
ARG AGENT_BROWSER_VERSION=0.41.0
ARG AGENT_BROWSER_SHA256_AMD64=399c7e9f06bd29aa933fdf25c0bd958eea06927fc0ce6f536ba6347aabd64628
ARG AGENT_BROWSER_SHA256_ARM64=e196e9cabe6cb1b01bd5912973628361b9f40a332683a12426abad007f0d3589
ARG PLAYWRIGHT_CHROMIUM_VERSION=156.0.8078.4
ARG PLAYWRIGHT_CHROMIUM_SHA256_ARM64=910a12fff6170202195f72ef3d353801c7063d301ab80b1f51b77d791fda60ce
# The agent's command-line tools (DESIGN §4.11): gh, and himalaya for mail (static musl builds).
ARG GH_VERSION=2.103.3
ARG GH_SHA256_AMD64=4c806978ffce1cec70853302a56d70ac6da4f19bae7adca4e1a8ab00633dd91d
ARG GH_SHA256_ARM64=ee32df2323c8847f5fac6cb6b4b69ca75828e9329a8f22f677021f7b3306ce63
ARG HIMALAYA_VERSION=2.2.1
ARG HIMALAYA_SHA256_AMD64=5c5ba2724c162f82d0a0c71b6c03224ed44f8bef7b14ace5da0635d3af2665a0
ARG HIMALAYA_SHA256_ARM64=1dc21c3dd6d948929e22e5cae49b9d0b3b30ff88fafd4493fd4460c6252e9d90
# The catalog's github connector (DESIGN §4.22): github/github-mcp-server, a Go binary not on npm.
ARG GITHUB_MCP_VERSION=2.1.0
ARG GITHUB_MCP_SHA256_AMD64=a442f9db287cea01008bcb55fe6bc0292270301046c3edbd1a8b22d6e4157b2b
ARG GITHUB_MCP_SHA256_ARM64=34d5a268f65cd1e9b7107fa976f3571ccd575f9df00cec489fec79a039923a22
# The Signal channel (channels spec §4.8): signal-cli's JVM build and the Temurin JDK whose jlink
# cuts its Java runtime. SIGNAL=0 leaves both out.
ARG SIGNAL=1
ARG SIGNAL_CLI_VERSION=0.14.10
ARG SIGNAL_CLI_SHA256=706a57d9faae7525cf732936efdabf210146e3c83f9e8ae7e4e62f5946121c33
ARG TEMURIN_VERSION=25.0.5+8
ARG TEMURIN_SHA256_AMD64=68982529869257d0ee6b824ec5aa270ff8b118bb438670da0b97ebdab178d7fb
# Voice (DESIGN §4.4): whisper.cpp, a cut-down ffmpeg, Piper and their models in /opt/zephyr/voice.
# VOICE=0 leaves the folder out, which turns voice off. KO_VOICE=1 adds Piper's Korean voice,
# ko_KR-kss-medium, trained on the KSS dataset under CC BY-NC-SA 4.0: non-commercial use only.
ARG VOICE=1
ARG KO_VOICE=0
ARG WHISPER_VERSION=1.9.5
ARG WHISPER_SHA256=ff1a9053feb509ff9d7729703355541ae9690073a6b1c40eb692c962e0dc1720
ARG FFMPEG_VERSION=7.1
ARG FFMPEG_SHA256=40973d44970dbc83ef302b0609f2e74982be2d85916dd2ee7472d30678a7abe6
# Hugging Face revisions of ggerganov/whisper.cpp, ggml-org/whisper-vad and rhasspy/piper-voices.
ARG WHISPER_MODELS_REV=5359861c739e955e79d9a303bcbc70fb988958b1
ARG WHISPER_SMALL_SHA256=ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb
# The smaller whisper model a phone call's utterances are transcribed with when calls.speech is
# local (DESIGN §4.26): 57 MB, about a third of the small model's time. CALL_STT=0 leaves it out,
# and such calls use the small model.
ARG CALL_STT=1
ARG WHISPER_BASE_SHA256=422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898
ARG VAD_REV=9ffd54a1e1ee413ddf265af9913beaf518d1639b
ARG VAD_SHA256=29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf
ARG PIPER_VOICES_REV=cf910fb59f1d79ab3b625a63993a108751c6c25d
ARG EN_VOICE_SHA256=5efe09e69902187827af646e1a6e9d269dee769f9877d17b16b1b46eeaaf019f
ARG EN_VOICE_JSON_SHA256=efe19c417bed055f2d69908248c6ba650fa135bc868b0e6abb3da181dab690a0
ARG KO_VOICE_SHA256=624fd774e26895f24bebae1bd9a3379e3394baeade4b584924f83e414096e2c9
ARG KO_VOICE_JSON_SHA256=153b5619d0580f824a59108d83ca19434de410eb8e4fe80325b58684fdc8a1df

# The base images, pinned by the digest of each tag's multi-arch index (README "Building the
# image" says how to move them). Every Ubuntu stage builds FROM ubuntu-24.04.
FROM node:24-bookworm-slim@sha256:dc26ae92c9fb15cf6b1d63e20f0944b2efe0f115b20e00be48fb23fceb061c54 AS node-24
FROM ubuntu:24.04@sha256:2198e4c65076e9cb0b3d695219df667c0e3b0072da64ca3c2648aca17dcecaf5 AS ubuntu-24.04

# Rust plus node 24 (the plugin stage scripts use both). Bookworm's glibc (2.36) is older than
# the final image's (2.39), so what is built here runs there.
FROM rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1 AS toolchain
COPY --from=node-24 /usr/local/ /usr/local/

# Fetches REPO at REF (a branch, tag or commit) into /src; an empty REPO leaves /src empty.
FROM toolchain AS fetch
RUN cat > /usr/local/bin/fetch <<'EOF' && chmod +x /usr/local/bin/fetch
#!/bin/sh
set -eu
[ -n "$1" ] || { mkdir -p /src; exit 0; }
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

# zephyr, zephyr-policy and zephyr-cursor. No cache mounts here or below: a shared BuildKit keeps
# them across builds, so one branch's build could leave crate sources that a release then compiles.
FROM toolchain AS rust
# The repository the image is published to, baked into its CLI (`zephyr upgrade` pulls from it);
# unset or empty, the CLI's own default.
ARG ZEPHYR_BUILD_REPO
COPY Cargo.toml Cargo.lock /src/zephyr/
COPY crates /src/zephyr/crates
RUN cd /src/zephyr && cargo build --release --locked -p zephyr -p zephyr-policy -p zephyr-cursor \
    && mkdir -p /out/bin /out/lib /out/ext \
    && cp /src/zephyr/target/release/zephyr /out/bin/ \
    && cp /src/zephyr/target/release/libzephyr_policy.so /out/lib/ \
    && cp /src/zephyr/target/release/libzephyr_cursor.so /out/ext/

# librpi_providers.so and librpi_mcp_adapter.so from rpi-package, in a stage of their own so a
# change to this repo does not rebuild them. Left out only when RPI_PACKAGE_REPO is empty; set, a
# tree that lacks them fails here.
FROM toolchain AS rpi-ext
ARG RPI_PACKAGE_REPO
COPY --from=rpi-package / /src/rpi-package
RUN mkdir -p /out/ext \
    && if [ -n "$RPI_PACKAGE_REPO" ]; then \
         cd /src/rpi-package && cargo build --release --locked -p rpi-providers -p rpi-mcp-adapter \
         && cp /src/rpi-package/target/release/librpi_providers.so /out/ext/ \
         && cp /src/rpi-package/target/release/librpi_mcp_adapter.so /out/ext/; \
       fi

# rpi-claude-agent: its stage.sh builds the cdylib and worker and installs the lockfile-pinned
# node_modules (npm ci --ignore-scripts, checked against the lockfiles' hashes). Cursor is not a
# staged package any more: crates/zephyr-cursor builds libzephyr_cursor.so in the rust stage.
# Left out only when PLUGINS_REPO is empty; set, a tree without stage.sh fails here.
FROM toolchain AS plugins
ARG PLUGINS_REPO
COPY --from=provider-plugins / /src/plugins
WORKDIR /src/plugins
RUN if [ -n "$PLUGINS_REPO" ]; then \
      packages/rpi-claude-agent/scripts/stage.sh /out/ext/rpi-claude-agent; \
    else mkdir -p /out/ext; fi

# The pinned upstream rpi release, checked against its published sha256.
FROM ubuntu-24.04 AS rpi
ARG RPI_VERSION RPI_SHA256_AMD64 RPI_SHA256_ARM64 TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && case "$TARGETARCH" in arm64) t=aarch64 s=$RPI_SHA256_ARM64 ;; *) t=x86_64 s=$RPI_SHA256_AMD64 ;; esac \
    && f="rpi-v$RPI_VERSION-$t-unknown-linux-gnu.tar.gz" \
    && curl -fsSLo "/tmp/$f" "https://github.com/bigfish1913/pi-rust/releases/download/v$RPI_VERSION/$f" \
    && echo "$s  /tmp/$f" | sha256sum -c - \
    && tar -xzf "/tmp/$f" --no-same-owner -C /usr/local/bin ./rpi

# gh, himalaya and github-mcp-server from their pinned releases, checked against their sha256s.
FROM ubuntu-24.04 AS tools
ARG GH_VERSION GH_SHA256_AMD64 GH_SHA256_ARM64 HIMALAYA_VERSION HIMALAYA_SHA256_AMD64 \
    HIMALAYA_SHA256_ARM64 GITHUB_MCP_VERSION GITHUB_MCP_SHA256_AMD64 GITHUB_MCP_SHA256_ARM64 TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && case "$TARGETARCH" in \
       arm64) g=arm64 gs=$GH_SHA256_ARM64 h=aarch64 hs=$HIMALAYA_SHA256_ARM64 m=arm64 ms=$GITHUB_MCP_SHA256_ARM64 ;; \
       *) g=amd64 gs=$GH_SHA256_AMD64 h=x86_64 hs=$HIMALAYA_SHA256_AMD64 m=x86_64 ms=$GITHUB_MCP_SHA256_AMD64 ;; esac \
    && d="gh_${GH_VERSION}_linux_$g" \
    && curl -fsSLo "/tmp/$d.tar.gz" "https://github.com/cli/cli/releases/download/v$GH_VERSION/$d.tar.gz" \
    && echo "$gs  /tmp/$d.tar.gz" | sha256sum -c - \
    && tar -xzf "/tmp/$d.tar.gz" --no-same-owner -C /usr/local/bin --strip-components 2 "$d/bin/gh" \
    && f="himalaya.$h-linux.tgz" \
    && curl -fsSLo "/tmp/$f" "https://github.com/pimalaya/himalaya/releases/download/v$HIMALAYA_VERSION/$f" \
    && echo "$hs  /tmp/$f" | sha256sum -c - \
    && tar -xzf "/tmp/$f" --no-same-owner -C /usr/local/bin himalaya \
    && f="github-mcp-server_Linux_$m.tar.gz" \
    && curl -fsSLo "/tmp/$f" "https://github.com/github/github-mcp-server/releases/download/v$GITHUB_MCP_VERSION/$f" \
    && echo "$ms  /tmp/$f" | sha256sum -c - \
    && tar -xzf "/tmp/$f" --no-same-owner -C /usr/local/bin github-mcp-server

# The default browser and agent-browser, which drives it, both checked against pinned sha256s.
FROM toolchain AS browser
ARG TARGETARCH FPC_VERSION FPC_SHA256_AMD64 AGENT_BROWSER_VERSION AGENT_BROWSER_SHA256_AMD64 \
    AGENT_BROWSER_SHA256_ARM64 PLAYWRIGHT_CHROMIUM_VERSION PLAYWRIGHT_CHROMIUM_SHA256_ARM64
RUN set -eu; mkdir -p /out/browser /out/bin; \
    case "$TARGETARCH" in \
    arm64) a=arm64 s=$AGENT_BROWSER_SHA256_ARM64; \
           curl -fsSLo /tmp/chrome.zip \
             "https://cdn.playwright.dev/builds/cft/$PLAYWRIGHT_CHROMIUM_VERSION/linux-arm64/chrome-linux-arm64.zip"; \
           echo "$PLAYWRIGHT_CHROMIUM_SHA256_ARM64  /tmp/chrome.zip" | sha256sum -c -; \
           unzip -q /tmp/chrome.zip -d /tmp/pw; cp -a /tmp/pw/chrome-linux-arm64/. /out/browser/ ;; \
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
FROM ubuntu-24.04 AS python
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

# signal-cli with a Java runtime jlinked to just the modules its jars use. Root runs it, a child of
# the daemon, by full path with its data in /var/lib/zephyr/signal, outside the agent's reach.
# bin/signal-cli pins JAVA_HOME so it never runs on another Java; jlink's --strip-debug needs
# binutils' objcopy. jdeps follows the main jar's references, since the full lib/ set does not
# resolve as modules. The JVM build, not the GraalVM native one (408 MB, and its link and register
# broke). libsignal's native library in its jars is x86_64 only, so arm64 builds nothing, as
# SIGNAL=0 does: no /opt/zephyr/signal turns the channel off.
FROM ubuntu-24.04 AS signal-1
ARG SIGNAL_CLI_VERSION SIGNAL_CLI_SHA256 TEMURIN_VERSION TEMURIN_SHA256_AMD64 TARGETARCH
RUN set -eu; mkdir /out; \
    case "$TARGETARCH" in arm64) exit 0 ;; esac; \
    apt-get update; apt-get install -y --no-install-recommends ca-certificates curl binutils; \
    f="signal-cli-$SIGNAL_CLI_VERSION.tar.gz"; \
    curl -fsSLo "/tmp/$f" "https://github.com/AsamK/signal-cli/releases/download/v$SIGNAL_CLI_VERSION/$f"; \
    echo "$SIGNAL_CLI_SHA256  /tmp/$f" | sha256sum -c -; \
    mkdir /out/signal; \
    tar -xzf "/tmp/$f" --no-same-owner -C /out/signal --strip-components 1 \
      "signal-cli-$SIGNAL_CLI_VERSION/bin/signal-cli" "signal-cli-$SIGNAL_CLI_VERSION/lib"; \
    mv /out/signal/bin/signal-cli /out/signal/bin/signal-cli-jvm; \
    j="OpenJDK25U-jdk_x64_linux_hotspot_$(echo "$TEMURIN_VERSION" | tr + _).tar.gz"; \
    curl -fsSLo "/tmp/$j" "https://github.com/adoptium/temurin25-binaries/releases/download/jdk-$TEMURIN_VERSION/$j"; \
    echo "$TEMURIN_SHA256_AMD64  /tmp/$j" | sha256sum -c -; \
    mkdir /tmp/jdk; tar -xzf "/tmp/$j" --no-same-owner -C /tmp/jdk --strip-components 1; \
    m=$(/tmp/jdk/bin/jdeps -q --print-module-deps --ignore-missing-deps --multi-release 25 --recursive \
      --class-path '/out/signal/lib/*' "/out/signal/lib/signal-cli-$SIGNAL_CLI_VERSION.jar"); \
    /tmp/jdk/bin/jlink --add-modules "$m" --strip-debug --no-man-pages --no-header-files \
      --compress=zip-6 --output /out/signal/jre; \
    printf '#!/bin/sh\nJAVA_HOME=/opt/zephyr/signal/jre exec /opt/zephyr/signal/bin/signal-cli-jvm "$@"\n' \
      > /out/signal/bin/signal-cli; \
    chmod -R u=rwX,go=rX /out/signal; chmod 755 /out/signal/bin/*
# Both stages hold /out: BuildKit caches a COPY from an empty stage as the last one it made.
FROM scratch AS signal-0
WORKDIR /out
FROM signal-${SIGNAL} AS signal-out

# whisper-cli with Vulkan, built on ubuntu 24.04 so its glibc is the final image's; glslc and the
# Vulkan and SPIR-V headers stay in this stage. ggml and libstdc++ are linked in: it needs
# libvulkan1 and libgomp1 (in the final image) and runs on the CPU with no GPU. GGML_NATIVE=OFF
# builds for AVX2-class x86 (or plain armv8), not for the build host's CPU.
# whisper-server too, which a phone call with calls.speech = local keeps loaded (DESIGN §4.26). It
# is patched to bind its --host as a unix socket path (its bundled cpp-httplib 0.20 has AF_UNIX),
# so it can never listen on a TCP port; the grep fails the build if the patch stops applying.
FROM ubuntu-24.04 AS voice-whisper
ARG WHISPER_VERSION WHISPER_SHA256
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl build-essential \
      cmake glslc libvulkan-dev spirv-headers \
    && curl -fsSLo /tmp/whisper.tar.gz \
       "https://github.com/ggml-org/whisper.cpp/archive/refs/tags/v$WHISPER_VERSION.tar.gz" \
    && echo "$WHISPER_SHA256  /tmp/whisper.tar.gz" | sha256sum -c - \
    && mkdir /src && tar -xzf /tmp/whisper.tar.gz -C /src --strip-components 1 \
    && sed -i 's|^    if (!svr->bind_to_port(|    svr->set_address_family(AF_UNIX);\n&|' \
       /src/examples/server/server.cpp \
    && grep -q '^    svr->set_address_family(AF_UNIX);$' /src/examples/server/server.cpp \
    && cmake -S /src -B /build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF \
       -DGGML_VULKAN=ON -DGGML_CCACHE=OFF -DWHISPER_BUILD_TESTS=OFF -DWHISPER_SDL2=OFF \
       -DCMAKE_EXE_LINKER_FLAGS="-static-libgcc -static-libstdc++" \
    && cmake --build /build -j"$(nproc)" --target whisper-cli whisper-server \
    && install -D -m 755 -s /build/bin/whisper-cli /out/bin/whisper-cli \
    && install -D -m 755 -s /build/bin/whisper-server /out/bin/whisper-server

# ffmpeg, static, with only what voice notes in and out need (and μ-law out, for a phone call's speech): the channels' audio containers and
# codecs, raw s16le in (Piper's PCM), and Opus, AAC and 16-bit WAV out.
FROM ubuntu-24.04 AS voice-ffmpeg
ARG FFMPEG_VERSION FFMPEG_SHA256
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
      build-essential nasm pkg-config libopus-dev \
    && curl -fsSLo /tmp/ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz" \
    && echo "$FFMPEG_SHA256  /tmp/ffmpeg.tar.xz" | sha256sum -c - \
    && mkdir /src && tar -xJf /tmp/ffmpeg.tar.xz -C /src --strip-components 1 && cd /src \
    && ./configure --disable-everything --disable-autodetect --disable-network --disable-doc \
       --disable-debug --disable-ffplay --disable-ffprobe --disable-avdevice \
       --pkg-config-flags=--static --extra-ldflags=-static --enable-libopus \
       --enable-protocol=file,pipe \
       --enable-demuxer=ogg,mov,matroska,mp3,wav,flac,aac,caf,amr,pcm_s16le \
       --enable-decoder='opus,libopus,vorbis,aac,mp3,mp3float,flac,pcm_*,alac,amrnb,amrwb' \
       --enable-parser=aac,flac,mpegaudio,opus,vorbis \
       --enable-encoder=libopus,aac,pcm_s16le,pcm_mulaw --enable-muxer=ogg,ipod,wav,pcm_mulaw --enable-filter=aresample \
    && make -j"$(nproc)" ffmpeg \
    && install -D -m 755 -s ffmpeg /out/bin/ffmpeg

# Piper: deploy/voice's locked packages in a venv on the final image's python3, its bytecode
# compiled here since nobody, who runs it, cannot write __pycache__.
FROM ubuntu-24.04 AS voice-py
COPY --from=ghcr.io/astral-sh/uv:0.13.6@sha256:af22a52f5eafef3d78e551599d405c02df61919d18028f0cea8eda93bc9986d0 /uv /usr/local/bin/uv
COPY deploy/voice/pyproject.toml deploy/voice/uv.lock /src/voice/
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates python3 \
    && UV_PROJECT_ENVIRONMENT=/opt/zephyr/voice/py UV_PYTHON=/usr/bin/python3.12 UV_PYTHON_DOWNLOADS=never \
       uv sync --project /src/voice --frozen --no-install-project --no-cache --link-mode copy \
       --compile-bytecode \
    && chmod -R go-w /opt/zephyr/voice/py

# whisper's model, Silero VAD and Piper's voices, each checked against its pinned sha256.
FROM ubuntu-24.04 AS voice-models
ARG KO_VOICE CALL_STT WHISPER_BASE_SHA256 WHISPER_MODELS_REV WHISPER_SMALL_SHA256 VAD_REV VAD_SHA256 PIPER_VOICES_REV \
    EN_VOICE_SHA256 EN_VOICE_JSON_SHA256 KO_VOICE_SHA256 KO_VOICE_JSON_SHA256
RUN set -eu; apt-get update && apt-get install -y --no-install-recommends ca-certificates curl; \
    get() { curl -fsSLo "/out/$1" "https://huggingface.co/$2/resolve/$3"; echo "$4  /out/$1" | sha256sum -c -; }; \
    mkdir -p /out/models /out/voices; \
    get models/ggml-small-q5_1.bin ggerganov/whisper.cpp "$WHISPER_MODELS_REV/ggml-small-q5_1.bin" \
      "$WHISPER_SMALL_SHA256"; \
    get models/ggml-silero-v5.1.2.bin ggml-org/whisper-vad "$VAD_REV/ggml-silero-v5.1.2.bin" "$VAD_SHA256"; \
    if [ "$CALL_STT" = 1 ]; then \
      get models/ggml-base-q5_1.bin ggerganov/whisper.cpp "$WHISPER_MODELS_REV/ggml-base-q5_1.bin" \
        "$WHISPER_BASE_SHA256"; \
    fi; \
    v="rhasspy/piper-voices"; en="$PIPER_VOICES_REV/en/en_US/lessac/medium/en_US-lessac-medium"; \
    get voices/en_US-lessac-medium.onnx "$v" "$en.onnx" "$EN_VOICE_SHA256"; \
    get voices/en_US-lessac-medium.onnx.json "$v" "$en.onnx.json" "$EN_VOICE_JSON_SHA256"; \
    if [ "$KO_VOICE" = 1 ]; then \
      ko="$PIPER_VOICES_REV/ko/ko_KR/kss/medium/ko_KR-kss-medium"; \
      get voices/ko_KR-kss-medium.onnx "$v" "$ko.onnx" "$KO_VOICE_SHA256"; \
      get voices/ko_KR-kss-medium.onnx.json "$v" "$ko.onnx.json" "$KO_VOICE_JSON_SHA256"; \
    fi; \
    chmod -R a+rX,go-w /out

FROM scratch AS voice-1
COPY --from=voice-whisper /out/bin/ /opt/zephyr/voice/bin/
COPY --from=voice-ffmpeg /out/bin/ /opt/zephyr/voice/bin/
COPY --from=voice-models /out/ /opt/zephyr/voice/
COPY --from=voice-py /opt/zephyr/voice/py/ /opt/zephyr/voice/py/
# An /opt/zephyr with nothing in it: the final stage copies this folder's contents, so VOICE=0
# leaves no /opt/zephyr/voice at all.
FROM scratch AS voice-0
WORKDIR /opt/zephyr
FROM voice-${VOICE} AS voice-out

FROM ubuntu-24.04
# Chromium's and Firefox's libraries, Vulkan/EGL for the GPU, the agent's desktop and its
# everyday command-line tools (§4.11).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates git curl python3 build-essential \
      jq ripgrep fd-find unzip zip less nano sqlite3 openssh-client zsh file tree \
      libnss3 libatk1.0-0t64 libatk-bridge2.0-0t64 libcups2t64 libdrm2 libxkbcommon0 libxcomposite1 \
      libxdamage1 libxfixes3 libxrandr2 libgbm1 libpango-1.0-0 libcairo2 libasound2t64 libxshmfence1 \
      libgtk-3-0t64 libdbus-glib-1-2 libx11-xcb1 libegl1 libgl1 libvulkan1 fonts-dejavu-core \
      fonts-nanum poppler-utils \
      xvfb openbox xdotool x11-utils imagemagick x11vnc nftables libnss3-tools qrencode \
    && rm -rf /var/lib/apt/lists/* \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd
COPY --from=node-24 /usr/local/bin/node /usr/local/bin/node
COPY --from=node-24 /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
COPY --from=tools /usr/local/bin/gh /usr/local/bin/himalaya /usr/local/bin/github-mcp-server /usr/local/bin/
COPY --from=rpi /usr/local/bin/rpi /usr/local/bin/rpi
COPY --from=rust /out/bin/zephyr /usr/local/bin/zephyr
COPY --from=rust /out/lib/libzephyr_policy.so /usr/local/lib/libzephyr_policy.so
COPY --from=rust /out/ext/ /opt/zephyr/ext/
COPY --from=rpi-ext /out/ext/ /opt/zephyr/ext/
COPY --from=plugins /out/ext/ /opt/zephyr/ext/
COPY --from=browser /out/browser/ /opt/zephyr/browser/
# The real agent-browser, out of the agent's PATH: `agent-browser` there is zephyr's approvals
# wrapper (approvals spec §4.4), which runs this one once the click is allowed.
COPY --from=browser /out/bin/agent-browser /opt/zephyr/libexec/agent-browser
COPY --from=python /opt/zephyr/py/ /opt/zephyr/py/
COPY --from=python /opt/zephyr/camoufox/ /opt/zephyr/camoufox/
COPY --from=python /usr/local/bin/uv /usr/local/bin/uv
# signal/ when built, else nothing: no empty /opt/zephyr/signal.
COPY --from=signal-out /out/ /opt/zephyr/
# ffmpeg, whisper-cli, Piper and their models (DESIGN §4.4), run as nobody; none with VOICE=0.
COPY --from=voice-out /opt/zephyr/ /opt/zephyr/
# The desktop's window manager config, root's (DESIGN §4.6): openbox never reads the agent's.
COPY deploy/openbox/ /opt/zephyr/openbox/
# The connector catalog and the starter skills (DESIGN §4.22, §4.23): root's, read by the agent,
# written by nobody but the image.
COPY deploy/connectors/ /opt/zephyr/connectors/
COPY deploy/skills/ /opt/zephyr/skills/
# `/opt/zephyr/gate/bin` (root's, 0755, holding only the agent-browser and xdotool wrappers) is
# first on the agent's PATH (agent::AGENT_PATH), so one the agent drops in its own bin does not
# shadow them; `zephyr doctor` checks the layout. The real xdotool stays at /usr/bin/xdotool.
RUN ln -s /opt/zephyr/camoufox/camoufox-python /usr/local/bin/camoufox-python \
    && ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && chmod -R u=rwX,go=rX /opt/zephyr/connectors /opt/zephyr/skills \
    && mkdir -p /opt/zephyr/gate/bin \
    && chmod 755 /opt/zephyr/libexec /opt/zephyr/gate /opt/zephyr/gate/bin \
    && ln -s /usr/local/bin/zephyr /opt/zephyr/gate/bin/agent-browser \
    && ln -s /usr/local/bin/zephyr /opt/zephyr/gate/bin/xdotool \
    && groupadd --gid 65533 zephyr-call \
    && useradd --uid 65533 --gid 65533 --no-create-home --home-dir /nonexistent \
       --shell /usr/sbin/nologin zephyr-call
# zephyr-call (above): the uid a local phone call's whisper-server and Piper run as (DESIGN
# §4.26), beside nobody's voice notes; no shell, no home, and no network under the floor.
# The token broker's CA (DESIGN §4.21) is made when a container starts, so the image only says
# where it will be: Camoufox installs the certificate its policies name (Chromium takes it from
# the agent's NSS store, with certutil from libnss3-tools; everything else from the system bundle).
RUN for p in /opt/zephyr/camoufox/cache/camoufox/browsers/official/*/distribution/policies.json; do \
      jq '.policies.Certificates.Install = ["/var/lib/zephyr-public/ca.pem"]' "$p" > "$p.new" \
      && mv "$p.new" "$p" && chmod 644 "$p"; \
    done
# The zephyr binary runs its daemon commands here, and the host commands outside.
ENV ZEPHYR_IN_CONTAINER=1
# The daemon starts an X display here for the agent (§4.11); every child inherits these.
# Chromium's own sandbox cannot start under --cap-drop ALL and no-new-privileges: the container
# is its sandbox. Vulkan reaches the GPU headless and on Xvfb alike (gl-egl falls back to llvmpipe).
# The agent installs into its own home, which outlives the image: `uv tool install` and
# `npm i -g` land in /zephyr/.local (its HOME is set at `docker run`). Only the agent's processes
# get /zephyr/.local/bin on their PATH (agent::AGENT_PATH): the agent writes there, so on root's
# PATH it would let the agent plant a `git` or `zephyr` that root then runs.
ENV PATH=/opt/zephyr/py/bin:$PATH \
    NPM_CONFIG_PREFIX=/zephyr/.local \
    ZEPHYR_DESKTOP=:99 \
    AGENT_BROWSER_EXECUTABLE_PATH=/opt/zephyr/browser/chrome \
    AGENT_BROWSER_PROFILE=/zephyr/browser \
    AGENT_BROWSER_HEADED=1 \
    AGENT_BROWSER_ARGS=--no-sandbox,--disable-dev-shm-usage,--use-angle=vulkan,--enable-features=Vulkan,--ignore-gpu-blocklist
# `docker run --init` makes docker-init PID 1; supervise starts `zephyr run` again if it crashes,
# so the agent's background commands (kept and reaped by PID 1) outlive a daemon crash.
# `zephyr status` fails when the daemon does not answer or its heartbeat is over 120 s old. It
# runs as the container's user (root) with its environment, as `docker exec zephyr zephyr status`.
HEALTHCHECK --interval=60s --timeout=20s --start-period=120s --retries=3 CMD ["/usr/local/bin/zephyr","status"]
CMD ["/usr/local/bin/zephyr", "supervise"]
