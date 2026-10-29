#!/usr/bin/env bash
# Builds the image and (re)starts the `zephyr` container. Run as yourself, after cargo build --release.
# Re-running upgrades: config, state, memory and credentials live in ~/.zephyr and are kept.
#
# The container runs as your uid with host networking. It sees only ~/.zephyr (as /zephyr) and
# $PROJECTS (same path); secrets inside $PROJECTS are masked with empty mounts.
#   log in:     docker exec -it zephyr rpi auth login   (then set `model` in ~/.zephyr/zephyr.toml)
#   chat:       docker exec -it zephyr zephyr chat
#   stop:       docker stop zephyr        (it restarts with Docker unless stopped)
#   uninstall:  docker rm -f zephyr && docker rmi zephyr && rm -rf ~/.zephyr
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
RPI_SRC="${RPI_SRC:-$REPO/../.always-on/rpi-0.12.0/bin/rpi}"
PROJECTS="${PROJECTS:-$HOME/Projects}"
DATA="$HOME/.zephyr"

# Extensions baked into the image (DESIGN §4.4), so providers never come from ~/.zephyr.
# RPI_PROVIDERS_SO (the librpi_providers.so file) and CURSOR_PKG (the rpi-cursor-lite dir) are optional.
EXT="$(mktemp -d)"
trap 'rm -rf "$EXT"' EXIT
EXT_PATHS=()
if [ -n "${RPI_PROVIDERS_SO:-}" ]; then
  cp "$RPI_PROVIDERS_SO" "$EXT/librpi_providers.so"
  EXT_PATHS+=("/opt/zephyr/ext/librpi_providers.so")
fi
if [ -n "${CURSOR_PKG:-}" ]; then
  cp -a "$CURSOR_PKG" "$EXT/rpi-cursor-lite"
  EXT_PATHS+=("/opt/zephyr/ext/rpi-cursor-lite/librpi_cursor_lite.so")
fi
EXT_LINE=""
if [ ${#EXT_PATHS[@]} -gt 0 ]; then
  EXT_LINE="extensions = [$(printf '"%s",' "${EXT_PATHS[@]}" | sed 's/,$//')]"
fi

docker build -t zephyr -f "$REPO/deploy/Dockerfile" --build-context rpi="$(dirname "$RPI_SRC")" \
  --build-context ext="$EXT" "$REPO"

mkdir -p "$DATA/.rpi/agent"
chmod 700 "$DATA"
if [ ! -e "$DATA/zephyr.toml" ]; then
  cat > "$DATA/zephyr.toml" <<TOML
rpi = "/usr/local/bin/rpi"
home = "/zephyr"
agent_dir = "/zephyr/.rpi/agent"
policy_plugin = "/usr/local/lib/libzephyr_policy.so"
$EXT_LINE
# model = "provider/model-id"   # set after logging in a provider

[owners]
chat = ["$(id -u)"]

[attach]
port = 7790
token_file = "/zephyr/tui.token"
TOML
elif [ -n "$EXT_LINE" ] && ! grep -q '^extensions' "$DATA/zephyr.toml"; then
  echo "add this line to $DATA/zephyr.toml (not edited here): $EXT_LINE"
fi
[ -e "$DATA/tui.token" ] || { umask 077; head -c 24 /dev/urandom | base64 | tr -d '/+=' > "$DATA/tui.token"; }
[ -e "$DATA/secrets.env" ] || { umask 077; printf '%s\n' '# KEY=value lines; re-run run.sh after editing' \
  '# OPENCODE_API_KEY=' '# CURSOR_API_KEY=' '# RPI_ANTIGRAVITY_OAUTH_CLIENT_ID=' '# RPI_ANTIGRAVITY_OAUTH_CLIENT_SECRET=' > "$DATA/secrets.env"; }

# shortcut: masks are computed at each run, so a secret created later stays visible until the next run.
MASKS=()
mask() { if [ -d "$1" ]; then MASKS+=(--mount "type=tmpfs,dst=$1"); elif [ -e "$1" ]; then MASKS+=(--mount "type=bind,src=/dev/null,dst=$1,readonly"); fi; }
mask "$PROJECTS/.parity-env.sh"
mask "$PROJECTS/.parity-lanes/archive"
while IFS= read -r -d '' f; do mask "$f"; done < <(find "$PROJECTS" -name node_modules -prune -o -name '.env*' -print0)
echo "masking $((${#MASKS[@]} / 2)) secret paths"

docker rm -f zephyr >/dev/null 2>&1 || true
docker run -d --name zephyr --init --restart unless-stopped --stop-timeout 30 \
  --network host --user "$(id -u):$(id -g)" \
  -e HOME=/zephyr -e ZEPHYR_CONFIG=/zephyr/zephyr.toml -e RPI_CODING_AGENT_DIR=/zephyr/.rpi/agent \
  --env-file "$DATA/secrets.env" \
  --mount "type=bind,src=$DATA,dst=/zephyr" \
  --mount "type=bind,src=$PROJECTS,dst=$PROJECTS" \
  "${MASKS[@]}" -w /zephyr zephyr
echo "started. Logs: docker logs -f zephyr"
