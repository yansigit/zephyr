#!/usr/bin/env bash
# Creates the `zephyr` account (DESIGN.md §4.4). Run once with sudo: sudo bash deploy/setup-account.sh
set -euo pipefail
OWNER="${SUDO_USER:?run via sudo}"
PROJECTS="/home/$OWNER/Projects"

id zephyr >/dev/null 2>&1 || useradd --create-home --shell /bin/bash zephyr
gpasswd -d zephyr docker 2>/dev/null || true   # docker access is root-equivalent
gpasswd -d zephyr sudo 2>/dev/null || true
chmod 700 "/home/$OWNER"
loginctl enable-linger zephyr

# Shared Projects access: rwX now and for new files. Revoke with: setfacl -R -x u:zephyr "$PROJECTS"
setfacl --modify u:zephyr:x "/home/$OWNER"          # traverse only, cannot list $HOME
setfacl -R --modify u:zephyr:rwX "$PROJECTS"
setfacl -R --modify d:u:zephyr:rwX "$PROJECTS"
for p in .ssh .rpi .pi .gnupg .config .local .mozilla; do
  [ -e "/home/$OWNER/$p" ] && setfacl -R -x u:zephyr "/home/$OWNER/$p" 2>/dev/null || true
done
echo "zephyr account ready; linger on; Projects shared at $PROJECTS"
