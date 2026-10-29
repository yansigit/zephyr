#!/usr/bin/env bash
# Removes Zephyr completely: service, account (with /home/zephyr: state, memory, credentials) and every
# ACL setup-account.sh added. Run once with sudo: sudo bash deploy/uninstall.sh
# Keep a copy of Zephyr's memory first if you want it: sudo cp -a /home/zephyr/agent/memory ~/zephyr-memory
set -euo pipefail
OWNER="${SUDO_USER:?run via sudo}"
PROJECTS="/home/$OWNER/Projects"

if id zephyr >/dev/null 2>&1; then
  sudo -u zephyr XDG_RUNTIME_DIR="/run/user/$(id -u zephyr)" systemctl --user disable --now zephyr 2>/dev/null || true
  loginctl disable-linger zephyr || true
  pkill -u zephyr || true; sleep 1; pkill -9 -u zephyr || true
fi

# Same walk as setup, with -x. Symlinks are skipped. Must run before userdel: afterwards the
# entries only show the old numeric uid, which a future account could inherit.
strip() {  # $1 find -type, $2 ACL entries to remove, $3 label
  local total count=0 n
  total=$(find "$PROJECTS" -type "$1" -printf . | wc -c)
  find "$PROJECTS" -type "$1" -print0 \
    | SPEC="$2" xargs -0 -r -n 5000 sh -c 'setfacl -x "$SPEC" -- "$@" 2>/dev/null; echo $#' sh \
    | while read -r n; do
        count=$((count + n))
        printf '\r%s: %d / %d (%d%%)' "$3" "$count" "$total" $((count * 100 / (total ? total : 1)))
      done
  echo
}
if id zephyr >/dev/null 2>&1; then
  strip d "u:zephyr,d:u:zephyr,d:u:$OWNER" "directories"
  strip f "u:zephyr" "files"
  setfacl -x u:zephyr "/home/$OWNER" 2>/dev/null || true
  userdel -r zephyr 2>/dev/null || userdel zephyr
fi
rm -rf /home/zephyr

# eCryptfs serves a stale cached ACL until the inode is evicted.
sync; echo 2 > /proc/sys/vm/drop_caches
left=$(getfacl -Rp "$PROJECTS" 2>/dev/null | grep -c 'zephyr' || true)
echo "zephyr removed. ACL entries still naming zephyr under Projects: $left"
echo "Left as is: /home/$OWNER stays mode 700 (Ubuntu's default is 750: chmod 750 /home/$OWNER), and"
echo "secret files setup made owner-only stay owner-only."
