# The self-hosted CI runner (deploy/runner/start.sh builds it as zephyr-runner:local): GitHub's
# runner image with everything .github/workflows needs baked in, so a job on zephyr-local installs
# nothing. The tests' tools are the ones GitHub's ubuntu runners have (tmux, python3, node, script),
# so no test skips here that runs there.
FROM rust:1.95-bookworm AS rust

FROM ghcr.io/actions/actions-runner:2.338.2
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev python3 tmux \
       git shellcheck curl ca-certificates nodejs procps \
    && rm -rf /var/lib/apt/lists/*
# The toolchain deploy/Dockerfile builds with (bookworm's glibc is older than 24.04's, so it runs here).
COPY --from=rust --chown=runner:runner /usr/local/rustup /usr/local/rustup
COPY --from=rust --chown=runner:runner /usr/local/cargo /usr/local/cargo
# Pinned releases, checked against their published sha256s.
ARG NEXTEST=0.9.146 NEXTEST_SHA256=682c21b777c333e96fd532e114d3a5a894e0729ab88d94c0a9f20f8419695428
ARG DENY=0.20.2 DENY_SHA256=9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f
ARG GH=2.103.2 GH_SHA256=62ccf747115ddc61237efffdde66a3c8f70ba249433dc64f70801089b59c59c8
RUN set -e; cd /tmp \
    && f=cargo-nextest-$NEXTEST-x86_64-unknown-linux-gnu.tar.gz \
    && curl -fsSLo $f https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-$NEXTEST/$f \
    && echo "$NEXTEST_SHA256  $f" | sha256sum -c - && tar xzf $f -C /usr/local/bin cargo-nextest \
    && f=cargo-deny-$DENY-x86_64-unknown-linux-musl.tar.gz \
    && curl -fsSLo $f https://github.com/EmbarkStudios/cargo-deny/releases/download/$DENY/$f \
    && echo "$DENY_SHA256  $f" | sha256sum -c - \
    && tar xzf $f --strip-components 1 -C /usr/local/bin cargo-deny-$DENY-x86_64-unknown-linux-musl/cargo-deny \
    && f=gh_${GH}_linux_amd64.tar.gz \
    && curl -fsSLo $f https://github.com/cli/cli/releases/download/v$GH/$f \
    && echo "$GH_SHA256  $f" | sha256sum -c - \
    && tar xzf $f --strip-components 2 -C /usr/local/bin gh_${GH}_linux_amd64/bin/gh \
    && rm -f /tmp/*.tar.gz
# The base image leaves the home world-writable; a test checks that no folder above its temp dir
# (RUNNER_TEMP, under it) is, as on GitHub's runners.
RUN chmod 755 /home/runner
# No sudo for jobs: the base image's runner is in group sudo with no password. start.sh also runs
# the container with no-new-privileges, under which sudo cannot work at all.
RUN gpasswd -d runner sudo
USER runner
# Each container serves one job and is removed (start.sh): the target dir, the cargo registry and
# everything else a job writes go with it.
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH CARGO_TARGET_DIR=/home/runner/_work/_target
RUN mkdir -p /home/runner/_work && rustup component add rustfmt clippy && cargo nextest --version \
    && cargo deny --version && gh --version && shellcheck --version && tmux -V && node --version
