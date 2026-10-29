# Zephyr in one container (DESIGN §4.4). Built and started by deploy/run.sh.
# Same base as the host (Mint 22 = Ubuntu 24.04) so the host-built binaries' glibc matches.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates git curl python3 build-essential \
    && rm -rf /var/lib/apt/lists/*
COPY target/release/zephyr /usr/local/bin/zephyr
COPY target/release/libzephyr_policy.so /usr/local/lib/libzephyr_policy.so
COPY --from=rpi rpi /usr/local/bin/rpi
# rpi-cursor-lite runs on node. Built on bookworm (glibc 2.36), older than this base's, so it runs here.
COPY --from=node:24-bookworm-slim /usr/local/bin/node /usr/local/bin/node
# Provider extensions staged by deploy/run.sh (--build-context ext=...); the only source of providers.
COPY --from=ext . /opt/zephyr/ext/
CMD ["zephyr", "run"]
