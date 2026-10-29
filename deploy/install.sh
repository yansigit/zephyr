#!/usr/bin/env bash
# Installs or upgrades Zephyr for the zephyr account. Run from your own account:
#   sudo -u zephyr XDG_RUNTIME_DIR=/run/user/$(id -u zephyr) bash deploy/install.sh
# Build first (cargo build --release). Re-running upgrades binaries and keeps config, state and secrets.
set -euo pipefail
[ "$(id -un)" = zephyr ] || { echo "run as zephyr (see header)"; exit 1; }
REPO="$(cd "$(dirname "$0")/.." && pwd)"
RPI_SRC="${RPI_SRC:-$REPO/../.always-on/rpi-0.12.0/bin/rpi}"
RPI_VER="$("$RPI_SRC" --version | awk '{print $2}')"

install -Dm755 "$REPO/target/release/zephyr" ~/bin/zephyr
install -Dm644 "$REPO/target/release/libzephyr_policy.so" ~/lib/libzephyr_policy.so
install -Dm755 "$RPI_SRC" ~/rpi/"$RPI_VER"/bin/rpi
ln -sfn ~/rpi/"$RPI_VER" ~/rpi/current
mkdir -p ~/agent ~/.rpi/agent
chmod 700 ~/agent ~/.rpi

if [ ! -e ~/agent/zephyr.toml ]; then
  cat > ~/agent/zephyr.toml <<TOML
rpi = "$HOME/rpi/current/bin/rpi"
home = "$HOME/agent"
agent_dir = "$HOME/.rpi/agent"
policy_plugin = "$HOME/lib/libzephyr_policy.so"
# model = "provider/model-id"   # set after logging in a provider

[owners]
chat = ["$(id -u)"]

[attach]
port = 7790
token_file = "$HOME/agent/tui.token"
TOML
fi
[ -e ~/agent/tui.token ] || { umask 077; head -c 24 /dev/urandom | base64 | tr -d '/+=' > ~/agent/tui.token; }
[ -e ~/agent/secrets.env ] || { umask 077; printf '# KEY=value lines, loaded by the service (mode 0600)\n' > ~/agent/secrets.env; }

install -Dm644 "$REPO/deploy/zephyr.service" ~/.config/systemd/user/zephyr.service
systemctl --user daemon-reload
systemctl --user enable zephyr >/dev/null
echo "installed zephyr + rpi $RPI_VER. Not started. Next: log in a provider, set model, then: systemctl --user start zephyr"
