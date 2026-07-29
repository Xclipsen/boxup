#!/bin/sh
set -eu
umask 077

usage() {
  printf '%s\n' 'Usage: setup-backup-sudo HOST BROWSE_USER' >&2
  exit 2
}

[ "$#" -eq 2 ] || usage
[ "$(id -u)" -eq 0 ] || {
  printf '%s\n' 'Run this helper as root, directly or through sudo.' >&2
  exit 1
}

profile=$1
browse_user=$2
case "$profile" in
  ''|*[!A-Za-z0-9_-]*|[-_]*|*[-_]) printf '%s\n' 'Invalid profile name.' >&2; exit 1 ;;
esac
case "$browse_user" in
  ''|-*|*[!A-Za-z0-9_.-]*) printf '%s\n' 'Invalid browse user.' >&2; exit 1 ;;
esac
browse_entry=$(getent passwd "$browse_user") || {
  printf 'Browse user does not exist: %s\n' "$browse_user" >&2
  exit 1
}
browse_user=$(printf '%s\n' "$browse_entry" | cut -d: -f1)
browse_uid=$(printf '%s\n' "$browse_entry" | cut -d: -f3)
case "$browse_user" in
  ''|-*|*[!A-Za-z0-9_.-]*) printf '%s\n' 'The browse user could not be resolved safely.' >&2; exit 1 ;;
esac
case "$browse_uid" in
  ''|*[!0-9]*) printf '%s\n' 'The browse user UID is invalid.' >&2; exit 1 ;;
esac
[ "$(printf '%s' "$browse_user" | tr '[:lower:]' '[:upper:]')" != ALL ] || {
  printf '%s\n' 'The reserved sudoers principal ALL cannot be a browse user.' >&2
  exit 1
}

config="/etc/boxup/$profile.toml"
/usr/lib/boxup/boxup-root --config "$config" validate-config

sudoers_dir=/etc/sudoers.d
[ -d "$sudoers_dir" ] && [ ! -L "$sudoers_dir" ] || {
  printf '%s\n' '/etc/sudoers.d must be an existing non-symlink directory.' >&2
  exit 1
}
[ "$(stat -c %u "$sudoers_dir")" -eq 0 ] && \
  [ $((0$(stat -c %a "$sudoers_dir") & 0022)) -eq 0 ] || {
  printf '%s\n' '/etc/sudoers.d must be root-owned and not group/other writable.' >&2
  exit 1
}

visudo=
for candidate in /usr/sbin/visudo /usr/bin/visudo; do
  if [ -x "$candidate" ]; then
    visudo=$candidate
    break
  fi
done
[ -n "$visudo" ] || {
  printf '%s\n' 'visudo is required to provision passwordless backup.' >&2
  exit 1
}

target="$sudoers_dir/boxup-backup-$profile-$browse_uid"
temporary=$(mktemp "$sudoers_dir/.boxup-backup.XXXXXX")
trap 'rm -f "$temporary"' EXIT HUP INT TERM
printf '%s ALL=(root) NOPASSWD: /usr/lib/boxup/boxup-root --config /etc/boxup/%s.toml backup --progress-json\n' \
  "$browse_user" "$profile" >"$temporary"
chmod 0440 "$temporary"
"$visudo" -cf "$temporary" >/dev/null

if [ -e "$target" ] || [ -L "$target" ]; then
  if [ -f "$target" ] && [ ! -L "$target" ] && \
    [ "$(stat -c %u "$target")" -eq 0 ] && \
    [ "$(stat -c %a "$target")" = 440 ] && cmp -s "$temporary" "$target"; then
    printf 'Passwordless backup is already configured for %s and %s.\n' \
      "$profile" "$browse_user"
    exit 0
  fi
  printf 'Existing sudoers rule differs; refusing to overwrite: %s\n' "$target" >&2
  exit 1
fi

install -m 0440 -o root -g root "$temporary" "$target"
rm -f "$temporary"
trap - EXIT HUP INT TERM
printf 'Configured passwordless foreground backup for %s and %s only.\n' \
  "$profile" "$browse_user"
