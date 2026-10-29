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
# One pass sets both the access and the default ACL; this walks every file, so large build dirs take minutes.
setfacl -R --modify u:zephyr:rwX,d:u:zephyr:rwX "$PROJECTS"
# Secrets inside Projects stay owner-only. Add paths here as needed.
DENY=("$PROJECTS/.parity-env.sh" "$PROJECTS/.parity-lanes/archive")
while IFS= read -r -d '' f; do DENY+=("$f"); done < <(find "$PROJECTS" -name '.env*' -not -path '*/node_modules/*' -print0)
for p in "${DENY[@]}"; do
  [ -e "$p" ] && setfacl -R -x u:zephyr "$p" && { [ -d "$p" ] && setfacl -R -x d:u:zephyr "$p" || true; }
done
echo "zephyr account ready; linger on; Projects shared at $PROJECTS"
