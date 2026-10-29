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
# Sets the ACL on every dir and file with a progress line. Symlinks are skipped (as setfacl -R does),
# so a link pointing outside Projects never widens access.
apply() {  # $1 find -type, $2 ACL spec, $3 label
  local total done=0 n
  total=$(find "$PROJECTS" -type "$1" -printf . | wc -c)
  find "$PROJECTS" -type "$1" -print0 \
    | SPEC="$2" xargs -0 -r -n 5000 sh -c 'setfacl --modify "$SPEC" -- "$@"; echo $#' sh \
    | while read -r n; do
        done=$((done + n))
        printf '\r%s: %d / %d (%d%%)' "$3" "$done" "$total" $((done * 100 / (total ? total : 1)))
      done
  echo
}
apply d "u:zephyr:rwX,d:u:zephyr:rwX,d:u:$OWNER:rwX" "directories"   # owner keeps rw on files zephyr creates
apply f "u:zephyr:rwX" "files"
# Secrets inside Projects stay owner-only. Add paths here as needed.
DENY=("$PROJECTS/.parity-env.sh" "$PROJECTS/.parity-lanes/archive")
while IFS= read -r -d '' f; do DENY+=("$f"); done < <(find "$PROJECTS" -name '.env*' -not -path '*/node_modules/*' -print0)
# Owner-only mode bits zero the ACL mask, which overrides any named zephyr entry. Removing ACL
# entries (setfacl -x) does not reliably stick on eCryptfs homes, so don't rely on it.
for p in "${DENY[@]}"; do
  [ -e "$p" ] && chmod -R go-rwx "$p"
done
# eCryptfs keeps a stale cached ACL after removal until the inode is evicted; force a reload.
sync; echo 2 > /proc/sys/vm/drop_caches
echo "zephyr account ready; linger on; Projects shared at $PROJECTS"
